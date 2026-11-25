//! Bounded gzip decompression for object bytes that must be treated as untrusted.

use std::io::Read;

use flate2::read::GzDecoder;

#[derive(Debug, thiserror::Error)]
pub enum GunzipError {
    #[error("decompressed data exceeds {limit} bytes")]
    TooLarge { limit: usize },
    #[error("invalid gzip data: {0}")]
    Invalid(#[from] std::io::Error),
}

/// Decompresses `bytes`, reading at most `limit + 1` bytes of output so a compression bomb is
/// refused without being fully expanded.
///
/// # Errors
///
/// [`GunzipError::TooLarge`] when the output would exceed `limit`; [`GunzipError::Invalid`] for
/// malformed gzip.
pub fn gunzip_bounded(bytes: &[u8], limit: usize) -> Result<Vec<u8>, GunzipError> {
    read_bounded(GzDecoder::new(bytes), limit)
}

fn read_bounded(reader: impl Read, limit: usize) -> Result<Vec<u8>, GunzipError> {
    let mut out = Vec::new();
    reader
        .take(u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1))
        .read_to_end(&mut out)?;
    if out.len() > limit {
        return Err(GunzipError::TooLarge { limit });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use flate2::{write::GzEncoder, Compression};

    use super::*;

    fn gz(data: &[u8]) -> Vec<u8> {
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(data).unwrap();
        enc.finish().unwrap()
    }

    #[test]
    fn within_limit_round_trips() {
        assert_eq!(gunzip_bounded(&gz(b"hello"), 5).unwrap(), b"hello");
    }

    #[test]
    fn bomb_is_refused_without_full_expansion() {
        let bomb = gz(&vec![0u8; 1 << 20]);
        assert!(bomb.len() < 4096);
        let limit = 64 * 1024;
        assert!(matches!(
            gunzip_bounded(&bomb, limit),
            Err(GunzipError::TooLarge { limit: l }) if l == limit
        ));
    }

    #[test]
    fn unbounded_source_is_read_only_one_byte_past_the_limit() {
        struct Zeros(usize);
        impl Read for Zeros {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                self.0 += buf.len();
                assert!(self.0 <= 1024 + 8192, "read far past the limit");
                buf.fill(0);
                Ok(buf.len())
            }
        }
        assert!(matches!(
            read_bounded(Zeros(0), 1024),
            Err(GunzipError::TooLarge { limit: 1024 })
        ));
    }

    #[test]
    fn garbage_is_invalid() {
        assert!(matches!(
            gunzip_bounded(b"not gzip", 10),
            Err(GunzipError::Invalid(_))
        ));
    }
}
