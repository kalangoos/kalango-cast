//! The writer's byte-level guarantees (offsets, where fragments are cut) and the safe opening of the output file.

use std::io::ErrorKind;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};

use cast_core::VideoCodec;
use cast_mux::{avc, open_output, Config, VideoTrack, Writer};

const PARAMS: &[u8] =
    &[0, 0, 0, 1, 0x67, 0x64, 0x00, 0x28, 0xac, 0xd9, 0x40, 0x78, 0x02, 0x27, 0xe5, 0x84, 0, 0, 0, 1, 0x68, 0xeb, 0xe3, 0xcb, 0x22, 0xc0];

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("kalango-cast-mux-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn config(fragment_ns: u64) -> Config {
    Config {
        video: Some(VideoTrack { codec: VideoCodec::H264, width: 1920, height: 1080, config: avc::avcc(PARAMS).unwrap() }),
        audio: vec![],
        fragment_ns,
        sync: false,
    }
}

/// The top-level boxes of a file: (type, start, size).
fn top_boxes(data: &[u8]) -> Vec<([u8; 4], usize, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 8 <= data.len() {
        let size = u32::from_be_bytes(data[i..i + 4].try_into().unwrap()) as usize;
        out.push((data[i + 4..i + 8].try_into().unwrap(), i, size));
        i += size.max(8);
    }
    out
}

/// Writes `frames` video samples at 30 fps; sample `i` is 100 bytes of `(i + 1) as u8`; keyframes at `keys`.
fn write(path: &Path, fragment_ns: u64, frames: u64, keys: &[u64]) {
    let mut w = Writer::create(path, &config(fragment_ns), false).unwrap();
    for i in 0..frames {
        w.write_video(&[(i + 1) as u8; 100], i * 1_000_000_000 / 30, keys.contains(&i)).unwrap();
    }
    w.finish().unwrap();
}

#[test]
fn the_trun_data_offset_points_at_the_samples_bytes() {
    let dir = scratch("offset");
    let path = dir.join("a.mp4");
    // a keyframe at 0 and at 2 s: the second flushes the first fragment (60 samples)
    write(&path, 2_000_000_000, 90, &[0, 60]);
    let data = std::fs::read(&path).unwrap();
    let moofs: Vec<_> = top_boxes(&data).into_iter().filter(|b| &b.0 == b"moof").collect();
    assert_eq!(moofs.len(), 2);
    for (n, (_, start, size)) in moofs.iter().enumerate() {
        let moof = &data[*start..start + size];
        let at = moof.windows(4).position(|w| w == b"trun").expect("trun");
        // after the type: version+flags (4), sample_count (4), data_offset (4)
        let offset = u32::from_be_bytes(moof[at + 12..at + 16].try_into().unwrap()) as usize;
        let first = if n == 0 { 1u8 } else { 61u8 };
        assert_eq!(data[start + offset], first, "fragment {n}: the first sample's byte");
        assert_eq!(&data[start + offset..start + offset + 100], &[first; 100]);
        // and the mdat header sits right before it
        assert_eq!(&data[start + offset - 4..start + offset], b"mdat");
    }
}

#[test]
fn fragments_are_cut_on_keyframes_only() {
    let dir = scratch("cut");
    let path = dir.join("a.mp4");
    // 5 s at 30 fps with a 1 s fragment target but a single keyframe: no early cut on a non-key frame
    write(&path, 1_000_000_000, 150, &[0]);
    let data = std::fs::read(&path).unwrap();
    assert_eq!(top_boxes(&data).iter().filter(|b| &b.0 == b"moof").count(), 1);
    // a second keyframe at 2.5 s: exactly one more fragment
    let path = dir.join("b.mp4");
    write(&path, 1_000_000_000, 150, &[0, 75]);
    let data = std::fs::read(&path).unwrap();
    assert_eq!(top_boxes(&data).iter().filter(|b| &b.0 == b"moof").count(), 2);
}

#[test]
fn a_new_file_is_private_and_exclusive() {
    let dir = scratch("new");
    let p = dir.join("a.mp4");
    let f = open_output(&p, false).unwrap();
    assert_eq!(f.metadata().unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(open_output(&p, false).unwrap_err().kind(), ErrorKind::AlreadyExists);
}

#[test]
fn overwrite_only_replaces_our_own_recordings() {
    let dir = scratch("over");
    // an MP4 of ours is replaced (truncated)
    let mp4 = dir.join("old.mp4");
    std::fs::write(&mp4, b"\0\0\0\x18ftypisom\0\0\0\0isomiso2").unwrap();
    let mut f = open_output(&mp4, true).unwrap();
    assert_eq!(std::fs::metadata(&mp4).unwrap().len(), 0);
    // the new recording starts at byte 0 (no hole left by the check that read the old header)
    std::io::Write::write_all(&mut f, b"new").unwrap();
    assert_eq!(std::fs::read(&mp4).unwrap(), b"new");
    // an empty file too
    let empty = dir.join("empty.mp4");
    std::fs::write(&empty, b"").unwrap();
    open_output(&empty, true).unwrap();
    // anything else is refused and left untouched, like a shell profile
    let rc = dir.join(".bashrc");
    std::fs::write(&rc, b"export PATH=$HOME/bin:$PATH\n").unwrap();
    assert_eq!(open_output(&rc, true).unwrap_err().kind(), ErrorKind::PermissionDenied);
    assert_eq!(std::fs::read(&rc).unwrap(), b"export PATH=$HOME/bin:$PATH\n");
}

#[test]
fn a_symlink_at_the_path_is_never_followed() {
    let dir = scratch("link");
    let victim = dir.join("victim.txt");
    std::fs::write(&victim, b"precious").unwrap();
    for (name, overwrite) in [("l1.mp4", false), ("l2.mp4", true)] {
        let link = dir.join(name);
        // as if planted between a check and the open: the open itself must refuse
        symlink(&victim, &link).unwrap();
        assert!(open_output(&link, overwrite).is_err(), "{name}");
        assert_eq!(std::fs::read(&victim).unwrap(), b"precious");
    }
    // nor a link to a recording of ours (it would pass the content check if it were followed)
    let own = dir.join("own.mp4");
    let own_bytes = b"\0\0\0\x18ftypisom\0\0\0\0isomiso2".to_vec();
    std::fs::write(&own, &own_bytes).unwrap();
    let to_own = dir.join("to-own.mp4");
    symlink(&own, &to_own).unwrap();
    assert!(open_output(&to_own, true).is_err());
    assert!(open_output(&to_own, false).is_err());
    assert_eq!(std::fs::read(&own).unwrap(), own_bytes, "our own recording is untouched");
    // a dangling link is not created through either
    let dangling = dir.join("dangling.mp4");
    symlink(dir.join("nowhere"), &dangling).unwrap();
    assert!(open_output(&dangling, false).is_err());
    assert!(!dir.join("nowhere").exists());
    // a hard link (a second name for someone's file) is refused too
    let hard = dir.join("hard.mp4");
    std::fs::write(dir.join("other.mp4"), b"").unwrap();
    std::fs::hard_link(dir.join("other.mp4"), &hard).unwrap();
    assert!(open_output(&hard, true).is_err());
}

#[test]
fn the_folder_must_be_a_real_folder_of_ours() {
    let dir = scratch("folder");
    let real = dir.join("real");
    std::fs::create_dir(&real).unwrap();
    let via_link = dir.join("link");
    symlink(&real, &via_link).unwrap();
    assert!(open_output(&via_link.join("a.mp4"), false).is_err(), "the folder itself is a link");
    // the root folder belongs to root (unless the tests run as root)
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        assert_eq!(open_output(Path::new("/kalango-cast-test.mp4"), false).unwrap_err().kind(), ErrorKind::PermissionDenied);
    }
    assert!(open_output(Path::new("relative.mp4"), false).is_err());
    // a folder of someone else (root's /usr) is refused with the reason, and nothing is created there
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        let e = open_output(Path::new("/usr/kalango-cast-test.mp4"), false).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::PermissionDenied);
        assert!(e.to_string().contains("not yours"), "{e}");
        assert!(!Path::new("/usr/kalango-cast-test.mp4").exists());
    }
}
