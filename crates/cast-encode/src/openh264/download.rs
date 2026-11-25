//! Cisco's OpenH264 binary, downloaded on demand.
//!
//! Cisco pays the H.264 patent royalties for its own binary when each person downloads it from Cisco; so Kalango Cast
//! never ships it, and downloads it only when the person asks for the software encoder (no hardware one). The
//! download is checked against SHA-256 pins compiled in here (the compressed file and the library inside it), and the
//! library is checked again every time it is loaded.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// The pinned release.
pub struct Release {
    pub version: &'static str,
    pub url: &'static str,
    /// SHA-256 of the `.so.bz2` Cisco serves.
    pub sha256_bz2: &'static str,
    /// SHA-256 of the library inside it.
    pub sha256_so: &'static str,
    pub file_name: &'static str,
}

pub const RELEASE: Release = Release {
    version: "2.6.0",
    url: "https://ciscobinary.openh264.org/libopenh264-2.6.0-linux64.8.so.bz2",
    sha256_bz2: "27ab53323c110b76214c1c72222f459d17febbcd1e252136cadc292b0308d75b",
    sha256_so: "2f0cde7c6a6abcf5cae76942894ea42897fa677bce4ed6c91a24dd1b041d5f04",
    file_name: "libopenh264-2.6.0-linux64.8.so",
};

/// Most a download may be (the real one is ~0.6 MB): a server sending more is refused.
const MAX_DOWNLOAD: u64 = 16 << 20;

#[derive(Debug)]
pub enum DownloadError {
    Io(io::Error),
    Http(String),
    /// The bytes do not match the pin (expected, got).
    Mismatch(&'static str, String),
    Decompress(String),
}

impl std::fmt::Display for DownloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            DownloadError::Io(e) => write!(f, "{e}"),
            DownloadError::Http(e) => write!(f, "{e}"),
            DownloadError::Mismatch(want, got) => write!(f, "SHA-256 {got} ≠ {want}"),
            DownloadError::Decompress(e) => write!(f, "bzip2: {e}"),
        }
    }
}

impl From<io::Error> for DownloadError {
    fn from(e: io::Error) -> Self {
        DownloadError::Io(e)
    }
}

pub fn sha256_hex(data: &[u8]) -> String {
    let d = Sha256::digest(data);
    d.iter().map(|b| format!("{b:02x}")).collect()
}

/// Where the library lives: `$XDG_DATA_HOME/kalango-cast/openh264/` (`~/.local/share/...`).
pub fn cache_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))?;
    Some(base.join("kalango-cast").join("openh264"))
}

/// The verified library in `dir`, if it is there and matches the pin.
pub fn verified_library(dir: &Path) -> Option<PathBuf> {
    let path = dir.join(RELEASE.file_name);
    let data = std::fs::read(&path).ok()?;
    (sha256_hex(&data) == RELEASE.sha256_so).then_some(path)
}

/// Checks a downloaded `.so.bz2` against the pins and returns the library bytes.
pub fn verify_and_unpack(bz2: &[u8]) -> Result<Vec<u8>, DownloadError> {
    let got = sha256_hex(bz2);
    if got != RELEASE.sha256_bz2 {
        return Err(DownloadError::Mismatch(RELEASE.sha256_bz2, got));
    }
    let mut so = Vec::with_capacity(2 << 20);
    bzip2_rs::DecoderReader::new(bz2).take(MAX_DOWNLOAD * 4).read_to_end(&mut so).map_err(|e| DownloadError::Decompress(e.to_string()))?;
    let got = sha256_hex(&so);
    if got != RELEASE.sha256_so {
        return Err(DownloadError::Mismatch(RELEASE.sha256_so, got));
    }
    Ok(so)
}

/// Installs verified library bytes into `dir` (written to a temporary name, then renamed: never a half file).
pub fn install(dir: &Path, so: &[u8]) -> Result<PathBuf, DownloadError> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(RELEASE.file_name);
    let tmp = dir.join(format!(".{}.part", RELEASE.file_name));
    let mut f = std::fs::File::create(&tmp)?;
    f.write_all(so)?;
    f.sync_all()?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

/// Downloads, verifies and installs the library into `dir` (or returns the one already there).
pub fn fetch(dir: &Path) -> Result<PathBuf, DownloadError> {
    if let Some(p) = verified_library(dir) {
        return Ok(p);
    }
    let response = ureq::get(RELEASE.url).call().map_err(|e| DownloadError::Http(e.to_string()))?;
    let mut bz2 = Vec::with_capacity(1 << 20);
    response.into_body().into_reader().take(MAX_DOWNLOAD).read_to_end(&mut bz2)?;
    let so = verify_and_unpack(&bz2)?;
    install(dir, &so)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_known_vectors() {
        assert_eq!(sha256_hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }

    #[test]
    fn a_tampered_download_is_refused_before_unpacking() {
        let err = verify_and_unpack(b"BZh91AY&SY not cisco").unwrap_err();
        assert!(matches!(err, DownloadError::Mismatch(want, _) if want == RELEASE.sha256_bz2));
    }

    #[test]
    fn a_library_that_does_not_match_the_pin_is_not_used() {
        let dir = std::env::temp_dir().join(format!("kalango-cast-openh264-pin-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(RELEASE.file_name), b"not the library").unwrap();
        assert!(verified_library(&dir).is_none());
    }

    /// The real download (network): set KALANGO_CAST_OPENH264_NET=1 to run it.
    #[test]
    fn downloads_and_verifies_ciscos_binary() {
        if std::env::var_os("KALANGO_CAST_OPENH264_NET").is_none() {
            eprintln!("skipped: KALANGO_CAST_OPENH264_NET not set");
            return;
        }
        let dir = std::env::temp_dir().join(format!("kalango-cast-openh264-net-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let p = fetch(&dir).unwrap();
        assert_eq!(verified_library(&dir), Some(p));
    }
}
