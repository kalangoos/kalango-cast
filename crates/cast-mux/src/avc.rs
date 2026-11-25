//! H.264 and HEVC bitstreams: encoders write Annex B (NAL units after `00 00 01` start codes); MP4 wants each NAL
//! unit behind its 4-byte length, with the parameter sets (SPS, PPS, VPS) in the sample description (`avcC`, `hvcC`)
//! instead of the samples.

use cast_core::VideoCodec;

/// The NAL units of an Annex B buffer (without their start codes).
pub fn nal_units(data: &[u8]) -> NalUnits<'_> {
    NalUnits { data, pos: find_start(data, 0).map(|(_, end)| end).unwrap_or(data.len()) }
}

pub struct NalUnits<'a> {
    data: &'a [u8],
    pos: usize,
}

/// The next start code at or after `from`: (where it begins, where the NAL unit after it begins).
fn find_start(data: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut i = from;
    while i + 3 <= data.len() {
        if data[i + 2] > 1 {
            i += 3;
        } else if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            let begin = if i > from && data[i - 1] == 0 { i - 1 } else { i };
            return Some((begin, i + 3));
        } else {
            i += 1;
        }
    }
    None
}

impl<'a> Iterator for NalUnits<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        if self.pos >= self.data.len() {
            return None;
        }
        let start = self.pos;
        let (end, next) = find_start(self.data, start).unwrap_or((self.data.len(), self.data.len()));
        self.pos = next;
        // trailing zero bytes belong to the next start code (or are padding)
        let mut nal = &self.data[start..end];
        while let [rest @ .., 0] = nal {
            nal = rest;
        }
        Some(nal)
    }
}

fn h264_type(nal: &[u8]) -> u8 {
    nal.first().map_or(0, |b| b & 0x1f)
}

fn hevc_type(nal: &[u8]) -> u8 {
    nal.first().map_or(0, |b| (b >> 1) & 0x3f)
}

/// Rewrites one access unit from Annex B into MP4 sample form in `out` (cleared first; its capacity is reused, so
/// after the first frames this does not allocate). Parameter sets and access unit delimiters are left out (they
/// live in the sample description). Returns whether the access unit is a random access point (IDR / IRAP).
pub fn annexb_to_sample(codec: VideoCodec, annexb: &[u8], out: &mut Vec<u8>) -> bool {
    out.clear();
    let mut key = false;
    for nal in nal_units(annexb) {
        if nal.is_empty() {
            continue;
        }
        let skip = match codec {
            VideoCodec::H264 => {
                let t = h264_type(nal);
                key |= t == 5;
                matches!(t, 7..=9)
            }
            VideoCodec::Hevc => {
                let t = hevc_type(nal);
                key |= (16..=23).contains(&t);
                matches!(t, 32..=35)
            }
        };
        if !skip {
            out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
            out.extend_from_slice(nal);
        }
    }
    key
}

/// The `avcC` box payload (AVCDecoderConfigurationRecord) from the first SPS and PPS of an Annex B buffer.
pub fn avcc(annexb: &[u8]) -> Option<Vec<u8>> {
    let sps = nal_units(annexb).find(|n| h264_type(n) == 7)?;
    let pps = nal_units(annexb).find(|n| h264_type(n) == 8)?;
    if sps.len() < 4 {
        return None;
    }
    let mut c = vec![1, sps[1], sps[2], sps[3], 0xff, 0xe1];
    c.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    c.extend_from_slice(sps);
    c.push(1);
    c.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    c.extend_from_slice(pps);
    // the High profiles add chroma format and bit depths (4:2:0, 8 bit: what the encoders here produce)
    if matches!(sps[1], 100 | 110 | 122 | 144) {
        c.extend_from_slice(&[0xfc | 1, 0xf8, 0xf8, 0]);
    }
    Some(c)
}

/// Removes emulation prevention bytes (`00 00 03` → `00 00`).
fn rbsp(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn bit(&mut self) -> Option<u32> {
        let byte = *self.data.get(self.pos / 8)?;
        let b = (byte >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Some(b as u32)
    }

    fn bits(&mut self, n: u32) -> Option<u32> {
        let mut v = 0;
        for _ in 0..n {
            v = v << 1 | self.bit()?;
        }
        Some(v)
    }

    /// Exp-Golomb unsigned.
    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some((1u32 << zeros) - 1 + self.bits(zeros)?)
    }
}

/// The `hvcC` box payload (HEVCDecoderConfigurationRecord) from the VPS, SPS and PPS of an Annex B buffer.
pub fn hvcc(annexb: &[u8]) -> Option<Vec<u8>> {
    let vps = nal_units(annexb).find(|n| hevc_type(n) == 32)?;
    let sps = nal_units(annexb).find(|n| hevc_type(n) == 33)?;
    let pps = nal_units(annexb).find(|n| hevc_type(n) == 34)?;
    let r = rbsp(sps.get(2..)?);
    if r.len() < 13 {
        return None;
    }
    let max_sub_layers_minus1 = (r[0] >> 1) & 7;
    let temporal_id_nested = r[0] & 1;
    let ptl = &r[1..13];
    // chroma format and bit depths follow the profile/tier/level; read them when there are no sub-layers (the
    // encoders here never make them), else assume 4:2:0 8 bit
    let (mut chroma, mut depth_luma, mut depth_chroma) = (1, 0, 0);
    if max_sub_layers_minus1 == 0 {
        let mut b = Bits { data: &r[13..], pos: 0 };
        let parsed = (|| {
            b.ue()?; // sps_seq_parameter_set_id
            let cf = b.ue()?;
            if cf == 3 {
                b.bit()?;
            }
            b.ue()?; // width
            b.ue()?; // height
            if b.bit()? == 1 {
                for _ in 0..4 {
                    b.ue()?;
                }
            }
            Some((cf, b.ue()?, b.ue()?))
        })();
        if let Some((cf, dl, dc)) = parsed {
            (chroma, depth_luma, depth_chroma) = (cf, dl, dc);
        }
    }
    let mut c = vec![1, ptl[0]];
    c.extend_from_slice(&ptl[1..5]);
    c.extend_from_slice(&ptl[5..11]);
    c.push(ptl[11]);
    c.extend_from_slice(&[0xf0, 0x00, 0xfc, 0xfc | chroma as u8, 0xf8 | depth_luma as u8, 0xf8 | depth_chroma as u8, 0, 0]);
    c.push(((max_sub_layers_minus1 + 1) << 3) | (temporal_id_nested << 2) | 3);
    c.push(3);
    for (t, nal) in [(32u8, vps), (33, sps), (34, pps)] {
        c.push(0x80 | t);
        c.extend_from_slice(&1u16.to_be_bytes());
        c.extend_from_slice(&(nal.len() as u16).to_be_bytes());
        c.extend_from_slice(nal);
    }
    Some(c)
}

/// The sample description payload for a codec from its parameter sets.
pub fn codec_config(codec: VideoCodec, annexb: &[u8]) -> Option<Vec<u8>> {
    match codec {
        VideoCodec::H264 => avcc(annexb),
        VideoCodec::Hevc => hvcc(annexb),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // a real H.264 High SPS/PPS pair (1920×1080), as an encoder emits them, then an IDR slice header
    const H264_AU: &[u8] = &[
        0, 0, 0, 1, 0x09, 0x10, // AUD
        0, 0, 0, 1, 0x67, 0x64, 0x00, 0x28, 0xac, 0xd9, 0x40, 0x78, 0x02, 0x27, 0xe5, 0x84, // SPS
        0, 0, 1, 0x68, 0xeb, 0xe3, 0xcb, 0x22, 0xc0, // PPS (3-byte start code)
        0, 0, 0, 1, 0x65, 0x88, 0x84, 0x00, 0x33, 0xff, // IDR slice
    ];

    #[test]
    fn nal_units_split_on_both_start_code_lengths() {
        let units: Vec<_> = nal_units(H264_AU).collect();
        assert_eq!(units.len(), 4);
        assert_eq!(units[0], &[0x09, 0x10]);
        assert_eq!(units[2], &[0x68, 0xeb, 0xe3, 0xcb, 0x22, 0xc0]);
        assert_eq!(units[3], &[0x65, 0x88, 0x84, 0x00, 0x33, 0xff]);
    }

    #[test]
    fn samples_keep_only_slices_with_lengths() {
        let mut out = Vec::new();
        assert!(annexb_to_sample(VideoCodec::H264, H264_AU, &mut out), "an IDR is a keyframe");
        assert_eq!(out, [0, 0, 0, 6, 0x65, 0x88, 0x84, 0x00, 0x33, 0xff]);
        let p_frame = [0, 0, 0, 1, 0x41, 0x9a, 0x00, 0x10];
        assert!(!annexb_to_sample(VideoCodec::H264, &p_frame, &mut out));
        assert_eq!(out, [0, 0, 0, 4, 0x41, 0x9a, 0x00, 0x10]);
    }

    #[test]
    fn avcc_has_profile_level_and_both_parameter_sets() {
        let c = avcc(H264_AU).unwrap();
        assert_eq!(&c[..6], &[1, 0x64, 0x00, 0x28, 0xff, 0xe1]);
        let sps_len = u16::from_be_bytes([c[6], c[7]]) as usize;
        assert_eq!(sps_len, 12);
        assert_eq!(c[8 + sps_len], 1, "one PPS");
        assert_eq!(&c[c.len() - 4..], &[0xfd, 0xf8, 0xf8, 0], "High profile extension");
    }

    #[test]
    fn emulation_prevention_is_removed() {
        assert_eq!(rbsp(&[0, 0, 3, 1, 0, 0, 3, 0]), [0, 0, 1, 0, 0, 0]);
    }

    #[test]
    fn hvcc_reads_the_profile_from_the_sps() {
        // VPS, SPS (Main, level 4.1, 1920×1080 4:2:0 8 bit), PPS from an HEVC encoder
        let vps: &[u8] = &[0x40, 0x01, 0x0c, 0x01, 0xff, 0xff, 0x01, 0x60, 0, 0, 3, 0, 0x90, 0, 0, 3, 0, 0, 3, 0, 0x7b, 0x95, 0x98, 0x09];
        let sps: &[u8] = &[
            0x42, 0x01, 0x01, 0x01, 0x60, 0, 0, 3, 0, 0x90, 0, 0, 3, 0, 0, 3, 0, 0x7b, 0xa0, 0x03, 0xc0, 0x80, 0x10, 0xe5, 0x96, 0x56, 0x69, 0x24,
            0xca, 0xf0, 0x10, 0x10, 0, 0, 3, 0, 0x10, 0, 0, 3, 0x01, 0xe0, 0x80,
        ];
        let pps: &[u8] = &[0x44, 0x01, 0xc1, 0x72, 0xb4, 0x62, 0x40];
        let mut au = Vec::new();
        for n in [vps, sps, pps] {
            au.extend_from_slice(&[0, 0, 0, 1]);
            au.extend_from_slice(n);
        }
        let c = hvcc(&au).unwrap();
        assert_eq!(c[0], 1);
        assert_eq!(c[1], 0x01, "profile space 0, tier 0, profile 1 (Main)");
        assert_eq!(c[12], 0x7b, "level 4.1");
        assert_eq!(c[16] & 3, 1, "4:2:0");
        assert_eq!(c[17] & 7, 0, "8 bit");
        assert_eq!(c[21] & 3, 3, "4-byte lengths");
        assert_eq!(c[22], 3, "three arrays");
        assert_eq!(c[23], 0x80 | 32);
    }
}
