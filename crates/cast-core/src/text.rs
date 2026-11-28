//! Text that came from outside (a client, the compositor: window titles, device names) and is shown to a person, on
//! the trusted channel or in the window. Nothing in it may fake a line, reorder or hide text.

/// A character that could fake or hide text: controls (C0, DEL, C1), line and paragraph separators, and the format
/// (Cf) and default-ignorable characters that draw nothing: soft hyphen, Arabic letter mark, zero-width and bidi
/// controls, word joiner and invisible operators, the Hangul fillers, variation selectors, the Tags block, and the
/// like.
pub fn is_unsafe_char(c: char) -> bool {
    c.is_control()
        || matches!(c,
            '\u{00AD}'
            | '\u{034F}'
            | '\u{0600}'..='\u{0605}'
            | '\u{061C}'
            | '\u{06DD}'
            | '\u{070F}'
            | '\u{08E2}'
            | '\u{115F}'..='\u{1160}'
            | '\u{17B4}'..='\u{17B5}'
            | '\u{180B}'..='\u{180F}'
            | '\u{200B}'..='\u{200F}'
            | '\u{2028}'..='\u{202E}'
            | '\u{2060}'..='\u{206F}'
            | '\u{3164}'
            | '\u{FE00}'..='\u{FE0F}'
            | '\u{FEFF}'
            | '\u{FFA0}'
            | '\u{FFF0}'..='\u{FFFB}'
            | '\u{110BD}'
            | '\u{1BCA0}'..='\u{1BCA3}'
            | '\u{1D173}'..='\u{1D17A}'
            | '\u{E0000}'..='\u{E0FFF}')
}

/// `s` with every unsafe character shown as `\u{..}`.
pub fn escape_unsafe(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if is_unsafe_char(c) {
            out.push_str(&format!("\\u{{{:x}}}", c as u32));
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_hides_text_is_unsafe() {
        for c in [
            '\u{061C}',
            '\u{2060}',
            '\u{2061}',
            '\u{2062}',
            '\u{2063}',
            '\u{2064}',
            '\u{00AD}',
            '\u{034F}',
            '\u{3164}',
            '\u{E0000}',
            '\u{E0041}',
            '\u{E007F}',
            '\u{200B}',
            '\u{202E}',
            '\u{2066}',
            '\u{FEFF}',
            '\u{FE0F}',
            '\u{1b}',
            '\u{9b}',
            '\n',
            '\u{2028}',
        ] {
            assert!(is_unsafe_char(c), "U+{:04X}", c as u32);
        }
        for c in ['a', 'ç', ' ', '日', '-', '"', '🙂'] {
            assert!(!is_unsafe_char(c), "{c}");
        }
        assert_eq!(escape_unsafe("a\u{E0041}b\u{061C}\u{e7}"), "a\\u{e0041}b\\u{61c}ç");
    }
}
