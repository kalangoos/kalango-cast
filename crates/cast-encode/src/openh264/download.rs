//! Cisco's OpenH264 binary, downloaded on demand.
//!
//! Cisco pays the H.264 patent royalties for its own binary when each person downloads it from Cisco; so Kalango Cast
//! never ships it, and downloads it only when the person asks for the software encoder (no hardware one). The
//! download is checked against SHA-256 pins compiled in here (the compressed file and the library inside it), and the
//! library is checked again every time it is loaded.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};
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
/// Most the unpacked library, or one read back from disk, may be (the real one is ~1.5 MB).
const MAX_LIBRARY: u64 = 64 << 20;

#[derive(Debug)]
pub enum DownloadError {
    Io(io::Error),
    Http(String),
    /// The bytes do not match the pin (expected, got).
    Mismatch(&'static str, String),
    Decompress(String),
    /// More bytes than the limit.
    TooBig,
}

impl std::fmt::Display for DownloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            DownloadError::Io(e) => write!(f, "{e}"),
            DownloadError::Http(e) => write!(f, "{e}"),
            DownloadError::Mismatch(want, got) => write!(f, "SHA-256 {got} ≠ {want}"),
            DownloadError::Decompress(e) => write!(f, "bzip2: {e}"),
            DownloadError::TooBig => write!(f, "too big"),
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

/// Reads at most `max` bytes; more is an error (never a silently truncated read).
fn read_limited(r: impl Read, max: u64) -> Result<Vec<u8>, DownloadError> {
    let mut v = Vec::new();
    r.take(max + 1).read_to_end(&mut v)?;
    if v.len() as u64 > max {
        return Err(DownloadError::TooBig);
    }
    Ok(v)
}

/// Opens `path` once, reads it, and returns its bytes only if their SHA-256 is `pin`: what was hashed is what the
/// caller gets, so nothing can be swapped between the check and the use (load these bytes, not the path:
/// [`memfd_copy`]).
pub fn read_checked(path: &Path, pin: &str) -> Option<Vec<u8>> {
    let data = read_limited(File::open(path).ok()?, MAX_LIBRARY).ok()?;
    (sha256_hex(&data) == pin).then_some(data)
}

/// An anonymous in-memory file holding `bytes` (for `dlopen("/proc/self/fd/N")`): the library that is loaded is
/// exactly the bytes that were verified, whatever happens to the file on disk afterwards.
pub fn memfd_copy(bytes: &[u8]) -> io::Result<File> {
    // SAFETY: memfd_create with a static NUL-terminated name.
    let fd = unsafe { libc::memfd_create(c"kalango-cast-openh264".as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fd is a fresh valid descriptor that nothing else owns.
    let mut f = File::from(unsafe { OwnedFd::from_raw_fd(fd) });
    f.write_all(bytes)?;
    Ok(f)
}

/// The verified library in `dir`, if it is there and matches the pin.
pub fn verified_library(dir: &Path) -> Option<PathBuf> {
    let path = dir.join(RELEASE.file_name);
    read_checked(&path, RELEASE.sha256_so).map(|_| path)
}

/// Checks a downloaded `.so.bz2` against the pins and returns the library bytes.
pub fn verify_and_unpack(bz2: &[u8]) -> Result<Vec<u8>, DownloadError> {
    unpack_checked(bz2, RELEASE.sha256_bz2, RELEASE.sha256_so, MAX_LIBRARY)
}

/// [`verify_and_unpack`] with the pins and the size limit as parameters.
fn unpack_checked(bz2: &[u8], pin_bz2: &'static str, pin_so: &'static str, max_so: u64) -> Result<Vec<u8>, DownloadError> {
    let got = sha256_hex(bz2);
    if got != pin_bz2 {
        return Err(DownloadError::Mismatch(pin_bz2, got));
    }
    let so = read_limited(bzip2_rs::DecoderReader::new(bz2), max_so).map_err(|e| match e {
        DownloadError::Io(e) => DownloadError::Decompress(e.to_string()),
        other => other,
    })?;
    let got = sha256_hex(&so);
    if got != pin_so {
        return Err(DownloadError::Mismatch(pin_so, got));
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
    let bz2 = read_limited(response.into_body().into_reader(), MAX_DOWNLOAD)?;
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

    const TEST_BZ2: &str = "425a6839314159265359797371d700000cd9800010400415002ae7de00200048a9e9a9e498f5061429a3d268d343408861cb1e71bf8801d2f5eaaf3baf7972b69ccda1928168b3959ba1d6c0407c5dc914e14241e5cdc75c";
    const TEST_BZ2_SHA: &str = "008dfa283580d50775abd0d1225762a454c97202d01b96f35304899bd105ac4b";
    const TEST_SO_SHA: &str = "a334f5763a3fc54050f0975de9edb7dc511aefdcde81e6139bea5d51946f5211";

    fn test_bz2() -> Vec<u8> {
        (0..TEST_BZ2.len()).step_by(2).map(|i| u8::from_str_radix(&TEST_BZ2[i..i + 2], 16).unwrap()).collect()
    }

    #[test]
    fn unpacking_checks_both_pins_and_the_size() {
        let bz2 = test_bz2();
        let ok = unpack_checked(&bz2, TEST_BZ2_SHA, TEST_SO_SHA, 1 << 20).unwrap();
        assert_eq!(sha256_hex(&ok), TEST_SO_SHA);
        // the compressed file's pin
        let bad = "0000000000000000000000000000000000000000000000000000000000000000";
        assert!(matches!(unpack_checked(&bz2, bad, TEST_SO_SHA, 1 << 20), Err(DownloadError::Mismatch(w, _)) if w == bad));
        // the library's pin (the bytes inside are not the pinned ones)
        assert!(matches!(unpack_checked(&bz2, TEST_BZ2_SHA, bad, 1 << 20), Err(DownloadError::Mismatch(w, _)) if w == bad));
        // the size limit: exactly the size passes, one byte less is refused (not silently truncated)
        assert!(unpack_checked(&bz2, TEST_BZ2_SHA, TEST_SO_SHA, 56).is_ok());
        assert!(matches!(unpack_checked(&bz2, TEST_BZ2_SHA, TEST_SO_SHA, 55), Err(DownloadError::TooBig)));
    }

    #[test]
    fn reads_are_limited_without_truncating() {
        assert_eq!(read_limited(&b"abcd"[..], 4).unwrap(), b"abcd");
        assert!(matches!(read_limited(&b"abcde"[..], 4), Err(DownloadError::TooBig)));
    }

    #[test]
    fn the_checked_bytes_are_the_bytes_that_get_loaded() {
        let dir = std::env::temp_dir().join(format!("kalango-cast-openh264-fd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("lib.so");
        std::fs::write(&p, b"library bytes").unwrap();
        let pin = sha256_hex(b"library bytes");
        let bytes = read_checked(&p, &pin).unwrap();
        // the file is swapped after the check: the in-memory copy still holds what was verified
        let mem = memfd_copy(&bytes).unwrap();
        std::fs::write(&p, b"evil").unwrap();
        let via = format!("/proc/self/fd/{}", std::os::fd::AsRawFd::as_raw_fd(&mem));
        assert_eq!(std::fs::read(via).unwrap(), b"library bytes");
        assert!(read_checked(&p, &pin).is_none(), "the swapped file no longer matches");
        assert!(read_checked(&dir.join("missing"), &pin).is_none());
        let _ = std::fs::remove_dir_all(&dir);
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
    #[ignore = "needs the network: set KALANGO_CAST_OPENH264_NET=1 and pass --ignored"]
    fn downloads_and_verifies_ciscos_binary() {
        assert!(std::env::var_os("KALANGO_CAST_OPENH264_NET").is_some(), "set KALANGO_CAST_OPENH264_NET=1");
        let dir = std::env::temp_dir().join(format!("kalango-cast-openh264-net-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let p = fetch(&dir).unwrap();
        assert_eq!(verified_library(&dir), Some(p));
    }
}
