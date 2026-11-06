//! Cleaning node-supplied text before it reaches Dart.

/// Remove invisible characters that could disguise node-supplied text shown to the
/// user: Unicode format characters (category Cf: bidi overrides, zero-width
/// characters, BOM, …), line and paragraph separators, Hangul and other fillers,
/// variation selectors and Mongolian free variation selectors.
pub(crate) fn strip_format_chars(text: &str) -> String {
    text.chars()
        .filter(|c| {
            !matches!(
                *c as u32,
                0x00AD
                    | 0x034F
                    | 0x0600..=0x0605
                    | 0x061C
                    | 0x06DD
                    | 0x070F
                    | 0x0890..=0x0891
                    | 0x08E2
                    | 0x115F..=0x1160
                    | 0x180B..=0x180F
                    | 0x200B..=0x200F
                    | 0x2028..=0x202E
                    | 0x2060..=0x2064
                    | 0x2066..=0x206F
                    | 0x3164
                    | 0xFE00..=0xFE0F
                    | 0xFEFF
                    | 0xFFA0
                    | 0xFFF9..=0xFFFB
                    | 0x110BD
                    | 0x110CD
                    | 0x13430..=0x1343F
                    | 0x1BCA0..=0x1BCA3
                    | 0x1D173..=0x1D17A
                    | 0xE0001
                    | 0xE0020..=0xE007F
                    | 0xE0100..=0xE01EF
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn strips_invisible_characters() {
        let text = "Rejected\u{202E}gnp.exe\u{200B} by\u{FEFF} node\u{2066}x\u{2069}\u{00AD}";
        assert_eq!(strip_format_chars(text), "Rejectedgnp.exe by nodex");

        let more = "a\u{2028}b\u{2029}c\u{034F}d\u{115F}\u{1160}e\u{3164}f\u{FFA0}g\u{FE0F}h\u{E0100}i\u{180B}\u{180D}\u{180F}j";
        assert_eq!(strip_format_chars(more), "abcdefghij");
    }

    #[test]
    fn keeps_visible_text() {
        assert_eq!(
            strip_format_chars("plain ascii, ünïcode ok, 日本語, emoji 🙂"),
            "plain ascii, ünïcode ok, 日本語, emoji 🙂"
        );
    }
}
