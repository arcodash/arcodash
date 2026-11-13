//! Removes quoted customer values from error text before it is stored.

const ELLIPSIS: char = '…';

/// A quote closes a literal only at the end of the text or before a non-alphanumeric char, and
/// never when backslash-escaped, so values containing the quote character are fully removed.
fn closes_literal(chars: &[char], j: usize, quote: char) -> bool {
    chars[j] == quote
        && chars[j - 1] != '\\'
        && chars.get(j + 1).is_none_or(|n| !n.is_alphanumeric())
}

/// Replaces `'…'` literals, `(col)=(value)` value groups and `: "value"` strings with `…`.
/// Quoted identifiers elsewhere are kept. An unterminated quote or group is replaced to the end.
#[must_use]
pub fn scrub_error_values(msg: &str) -> String {
    let chars: Vec<char> = msg.chars().collect();
    let mut out = String::with_capacity(msg.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let after_colon_space = i >= 2 && chars[i - 2] == ':' && chars[i - 1] == ' ';
        let opens_literal = c == '\'' && (i == 0 || !chars[i - 1].is_alphanumeric());
        if opens_literal || (c == '"' && after_colon_space) {
            out.push(c);
            out.push(ELLIPSIS);
            match (i + 1..chars.len()).find(|&j| closes_literal(&chars, j, c)) {
                Some(j) => {
                    out.push(c);
                    i = j + 1;
                }
                None => i = chars.len(),
            }
        } else if c == '(' && i >= 2 && chars[i - 1] == '=' && chars[i - 2] == ')' {
            out.push('(');
            out.push(ELLIPSIS);
            let mut depth = 1usize;
            let mut j = i + 1;
            while j < chars.len() {
                match chars[j] {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
            if j < chars.len() {
                out.push(')');
            }
            i = j + 1;
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::scrub_error_values as s;

    #[test]
    fn scrubs_postgres_key_detail() {
        assert_eq!(
            s("Key (email)=(bob@x.com) already exists."),
            "Key (email)=(…) already exists."
        );
    }

    #[test]
    fn scrubs_multi_column_key_detail() {
        assert_eq!(
            s("Key (a, b)=(1, x) already exists."),
            "Key (a, b)=(…) already exists."
        );
        assert_eq!(s("Key (a)=(f(1), x) dup"), "Key (a)=(…) dup");
    }

    #[test]
    fn scrubs_double_quoted_value_after_colon() {
        assert_eq!(
            s("invalid input syntax for type integer: \"abc\""),
            "invalid input syntax for type integer: \"…\""
        );
    }

    #[test]
    fn scrubs_single_quoted_literals() {
        assert_eq!(
            s("Duplicate entry 'bob@x.com' for key 'users.email'"),
            "Duplicate entry '…' for key '…'"
        );
        assert_eq!(s("it doesn't exist"), "it doesn't exist");
    }

    #[test]
    fn scrubs_value_with_embedded_apostrophe() {
        assert_eq!(
            s("Duplicate entry 'O'Brien' for key 'users.name'"),
            "Duplicate entry '…' for key '…'"
        );
    }

    #[test]
    fn scrubs_backslash_escaped_quote() {
        assert_eq!(s(r"bad 'a\'b' x"), "bad '…' x");
    }

    #[test]
    fn scrubs_embedded_double_quote_after_colon() {
        assert_eq!(s("err: \"a\"b\" x"), "err: \"…\" x");
    }

    #[test]
    fn scrubs_doubled_quote_escape_fully() {
        assert_eq!(s("bad 'it''s' x"), "bad '…''…' x");
    }

    #[test]
    fn keeps_quoted_identifiers() {
        let m = "relation \"users\" does not exist";
        assert_eq!(s(m), m);
        let m = "column \"x\" of relation \"y\" does not exist";
        assert_eq!(s(m), m);
    }

    #[test]
    fn unterminated_quote_is_scrubbed_to_end() {
        assert_eq!(s("bad value 'bob@x.com and more"), "bad value '…");
        assert_eq!(s("syntax: \"abc"), "syntax: \"…");
        assert_eq!(s("Key (a)=(bob"), "Key (a)=(…");
    }

    #[test]
    fn handles_multi_line_messages() {
        assert_eq!(
            s("ERROR: dup\nDETAIL: Key (e)=(a\nb) exists\nvalue 'x\ny' z"),
            "ERROR: dup\nDETAIL: Key (e)=(…) exists\nvalue '…' z"
        );
    }

    #[test]
    fn scrub_is_idempotent() {
        for m in [
            "Key (email)=(bob@x.com) already exists.",
            "Duplicate entry 'a' for key 'b'",
            "bad: \"abc\" and 'zz",
            "Duplicate entry 'O'Brien' for key 'k'",
            "bad 'it''s' x",
            "Key (a)=(x",
            "()=()'\"(((",
        ] {
            let once = s(m);
            assert_eq!(s(&once), once, "{m}");
        }
    }

    #[test]
    fn scrub_handles_empty_and_plain_text() {
        assert_eq!(s(""), "");
        assert_eq!(s("connection refused"), "connection refused");
        assert_eq!(s("é 'ü' ñ"), "é '…' ñ");
    }
}
