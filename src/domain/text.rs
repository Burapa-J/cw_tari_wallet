//! Cleaning node-supplied text before it reaches Dart.

/// Remove invisible and control characters that could disguise node-supplied text
/// shown to the user: ASCII C0/C1 control characters (normalising `\t`, `\n`, and
/// `\r` to spaces), Unicode format characters (category Cf: bidi overrides,
/// zero-width characters, BOM, …), line and paragraph separators, Hangul and other
/// fillers, variation selectors and Mongolian free variation selectors.
pub(crate) fn strip_format_chars(text: &str) -> String {
    text.chars()
        .filter_map(|c| match c as u32 {
            0x0009 | 0x000A | 0x000D => Some(' '),
            0x0000..=0x001F
            | 0x007F..=0x009F
            | 0x00AD
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
            | 0xE0100..=0xE01EF => None,
            _ => Some(c),
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

    #[test]
    fn strips_c0_and_c1_control_characters() {
        let forged = "bad\x00\t\r\n\x1b[31m\x7f\u{0085}\u{009B}node response";
        assert_eq!(strip_format_chars(forged), "bad   [31mnode response");

        // Every C0 (0x00..=0x1F) and C1 (0x7F..=0x9F) control character is stripped
        // except \t, \n, \r which are normalised to ASCII spaces.
        for code in (0x00u32..=0x1Fu32).chain(0x7Fu32..=0x9Fu32) {
            let ch = char::from_u32(code).expect("valid control char");
            let input = format!("prefix{ch}suffix");
            let expected = if matches!(code, 0x09 | 0x0A | 0x0D) {
                "prefix suffix"
            } else {
                "prefixsuffix"
            };
            assert_eq!(
                strip_format_chars(&input),
                expected,
                "control char U+{code:04X} must be handled"
            );
        }
    }

    #[test]
    fn strips_tag_and_musical_format_characters() {
        let tags = "ok\u{E0001}\u{E0020}\u{E007F}\u{1BCA0}\u{1D173}\u{1D17A}\u{13430}done";
        assert_eq!(strip_format_chars(tags), "okdone");
    }
}
