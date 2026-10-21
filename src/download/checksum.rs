//! Checksum verification of finished downloads (only when the server or a plugin supplies a hash).

use anyhow::{bail, Context, Result};
use md5::Md5;
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha512};
use std::fmt;
use std::path::Path;
use std::str::FromStr;
use tokio::fs::File;
use tokio::io::AsyncReadExt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashAlgo {
    Md5,
    Sha1,
    Sha256,
    Sha512,
}

impl HashAlgo {
    fn hex_len(self) -> usize {
        match self {
            Self::Md5 => 32,
            Self::Sha1 => 40,
            Self::Sha256 => 64,
            Self::Sha512 => 128,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Md5 => "md5",
            Self::Sha1 => "sha1",
            Self::Sha256 => "sha256",
            Self::Sha512 => "sha512",
        }
    }
}

impl FromStr for HashAlgo {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        Ok(match s.to_ascii_lowercase().replace('-', "").as_str() {
            "md5" => Self::Md5,
            "sha1" => Self::Sha1,
            "sha256" => Self::Sha256,
            "sha512" => Self::Sha512,
            other => bail!("unsupported hash algorithm: {other}"),
        })
    }
}

/// Expected hash of a file: algorithm plus lowercase hex digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checksum {
    pub algo: HashAlgo,
    pub hex: String,
}

impl Checksum {
    pub fn new(algo: HashAlgo, hex: &str) -> Result<Self> {
        let hex = hex.trim().to_ascii_lowercase();
        if hex.len() != algo.hex_len() || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!(
                "invalid {} digest: expected {} hex characters",
                algo.name(),
                algo.hex_len()
            );
        }
        Ok(Self { algo, hex })
    }
}

/// Parses `algo:hexdigest`, e.g. `sha256:ab12…`.
impl FromStr for Checksum {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let (algo, hex) = s
            .split_once(':')
            .context("checksum must look like `algo:hexdigest`")?;
        Self::new(algo.parse()?, hex)
    }
}

impl fmt::Display for Checksum {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.algo.name(), self.hex)
    }
}

enum Hasher {
    Md5(Md5),
    Sha1(Sha1),
    Sha256(Sha256),
    Sha512(Sha512),
}

impl Hasher {
    fn new(algo: HashAlgo) -> Self {
        match algo {
            HashAlgo::Md5 => Self::Md5(Md5::new()),
            HashAlgo::Sha1 => Self::Sha1(Sha1::new()),
            HashAlgo::Sha256 => Self::Sha256(Sha256::new()),
            HashAlgo::Sha512 => Self::Sha512(Sha512::new()),
        }
    }

    fn update(&mut self, data: &[u8]) {
        match self {
            Self::Md5(h) => h.update(data),
            Self::Sha1(h) => h.update(data),
            Self::Sha256(h) => h.update(data),
            Self::Sha512(h) => h.update(data),
        }
    }

    fn finish_hex(self) -> String {
        let bytes: Vec<u8> = match self {
            Self::Md5(h) => h.finalize().to_vec(),
            Self::Sha1(h) => h.finalize().to_vec(),
            Self::Sha256(h) => h.finalize().to_vec(),
            Self::Sha512(h) => h.finalize().to_vec(),
        };
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }
}

/// Streams `path` through the hash; never holds the whole file in memory.
pub async fn hash_file(path: &Path, algo: HashAlgo) -> Result<String> {
    let mut file = File::open(path)
        .await
        .with_context(|| format!("opening {path:?} for hashing"))?;
    let mut hasher = Hasher::new(algo);
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finish_hex())
}

/// Fails with both digests in the message if the file does not match.
pub async fn verify_file(path: &Path, expected: &Checksum) -> Result<()> {
    let actual = hash_file(path, expected.algo).await?;
    if actual != expected.hex {
        bail!(
            "checksum mismatch ({}): expected {}, got {actual}",
            expected.algo.name(),
            expected.hex
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Well-known digests of the ASCII string "abc".
    const MD5: &str = "900150983cd24fb0d6963f7d28e17f72";
    const SHA1: &str = "a9993e364706816aba3e25717850c26c9cd0d89d";
    const SHA256: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    const SHA512: &str = "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f";

    async fn abc_file() -> (tempdir::Guard, std::path::PathBuf) {
        let dir = tempdir::Guard::new();
        let path = dir.0.join("abc.bin");
        tokio::fs::write(&path, b"abc").await.unwrap();
        (dir, path)
    }

    /// Minimal temp dir so no extra dev-dependency is needed.
    mod tempdir {
        pub struct Guard(pub std::path::PathBuf);
        impl Guard {
            pub fn new() -> Self {
                let p = std::env::temp_dir().join(format!("rdl-checksum-{}", uuid::Uuid::new_v4()));
                std::fs::create_dir_all(&p).unwrap();
                Self(p)
            }
        }
        impl Drop for Guard {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    #[tokio::test]
    async fn all_algorithms_match_known_vectors() {
        let (_g, path) = abc_file().await;
        for (algo, hex) in [
            (HashAlgo::Md5, MD5),
            (HashAlgo::Sha1, SHA1),
            (HashAlgo::Sha256, SHA256),
            (HashAlgo::Sha512, SHA512),
        ] {
            assert_eq!(hash_file(&path, algo).await.unwrap(), hex);
        }
    }

    #[tokio::test]
    async fn verify_accepts_match_and_rejects_mismatch() {
        let (_g, path) = abc_file().await;
        let good: Checksum = format!("SHA-256:{}", SHA256.to_uppercase())
            .parse()
            .unwrap();
        verify_file(&path, &good).await.unwrap();

        let bad = Checksum::new(HashAlgo::Sha256, &"0".repeat(64)).unwrap();
        let err = verify_file(&path, &bad).await.unwrap_err().to_string();
        assert!(err.contains("mismatch") && err.contains(SHA256));
    }

    #[test]
    fn parsing_rejects_garbage() {
        assert!("nocolon".parse::<Checksum>().is_err());
        assert!("crc32:abcd".parse::<Checksum>().is_err());
        assert!("sha256:abcd".parse::<Checksum>().is_err());
        assert!(format!("md5:{}", "g".repeat(32))
            .parse::<Checksum>()
            .is_err());
    }

    #[test]
    fn display_roundtrips() {
        let c: Checksum = format!("sha1:{SHA1}").parse().unwrap();
        assert_eq!(c.to_string().parse::<Checksum>().unwrap(), c);
    }

    #[tokio::test]
    async fn missing_file_is_an_error() {
        assert!(hash_file(Path::new("/nonexistent/rdl"), HashAlgo::Md5)
            .await
            .is_err());
    }
}
