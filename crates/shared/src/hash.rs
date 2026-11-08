//! Query result cache key hashing, identical between backend and worker so cache lookups
//! agree (`query_results (org_id, data_source_id, query_hash)`).

use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

use crate::secrets::hex_encode;

/// Deterministic cache-key hash for a query + its bind parameters.
///
/// Whitespace is normalized and params are key-sorted via a `BTreeMap`, so cosmetic differences
/// don't bust the cache. `MAX_QUERY_ROWS` (read from env, which backend and worker must agree on)
/// is folded in so changing the cap invalidates entries truncated under the old one. The parts are
/// joined with separators that can't appear in their encodings, then SHA-256 hex-digested.
#[must_use]
pub fn gen_query_hash(query: &str, params: &serde_json::Value) -> String {
    let normalized_query = normalize_whitespace(query);
    let canonical_params = canonicalize(params);
    let max_query_rows: u32 = crate::config::env_parsed("MAX_QUERY_ROWS", 10_000);

    let mut hasher = Sha256::new();
    hasher.update(normalized_query.as_bytes());
    hasher.update(b"\x01"); // separator byte that won't appear in either operand
    hasher.update(canonical_params.as_bytes());
    hasher.update(b"\x02"); // separator byte distinct from the one above
    hasher.update(max_query_rows.to_le_bytes());

    hex_encode(&hasher.finalize())
}

/// Collapse whitespace in ordinary SQL code, copying string literals, quoted identifiers, comments
/// and dollar-quoted bodies verbatim. Unparseable input is returned trimmed but untouched: a missed
/// cache hit is correct, a shared key is not.
fn normalize_whitespace(input: &str) -> String {
    let Some(code) = crate::connectors::scan_code_positions(input) else {
        return input.trim().to_string();
    };
    let mut code = code.iter().map(|(i, _)| *i).peekable();
    let mut out = String::with_capacity(input.len());
    let mut pending_space = false;
    for (i, c) in input.char_indices() {
        while code.peek().is_some_and(|&ci| ci < i) {
            code.next();
        }
        let is_code = code.peek() == Some(&i);
        if is_code && c.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(c);
    }
    out
}

/// Re-serialize a `serde_json::Value` with all object keys sorted recursively.
fn canonicalize(value: &serde_json::Value) -> String {
    let effective = if value.is_null() {
        &serde_json::Value::Object(serde_json::Map::default())
    } else {
        value
    };
    serde_json::to_string(&sort_value(effective)).expect("serializing a sorted Value cannot fail")
}

fn sort_value(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let sorted: BTreeMap<String, serde_json::Value> = map
                .iter()
                .map(|(k, v)| (k.clone(), sort_value(v)))
                .collect();
            // `Map` is a `BTreeMap` by default, but `bson` turns on serde_json's `preserve_order`
            // (feature unification), making it insertion-ordered; collecting from the sorted
            // `BTreeMap` keeps keys sorted either way.
            serde_json::Value::Object(sorted.into_iter().collect())
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(sort_value).collect())
        }
        other => other.clone(),
    }
}

/// Hash an API key for storage/lookup; only the digest is persisted.
///
/// Plain unsalted SHA-256, deliberately: keys are 128+ bits of server-generated randomness (nothing
/// to brute-force) and lookup needs a deterministic digest. The hex form must match Postgres
/// `encode(sha256(api_key::bytea), 'hex')`, used by the backfill migration.
#[must_use]
pub fn hash_api_key(key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(key.as_bytes());
    hex_encode(&hasher.finalize())
}

/// Mint a raw API key: 32 bytes from the OS CSPRNG, hex-encoded (a full 256 bits, unlike two
/// concatenated `UUIDv4s`). All provisioning paths share this entropy.
#[must_use]
pub fn generate_api_key() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("OS CSPRNG unavailable");
    hex_encode(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// `gen_query_hash("SELECT 1", {})` under each `MAX_QUERY_ROWS` value, restoring the var after.
    fn hashes_under_max_query_rows(values: &[&str]) -> Vec<String> {
        let _guard = crate::test_support::hold_max_query_rows_env();
        let previous = std::env::var("MAX_QUERY_ROWS").ok();
        let hashes = values
            .iter()
            .map(|v| {
                unsafe { std::env::set_var("MAX_QUERY_ROWS", v) };
                gen_query_hash("SELECT 1", &json!({}))
            })
            .collect();
        match previous {
            Some(v) => unsafe { std::env::set_var("MAX_QUERY_ROWS", v) },
            None => unsafe { std::env::remove_var("MAX_QUERY_ROWS") },
        }
        hashes
    }

    #[test]
    fn max_query_rows_changes_the_hash() {
        let h = hashes_under_max_query_rows(&["10000", "50000"]);
        assert_ne!(
            h[0], h[1],
            "a cache entry computed under one row cap must not be reused under a different one"
        );
    }

    #[test]
    fn max_query_rows_produces_a_stable_hash_for_the_same_value() {
        let hashes = hashes_under_max_query_rows(&["25000", "25000"]);
        assert_eq!(hashes[0], hashes[1]);
    }

    #[test]
    fn identical_query_and_params_produce_identical_hash() {
        let _guard = crate::test_support::hold_max_query_rows_env();
        let params = json!({"a": 1, "b": 2});
        let h1 = gen_query_hash("SELECT 1", &params);
        let h2 = gen_query_hash("SELECT 1", &params);
        assert_eq!(h1, h2);
    }

    #[test]
    fn param_key_order_does_not_affect_hash() {
        let _guard = crate::test_support::hold_max_query_rows_env();
        let params_ab = json!({"a": 1, "b": 2});
        let params_ba = json!({"b": 2, "a": 1});
        let h_ab = gen_query_hash("SELECT * FROM t WHERE x = :a AND y = :b", &params_ab);
        let h_ba = gen_query_hash("SELECT * FROM t WHERE x = :a AND y = :b", &params_ba);
        assert_eq!(h_ab, h_ba);
    }

    #[test]
    fn nested_object_key_order_does_not_affect_hash() {
        let _guard = crate::test_support::hold_max_query_rows_env();
        let nested_ab = json!({"outer": {"a": 1, "b": 2}});
        let nested_ba = json!({"outer": {"b": 2, "a": 1}});
        let h_ab = gen_query_hash("SELECT 1", &nested_ab);
        let h_ba = gen_query_hash("SELECT 1", &nested_ba);
        assert_eq!(h_ab, h_ba);
    }

    #[test]
    fn whitespace_only_query_differences_collapse_to_same_hash() {
        let _guard = crate::test_support::hold_max_query_rows_env();
        let params = json!({});
        let h1 = gen_query_hash("SELECT   1\n  FROM   foo", &params);
        let h2 = gen_query_hash("SELECT 1 FROM foo", &params);
        let h3 = gen_query_hash("  SELECT 1 FROM foo  ", &params);
        assert_eq!(h1, h2);
        assert_eq!(h2, h3);
    }

    #[test]
    fn different_queries_produce_different_hashes() {
        let _guard = crate::test_support::hold_max_query_rows_env();
        let params = json!({});
        let h1 = gen_query_hash("SELECT 1", &params);
        let h2 = gen_query_hash("SELECT 2", &params);
        assert_ne!(h1, h2);
    }

    #[test]
    fn different_params_produce_different_hashes() {
        let _guard = crate::test_support::hold_max_query_rows_env();
        let h1 = gen_query_hash("SELECT 1", &json!({"a": 1}));
        let h2 = gen_query_hash("SELECT 1", &json!({"a": 2}));
        assert_ne!(h1, h2);
    }

    #[test]
    fn hash_is_64_char_hex_digest() {
        let _guard = crate::test_support::hold_max_query_rows_env();
        let h = gen_query_hash("SELECT 1", &json!({}));
        assert_eq!(h.len(), 64);
        assert!(h.bytes().all(|b| b.is_ascii_hexdigit()));
    }

    // Known-answer test: pinned so algorithm drift (and backend/worker cache-key mismatch) breaks
    // loudly.
    #[test]
    fn known_hash_is_stable() {
        let _guard = crate::test_support::hold_max_query_rows_env();
        let h = gen_query_hash("SELECT 1", &json!({}));
        assert_eq!(
            h,
            "7ff123a04e82786a960d34d85383cc3fa4c478e8f70cc777afd7db05e3a2d3ad"
        );
    }

    #[test]
    fn empty_query_empty_params_stable() {
        let _guard = crate::test_support::hold_max_query_rows_env();
        let h1 = gen_query_hash("", &json!({}));
        let h2 = gen_query_hash("", &json!({}));
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64);
    }

    #[test]
    fn array_order_affects_hash() {
        let _guard = crate::test_support::hold_max_query_rows_env();
        let h1 = gen_query_hash("SELECT 1", &json!({"ids": [1, 2, 3]}));
        let h2 = gen_query_hash("SELECT 1", &json!({"ids": [3, 2, 1]}));
        assert_ne!(
            h1, h2,
            "array order must affect hash — sort_value preserves array order intentionally"
        );
    }

    #[test]
    fn unicode_query_normalizes_without_panic() {
        let _guard = crate::test_support::hold_max_query_rows_env();
        let h = gen_query_hash("SELECT '日本語'   FROM   tbl", &json!({}));
        let h2 = gen_query_hash("SELECT '日本語' FROM tbl", &json!({}));
        assert_eq!(h, h2);
        assert_eq!(h.len(), 64);
    }

    #[test]
    fn api_key_hash_is_deterministic_and_distinct_per_key() {
        assert_eq!(hash_api_key("abc123"), hash_api_key("abc123"));
        assert_ne!(hash_api_key("abc123"), hash_api_key("abc124"));
        assert_eq!(hash_api_key("abc123").len(), 64);
    }

    /// Must equal Postgres `encode(sha256('abc123'::bytea), 'hex')`; a divergence would lock out
    /// pre-existing keys.
    #[test]
    fn api_key_hash_matches_the_known_sha256_vector() {
        assert_eq!(
            hash_api_key("abc123"),
            "6ca13d52ca70c883e0f0bb101e425a89e8624de51db2d2392593af6a84118090"
        );
    }

    #[test]
    fn api_key_hash_never_returns_the_key_itself() {
        let key = "0123456789abcdef0123456789abcdef";
        assert_ne!(hash_api_key(key), key);
    }

    #[test]
    fn whitespace_inside_a_string_literal_is_preserved_and_changes_the_hash() {
        let _guard = crate::test_support::hold_max_query_rows_env();
        let params = json!({});
        let h1 = gen_query_hash("SELECT * FROM t WHERE n = 'a  b'", &params);
        let h2 = gen_query_hash("SELECT * FROM t WHERE n = 'a b'", &params);
        assert_ne!(h1, h2);
    }

    #[test]
    fn whitespace_outside_a_string_literal_still_collapses() {
        let _guard = crate::test_support::hold_max_query_rows_env();
        let params = json!({});
        let h1 = gen_query_hash("SELECT   *\n FROM t", &params);
        let h2 = gen_query_hash("SELECT * FROM t", &params);
        assert_eq!(h1, h2);
    }

    #[test]
    fn doubled_single_quote_escape_keeps_interior_spaces_in_one_literal() {
        let _guard = crate::test_support::hold_max_query_rows_env();
        let params = json!({});
        let h1 = gen_query_hash("SELECT 'it''s  here'", &params);
        let h2 = gen_query_hash("SELECT 'it''s here'", &params);
        assert_ne!(
            h1, h2,
            "the doubled-quote escape must not end the literal early"
        );
    }

    #[test]
    fn unterminated_string_literal_does_not_panic_and_is_stable() {
        let _guard = crate::test_support::hold_max_query_rows_env();
        let params = json!({});
        let h1 = gen_query_hash("SELECT 'abc", &params);
        let h2 = gen_query_hash("SELECT 'abc", &params);
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64);
    }

    #[test]
    fn double_quoted_identifier_whitespace_is_preserved() {
        let _guard = crate::test_support::hold_max_query_rows_env();
        let params = json!({});
        let h1 = gen_query_hash("SELECT \"a  b\" FROM t", &params);
        let h2 = gen_query_hash("SELECT \"a b\" FROM t", &params);
        assert_ne!(h1, h2);
    }

    #[test]
    fn dollar_quoted_body_whitespace_is_preserved() {
        let _guard = crate::test_support::hold_max_query_rows_env();
        let params = json!({});
        let h1 = gen_query_hash("SELECT $tag$a  b$tag$", &params);
        let h2 = gen_query_hash("SELECT $tag$a b$tag$", &params);
        assert_ne!(h1, h2);
    }

    #[test]
    fn a_backslash_escaped_literal_is_never_collapsed() {
        let _guard = crate::test_support::hold_max_query_rows_env();
        // The scanner can't safely delimit a backslash-escaped single-quoted literal (MySQL and
        // Postgres disagree), so it returns `None` and the string is left untouched.
        let params = json!({});
        let h1 = gen_query_hash("SELECT 'a\\'  b'", &params);
        let h2 = gen_query_hash("SELECT 'a\\' b'", &params);
        assert_ne!(h1, h2);
    }

    #[test]
    fn a_comment_only_difference_does_not_collide() {
        let _guard = crate::test_support::hold_max_query_rows_env();
        let params = json!({});
        let h1 = gen_query_hash("SELECT 1 -- a  b", &params);
        let h2 = gen_query_hash("SELECT 1 -- a b", &params);
        assert_ne!(h1, h2);
    }

    #[test]
    fn generate_api_key_is_64_char_lowercase_hex() {
        let key = generate_api_key();
        assert_eq!(key.len(), 64);
        assert!(key
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
    }

    #[test]
    fn generate_api_key_is_random_across_calls() {
        let keys: std::collections::HashSet<String> =
            (0..100).map(|_| generate_api_key()).collect();
        assert_eq!(
            keys.len(),
            100,
            "100 calls must not collide or repeat a fixed value"
        );
    }
}
