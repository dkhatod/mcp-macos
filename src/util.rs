//! Small helpers shared by tool-group modules.

/// Escapes `s` into a double-quoted JavaScript string literal, safe to
/// interpolate into JXA source. Control characters become `\uXXXX`.
pub(crate) fn js_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::js_str;

    #[test]
    fn escapes_quotes_backslashes_and_control_chars() {
        assert_eq!(js_str("a\"b\\c"), r#""a\"b\\c""#);
        assert_eq!(js_str("line\nbreak"), "\"line\\nbreak\"");
        assert_eq!(js_str("\u{1}"), "\"\\u0001\"");
    }
}
