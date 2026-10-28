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
//! continuation bytes, and a 1024-bit map says which 64-codepoint blocks are
//! entirely two-column, non-joining characters.

use std::sync::OnceLock;

use alacritty_terminal::term::grapheme::{Info, may_join};

pub struct Tables {
    /// Two bits per BMP codepoint.
    cp2: Box<[u8; 0x4000]>,
    /// One bit per 64-codepoint BMP block: every codepoint is class 2.
    wide_blocks: [u8; 128],
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
    let mut wide_blocks = [0u8; 128];
    for block in 0..1024u32 {
        let mut all_wide = true;
        for cp in block * 64..block * 64 + 64 {
            let class = class_of(cp);
            all_wide &= class == 2;
            cp2[(cp >> 2) as usize] |= class << ((cp & 3) * 2);
        }
        // Surrogates never decode, so their blocks may count as wide.
        if (0xD800 >> 6..0xE000 >> 6).contains(&block) {
            all_wide = false;
        }
        if all_wide {
            wide_blocks[(block >> 3) as usize] |= 1 << (block & 7);
        }
    }
    Tables { cp2, wide_blocks }
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
            // Block = (b0 & 0xF) << 6 | (b1 & 0x3F); its byte in the map is
            // block >> 3 (0..128), its bit block & 7 == b1 & 7.
            let index = vorrq_u8(
                vshlq_n_u8::<3>(vandq_u8(b0, vdupq_n_u8(0x0F))),
                vshrq_n_u8::<3>(vandq_u8(b1, vdupq_n_u8(0x3F))),
            );
            let map = &self.wide_blocks;
            let low = vqtbl4q_u8(
                uint8x16x4_t(
                    vld1q_u8(map.as_ptr()),
                    vld1q_u8(map.as_ptr().add(16)),
                    vld1q_u8(map.as_ptr().add(32)),
                    vld1q_u8(map.as_ptr().add(48)),
                ),
                index,
            );
            let high = vqtbl4q_u8(
                uint8x16x4_t(
                    vld1q_u8(map.as_ptr().add(64)),
                    vld1q_u8(map.as_ptr().add(80)),
                    vld1q_u8(map.as_ptr().add(96)),
                    vld1q_u8(map.as_ptr().add(112)),
                ),
                // Indices below 64 wrap past 191 and read as zero.
                vsubq_u8(index, vdupq_n_u8(64)),
            );
            let bit = vshlq_u8(
                vdupq_n_u8(1),
                vreinterpretq_s8_u8(vandq_u8(b1, vdupq_n_u8(7))),
            );
            let ok = vandq_u8(lead, vtstq_u8(vorrq_u8(low, high), bit));
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

    #[cfg(not(target_arch = "aarch64"))]
    #[inline(always)]
    pub fn wide_run16(&self, _bytes: &[u8], _out: &mut [u32; 16]) -> usize {
        0
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
        if cfg!(target_arch = "aarch64") {
            assert_eq!(tables.wide_run16(&bytes, &mut out), 5);
        }
    }

    #[test]
    fn wide_runs_stop_at_the_first_other_character() {
        let tables = tables();
        let mut out = [0u32; 16];
        let text: String = "漢字かな交じり文、日本語の文章です。ありがとう".into();
        let bytes = text.as_bytes();
        let count = tables.wide_run16(&bytes[..48], &mut out);
        if cfg!(target_arch = "aarch64") {
            // 漢字 are CJK; か falls in a block with combining marks.
            let expected = text
                .chars()
                .take(16)
                .take_while(|&c| {
                    let cp = c as u32;
                    (0..64).all(|i| class_of((cp & !63) + i) == 2)
                })
                .count();
            assert_eq!(count, expected);
            for (i, c) in text.chars().take(count).enumerate() {
                assert_eq!(out[i], c as u32);
            }
        }
    }
}
