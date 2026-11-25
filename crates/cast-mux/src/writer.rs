//! The fragmented MP4 writer.

use std::ffi::CString;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::Path;

use cast_core::clock::ns_to_ticks;
use cast_core::VideoCodec;

use crate::boxes::{self, close, open, open_full};

/// Video track timescale (ticks per second): the MPEG 90 kHz clock, fine enough for variable frame rates.
pub const VIDEO_TIMESCALE: u32 = 90_000;
/// Audio track timescale: Opus always runs at 48 kHz.
pub const AUDIO_TIMESCALE: u32 = 48_000;
/// The movie (mvhd) timescale.
const MOVIE_TIMESCALE: u32 = 1000;

/// Sample flags (ISO/IEC 14496-12 8.8.3.1): a sync sample depends on nothing; others depend on earlier ones and are
/// not sync samples.
const FLAGS_SYNC: u32 = 0x0200_0000;
const FLAGS_NON_SYNC: u32 = 0x0101_0000;

/// A fragment is flushed early past this much pending data (a long GOP at a high bitrate).
const MAX_PENDING_BYTES: usize = 48 << 20;

#[derive(Clone, Debug)]
pub struct VideoTrack {
    pub codec: VideoCodec,
    pub width: u32,
    pub height: u32,
    /// The `avcC` / `hvcC` payload ([`crate::avc::codec_config`]).
    pub config: Vec<u8>,
}

/// An Opus track.
#[derive(Clone, Debug)]
pub struct AudioTrack {
    pub channels: u8,
    /// Samples (at 48 kHz) the decoder drops at the start (the encoder's lookahead).
    pub pre_skip: u16,
    pub input_sample_rate: u32,
    /// The track's name (handler name), for people: "Desktop", "Microphone".
    pub name: String,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub video: Option<VideoTrack>,
    pub audio: Vec<AudioTrack>,
    /// Target fragment length (a video fragment ends on the first keyframe after it).
    pub fragment_ns: u64,
    /// fdatasync each fragment before publishing it (crash safety; on by default).
    pub sync: bool,
}

#[derive(Clone, Copy, Debug)]
struct SampleMeta {
    duration: u32,
    size: u32,
    flags: u32,
}

struct TrackState {
    id: u32,
    timescale: u32,
    video: bool,
    /// Decode time (ticks) of the first pending sample.
    base_ticks: Option<u64>,
    /// Decode time (ticks) after the last pending sample.
    end_ticks: u64,
    samples: Vec<SampleMeta>,
    data: Vec<u8>,
    /// Video: the last sample, waiting for the next one's timestamp to know its duration.
    held: Option<(u64, bool)>,
    held_data: Vec<u8>,
    last_duration: u32,
    count: u64,
}

impl TrackState {
    fn pending_ticks(&self) -> u64 {
        self.base_ticks.map_or(0, |b| self.end_ticks - b)
    }

    fn push(&mut self, at_ticks: u64, duration: u32, flags: u32, data: &[u8]) {
        if self.base_ticks.is_none() {
            self.base_ticks = Some(at_ticks);
            self.end_ticks = at_ticks;
        }
        self.end_ticks += duration as u64;
        self.samples.push(SampleMeta { duration, size: data.len() as u32, flags });
        self.data.extend_from_slice(data);
        self.count += 1;
    }
}

/// What a finished recording holds.
#[derive(Clone, Debug, Default)]
pub struct Summary {
    pub duration_ns: u64,
    pub video_frames: u64,
    pub audio_packets: u64,
    pub fragments: u32,
    pub bytes: u64,
}

/// Opens the output file safely: the folder must be a real directory (not a link) owned by this user; a new file
/// is created exclusively (never through a link, mode 0600); an existing one is replaced only with `overwrite`, only if
/// it is a regular file of this user with a single name that is empty or already an MP4 (`ftyp`), and it is
/// checked on the open file (no check-then-open race), so `.bashrc` or a planted symlink are never truncated.
pub fn open_output(path: &Path, overwrite: bool) -> io::Result<File> {
    let bad = |m: &str| io::Error::new(io::ErrorKind::InvalidInput, m.to_string());
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).ok_or_else(|| bad("the path has no folder"))?;
    let name = path.file_name().ok_or_else(|| bad("the path has no file name"))?;
    let cname = CString::new(name.as_bytes()).map_err(|_| bad("NUL in the file name"))?;
    let cparent = CString::new(parent.as_os_str().as_bytes()).map_err(|_| bad("NUL in the folder name"))?;
    // SAFETY: cparent is a NUL-terminated path; the returned fd (if any) is owned below.
    let raw = unsafe { libc::open(cparent.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC) };
    if raw < 0 {
        let e = io::Error::last_os_error();
        let why = match e.raw_os_error() {
            Some(libc::ELOOP) => "the folder is a symbolic link (use the real folder)".to_string(),
            Some(libc::ENOTDIR) => "the folder is not a folder".to_string(),
            _ => format!("the folder cannot be opened: {e}"),
        };
        return Err(io::Error::new(e.kind(), why));
    }
    // SAFETY: raw is a fresh, valid fd that nothing else owns.
    let dir = File::from(unsafe { OwnedFd::from_raw_fd(raw) });
    // SAFETY: geteuid has no preconditions.
    let me = unsafe { libc::geteuid() };
    if dir.metadata()?.uid() != me {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("the folder {} is not yours: record into a folder you own (for example ~/Videos)", parent.display()),
        ));
    }
    let openat = |flags: libc::c_int| -> io::Result<File> {
        // SAFETY: dir is an open directory fd, cname a NUL-terminated name; the fd returned is owned right away.
        let fd = unsafe { libc::openat(dir.as_raw_fd(), cname.as_ptr(), flags | libc::O_NOFOLLOW | libc::O_CLOEXEC, 0o600 as libc::c_uint) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fd is a fresh, valid fd that nothing else owns.
        Ok(File::from(unsafe { OwnedFd::from_raw_fd(fd) }))
    };
    match openat(libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL) {
        Ok(f) => return Ok(f),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists && overwrite => {}
        Err(e) => return Err(e),
    }
    let mut f = openat(libc::O_RDWR)?;
    let meta = f.metadata()?;
    if !meta.is_file() || meta.uid() != me || meta.nlink() != 1 {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "not a regular file of yours with a single name"));
    }
    let mut head = [0u8; 8];
    let n = f.read(&mut head)?;
    if n != 0 && !(n == 8 && &head[4..8] == b"ftyp") {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "the existing file is not an MP4 recording"));
    }
    f.set_len(0)?;
    // the read above moved the offset: the new recording starts at byte 0, with no hole
    f.seek(SeekFrom::Start(0))?;
    Ok(f)
}

pub struct Writer {
    file: File,
    end: u64,
    tracks: Vec<TrackState>,
    seq: u32,
    fragment_ns: u64,
    sync: bool,
    moof: Vec<u8>,
    mvhd_duration_at: u64,
    mehd_duration_at: u64,
    /// Video fragments' (decode time, moof offset), for the `mfra` seek index.
    keyframes: Vec<(u64, u64)>,
}

impl Writer {
    /// Creates the file ([`open_output`]) and writes `ftyp` + `moov` (synced: an empty recording is already a valid
    /// file).
    pub fn create(path: &Path, config: &Config, overwrite: bool) -> io::Result<Writer> {
        Self::from_file(open_output(path, overwrite)?, config)
    }

    /// Writes `ftyp` + `moov` to a file already opened for writing at its start (`create` for callers that opened
    /// the file themselves, and for tests that need a failing disk).
    pub fn from_file(file: File, config: &Config) -> io::Result<Writer> {
        let mut tracks = Vec::new();
        let mut id = 1;
        if config.video.is_some() {
            tracks.push(TrackState::new(id, VIDEO_TIMESCALE, true));
            id += 1;
        }
        for _ in &config.audio {
            tracks.push(TrackState::new(id, AUDIO_TIMESCALE, false));
            id += 1;
        }
        let mut head = Vec::with_capacity(4096);
        ftyp(&mut head, config);
        let (mvhd_at, mehd_at) = moov(&mut head, config);
        let mut w = Writer {
            file,
            end: 0,
            tracks,
            seq: 0,
            fragment_ns: config.fragment_ns.max(100_000_000),
            sync: config.sync,
            moof: Vec::with_capacity(64 << 10),
            mvhd_duration_at: mvhd_at as u64,
            mehd_duration_at: mehd_at as u64,
            keyframes: Vec::new(),
        };
        w.file.write_all(&head)?;
        w.end = head.len() as u64;
        if w.sync {
            w.file.sync_data()?;
        }
        Ok(w)
    }

    fn video_index(&self) -> Option<usize> {
        self.tracks.iter().position(|t| t.video)
    }

    /// Adds a video sample (MP4 form, [`crate::avc::annexb_to_sample`]) presented at `pts_ns` (media time). Samples
    /// must come in presentation order without reordering (the encoders here make no B-frames).
    pub fn write_video(&mut self, data: &[u8], pts_ns: u64, keyframe: bool) -> io::Result<()> {
        let Some(vi) = self.video_index() else { return Ok(()) };
        let ticks = ns_to_ticks(pts_ns, VIDEO_TIMESCALE);
        let t = &mut self.tracks[vi];
        if let Some((prev, prev_key)) = t.held.take() {
            // a timestamp that does not advance still gets one tick (players reject zero durations)
            let duration = ticks.saturating_sub(prev).max(1) as u32;
            let at = if t.base_ticks.is_none() { prev } else { t.end_ticks };
            let held = std::mem::take(&mut t.held_data);
            t.push(at, duration, if prev_key { FLAGS_SYNC } else { FLAGS_NON_SYNC }, &held);
            t.held_data = held;
            t.last_duration = duration;
        } else if t.count > 0 && ticks < t.end_ticks {
            // never go back in time
            return Ok(());
        }
        let fragment_ticks = ns_to_ticks(self.fragment_ns, VIDEO_TIMESCALE);
        let pending = self.tracks[vi].pending_ticks();
        let pending_bytes: usize = self.tracks.iter().map(|t| t.data.len()).sum();
        if (keyframe && pending >= fragment_ticks) || pending_bytes > MAX_PENDING_BYTES {
            self.flush()?;
        }
        let t = &mut self.tracks[vi];
        t.held = Some((ticks.max(t.end_ticks), keyframe));
        t.held_data.clear();
        t.held_data.extend_from_slice(data);
        Ok(())
    }

    /// Adds an audio packet of audio track `index` (0-based among the audio tracks) starting at `pts_ns` and lasting
    /// `duration` samples at 48 kHz. Packets of a track are contiguous: the first packet's time sets the track's start,
    /// later ones follow it.
    pub fn write_audio(&mut self, index: usize, data: &[u8], pts_ns: u64, duration: u32) -> io::Result<()> {
        let first_audio = self.tracks.iter().position(|t| !t.video).unwrap_or(self.tracks.len());
        let Some(t) = self.tracks.get_mut(first_audio + index) else { return Ok(()) };
        let at = if t.count == 0 { ns_to_ticks(pts_ns, AUDIO_TIMESCALE) } else { t.end_ticks };
        t.push(at, duration, FLAGS_SYNC, data);
        if self.video_index().is_none() {
            // no video to cut fragments on keyframes: cut on time
            if self.tracks[first_audio + index].pending_ticks() >= ns_to_ticks(self.fragment_ns, AUDIO_TIMESCALE) {
                self.flush()?;
            }
        } else {
            let pending_bytes: usize = self.tracks.iter().map(|t| t.data.len()).sum();
            if pending_bytes > MAX_PENDING_BYTES {
                self.flush()?;
            }
        }
        Ok(())
    }

    /// Writes the pending samples as one fragment: the index goes out as `free`, then the data, then (after a sync)
    /// the index is published as `moof`.
    fn flush(&mut self) -> io::Result<()> {
        if self.tracks.iter().all(|t| t.samples.is_empty()) {
            return Ok(());
        }
        self.seq += 1;
        let moof_at = self.end;
        let m = &mut self.moof;
        m.clear();
        let moof = open(m, b"moof");
        let mfhd = open_full(m, b"mfhd", 0, 0);
        boxes::u32(m, self.seq);
        close(m, mfhd);
        let mut data_offsets = Vec::with_capacity(self.tracks.len());
        for t in self.tracks.iter().filter(|t| !t.samples.is_empty()) {
            let traf = open(m, b"traf");
            // default-base-is-moof: data offsets count from the moof's first byte
            let tfhd = open_full(m, b"tfhd", 0, 0x02_0000);
            boxes::u32(m, t.id);
            close(m, tfhd);
            let tfdt = open_full(m, b"tfdt", 1, 0);
            boxes::u64(m, t.base_ticks.unwrap_or(0));
            close(m, tfdt);
            // data offset, and per sample: duration, size, flags
            let trun = open_full(m, b"trun", 0, 0x0001 | 0x0100 | 0x0200 | 0x0400);
            boxes::u32(m, t.samples.len() as u32);
            data_offsets.push(m.len());
            boxes::u32(m, 0);
            for s in &t.samples {
                boxes::u32(m, s.duration);
                boxes::u32(m, s.size);
                boxes::u32(m, s.flags);
            }
            close(m, trun);
            close(m, traf);
        }
        close(m, moof);
        let moof_len = m.len();
        let data_len: usize = self.tracks.iter().map(|t| t.data.len()).sum();
        let mut offset = moof_len + 8;
        for (i, t) in self.tracks.iter().filter(|t| !t.samples.is_empty()).enumerate() {
            m[data_offsets[i]..data_offsets[i] + 4].copy_from_slice(&(offset as u32).to_be_bytes());
            offset += t.data.len();
        }
        // until its data is on disk, the index is a box players skip
        m[4..8].copy_from_slice(b"free");
        m.extend_from_slice(&((data_len + 8) as u32).to_be_bytes());
        m.extend_from_slice(b"mdat");
        self.file.write_all(m)?;
        for t in &self.tracks {
            self.file.write_all(&t.data)?;
        }
        if self.sync {
            self.file.sync_data()?;
        }
        self.file.write_all_at(b"moof", moof_at + 4)?;
        self.end += (moof_len + 8 + data_len) as u64;
        if let Some(vi) = self.video_index() {
            let v = &self.tracks[vi];
            if v.samples.first().is_some_and(|s| s.flags == FLAGS_SYNC) {
                self.keyframes.push((v.base_ticks.unwrap_or(0), moof_at));
            }
        }
        for t in &mut self.tracks {
            if !t.samples.is_empty() {
                t.base_ticks = Some(t.end_ticks);
            }
            t.samples.clear();
            t.data.clear();
        }
        Ok(())
    }

    /// Writes what is pending, the seek index (`mfra`) and the final durations, and syncs.
    pub fn finish(mut self) -> io::Result<Summary> {
        if let Some(vi) = self.video_index() {
            let t = &mut self.tracks[vi];
            if let Some((prev, key)) = t.held.take() {
                let duration = if t.last_duration > 0 { t.last_duration } else { VIDEO_TIMESCALE / 60 };
                let at = if t.base_ticks.is_none() { prev } else { t.end_ticks };
                let held = std::mem::take(&mut t.held_data);
                t.push(at, duration, if key { FLAGS_SYNC } else { FLAGS_NON_SYNC }, &held);
            }
        }
        self.flush()?;
        if let Some(vi) = self.video_index() {
            let id = self.tracks[vi].id;
            let mut m = Vec::with_capacity(64 + self.keyframes.len() * 19);
            let mfra = open(&mut m, b"mfra");
            let tfra = open_full(&mut m, b"tfra", 1, 0);
            boxes::u32(&mut m, id);
            // traf, trun and sample numbers in one byte each
            boxes::u32(&mut m, 0);
            boxes::u32(&mut m, self.keyframes.len() as u32);
            for (time, at) in &self.keyframes {
                boxes::u64(&mut m, *time);
                boxes::u64(&mut m, *at);
                m.extend_from_slice(&[1, 1, 1]);
            }
            close(&mut m, tfra);
            let mfro = open_full(&mut m, b"mfro", 0, 0);
            let mfro_size = (m.len() - mfra + 4) as u32;
            boxes::u32(&mut m, mfro_size);
            close(&mut m, mfro);
            close(&mut m, mfra);
            self.file.write_all(&m)?;
            self.end += m.len() as u64;
        }
        let duration_ns = self.tracks.iter().map(|t| cast_core::clock::ticks_to_ns(t.end_ticks, t.timescale)).max().unwrap_or(0);
        let movie = ns_to_ticks(duration_ns, MOVIE_TIMESCALE);
        self.file.write_all_at(&movie.to_be_bytes(), self.mvhd_duration_at)?;
        self.file.write_all_at(&movie.to_be_bytes(), self.mehd_duration_at)?;
        self.file.sync_all()?;
        Ok(Summary {
            duration_ns,
            video_frames: self.tracks.iter().filter(|t| t.video).map(|t| t.count).sum(),
            audio_packets: self.tracks.iter().filter(|t| !t.video).map(|t| t.count).sum(),
            fragments: self.seq,
            bytes: self.end,
        })
    }

    /// Bytes written so far.
    pub fn bytes(&self) -> u64 {
        self.end
    }
}

impl TrackState {
    fn new(id: u32, timescale: u32, video: bool) -> Self {
        TrackState {
            id,
            timescale,
            video,
            base_ticks: None,
            end_ticks: 0,
            samples: Vec::with_capacity(256),
            data: Vec::with_capacity(if video { 4 << 20 } else { 64 << 10 }),
            held: None,
            held_data: Vec::with_capacity(if video { 512 << 10 } else { 0 }),
            last_duration: 0,
            count: 0,
        }
    }
}

fn ftyp(b: &mut Vec<u8>, config: &Config) {
    let at = open(b, b"ftyp");
    b.extend_from_slice(b"isom");
    boxes::u32(b, 0x200);
    b.extend_from_slice(b"isomiso6mp41");
    match config.video.as_ref().map(|v| v.codec) {
        Some(VideoCodec::H264) => b.extend_from_slice(b"avc1"),
        Some(VideoCodec::Hevc) => b.extend_from_slice(b"hvc1"),
        None => {}
    }
    if !config.audio.is_empty() {
        b.extend_from_slice(b"Opus");
    }
    close(b, at);
}

/// Seconds between 1904-01-01 (MP4's epoch) and 1970-01-01.
const MP4_EPOCH_OFFSET: u64 = 2_082_844_800;

fn now_mp4() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()) + MP4_EPOCH_OFFSET
}

/// Writes `moov`; returns where mvhd's and mehd's durations are (patched at the end).
fn moov(b: &mut Vec<u8>, config: &Config) -> (usize, usize) {
    let now = now_mp4();
    let moov = open(b, b"moov");
    let mvhd = open_full(b, b"mvhd", 1, 0);
    boxes::u64(b, now);
    boxes::u64(b, now);
    boxes::u32(b, MOVIE_TIMESCALE);
    let mvhd_duration = b.len();
    boxes::u64(b, 0);
    boxes::u32(b, 0x0001_0000);
    boxes::u16(b, 0x0100);
    boxes::zeros(b, 10);
    boxes::matrix(b);
    boxes::zeros(b, 24);
    let tracks = config.video.is_some() as u32 + config.audio.len() as u32;
    boxes::u32(b, tracks + 1);
    close(b, mvhd);
    let mut id = 1;
    if let Some(v) = &config.video {
        trak(b, id, now, Kind::Video(v));
        id += 1;
    }
    for a in &config.audio {
        trak(b, id, now, Kind::Audio(a));
        id += 1;
    }
    let mvex = open(b, b"mvex");
    let mehd = open_full(b, b"mehd", 1, 0);
    let mehd_duration = b.len();
    boxes::u64(b, 0);
    close(b, mehd);
    for track in 1..id {
        let trex = open_full(b, b"trex", 0, 0);
        boxes::u32(b, track);
        boxes::u32(b, 1);
        boxes::zeros(b, 12);
        close(b, trex);
    }
    close(b, mvex);
    close(b, moov);
    (mvhd_duration, mehd_duration)
}

enum Kind<'a> {
    Video(&'a VideoTrack),
    Audio(&'a AudioTrack),
}

fn trak(b: &mut Vec<u8>, id: u32, now: u64, kind: Kind) {
    let trak = open(b, b"trak");
    let tkhd = open_full(b, b"tkhd", 1, 0x3);
    boxes::u64(b, now);
    boxes::u64(b, now);
    boxes::u32(b, id);
    boxes::u32(b, 0);
    boxes::u64(b, 0);
    boxes::zeros(b, 8);
    boxes::u16(b, 0);
    boxes::u16(b, 0);
    boxes::u16(b, if matches!(kind, Kind::Audio(_)) { 0x0100 } else { 0 });
    boxes::u16(b, 0);
    boxes::matrix(b);
    let (w, h) = match &kind {
        Kind::Video(v) => (v.width, v.height),
        Kind::Audio(_) => (0, 0),
    };
    boxes::u32(b, w << 16);
    boxes::u32(b, h << 16);
    close(b, tkhd);
    let mdia = open(b, b"mdia");
    let mdhd = open_full(b, b"mdhd", 1, 0);
    boxes::u64(b, now);
    boxes::u64(b, now);
    boxes::u32(b, if matches!(kind, Kind::Video(_)) { VIDEO_TIMESCALE } else { AUDIO_TIMESCALE });
    boxes::u64(b, 0);
    // language "und", packed ISO-639-2/T
    boxes::u16(b, 0x55c4);
    boxes::u16(b, 0);
    close(b, mdhd);
    let hdlr = open_full(b, b"hdlr", 0, 0);
    boxes::u32(b, 0);
    let name = match &kind {
        Kind::Video(_) => {
            b.extend_from_slice(b"vide");
            "Kalango Cast"
        }
        Kind::Audio(a) => {
            b.extend_from_slice(b"soun");
            a.name.as_str()
        }
    };
    boxes::zeros(b, 12);
    b.extend_from_slice(name.as_bytes());
    b.push(0);
    close(b, hdlr);
    let minf = open(b, b"minf");
    match kind {
        Kind::Video(_) => {
            let vmhd = open_full(b, b"vmhd", 0, 1);
            boxes::zeros(b, 8);
            close(b, vmhd);
        }
        Kind::Audio(_) => {
            let smhd = open_full(b, b"smhd", 0, 0);
            boxes::zeros(b, 4);
            close(b, smhd);
        }
    }
    let dinf = open(b, b"dinf");
    let dref = open_full(b, b"dref", 0, 0);
    boxes::u32(b, 1);
    let url = open_full(b, b"url ", 0, 1);
    close(b, url);
    close(b, dref);
    close(b, dinf);
    let stbl = open(b, b"stbl");
    let stsd = open_full(b, b"stsd", 0, 0);
    boxes::u32(b, 1);
    match kind {
        Kind::Video(v) => {
            let entry = open(b, if v.codec == VideoCodec::H264 { b"avc1" } else { b"hvc1" });
            boxes::zeros(b, 6);
            boxes::u16(b, 1);
            boxes::zeros(b, 16);
            boxes::u16(b, v.width as u16);
            boxes::u16(b, v.height as u16);
            boxes::u32(b, 0x0048_0000);
            boxes::u32(b, 0x0048_0000);
            boxes::u32(b, 0);
            boxes::u16(b, 1);
            let mut compressor = [0u8; 32];
            let label = b"Kalango Cast";
            compressor[0] = label.len() as u8;
            compressor[1..=label.len()].copy_from_slice(label);
            b.extend_from_slice(&compressor);
            boxes::u16(b, 0x0018);
            boxes::u16(b, 0xffff);
            let cfg = open(b, if v.codec == VideoCodec::H264 { b"avcC" } else { b"hvcC" });
            b.extend_from_slice(&v.config);
            close(b, cfg);
            close(b, entry);
        }
        Kind::Audio(a) => {
            let entry = open(b, b"Opus");
            boxes::zeros(b, 6);
            boxes::u16(b, 1);
            boxes::zeros(b, 8);
            boxes::u16(b, a.channels as u16);
            boxes::u16(b, 16);
            boxes::zeros(b, 4);
            boxes::u32(b, AUDIO_TIMESCALE << 16);
            // OpusSpecificBox ("Encapsulation of Opus in ISO Base Media File Format" 4.3.2)
            let dops = open(b, b"dOps");
            boxes::u8(b, 0);
            boxes::u8(b, a.channels);
            boxes::u16(b, a.pre_skip);
            boxes::u32(b, a.input_sample_rate);
            boxes::u16(b, 0);
            boxes::u8(b, 0);
            close(b, dops);
            close(b, entry);
        }
    }
    close(b, stsd);
    for kind in [b"stts", b"stsc", b"stco"] {
        let at = open_full(b, kind, 0, 0);
        boxes::u32(b, 0);
        close(b, at);
    }
    let stsz = open_full(b, b"stsz", 0, 0);
    boxes::u32(b, 0);
    boxes::u32(b, 0);
    close(b, stsz);
    close(b, stbl);
    close(b, minf);
    close(b, mdia);
    close(b, trak);
}
