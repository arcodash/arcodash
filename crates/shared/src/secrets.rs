//! AES-256-GCM encrypt/decrypt for `data_sources.encrypted_options` and notification
//! destination secrets, keyed by `ENCRYPTION_KEY` (see [`crate::config::Config`]).

use aes_gcm::aead::{Aead, Generate, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{anyhow, Context, Result};
use std::fmt::Write as _;

const NONCE_LEN: usize = 12;

/// Encrypt `plaintext` with AES-256-GCM under `key`. A random 96-bit nonce is generated and
/// prepended to the returned ciphertext (`nonce || ciphertext`), so [`decrypt`] is self
/// contained given just the key.
///
/// NOTE: nonce-birthday bound — a random 96-bit nonce repeating under the *same* key
/// catastrophically breaks GCM (nonce reuse leaks the authentication key and enables forgery).
/// By the birthday bound that becomes a real risk somewhere around 2^32 encryptions under one
/// `ENCRYPTION_KEY` (~4 billion). This app is nowhere near that volume (one encryption per
/// data-source/destination secret write, not per query), so it's accepted as-is; if
/// `ENCRYPTION_KEY` rotation is ever automated, cycling the key well before that volume is the
/// mitigation, not switching nonce schemes.
pub fn encrypt(key: &[u8; 32], plaintext: &[u8]) -> Result<Vec<u8>> {
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|e| anyhow!("invalid key: {e}"))?;
    let nonce = Nonce::try_generate().map_err(|e| anyhow!("nonce generation failed: {e}"))?;

    let ciphertext = cipher
        .encrypt(&nonce, plaintext)
        .map_err(|e| anyhow!("encryption failed: {e}"))?;

    let mut out = Vec::with_capacity(NONCE_LEN + ciphertext.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

/// Decrypt data produced by [`encrypt`]: splits the leading nonce off `ciphertext` and decrypts the
/// remainder under `key`.
///
/// Silent on failure — callers that expect a possible miss as part of normal operation (see
/// [`decrypt_with_either`]) use [`decrypt_quiet`] instead so a routine fallback never counts as the
/// crate's own failure signal.
pub fn decrypt(key: &[u8; 32], ciphertext: &[u8]) -> Result<Vec<u8>> {
    decrypt_quiet(key, ciphertext).inspect_err(|_| {
        // Never log the key or ciphertext — only that a decrypt failed, which is the crate's
        // own signal independent of whatever the caller does (or doesn't) log.
        crate::metrics::record_secret_decrypt_failure();
        tracing::warn!("secrets::decrypt failed");
    })
}

fn decrypt_quiet(key: &[u8; 32], ciphertext: &[u8]) -> Result<Vec<u8>> {
    if ciphertext.len() < NONCE_LEN {
        return Err(anyhow!(
            "ciphertext too short to contain a nonce ({} bytes, need at least {NONCE_LEN})",
            ciphertext.len()
        ));
    }

    let (nonce_bytes, sealed) = ciphertext.split_at(NONCE_LEN);
    let nonce = Nonce::try_from(nonce_bytes).map_err(|_| anyhow!("invalid nonce length"))?;
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|e| anyhow!("invalid key: {e}"))?;

    cipher
        .decrypt(&nonce, sealed)
        .map_err(|e| anyhow!("decryption failed: {e}"))
}

/// Decrypt `ciphertext` under `new_key` first, falling back to `old_key` on failure.
///
/// Used during key rotation: a row already re-encrypted under `new_key` (e.g. a prior run of the
/// rotation utility that was interrupted partway through) decrypts immediately without needing
/// `old_key` at all, which is what makes re-running the rotation safe/idempotent. Every
/// not-yet-rotated row misses `new_key` as a matter of course while a rotation is in progress, so
/// that first attempt goes through the quiet path — only a miss on `old_key` too (a genuinely bad
/// key or corrupt ciphertext) counts as the crate's failure signal.
pub fn decrypt_with_either(
    new_key: &[u8; 32],
    old_key: &[u8; 32],
    ciphertext: &[u8],
) -> Result<Vec<u8>> {
    // Logs which key decrypted the row, never the key material itself — this is the only signal
    // available during a rotation to tell whether it has actually converged.
    decrypt_quiet(new_key, ciphertext).map_or_else(
        |_| {
            decrypt(old_key, ciphertext).inspect(|_| {
                tracing::debug!(key = "old", "decrypted with pre-rotation key");
            })
        },
        |plaintext| {
            tracing::debug!(key = "new", "decrypted with rotation key");
            Ok(plaintext)
        },
    )
}

/// Parse a hex-encoded 32-byte (64 hex char) encryption key, as loaded from `ENCRYPTION_KEY`.
pub fn parse_key_hex(hex_key: &str) -> Result<[u8; 32]> {
    if hex_key.len() != 64 {
        return Err(anyhow!(
            "ENCRYPTION_KEY must be exactly 64 hex characters (32 bytes), got {} characters",
            hex_key.len()
        ));
    }

    let bytes = hex_decode(hex_key).context("ENCRYPTION_KEY is not valid hex")?;
    bytes
        .try_into()
        .map_err(|_| anyhow!("ENCRYPTION_KEY did not decode to exactly 32 bytes"))
}

/// Lowercase, zero-padded hex encoding.
///
/// The inverse of [`hex_decode`], and the one place every
/// binary-to-hex conversion in the workspace routes through, so cache keys, API keys and derived
/// idempotency keys all format bytes identically.
#[must_use]
pub fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        write!(out, "{b:02x}").expect("write! to a String cannot fail");
    }
    out
}

fn hex_decode(s: &str) -> Result<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return Err(anyhow!("hex string must have an even length"));
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    for chunk in s.as_bytes().chunks(2) {
        let byte_str = std::str::from_utf8(chunk).map_err(|_| anyhow!("invalid hex"))?;
        let byte = u8::from_str_radix(byte_str, 16).map_err(|_| anyhow!("invalid hex digit"))?;
        out.push(byte);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_key() -> [u8; 32] {
        [7u8; 32]
    }

    #[test]
    fn encrypt_then_decrypt_round_trips() {
        let key = test_key();
        let plaintext = b"super secret connection string";
        let ciphertext = encrypt(&key, plaintext).unwrap();
        assert_ne!(ciphertext, plaintext);
        let decrypted = decrypt(&key, &ciphertext).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    /// NIST GCM spec test case 15 (AES-256, 96-bit IV, no AAD), stored as `nonce || ct || tag`.
    #[test]
    fn decrypts_a_nist_aes_256_gcm_vector() {
        let key: [u8; 32] =
            hex_decode("feffe9928665731c6d6a8f9467308308feffe9928665731c6d6a8f9467308308")
                .unwrap()
                .try_into()
                .unwrap();
        let stored = hex_decode(concat!(
            "cafebabefacedbaddecaf888",
            "522dc1f099567d07f47f37a32a84427d643a8cdcbfe5c0c97598a2bd2555d1aa",
            "8cb08e48590dbb3da7b08b1056828838c5f61e6393ba7a0abcc9f662898015ad",
            "b094dac5d93471bdec1a502270e3cc6c",
        ))
        .unwrap();
        let expected = hex_decode(concat!(
            "d9313225f88406e5a55909c5aff5269a86a7a9531534f7da2e4c303d8a318a72",
            "1c3c0c95956809532fcf0e2449a6b525b16aedf5aa0de657ba637b391aafd255",
        ))
        .unwrap();
        assert_eq!(decrypt(&key, &stored).unwrap(), expected);
    }

    #[test]
    fn different_calls_use_different_nonces() {
        let key = test_key();
        let a = encrypt(&key, b"same plaintext").unwrap();
        let b = encrypt(&key, b"same plaintext").unwrap();
        assert_ne!(a, b, "ciphertexts should differ due to random nonces");
    }

    #[test]
    fn decrypt_with_either_tries_new_key_first_then_falls_back_to_old() {
        let old_key = test_key();
        let new_key = [3u8; 32];
        let plaintext = b"rotate me";

        // Not-yet-rotated row: only decryptable under the old key.
        let old_ct = encrypt(&old_key, plaintext).unwrap();
        assert_eq!(
            decrypt_with_either(&new_key, &old_key, &old_ct).unwrap(),
            plaintext
        );

        // Already-rotated row (e.g. a prior interrupted run): decryptable under the new key
        // alone, so a rerun doesn't need old_key to still be valid.
        let new_ct = encrypt(&new_key, plaintext).unwrap();
        assert_eq!(
            decrypt_with_either(&new_key, &old_key, &new_ct).unwrap(),
            plaintext
        );
    }

    #[test]
    fn decrypt_with_either_falling_back_to_old_key_does_not_count_as_a_failure() {
        let _counter = crate::test_support::hold_decrypt_counter();
        let old_key = test_key();
        let new_key = [3u8; 32];
        let ciphertext = encrypt(&old_key, b"not yet rotated").unwrap();

        let before_count = decrypt_failures();
        assert!(decrypt_with_either(&new_key, &old_key, &ciphertext).is_ok());
        let after_count = decrypt_failures();
        assert_eq!(
            after_count, before_count,
            "an expected miss on the not-yet-active key during rotation must not count as a \
             decrypt failure"
        );
    }

    #[test]
    fn decrypt_with_either_fails_loudly_when_neither_key_works() {
        let _counter = crate::test_support::hold_decrypt_counter();
        let ct = encrypt(&[9u8; 32], b"data").unwrap();
        assert!(decrypt_with_either(&test_key(), &[8u8; 32], &ct).is_err());
    }

    #[test]
    fn decrypt_fails_with_wrong_key() {
        let _counter = crate::test_support::hold_decrypt_counter();
        let ciphertext = encrypt(&test_key(), b"data").unwrap();
        let wrong_key = [9u8; 32];
        assert!(decrypt(&wrong_key, &ciphertext).is_err());
    }

    /// Asserts the delta a single failing decrypt produces; the lock keeps other failing decrypts
    /// out.
    #[test]
    fn decrypt_failure_under_wrong_key_moves_the_counter_by_one() {
        let _counter = crate::test_support::hold_decrypt_counter();
        let before_count = decrypt_failures();

        let ciphertext = encrypt(&test_key(), b"data").unwrap();
        let wrong_key = [9u8; 32];
        assert!(decrypt(&wrong_key, &ciphertext).is_err());

        assert_eq!(decrypt_failures(), before_count + 1);
    }

    /// Current `arcodash_secret_decrypt_failures_total`.
    fn decrypt_failures() -> u64 {
        let mut rendered = String::new();
        crate::metrics::render(&mut rendered);
        rendered
            .lines()
            .find_map(|line| line.strip_prefix("arcodash_secret_decrypt_failures_total "))
            .and_then(|v| v.trim().parse().ok())
            .expect("counter line must be present")
    }

    #[test]
    fn parse_key_hex_round_trips() {
        let hex = "07".repeat(32);
        let parsed = parse_key_hex(&hex).unwrap();
        assert_eq!(parsed, [7u8; 32]);
    }

    #[test]
    fn parse_key_hex_rejects_wrong_length() {
        assert!(parse_key_hex("0011").is_err());
    }

    #[test]
    fn parse_key_hex_rejects_invalid_hex() {
        let bad = "zz".repeat(32);
        assert!(parse_key_hex(&bad).is_err());
    }

    #[test]
    fn tampered_ciphertext_fails_gcm_auth() {
        let _counter = crate::test_support::hold_decrypt_counter();
        let key = test_key();
        let mut ciphertext = encrypt(&key, b"secret data").unwrap();
        // Flip a byte in the sealed portion (after the nonce).
        let last = ciphertext.len() - 1;
        ciphertext[last] ^= 0xFF;
        assert!(decrypt(&key, &ciphertext).is_err());
    }

    #[test]
    fn ciphertext_shorter_than_nonce_errors() {
        let _counter = crate::test_support::hold_decrypt_counter();
        let key = test_key();
        let short = vec![0u8; NONCE_LEN - 1];
        let err = decrypt(&key, &short).unwrap_err();
        assert!(err.to_string().contains("too short"), "got: {err}");
    }

    #[test]
    fn parse_key_hex_accepts_uppercase() {
        let hex = "AB".repeat(32); // 64 uppercase hex chars
        let parsed = parse_key_hex(&hex).unwrap();
        assert_eq!(parsed, [0xABu8; 32]);
    }

    #[test]
    fn parse_key_hex_rejects_63_chars() {
        let hex = "0".repeat(63);
        assert!(parse_key_hex(&hex).is_err());
    }

    #[test]
    fn parse_key_hex_rejects_65_chars() {
        let hex = "0".repeat(65);
        assert!(parse_key_hex(&hex).is_err());
    }

    #[test]
    fn round_trip_empty_plaintext() {
        let key = test_key();
        let ct = encrypt(&key, b"").unwrap();
        let pt = decrypt(&key, &ct).unwrap();
        assert_eq!(pt, b"");
    }

    #[test]
    fn round_trip_large_plaintext() {
        let key = test_key();
        let big = vec![0xABu8; 1024 * 1024 + 7]; // >1 MB
        let ct = encrypt(&key, &big).unwrap();
        let pt = decrypt(&key, &ct).unwrap();
        assert_eq!(pt, big);
    }
}
