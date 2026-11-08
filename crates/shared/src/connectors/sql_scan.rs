//! Best-effort SQL scanning used by [`apply_auto_limit`]: a heuristic lexer that skips over
//! string/quoted-identifier literals, comments, and dollar-quoted bodies so a keyword or
//! statement separator found elsewhere is trusted to be real SQL, not text inside a literal.

/// Best-effort SQL rewrite: append `LIMIT {max_rows + 1}` to a single, unbounded `SELECT`/`WITH`
/// query so a runaway report can't pull unbounded rows into memory.
///
/// The `+1` lets the caller detect "more rows than the cap" without a `COUNT`.
///
/// A heuristic, not a SQL parser: it finds a *top-level* `LIMIT`/`FETCH` via
/// [`scan_code_positions`] and no-ops on anything that looks like multiple statements. False
/// negatives (skipping a limitable query) are accepted; never mangle a query that already
/// paginates.
#[must_use]
pub fn apply_auto_limit(sql: &str, max_rows: u32) -> String {
    let trimmed = sql.trim();
    let body = trimmed.trim_end_matches(|c: char| c == ';' || c.is_whitespace());
    let lower = body.to_ascii_lowercase();

    // A backslash in a single-quoted literal or a top-level `#` means different things in MySQL and
    // Postgres; guessing would mangle a working query, so leave it untouched.
    let Some(code) = scan_code_positions(&lower) else {
        return sql.to_string();
    };

    // `lower` has no leading whitespace (`sql.trim()` above). A `;` at any code position signals
    // multiple statements.
    if code.iter().any(|&(_, c)| c == ';')
        || !(lower.starts_with("select") || lower.starts_with("with"))
        || has_top_level_limit_or_fetch(&lower, &code)
    {
        return sql.to_string();
    }

    // A new line (not a space) keeps an appended `LIMIT` out of a trailing `--` comment.
    format!("{body}\nLIMIT {}", max_rows.saturating_add(1))
}

/// If `s` starts with a valid dollar-quote tag (`$$` or `$tag$`, Postgres identifier rules),
/// returns the byte offset of the tag's closing `$`. A `$1` placeholder, `$1$`, or an operator `$`
/// returns `None`.
fn dollar_tag_len(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    if bytes.first() != Some(&b'$') {
        return None;
    }
    let mut i = 1;
    if i < bytes.len() && bytes[i] != b'$' {
        if !(bytes[i].is_ascii_alphabetic() || bytes[i] == b'_') {
            return None;
        }
        i += 1;
        while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
            i += 1;
        }
    }
    (bytes.get(i) == Some(&b'$')).then_some(i)
}

/// Walk `sql`, yielding `(byte index, char)` only for ordinary SQL code positions, skipping
/// string/quoted-identifier literals, `--` and (nesting-aware) `/* */` comments, and `$tag$`
/// bodies. [`apply_auto_limit`] and `hash.rs`'s query normalization share it.
///
/// Returns `None` on dialect-dependent constructs (a backslash in a single-quoted literal, or `#`
/// at a code position), since picking a dialect risks corrupting the query.
pub fn scan_code_positions(sql: &str) -> Option<Vec<(usize, char)>> {
    let mut out = Vec::with_capacity(sql.len());
    let mut i = 0;
    while i < sql.len() {
        let c = sql[i..].chars().next().expect("i is a valid char boundary");
        match c {
            '\'' | '"' | '`' => {
                let quote = c;
                i += c.len_utf8();
                while i < sql.len() {
                    let c2 = sql[i..].chars().next().expect("i is a valid char boundary");
                    if quote == '\'' && c2 == '\\' {
                        return None;
                    }
                    i += c2.len_utf8();
                    if c2 == quote {
                        // A doubled quote (`''`, `""`) is an escaped quote, still inside the
                        // literal.
                        if sql[i..].starts_with(quote) {
                            i += quote.len_utf8();
                            continue;
                        }
                        break;
                    }
                }
            }
            '#' => return None,
            '-' if sql[i..].starts_with("--") => {
                i += sql[i..].find('\n').unwrap_or(sql.len() - i);
            }
            '/' if sql[i..].starts_with("/*") => {
                let mut depth = 1u32;
                i += 2;
                while i < sql.len() && depth > 0 {
                    if sql[i..].starts_with("/*") {
                        depth += 1;
                        i += 2;
                    } else if sql[i..].starts_with("*/") {
                        depth -= 1;
                        i += 2;
                    } else {
                        i += sql[i..]
                            .chars()
                            .next()
                            .expect("i is a valid char boundary")
                            .len_utf8();
                    }
                }
            }
            '$' => {
                if let Some(tag_close) = dollar_tag_len(&sql[i..]) {
                    let tag = &sql[i..=(i + tag_close)];
                    let after_open = i + tag_close + 1;
                    i = sql[after_open..]
                        .find(tag)
                        .map_or(sql.len(), |rel_close| after_open + rel_close + tag.len());
                } else {
                    out.push((i, c));
                    i += c.len_utf8();
                }
            }
            _ => {
                out.push((i, c));
                i += c.len_utf8();
            }
        }
    }
    Some(out)
}

/// Scan the code positions from [`scan_code_positions`] for a top-level (paren-depth-0) `limit` or
/// `fetch`; `lower_sql` is the lowercased string `code` was scanned from.
fn has_top_level_limit_or_fetch(lower_sql: &str, code: &[(usize, char)]) -> bool {
    let mut depth: i32 = 0;

    for &(i, c) in code {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            _ => {}
        }
        if depth == 0
            && ["limit", "fetch"].iter().any(|kw| {
                lower_sql[i..].starts_with(kw) && is_word_boundary(lower_sql, i, i + kw.len())
            })
        {
            return true;
        }
    }
    false
}

fn is_word_boundary(s: &str, start: usize, end: usize) -> bool {
    let is_ident_byte = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let before_ok = start == 0 || !is_ident_byte(s.as_bytes()[start - 1]);
    let after_ok = end >= s.len() || !is_ident_byte(s.as_bytes()[end]);
    before_ok && after_ok
}

/// `SELECT * FROM <table>` capped at `limit` rows, with `table` (`name` or `schema.name`)
/// quoted for the connector's dialect. `None` when the type has no SQL table to sample.
#[must_use]
pub fn sample_table_sql(type_name: &str, table: &str, limit: u32) -> Option<String> {
    /// Wraps each part in `open`/`close`, doubling any `close` inside it.
    fn quote(parts: &[&str], open: char, close: char) -> String {
        parts
            .iter()
            .map(|p| {
                format!(
                    "{open}{}{close}",
                    p.replace(close, &format!("{close}{close}"))
                )
            })
            .collect::<Vec<_>>()
            .join(".")
    }
    let parts: Vec<&str> = table.split('.').collect();
    let name = match type_name {
        "pg" | "redshift" | "sqlite" | "snowflake" | "athena" | "clickhouse" => {
            quote(&parts, '"', '"')
        }
        "mysql" | "mariadb" => quote(&parts, '`', '`'),
        "bigquery" => quote(&[table], '`', '`'),
        "mssql" => {
            return Some(format!(
                "SELECT TOP {limit} * FROM {}",
                quote(&parts, '[', ']')
            ))
        }
        _ => return None,
    };
    Some(format!("SELECT * FROM {name} LIMIT {limit}"))
}

#[cfg(test)]
mod auto_limit_tests {
    use super::apply_auto_limit;

    #[test]
    fn appends_limit_on_bare_select() {
        let out = apply_auto_limit("SELECT * FROM users", 10000);
        assert_eq!(out, "SELECT * FROM users\nLIMIT 10001");
    }

    #[test]
    fn appends_limit_on_bare_with() {
        let out = apply_auto_limit("WITH t AS (SELECT 1) SELECT * FROM t", 100);
        assert_eq!(out, "WITH t AS (SELECT 1) SELECT * FROM t\nLIMIT 101");
    }

    #[test]
    fn noops_on_existing_top_level_limit() {
        let sql = "SELECT * FROM users LIMIT 50";
        assert_eq!(apply_auto_limit(sql, 10000), sql);
    }

    #[test]
    fn noops_on_existing_fetch() {
        let sql = "SELECT * FROM users ORDER BY id OFFSET 0 ROWS FETCH NEXT 50 ROWS ONLY";
        assert_eq!(apply_auto_limit(sql, 10000), sql);
    }

    #[test]
    fn noops_on_insert() {
        let sql = "INSERT INTO users (name) VALUES ('a')";
        assert_eq!(apply_auto_limit(sql, 10000), sql);
    }

    #[test]
    fn noops_on_multiple_statements() {
        let sql = "SELECT 1; SELECT 2";
        assert_eq!(apply_auto_limit(sql, 10000), sql);
    }

    #[test]
    fn ignores_limit_keyword_inside_string_literal_and_subquery() {
        // "limit" inside a string literal and a subquery is not a top-level LIMIT, so auto-limit
        // still applies.
        let sql =
            "SELECT * FROM t WHERE name = 'no limit here' AND id IN (SELECT id FROM u LIMIT 5)";
        let out = apply_auto_limit(sql, 100);
        assert_eq!(out, format!("{sql}\nLIMIT 101"));
    }

    #[test]
    fn appends_limit_after_a_trailing_line_comment_instead_of_inside_it() {
        // A `LIMIT` appended after a trailing `-- x` comment would land inside the comment, leaving
        // the query unbounded.
        let out = apply_auto_limit("SELECT * FROM t -- x", 1000);
        assert_eq!(out, "SELECT * FROM t -- x\nLIMIT 1001");
    }

    #[test]
    fn apostrophe_inside_a_line_comment_does_not_eat_the_scanner() {
        let sql = "SELECT * FROM t -- don't\nLIMIT 5";
        assert_eq!(apply_auto_limit(sql, 1000), sql);
    }

    #[test]
    fn quoted_identifier_named_limit_is_not_mistaken_for_the_keyword() {
        let out = apply_auto_limit("SELECT \"limit\" FROM t", 1000);
        assert_eq!(out, "SELECT \"limit\" FROM t\nLIMIT 1001");
    }

    #[test]
    fn semicolon_inside_a_string_literal_does_not_suppress_the_limit() {
        let out = apply_auto_limit("SELECT * FROM t WHERE c = 'a;b'", 1000);
        assert_eq!(out, "SELECT * FROM t WHERE c = 'a;b'\nLIMIT 1001");
    }

    #[test]
    fn real_statement_separator_still_bails() {
        let sql = "SELECT 1; SELECT 2";
        assert_eq!(apply_auto_limit(sql, 1000), sql);
    }

    #[test]
    fn dollar_quoted_body_containing_a_semicolon_does_not_suppress_the_limit() {
        let out = apply_auto_limit("SELECT $$a;b$$::text", 1000);
        assert_eq!(out, "SELECT $$a;b$$::text\nLIMIT 1001");
    }

    #[test]
    fn digit_leading_dollar_tag_is_not_treated_as_a_quote_open() {
        // `$1$` is not a valid dollar-quote tag (must start with a letter or underscore), so the
        // following semicolon is a real separator.
        let sql = "SELECT * FROM t WHERE c = $1$; SELECT 2";
        assert_eq!(apply_auto_limit(sql, 1000), sql);
    }

    #[test]
    fn limit_keyword_inside_a_block_comment_is_not_mistaken_for_a_real_limit() {
        let out = apply_auto_limit("SELECT * FROM t /* limit */", 1000);
        assert_eq!(out, "SELECT * FROM t /* limit */\nLIMIT 1001");
    }

    #[test]
    fn nested_block_comment_is_fully_skipped() {
        let out = apply_auto_limit(
            "SELECT * FROM t /* outer /* inner limit */ still comment */",
            1000,
        );
        assert_eq!(
            out,
            "SELECT * FROM t /* outer /* inner limit */ still comment */\nLIMIT 1001"
        );
    }

    #[test]
    fn backslash_escaped_quote_is_left_untouched_rather_than_mis_scanned() {
        // MySQL's `\'` closes the literal early under a doubled-quote-only scanner, hiding the real
        // LIMIT and appending a conflicting one.
        let sql = "SELECT * FROM t WHERE c = 'it\\'s' LIMIT 5";
        assert_eq!(apply_auto_limit(sql, 1000), sql);
    }

    #[test]
    fn hash_comment_is_left_untouched_rather_than_mis_scanned() {
        let sql = "SELECT 1 # comment";
        assert_eq!(apply_auto_limit(sql, 1000), sql);
    }

    #[test]
    fn doubled_quote_escape_still_gets_its_limit() {
        let out = apply_auto_limit("SELECT * FROM t WHERE c = 'it''s'", 1000);
        assert_eq!(out, "SELECT * FROM t WHERE c = 'it''s'\nLIMIT 1001");
    }

    #[test]
    fn sample_table_sql_quotes_per_dialect() {
        use super::sample_table_sql as f;
        assert_eq!(
            f("pg", "orders", 5).unwrap(),
            r#"SELECT * FROM "orders" LIMIT 5"#
        );
        assert_eq!(
            f("pg", "sales.orders", 5).unwrap(),
            r#"SELECT * FROM "sales"."orders" LIMIT 5"#
        );
        assert_eq!(
            f("pg", r#"we"ird"#, 1).unwrap(),
            r#"SELECT * FROM "we""ird" LIMIT 1"#
        );
        assert_eq!(
            f("mysql", "a.b", 2).unwrap(),
            "SELECT * FROM `a`.`b` LIMIT 2"
        );
        assert_eq!(
            f("mysql", "a`b", 2).unwrap(),
            "SELECT * FROM `a``b` LIMIT 2"
        );
        assert_eq!(
            f("bigquery", "d.t", 3).unwrap(),
            "SELECT * FROM `d.t` LIMIT 3"
        );
        assert_eq!(
            f("mssql", "s.t]x", 4).unwrap(),
            "SELECT TOP 4 * FROM [s].[t]]x]"
        );
        for t in ["mongodb", "es", "json", "query_results", "url"] {
            assert!(f(t, "x", 1).is_none(), "{t}");
        }
    }
}
