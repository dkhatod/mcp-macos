//! Shared helpers for contract-test modules.

/// Guards against the class of regression where a format-template edit
/// leaves a generated JXA script syntactically dead (every call 500s at
/// parse time).
///
/// A small JS lexer rather than a naive counter: generated scripts legally
/// contain double-quoted string literals with apostrophes, regex literals
/// with embedded quotes (`/"?([^"<]+?)"/` in mail's nameOf), and comments.
/// A single/double-quote-only scanner misreads those and false-positives.
pub fn balanced(script: &str) -> bool {
    let ch: Vec<char> = script.chars().collect();
    let n = ch.len();
    let mut i = 0usize;
    let mut depth: i32 = 0;
    // Last significant char in code position ('"' marks "an operand just
    // ended", which forbids a regex literal start) plus the trailing
    // identifier, so `return /…/` style keywords allow regexes too.
    let mut prev: Option<char> = None;
    let mut word = String::new();

    while i < n {
        let c = ch[i];
        if c.is_ascii_alphanumeric() || c == '_' || c == '$' {
            word.push(c);
            prev = Some(c);
            i += 1;
            continue;
        }
        match c {
            '/' if i + 1 < n && ch[i + 1] == '/' => {
                while i < n && ch[i] != '\n' {
                    i += 1;
                }
            }
            '/' if i + 1 < n && ch[i + 1] == '*' => {
                i += 2;
                while i + 1 < n && !(ch[i] == '*' && ch[i + 1] == '/') {
                    i += 1;
                }
                i = (i + 2).min(n);
                prev = Some('"');
            }
            '/' if regex_allowed(prev, &word) => {
                i += 1;
                let mut in_class = false;
                while i < n {
                    match ch[i] {
                        '\\' => i += 1,
                        '[' => in_class = true,
                        ']' => in_class = false,
                        '/' if !in_class => break,
                        '\n' => break, // not a regex after all; resync at code level
                        _ => {}
                    }
                    i += 1;
                }
                prev = Some('"');
            }
            '\'' | '"' | '`' => {
                let q = c;
                i += 1;
                while i < n {
                    match ch[i] {
                        '\\' => i += 1,
                        x if x == q => break,
                        _ => {}
                    }
                    i += 1;
                }
                prev = Some('"');
            }
            '(' | '{' => depth += 1,
            ')' | '}' => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => {}
        }
        if !c.is_whitespace() && prev != Some('"') {
            prev = Some(c);
        }
        word.clear();
        i += 1;
    }
    depth == 0
}

/// Whether a `/` at this point may open a regex literal (vs. division).
fn regex_allowed(prev: Option<char>, last_word: &str) -> bool {
    match prev {
        None => true,
        Some(c) => {
            matches!(
                c,
                '(' | ','
                    | '['
                    | '{'
                    | ';'
                    | ':'
                    | '!'
                    | '&'
                    | '|'
                    | '?'
                    | '+'
                    | '-'
                    | '*'
                    | '%'
                    | '^'
                    | '<'
                    | '>'
                    | '~'
                    | '='
            ) || matches!(
                last_word,
                "return"
                    | "typeof"
                    | "instanceof"
                    | "in"
                    | "of"
                    | "new"
                    | "delete"
                    | "void"
                    | "case"
                    | "do"
                    | "else"
            )
        }
    }
}
