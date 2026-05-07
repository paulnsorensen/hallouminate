use std::fs::File;
use std::io;
use std::path::Path;

use crate::domains::common::Result;

pub fn blake3_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    io::copy(&mut file, &mut hasher)?;
    Ok(hasher.finalize().to_hex().to_string())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn write_tmp(name: &str, body: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join(name);
        fs::write(&path, body).expect("write");
        (tmp, path)
    }

    #[test]
    fn blake3_file_matches_official_abc_vector() {
        let (_tmp, path) = write_tmp("abc.txt", b"abc");
        let hex = blake3_file(&path).expect("hash");
        assert_eq!(
            hex,
            "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85"
        );
    }

    #[test]
    fn blake3_file_matches_official_empty_vector() {
        let (_tmp, path) = write_tmp("empty.txt", b"");
        let hex = blake3_file(&path).expect("hash");
        assert_eq!(
            hex,
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        );
    }

    #[test]
    fn blake3_file_distinguishes_distinct_content() {
        let (_t1, p1) = write_tmp("a", b"the spice");
        let (_t2, p2) = write_tmp("b", b"must flow");
        assert_ne!(blake3_file(&p1).unwrap(), blake3_file(&p2).unwrap());
    }

    #[test]
    fn blake3_file_streams_large_input_correctly() {
        let body = vec![0xABu8; 256 * 1024];
        let (_tmp, path) = write_tmp("big.bin", &body);
        let streamed = blake3_file(&path).expect("hash");
        let in_memory = blake3::hash(&body).to_hex().to_string();
        assert_eq!(streamed, in_memory);
    }

    #[test]
    fn blake3_file_missing_path_returns_io_error() {
        let err = blake3_file(Path::new("/no/such/file/aaa")).expect_err("must fail");
        assert!(
            matches!(err, crate::domains::common::HallouminateError::Io(_)),
            "expected Io variant, got {err:?}"
        );
    }
}
