//! Grapheme clusters and their cell widths.
//!
//! Claude Code is a Bun binary and lays out its screen with `Bun.stringWidth`,
//! which measures whole grapheme clusters: `❤️` (heart + VS16) is two
//! columns, a ZWJ family `👨‍👩‍👧‍👦` is two, a skin-toned `👍🏽` is two
//! and a flag `🇷🇴` is one two-column glyph. A terminal that measures
//! codepoint by codepoint disagrees (1, 8, 4) and every relative cursor move
//! after such a cluster lands on the wrong cell, leaving debris on screen.
//!
//! [`grapheme_table`](super::grapheme_table) is Bun's own per-codepoint
//! classification and [`Cluster::width`] is its cluster width rule, so the
//! default layout matches what Claude Code assumes. Programs that enable
//! mode 2027 get the grapheme-clustering rule instead: a cluster is as wide
//! as its first codepoint unless a variation selector or emoji sequence
//! says otherwise.

use super::grapheme_table::{STAGE1, STAGE2};

const CLASS_MASK: u8 = 0x1F;
const WIDTH_SHIFT: u8 = 5;
const WIDTH_MASK: u8 = 0x3;
const WIDTH_AMBIGUOUS: u8 = 3;
const EMOJI_BIT: u8 = 0x80;

/// A codepoint's packed classification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Info(u8);

impl Info {
    #[inline]
    pub(crate) fn of(c: char) -> Self {
        let cp = c as usize;
        let block = STAGE1[cp >> 8] as usize;
        Self(STAGE2[(block << 8) | (cp & 0xFF)])
    }

    #[inline]
    pub(crate) fn class(self) -> Class {
        Class::from_bits(self.0 & CLASS_MASK)
    }

    /// Terminal columns of the codepoint alone; East Asian ambiguous is narrow.
    #[inline]
    pub(crate) fn width(self) -> usize {
        match (self.0 >> WIDTH_SHIFT) & WIDTH_MASK {
            WIDTH_AMBIGUOUS => 1,
            width => width as usize,
        }
    }

    #[inline]
    fn emoji(self) -> bool {
        self.0 & EMOJI_BIT != 0
    }
}

/// Grapheme break class, including the Indic conjunct break classes (UAX #29).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Class {
    Other,
    Prepend,
    RegionalIndicator,
    SpacingMark,
    L,
    V,
    T,
    Lv,
    Lvt,
    Zwj,
    Zwnj,
    ExtendedPictographic,
    EmojiModifierBase,
    EmojiModifier,
    IndicExtend,
    IndicLinker,
    IndicConsonant,
}

impl Class {
    const ALL: [Self; 17] = [
        Self::Other,
        Self::Prepend,
        Self::RegionalIndicator,
        Self::SpacingMark,
        Self::L,
        Self::V,
        Self::T,
        Self::Lv,
        Self::Lvt,
        Self::Zwj,
        Self::Zwnj,
        Self::ExtendedPictographic,
        Self::EmojiModifierBase,
        Self::EmojiModifier,
        Self::IndicExtend,
        Self::IndicLinker,
        Self::IndicConsonant,
    ];

    #[inline]
    fn from_bits(bits: u8) -> Self {
        Self::ALL
            .get(usize::from(bits))
            .copied()
            .unwrap_or(Self::Other)
    }

    #[inline]
    fn extend(self) -> bool {
        matches!(self, Self::Zwnj | Self::IndicExtend | Self::IndicLinker)
    }

    #[inline]
    fn indic_extend(self) -> bool {
        matches!(self, Self::IndicExtend | Self::Zwj)
    }

    #[inline]
    fn pictographic(self) -> bool {
        matches!(self, Self::ExtendedPictographic | Self::EmojiModifierBase)
    }

    /// Whether a codepoint of this class can continue the cluster before it.
    /// Everything else (letters, digits, CJK, …) always starts a new one,
    /// except after a Prepend, which this terminal never joins.
    #[inline]
    pub(crate) fn may_continue(self) -> bool {
        !matches!(self, Self::Other | Self::Prepend | Self::Lv | Self::Lvt)
    }
}

/// Whether `c` may change the cluster before it: the variation selectors,
/// the keycap mark and ZWJ, or a class that can continue a cluster.
#[inline]
pub(crate) fn may_join(c: char, info: Info) -> bool {
    is_presentation_mark(c) || info.class().may_continue()
}

/// Whether `c` in a cluster can bring an emoji width rule into play.
#[inline]
pub(crate) fn sets_presentation(c: char) -> bool {
    is_presentation_mark(c) || is_regional_indicator(c) || is_skin_tone(c)
}

/// Codepoints that change a cluster's width by their presence alone.
#[inline]
pub(crate) fn is_presentation_mark(c: char) -> bool {
    matches!(c, '\u{FE0E}' | '\u{FE0F}' | '\u{20E3}' | '\u{200D}')
}

/// State carried between [`breaks`] calls within one run of codepoints.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum BreakState {
    #[default]
    Default,
    RegionalIndicator,
    Pictographic,
    IndicConsonant,
    IndicLinker,
}

/// Whether a grapheme cluster boundary falls between consecutive codepoints
/// of classes `a` and `b` (UAX #29 including GB9c, as Bun implements it).
#[inline]
pub(crate) fn breaks(a: Class, b: Class, state: &mut BreakState) -> bool {
    use BreakState as S;
    use Class as G;

    match *state {
        S::RegionalIndicator if a != G::RegionalIndicator || b != G::RegionalIndicator => {
            *state = S::Default;
        }
        S::Pictographic => {
            let expected = |g: G| {
                g.extend()
                    || matches!(
                        g,
                        G::Zwj | G::ExtendedPictographic | G::EmojiModifierBase | G::EmojiModifier
                    )
            };
            if !expected(a) || !expected(b) {
                *state = S::Default;
            }
        }
        S::IndicConsonant | S::IndicLinker => {
            let expected = |g: G| {
                matches!(
                    g,
                    G::IndicConsonant | G::IndicLinker | G::IndicExtend | G::Zwj
                )
            };
            if !expected(a) || !expected(b) {
                *state = S::Default;
            }
        }
        _ => (),
    }

    // GB6-GB8: Hangul syllable sequences.
    if a == G::L && matches!(b, G::L | G::V | G::Lv | G::Lvt) {
        return false;
    }
    if matches!(a, G::Lv | G::V) && matches!(b, G::V | G::T) {
        return false;
    }
    if matches!(a, G::Lvt | G::T) && b == G::T {
        return false;
    }
    // GB9a, GB9b.
    if b == G::SpacingMark || a == G::Prepend {
        return false;
    }

    // GB9c: Indic conjuncts.
    if a == G::IndicConsonant {
        if b.indic_extend() {
            *state = S::IndicConsonant;
            return false;
        }
        if b == G::IndicLinker {
            *state = S::IndicLinker;
            return false;
        }
    } else if *state == S::IndicConsonant {
        if b == G::IndicLinker {
            *state = S::IndicLinker;
            return false;
        }
        if b.indic_extend() {
            return false;
        }
        *state = S::Default;
    } else if *state == S::IndicLinker {
        if b == G::IndicLinker || b.indic_extend() {
            return false;
        }
        *state = S::Default;
        if b == G::IndicConsonant {
            return false;
        }
    }

    // GB11: emoji ZWJ and modifier sequences.
    if a.pictographic() {
        if b.extend() || b == G::Zwj || (a == G::EmojiModifierBase && b == G::EmojiModifier) {
            *state = S::Pictographic;
            return false;
        }
    } else if *state == S::Pictographic {
        if (a.extend() || a == G::EmojiModifier) && (b.extend() || b == G::Zwj) {
            return false;
        }
        *state = S::Default;
        if a == G::Zwj && b.pictographic() {
            return false;
        }
    }

    // GB12, GB13: regional indicator pairs.
    if a == G::RegionalIndicator && b == G::RegionalIndicator {
        if *state == S::Default {
            *state = S::RegionalIndicator;
            return false;
        }
        *state = S::Default;
        return true;
    }

    // GB9, GB999.
    !(b.extend() || b == G::Zwj)
}

/// One grapheme cluster's width facts, accumulated codepoint by codepoint.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Cluster {
    first: char,
    base_width: u8,
    sum: u16,
    count: u8,
    emoji_base: bool,
    keycap: bool,
    regional: bool,
    skin_tone: bool,
    zwj: bool,
    vs15: bool,
    vs16: bool,
}

impl Cluster {
    #[inline]
    pub(crate) fn new(c: char, info: Info) -> Self {
        let width = info.width() as u8;
        Self {
            first: c,
            base_width: width,
            sum: u16::from(width),
            count: 1,
            emoji_base: info.emoji(),
            keycap: c == '\u{20E3}',
            regional: is_regional_indicator(c),
            skin_tone: is_skin_tone(c),
            zwj: c == '\u{200D}',
            vs15: false,
            vs16: false,
        }
    }

    #[inline]
    pub(crate) fn add(&mut self, c: char, info: Info) {
        self.count = self.count.saturating_add(1);
        self.keycap |= c == '\u{20E3}';
        self.regional |= is_regional_indicator(c);
        self.skin_tone |= is_skin_tone(c);
        self.zwj |= c == '\u{200D}';
        self.vs15 |= c == '\u{FE0E}';
        self.vs16 |= c == '\u{FE0F}';
        self.sum = self.sum.saturating_add(info.width() as u16).min(1023);
    }

    /// Whether an emoji or presentation rule decides the width, rather than
    /// the widths of the codepoints themselves.
    #[inline]
    pub(crate) fn presentation(&self) -> bool {
        self.regional
            || self.keycap
            || (self.emoji_base && (self.skin_tone || self.zwj))
            || self.vs15
            || self.vs16
    }

    /// Columns of the cluster: Bun's `stringWidth` rule by default, the
    /// mode 2027 rule when `clustering` is set.
    #[inline]
    pub(crate) fn width(&self, clustering: bool) -> usize {
        if self.regional && self.count >= 2 {
            return 2;
        }
        if self.keycap {
            return 2;
        }
        if self.regional {
            return 1;
        }
        if self.emoji_base && (self.skin_tone || self.zwj) {
            return 2;
        }
        let emoji_capable = self.emoji_base || matches!(self.first, '\u{A9}' | '\u{AE}');
        if self.vs16 && emoji_capable {
            return 2;
        }
        if clustering {
            // Text presentation narrows an emoji; otherwise the first
            // codepoint decides.
            if self.vs15 && emoji_capable {
                return 1;
            }
            return usize::from(self.base_width);
        }
        if self.vs15 || self.vs16 {
            return usize::from(self.base_width);
        }
        usize::from(self.sum)
    }
}

#[inline]
fn is_regional_indicator(c: char) -> bool {
    ('\u{1F1E6}'..='\u{1F1FF}').contains(&c)
}

#[inline]
fn is_skin_tone(c: char) -> bool {
    ('\u{1F3FB}'..='\u{1F3FF}').contains(&c)
}

/// The width of `text` as Bun's `stringWidth` (or mode 2027) measures it.
pub(crate) fn str_width(text: &str, clustering: bool) -> usize {
    let mut total = 0;
    let mut current: Option<(Cluster, Class)> = None;
    let mut state = BreakState::Default;
    for c in text.chars() {
        let info = Info::of(c);
        match current.as_mut() {
            Some((cluster, previous)) if !breaks(*previous, info.class(), &mut state) => {
                cluster.add(c, info);
                *previous = info.class();
            }
            _ => {
                if let Some((cluster, _)) = current {
                    total += cluster.width(clustering);
                }
                state = BreakState::Default;
                current = Some((Cluster::new(c, info), info.class()));
            }
        }
    }
    total + current.map_or(0, |(cluster, _)| cluster.width(clustering))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Widths recorded from `Bun.stringWidth` in Bun 1.4.4, the runtime
    /// embedded in Claude Code 2.1.
    #[test]
    fn default_widths_match_bun() {
        for (text, width) in [
            ("❤️", 2),
            ("❤", 1),
            ("👨‍👩‍👧‍👦", 2),
            ("🧑‍💻", 2),
            ("🇷🇴", 2),
            ("🇷", 1),
            ("🇷🇴🇷", 3),
            ("🏳️‍🌈", 2),
            ("🏳", 1),
            ("👍🏽", 2),
            ("1️⃣", 2),
            ("#⃣", 2),
            ("\u{AD}", 0),
            ("⌚︎", 2),
            ("☺︎", 1),
            ("❤️‍🔥", 2),
            ("🏴‍☠️", 2),
            ("e\u{FE0F}", 1),
            ("©️", 2),
            ("x\u{200D}y", 2),
            ("😀\u{200D}a", 3),
            ("a\u{301}", 1),
            ("각", 2),
            ("\u{1100}\u{1161}", 2),
            ("हिन्दी", 3),
            ("কি", 1),
            ("你好", 4),
        ] {
            assert_eq!(str_width(text, false), width, "{text:?}");
        }
    }

    #[test]
    fn clustering_mode_uses_the_first_codepoint() {
        for (text, width) in [
            ("❤️", 2),
            ("⌚︎", 1),
            ("👨‍👩‍👧‍👦", 2),
            ("🇷🇴", 2),
            ("हिन्दी", 2),
            ("\u{1100}\u{1100}", 2),
        ] {
            assert_eq!(str_width(text, true), width, "{text:?}");
        }
    }
}
