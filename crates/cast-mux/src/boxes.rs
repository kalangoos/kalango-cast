//! Writing ISO BMFF boxes into a byte buffer: a box is opened (its size left as zero), filled, then closed (the size
//! written back).

pub fn open(buf: &mut Vec<u8>, kind: &[u8; 4]) -> usize {
    let at = buf.len();
    buf.extend_from_slice(&[0, 0, 0, 0]);
    buf.extend_from_slice(kind);
    at
}

/// A "full box": version and 24-bit flags after the header.
pub fn open_full(buf: &mut Vec<u8>, kind: &[u8; 4], version: u8, flags: u32) -> usize {
    let at = open(buf, kind);
    buf.push(version);
    buf.extend_from_slice(&flags.to_be_bytes()[1..]);
    at
}

pub fn close(buf: &mut [u8], at: usize) {
    let size = (buf.len() - at) as u32;
    buf[at..at + 4].copy_from_slice(&size.to_be_bytes());
}

pub fn u8(buf: &mut Vec<u8>, v: u8) {
    buf.push(v);
}

pub fn u16(buf: &mut Vec<u8>, v: u16) {
    buf.extend_from_slice(&v.to_be_bytes());
}

pub fn u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_be_bytes());
}

pub fn u64(buf: &mut Vec<u8>, v: u64) {
    buf.extend_from_slice(&v.to_be_bytes());
}

pub fn zeros(buf: &mut Vec<u8>, n: usize) {
    buf.resize(buf.len() + n, 0);
}

/// The unity transformation matrix of `mvhd` and `tkhd`.
pub fn matrix(buf: &mut Vec<u8>) {
    for v in [0x0001_0000u32, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000] {
        u32(buf, v);
    }
}
