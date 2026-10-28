//! Width lookup fused with UTF-8 decoding.
//!
//! Print runs arrive validated (the parser's ground state has already
//! checked them), so decoding is arithmetic on the lead byte. The width is
//! read from one table indexed by the decoded codepoint instead of the
//! two-stage grapheme table plus the join test: each BMP codepoint has two
//! bits, `0` (take the general path: joins a cluster, zero width, or a
//! control), `1` (one column, starts its own cluster) or `2` (two columns,
//! starts its own cluster). Codepoints past the BMP always take the general
//! path; the table is 16 KiB, built once per process from the same
//! grapheme table the general path uses, so the two cannot disagree.
//!
//! On aarch64, runs of three-byte characters (CJK, kana, Hangul) are
//! classified sixteen at a time with NEON: a structure load splits lead and
//! continuation bytes, and two 2048-bit maps over 32-codepoint blocks say
//! which codepoints are two-column, non-joining characters: one for all but
//! the block's first codepoint, one for that first codepoint. So CJK
//! punctuation (U+3001.., U+FF01..) stays in the vector runs while the
//! marks and holes next to it do not. Other targets use the same maps one
//! character at a time.

use std::sync::OnceLock;

use alacritty_terminal::term::grapheme::{Info, may_join};

pub struct Tables {
    /// Two bits per BMP codepoint.
    cp2: Box<[u8; 0x4000]>,
    /// One bit per 32-codepoint BMP block: codepoints 1..32 of the block
    /// are all class 2.
    wide_rest: [u8; 256],
    /// One bit per 32-codepoint BMP block: its first codepoint is class 2.
    wide_first: [u8; 256],
}

pub fn tables() -> &'static Tables {
    static TABLES: OnceLock<Tables> = OnceLock::new();
    TABLES.get_or_init(build)
}

fn class_of(cp: u32) -> u8 {
    if cp < 0xA0 {
        return 0;
    }
    let Some(c) = char::from_u32(cp) else {
        return 0;
    };
    let info = Info::of(c);
    if may_join(c, info) {
        return 0;
    }
    match info.width() {
        1 => 1,
        2 => 2,
        _ => 0,
    }
}

fn build() -> Tables {
    let mut cp2 = Box::new([0u8; 0x4000]);
    let mut wide_rest = [0u8; 256];
    let mut wide_first = [0u8; 256];
    for block in 0..2048u32 {
        let mut rest = true;
        for cp in block * 32..block * 32 + 32 {
            let class = class_of(cp);
            if cp % 32 != 0 {
                rest &= class == 2;
            }
            cp2[(cp >> 2) as usize] |= class << ((cp & 3) * 2);
        }
        let first = class_of(block * 32) == 2;
        // Surrogates and overlong forms never decode as valid, and must not
        // pass the vector path's check (which does not look for them).
        let invalid = block * 32 < 0x800 || (0xD800..0xE000).contains(&(block * 32));
        let bit = 1 << (block & 7);
        if rest && !invalid {
            wide_rest[(block >> 3) as usize] |= bit;
        }
        if first && !invalid {
            wide_first[(block >> 3) as usize] |= bit;
        }
    }
    Tables {
        cp2,
        wide_rest,
        wide_first,
    }
}

impl Tables {
    /// Validates and decodes the character at `bytes[0]`, returning
    /// `(codepoint, byte length, class)`; `None` for ASCII, invalid or
    /// incomplete UTF-8. Characters past the BMP are class 0.
    #[inline(always)]
    pub fn decode(&self, bytes: &[u8]) -> Option<(u32, usize, u8)> {
        let cont = |i: usize| bytes.get(i).map(|&b| b as u32 ^ 0x80).filter(|&b| b < 0x40);
        let b0 = bytes[0] as u32;
        if b0 < 0xC2 {
            None
        } else if b0 < 0xE0 {
            let cp = (b0 & 0x1F) << 6 | cont(1)?;
            Some((cp, 2, self.class(cp)))
        } else if b0 < 0xF0 {
            let cp = (b0 & 0x0F) << 12 | cont(1)? << 6 | cont(2)?;
            // Overlong forms and surrogates are invalid.
            if cp < 0x800 || (0xD800..0xE000).contains(&cp) {
                return None;
            }
            Some((cp, 3, self.class(cp)))
        } else if b0 < 0xF5 {
            let cp = (b0 & 0x07) << 18 | cont(1)? << 12 | cont(2)? << 6 | cont(3)?;
            if !(0x1_0000..0x11_0000).contains(&cp) {
                return None;
            }
            Some((cp, 4, 0))
        } else {
            None
        }
    }

    /// Whether the vector path takes three-byte character `cp`: by the two
    /// 32-codepoint maps, always a two-column, non-joining character.
    #[inline(always)]
    pub fn vector_accepts(&self, cp: u32) -> bool {
        let block = (cp >> 5) as usize;
        let map = if cp & 31 == 0 {
            &self.wide_first
        } else {
            &self.wide_rest
        };
        (0x800..0x1_0000).contains(&cp) && map[block >> 3] & (1 << (block & 7)) != 0
    }

    #[inline(always)]
    fn class(&self, cp: u32) -> u8 {
        (self.cp2[(cp >> 2) as usize] >> ((cp & 3) * 2)) & 3
    }

    /// The number of leading three-byte, two-column, non-joining characters
    /// among the next sixteen, writing their codepoints to `out`. `bytes`
    /// holds at least 48 bytes; they need not be valid UTF-8.
    #[cfg(target_arch = "aarch64")]
    #[inline(always)]
    pub fn wide_run16(&self, bytes: &[u8], out: &mut [u32; 16]) -> usize {
        use std::arch::aarch64::*;
        debug_assert!(bytes.len() >= 48);
        // SAFETY: NEON is baseline on aarch64; 48 bytes are in bounds.
        unsafe {
            let v = vld3q_u8(bytes.as_ptr());
            let (b0, b1, b2) = (v.0, v.1, v.2);
            // Three-byte leads with two continuation bytes. Overlong forms
            // land in blocks below U+0800 and surrogates in U+D800..DFFF,
            // none of which the map accepts, so this also validates.
            let lead = vandq_u8(
                vceqq_u8(vandq_u8(b0, vdupq_n_u8(0xF0)), vdupq_n_u8(0xE0)),
                vandq_u8(
                    vceqq_u8(vandq_u8(b1, vdupq_n_u8(0xC0)), vdupq_n_u8(0x80)),
                    vceqq_u8(vandq_u8(b2, vdupq_n_u8(0xC0)), vdupq_n_u8(0x80)),
                ),
            );
            // Block = cp >> 5; its byte in a map is cp >> 8 =
            // (b0 & 0xF) << 4 | (b1 & 0x3F) >> 2 (0..256, four 64-byte
            // lookups, an index past a table reading as zero), its bit
            // (cp >> 5) & 7 = (b1 & 3) << 1 | (b2 >> 5) & 1.
            let index = vorrq_u8(
                vshlq_n_u8::<4>(vandq_u8(b0, vdupq_n_u8(0x0F))),
                vshrq_n_u8::<2>(vandq_u8(b1, vdupq_n_u8(0x3F))),
            );
            let lookup = |map: &[u8; 256]| {
                let t = map.as_ptr();
                let part = |at: usize| {
                    uint8x16x4_t(
                        vld1q_u8(t.add(at)),
                        vld1q_u8(t.add(at + 16)),
                        vld1q_u8(t.add(at + 32)),
                        vld1q_u8(t.add(at + 48)),
                    )
                };
                vorrq_u8(
                    vorrq_u8(
                        vqtbl4q_u8(part(0), index),
                        vqtbl4q_u8(part(64), vsubq_u8(index, vdupq_n_u8(64))),
                    ),
                    vorrq_u8(
                        vqtbl4q_u8(part(128), vsubq_u8(index, vdupq_n_u8(128))),
                        vqtbl4q_u8(part(192), vsubq_u8(index, vdupq_n_u8(192))),
                    ),
                )
            };
            // The first codepoint of a block (b2 & 0x1F == 0) reads the
            // other map.
            let first = vceqq_u8(vandq_u8(b2, vdupq_n_u8(0x1F)), vdupq_n_u8(0));
            let map = vbslq_u8(first, lookup(&self.wide_first), lookup(&self.wide_rest));
            let bit = vshlq_u8(
                vdupq_n_u8(1),
                vreinterpretq_s8_u8(vorrq_u8(
                    vshlq_n_u8::<1>(vandq_u8(b1, vdupq_n_u8(3))),
                    vandq_u8(vshrq_n_u8::<5>(b2), vdupq_n_u8(1)),
                )),
            );
            let ok = vandq_u8(lead, vtstq_u8(map, bit));
            // Four mask bits per lane; count the leading accepted lanes.
            let mask = vget_lane_u64::<0>(vreinterpret_u64_u8(vshrn_n_u16::<4>(
                vreinterpretq_u16_u8(ok),
            )));
            let count = ((!mask).trailing_zeros() / 4) as usize;
            if count == 0 {
                return 0;
            }
            // Codepoints: (b0 & 0xF) << 12 | (b1 & 0x3F) << 6 | (b2 & 0x3F).
            let hi = vandq_u8(b0, vdupq_n_u8(0x0F));
            let mid = vandq_u8(b1, vdupq_n_u8(0x3F));
            let lo = vandq_u8(b2, vdupq_n_u8(0x3F));
            let lanes = |h: uint8x8_t, m: uint8x8_t, l: uint8x8_t| {
                vorrq_u16(
                    vorrq_u16(vshll_n_u8::<8>(vshl_n_u8::<4>(h)), vshll_n_u8::<6>(m)),
                    vmovl_u8(l),
                )
            };
            let first = lanes(vget_low_u8(hi), vget_low_u8(mid), vget_low_u8(lo));
            let second = lanes(vget_high_u8(hi), vget_high_u8(mid), vget_high_u8(lo));
            let p = out.as_mut_ptr();
            vst1q_u32(p, vmovl_u16(vget_low_u16(first)));
            vst1q_u32(p.add(4), vmovl_u16(vget_high_u16(first)));
            vst1q_u32(p.add(8), vmovl_u16(vget_low_u16(second)));
            vst1q_u32(p.add(12), vmovl_u16(vget_high_u16(second)));
            count
        }
    }

    /// The same, one character at a time.
    #[cfg(not(target_arch = "aarch64"))]
    #[inline(always)]
    pub fn wide_run16(&self, bytes: &[u8], out: &mut [u32; 16]) -> usize {
        debug_assert!(bytes.len() >= 48);
        for (k, slot) in out.iter_mut().enumerate() {
            let (b0, b1, b2) = (bytes[3 * k], bytes[3 * k + 1], bytes[3 * k + 2]);
            if b0 & 0xF0 != 0xE0 || b1 & 0xC0 != 0x80 || b2 & 0xC0 != 0x80 {
                return k;
            }
            let cp = ((b0 & 0x0F) as u32) << 12 | ((b1 & 0x3F) as u32) << 6 | (b2 & 0x3F) as u32;
            if !self.vector_accepts(cp) {
                return k;
            }
            *slot = cp;
        }
        16
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fused_classes_match_the_grapheme_table() {
        let tables = tables();
        for cp in 0xA0u32..0x1_0000 {
            let Some(c) = char::from_u32(cp) else {
                continue;
            };
            let mut buf = [0u8; 4];
            let (decoded, len, class) = tables
                .decode(c.encode_utf8(&mut buf).as_bytes())
                .expect("valid");
            assert_eq!((decoded, len), (cp, c.len_utf8()));
            assert_eq!(class, class_of(cp), "{cp:#x}");
        }
    }

    #[test]
    fn the_vector_path_takes_only_two_column_non_joining_characters() {
        let tables = tables();
        let mut taken = 0;
        for cp in 0x800u32..0x1_0000 {
            let class = char::from_u32(cp).map_or(0, |_| class_of(cp));
            if tables.vector_accepts(cp) {
                assert_eq!(class, 2, "{cp:#x}");
                taken += 1;
            }
        }
        assert!(taken > 40_000, "{taken}");
        // The punctuation the 64-codepoint map used to send to the scalar
        // path, and the holes beside it.
        for (cp, want) in [
            (0x3001, true),
            (0x3002, true),
            (0xFF0C, true),
            (0xFF00, false),
            (0x302A, false),
            (0x3040, false),
            (0x3041, true),
        ] {
            assert_eq!(tables.vector_accepts(cp), want, "{cp:#x}");
        }
        // NEON and scalar agree with `vector_accepts` on every three-byte
        // character, sixteen at a time, at every lane.
        let mut out = [0u32; 16];
        for start in (0x800u32..0x1_0000 - 16).step_by(7) {
            if (start..start + 16).any(|cp| char::from_u32(cp).is_none()) {
                continue;
            }
            let mut bytes = Vec::new();
            for k in 0..16 {
                let cp = start + k;
                let c = char::from_u32(cp).expect("a scalar");
                let mut buf = [0u8; 4];
                bytes.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
            if bytes.len() != 48 {
                continue;
            }
            let want = (0..16u32)
                .take_while(|&k| {
                    char::from_u32(start + k).is_some() && tables.vector_accepts(start + k)
                })
                .count();
            assert_eq!(tables.wide_run16(&bytes, &mut out), want, "{start:#x}");
            for (k, &cp) in out[..want].iter().enumerate() {
                assert_eq!(cp, start + k as u32);
            }
        }
        // Surrogates and overlong forms, which do not decode, never pass.
        for bad in [[0xEDu8, 0xA0, 0x80], [0xE0, 0x80, 0x80], [0xE0, 0x9F, 0xBF]] {
            let bytes: Vec<u8> = bad.iter().copied().cycle().take(48).collect();
            assert_eq!(tables.wide_run16(&bytes, &mut out), 0, "{bad:x?}");
        }
    }

    #[test]
    fn invalid_utf8_is_rejected() {
        let tables = tables();
        for bad in [
            &b"\xC0\x80"[..],
            b"\xC1\xBF",
            b"\xE0\x80\x80",
            b"\xED\xA0\x80",
            b"\xF4\x90\x80\x80",
            b"\xF5\x80\x80\x80",
            b"\xE4\xB8",
            b"\xE4\x41\x80",
            b"\x80",
        ] {
            assert_eq!(tables.decode(bad), None, "{bad:x?}");
        }
        let mut out = [0u32; 16];
        let mut bytes = "漢".repeat(16).into_bytes();
        bytes[3 * 5 + 2] = b'A';
        assert_eq!(tables.wide_run16(&bytes, &mut out), 5);
    }

    #[test]
    fn wide_runs_stop_at_the_first_other_character() {
        let tables = tables();
        let mut out = [0u32; 16];
        let text: String = "漢字かな交じり文、日本語の文章です。ありがとう".into();
        let bytes = text.as_bytes();
        let count = tables.wide_run16(&bytes[..48], &mut out);
        {
            // 漢字かな交じ stay in the run (か shares a 32-codepoint block
            // with the hole U+3040 but is not its first codepoint); り
            // shares one with the marks U+3099 and U+309A.
            let expected = text
                .chars()
                .take(16)
                .take_while(|&c| tables.vector_accepts(c as u32))
                .count();
            assert_eq!(count, expected);
            assert_eq!(count, 6, "CJK and kana stay in the run");
            for (i, c) in text.chars().take(count).enumerate() {
                assert_eq!(out[i], c as u32);
            }
        }
    }
}
