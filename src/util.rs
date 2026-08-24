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

/// Serializes rows one-per-line inside a JSON array so client-side output
/// compactors degrade gracefully: they drop whole records from the middle
/// instead of slicing through a single giant line (observed with OMP).
pub(crate) fn join_rows(rows: &[serde_json::Value]) -> String {
    let mut s = String::new();
    for (i, r) in rows.iter().enumerate() {
        if i > 0 {
            s.push_str(",\n");
        }
        s.push_str(&r.to_string());
    }
    s
}

/// When a builder returns its payload as a pre-serialized multi-line JSON
/// string (row-per-line diet), the transport envelope carries it as
/// `Value::String`; unwrap and re-parse so callers see the object shape.
pub(crate) fn unwrap_string_payload(
    v: serde_json::Value,
) -> Result<serde_json::Value, personai_core::macos::AppleError> {
    match v {
        serde_json::Value::String(s) => serde_json::from_str(&s)
            .map_err(|e| personai_core::macos::AppleError::Parse(e.to_string())),
        other => Ok(other),
    }
}
