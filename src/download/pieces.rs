//! Piece checksums (Metalink `<pieces>`): verify a finished file chunk by chunk,
//! so one damaged chunk can be repaired without downloading the whole file again.

use anyhow::Result;
use std::path::Path;

use super::checksum::{hash_range, HashAlgo};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PieceHashes {
    pub algo: HashAlgo,
    /// Size of every piece except possibly the last one.
    pub length: u64,
    /// Lowercase hex digest per piece, in file order.
    pub hashes: Vec<String>,
}

impl PieceHashes {
    /// Builds a table, or `None` if it cannot be trusted (no pieces, zero length,
    /// or a digest of the wrong size / not hex).
    pub fn new(algo: HashAlgo, length: u64, hashes: Vec<String>) -> Option<Self> {
        if length == 0 || hashes.is_empty() {
            return None;
        }
        let hashes: Option<Vec<String>> = hashes
            .iter()
            .map(|h| super::checksum::Checksum::new(algo, h).ok().map(|c| c.hex))
            .collect();
        Some(Self {
            algo,
            length,
            hashes: hashes?,
        })
    }

    /// Whether the table describes a file of `total` bytes (the right number of pieces).
    pub fn fits(&self, total: u64) -> bool {
        self.length > 0 && self.hashes.len() as u64 == total.div_ceil(self.length)
    }

    /// Byte range `[start, end)` of piece `index` in a file of `total` bytes.
    pub fn range(&self, index: usize, total: u64) -> (u64, u64) {
        let start = index as u64 * self.length;
        (start, (start + self.length).min(total))
    }
}

/// Indexes of the pieces of `path` whose hash does not match.
pub async fn bad_pieces(path: &Path, pieces: &PieceHashes, total: u64) -> Result<Vec<usize>> {
    let mut bad = Vec::new();
    for (i, expected) in pieces.hashes.iter().enumerate() {
        let (start, end) = pieces.range(i, total);
        let actual = hash_range(path, pieces.algo, start, end - start).await?;
        if !actual.eq_ignore_ascii_case(expected) {
            bad.push(i);
        }
    }
    Ok(bad)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::download::checksum::hash_bytes;

    fn table(data: &[u8], length: usize) -> PieceHashes {
        PieceHashes {
            algo: HashAlgo::Sha1,
            length: length as u64,
            hashes: data
                .chunks(length)
                .map(|c| hash_bytes(HashAlgo::Sha1, c))
                .collect(),
        }
    }

    fn temp(data: &[u8]) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("rdl-pieces-{}", uuid::Uuid::new_v4()));
        std::fs::write(&p, data).unwrap();
        p
    }

    #[test]
    fn geometry() {
        let t = table(&[0u8; 25], 10);
        assert!(t.fits(25));
        assert!(!t.fits(31), "31 bytes need four pieces");
        assert_eq!(t.range(0, 25), (0, 10));
        assert_eq!(t.range(2, 25), (20, 25), "last piece is shorter");
    }

    #[tokio::test]
    async fn intact_file_has_no_bad_pieces() {
        let data: Vec<u8> = (0..95u8).collect();
        let p = temp(&data);
        assert!(bad_pieces(&p, &table(&data, 10), 95)
            .await
            .unwrap()
            .is_empty());
        let _ = std::fs::remove_file(p);
    }

    #[tokio::test]
    async fn damaged_pieces_are_found() {
        let data: Vec<u8> = (0..95u8).collect();
        let t = table(&data, 10);
        let mut broken = data.clone();
        broken[13] ^= 0xff; // piece 1
        broken[94] ^= 0xff; // last, short piece 9
        let p = temp(&broken);
        assert_eq!(bad_pieces(&p, &t, 95).await.unwrap(), vec![1, 9]);
        let _ = std::fs::remove_file(p);
    }
}
