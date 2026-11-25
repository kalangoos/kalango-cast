//! Reads a fragmented MP4 back: its tracks and the samples of every complete fragment. A torn tail (a crash mid
//! fragment) is reported, not an error: what comes before it is the recording.

use std::io;
use std::path::Path;

#[derive(Clone, Debug, Default)]
pub struct TrackInfo {
    pub id: u32,
    /// `vide` or `soun`.
    pub handler: String,
    /// The sample entry: `avc1`, `hvc1`, `Opus`.
    pub codec: String,
    pub timescale: u32,
    pub name: String,
    pub samples: u64,
    pub sync_samples: u64,
    pub bytes: u64,
    /// Decode time of the first sample and after the last one, in the track's ticks.
    pub first_ticks: Option<u64>,
    pub end_ticks: u64,
}

impl TrackInfo {
    pub fn start_s(&self) -> f64 {
        self.first_ticks.unwrap_or(0) as f64 / self.timescale.max(1) as f64
    }

    pub fn end_s(&self) -> f64 {
        self.end_ticks as f64 / self.timescale.max(1) as f64
    }

    pub fn duration_s(&self) -> f64 {
        self.end_s() - self.start_s()
    }
}

#[derive(Clone, Debug, Default)]
pub struct Inspection {
    pub brand: String,
    pub tracks: Vec<TrackInfo>,
    pub fragments: u32,
    /// Bytes after the last complete fragment (a torn fragment, an unpublished `free` index, or `mfra`).
    pub tail_bytes: u64,
    pub has_mfra: bool,
    /// Movie duration from `mvhd` in ms (0 until the recording is finished).
    pub movie_duration_ms: u64,
}

impl Inspection {
    pub fn video(&self) -> Option<&TrackInfo> {
        self.tracks.iter().find(|t| t.handler == "vide")
    }

    pub fn audio(&self) -> impl Iterator<Item = &TrackInfo> {
        self.tracks.iter().filter(|t| t.handler == "soun")
    }
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn be64(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_be_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

/// The child boxes of a container body: (type, body). Stops at a malformed child.
fn children(body: &[u8]) -> Vec<([u8; 4], &[u8])> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 8 <= body.len() {
        let size = be32(body, i).unwrap_or(0) as usize;
        let kind: [u8; 4] = body[i + 4..i + 8].try_into().unwrap_or([0; 4]);
        if size < 8 || i + size > body.len() {
            break;
        }
        out.push((kind, &body[i + 8..i + size]));
        i += size;
    }
    out
}

fn child<'a>(body: &'a [u8], kind: &[u8; 4]) -> Option<&'a [u8]> {
    children(body).into_iter().find(|(k, _)| k == kind).map(|(_, b)| b)
}

pub fn inspect_file(path: &Path) -> io::Result<Inspection> {
    let data = std::fs::read(path)?;
    Ok(inspect(&data))
}

/// Parses a whole file held in memory.
pub fn inspect(data: &[u8]) -> Inspection {
    let mut out = Inspection::default();
    let mut i = 0usize;
    let mut last_good = 0usize;
    let mut prev = [0u8; 4];
    while i + 8 <= data.len() {
        let mut size = be32(data, i).unwrap_or(0) as u64;
        let kind: [u8; 4] = data[i + 4..i + 8].try_into().unwrap_or([0; 4]);
        let mut header = 8;
        if size == 1 {
            size = be64(data, i + 8).unwrap_or(0);
            header = 16;
        }
        if size < header as u64 || i as u64 + size > data.len() as u64 {
            break;
        }
        let body = &data[i + header..i + size as usize];
        match &kind {
            b"ftyp" => out.brand = String::from_utf8_lossy(body.get(..4).unwrap_or(b"")).into_owned(),
            b"moov" => read_moov(body, &mut out),
            b"moof" => {
                // the fragment counts only with its whole mdat after it
                let next = i + size as usize;
                let mdat_ok = be32(data, next).is_some_and(|s| next + s as usize <= data.len()) && data.get(next + 4..next + 8) == Some(b"mdat");
                if !mdat_ok {
                    break;
                }
                read_moof(body, &mut out);
                out.fragments += 1;
            }
            // media data counts once its index is published (after a `free`, it is a torn fragment)
            b"mdat" if &prev == b"moof" => last_good = i + size as usize,
            b"mfra" => out.has_mfra = true,
            _ => {}
        }
        if matches!(&kind, b"ftyp" | b"moov" | b"mfra") {
            last_good = i + size as usize;
        }
        prev = kind;
        i += size as usize;
    }
    out.tail_bytes = (data.len() - last_good.min(data.len())) as u64;
    out
}

fn read_moov(moov: &[u8], out: &mut Inspection) {
    if let Some(mvhd) = child(moov, b"mvhd") {
        out.movie_duration_ms = if mvhd[0] == 1 { be64(mvhd, 24).unwrap_or(0) } else { be32(mvhd, 16).unwrap_or(0) as u64 };
    }
    for (kind, trak) in children(moov) {
        if &kind != b"trak" {
            continue;
        }
        let mut t = TrackInfo::default();
        if let Some(tkhd) = child(trak, b"tkhd") {
            t.id = if tkhd[0] == 1 { be32(tkhd, 20) } else { be32(tkhd, 12) }.unwrap_or(0);
        }
        if let Some(mdia) = child(trak, b"mdia") {
            if let Some(mdhd) = child(mdia, b"mdhd") {
                t.timescale = if mdhd[0] == 1 { be32(mdhd, 20) } else { be32(mdhd, 12) }.unwrap_or(0);
            }
            if let Some(hdlr) = child(mdia, b"hdlr") {
                t.handler = String::from_utf8_lossy(hdlr.get(8..12).unwrap_or(b"")).into_owned();
                t.name = String::from_utf8_lossy(hdlr.get(24..).unwrap_or(b"")).trim_end_matches('\0').to_string();
            }
            let stsd = child(mdia, b"minf").and_then(|m| child(m, b"stbl")).and_then(|s| child(s, b"stsd"));
            if let Some(stsd) = stsd {
                t.codec = String::from_utf8_lossy(stsd.get(12..16).unwrap_or(b"")).into_owned();
            }
        }
        out.tracks.push(t);
    }
}

fn read_moof(moof: &[u8], out: &mut Inspection) {
    for (kind, traf) in children(moof) {
        if &kind != b"traf" {
            continue;
        }
        let Some(id) = child(traf, b"tfhd").and_then(|b| be32(b, 4)) else { continue };
        let base = child(traf, b"tfdt").and_then(|b| if b[0] == 1 { be64(b, 4) } else { be32(b, 4).map(u64::from) });
        let Some(t) = out.tracks.iter_mut().find(|t| t.id == id) else { continue };
        let mut time = base.unwrap_or(t.end_ticks);
        if t.first_ticks.is_none() {
            t.first_ticks = Some(time);
        }
        for (k, trun) in children(traf) {
            if &k != b"trun" {
                continue;
            }
            let flags = be32(trun, 0).unwrap_or(0) & 0xff_ffff;
            let count = be32(trun, 4).unwrap_or(0);
            let mut at = 8;
            if flags & 0x1 != 0 {
                at += 4;
            }
            if flags & 0x4 != 0 {
                at += 4;
            }
            for _ in 0..count {
                let mut duration = 0;
                let mut sflags = 0;
                if flags & 0x100 != 0 {
                    duration = be32(trun, at).unwrap_or(0);
                    at += 4;
                }
                if flags & 0x200 != 0 {
                    t.bytes += be32(trun, at).unwrap_or(0) as u64;
                    at += 4;
                }
                if flags & 0x400 != 0 {
                    sflags = be32(trun, at).unwrap_or(0);
                    at += 4;
                }
                if flags & 0x800 != 0 {
                    at += 4;
                }
                time += duration as u64;
                t.samples += 1;
                if sflags & 0x0001_0000 == 0 {
                    t.sync_samples += 1;
                }
            }
        }
        t.end_ticks = time;
    }
}
