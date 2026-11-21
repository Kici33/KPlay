//! Minimal H.264 helpers: avcC parsing and SPS dimension extraction.

/// SPS and PPS NAL units (without start codes) from an `avcC` record.
pub struct ParamSets {
    pub sps: Vec<u8>,
    pub pps: Vec<u8>,
}

pub fn parse_avcc(p: &[u8]) -> Option<ParamSets> {
    if p.len() < 8 || p[0] != 1 {
        return None;
    }
    let n_sps = (p[5] & 0x1f) as usize;
    let mut pos = 6;
    let mut sps = None;
    for _ in 0..n_sps {
        let len = u16::from_be_bytes([*p.get(pos)?, *p.get(pos + 1)?]) as usize;
        let nal = p.get(pos + 2..pos + 2 + len)?;
        sps.get_or_insert_with(|| nal.to_vec());
        pos += 2 + len;
    }
    let n_pps = *p.get(pos)? as usize;
    pos += 1;
    let mut pps = None;
    for _ in 0..n_pps {
        let len = u16::from_be_bytes([*p.get(pos)?, *p.get(pos + 1)?]) as usize;
        let nal = p.get(pos + 2..pos + 2 + len)?;
        pps.get_or_insert_with(|| nal.to_vec());
        pos += 2 + len;
    }
    Some(ParamSets { sps: sps?, pps: pps? })
}

/// Convert AVCC (4-byte length prefixed) NAL units to Annex B, appending an
/// access-unit delimiter so decoders can emit the frame immediately rather
/// than waiting for the start of the next one (saves a frame of latency).
pub fn avcc_to_annexb(data: &[u8], out: &mut Vec<u8>) -> bool {
    let mut pos = 0;
    while pos + 4 <= data.len() {
        let len = u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        pos += 4;
        if len == 0 || pos + len > data.len() {
            return false;
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&data[pos..pos + len]);
        pos += len;
    }
    out.extend_from_slice(&[0, 0, 0, 1, 0x09, 0xf0]); // AUD
    pos == data.len()
}

struct Bits<'a> {
    d: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn bit(&mut self) -> Option<u32> {
        let byte = *self.d.get(self.pos / 8)?;
        let b = (byte >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Some(b as u32)
    }
    fn bits(&mut self, n: u32) -> Option<u32> {
        (0..n).try_fold(0u32, |acc, _| Some((acc << 1) | self.bit()?))
    }
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
    fn se(&mut self) -> Option<i32> {
        let k = self.ue()? as i32;
        Some(if k & 1 == 1 { (k + 1) / 2 } else { -(k / 2) })
    }
}

/// Decoded picture size (after cropping) from an SPS NAL (with header byte).
pub fn sps_dimensions(sps: &[u8]) -> Option<(u32, u32)> {
    // strip emulation prevention bytes
    let mut rbsp = Vec::with_capacity(sps.len());
    let mut zeros = 0;
    for &b in sps.iter().skip(1) {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        rbsp.push(b);
    }
    let mut r = Bits { d: &rbsp, pos: 0 };
    let profile = r.bits(8)?;
    r.bits(16)?; // constraint flags + level
    r.ue()?; // sps id

    let mut chroma = 1;
    if matches!(profile, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135) {
        chroma = r.ue()?;
        if chroma == 3 {
            r.bit()?;
        }
        r.ue()?;
        r.ue()?;
        r.bit()?;
        if r.bit()? == 1 {
            for i in 0..(if chroma != 3 { 8 } else { 12 }) {
                if r.bit()? == 1 {
                    let size = if i < 6 { 16 } else { 64 };
                    let (mut last, mut next) = (8i32, 8i32);
                    for _ in 0..size {
                        if next != 0 {
                            next = (last + r.se()? + 256) % 256;
                        }
                        if next != 0 {
                            last = next;
                        }
                    }
                }
            }
        }
    }
    r.ue()?; // log2_max_frame_num
    match r.ue()? {
        0 => {
            r.ue()?;
        }
        1 => {
            r.bit()?;
            r.se()?;
            r.se()?;
            for _ in 0..r.ue()? {
                r.se()?;
            }
        }
        _ => {}
    }
    r.ue()?; // max_num_ref_frames
    r.bit()?; // gaps
    let w_mbs = r.ue()? + 1;
    let h_units = r.ue()? + 1;
    let frame_mbs_only = r.bit()?;
    if frame_mbs_only == 0 {
        r.bit()?;
    }
    r.bit()?; // direct_8x8
    let (mut cl, mut cr, mut ct, mut cb) = (0, 0, 0, 0);
    if r.bit()? == 1 {
        cl = r.ue()?;
        cr = r.ue()?;
        ct = r.ue()?;
        cb = r.ue()?;
    }
    let (sub_w, sub_h) = match chroma {
        0 => (1, 1),
        1 => (2, 2),
        2 => (2, 1),
        _ => (1, 1),
    };
    let unit_y = sub_h * (2 - frame_mbs_only);
    let w = (w_mbs * 16).checked_sub((cl + cr) * sub_w)?;
    let h = (h_units * 16 * (2 - frame_mbs_only)).checked_sub((ct + cb) * unit_y)?;
    Some((w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sps_phone_sized() {
        // x264 High 4:4:4 Predictive, 886x1920 (cropped from 896x1920)
        let sps = [
            0x67, 0xf4, 0x00, 0x28, 0x91, 0x96, 0x80, 0xe0, 0x0f, 0x1e, 0x2f, 0xc0, 0x44, 0x00, 0x00, 0x03,
            0x00, 0x04, 0x00, 0x00, 0x03, 0x00, 0xf2, 0x3c, 0x60, 0xca, 0x80,
        ];
        assert_eq!(sps_dimensions(&sps), Some((886, 1920)));
    }

    #[test]
    fn annexb_conversion() {
        let avcc = [0, 0, 0, 2, 0x65, 0x88, 0, 0, 0, 1, 0x41];
        let mut out = Vec::new();
        assert!(avcc_to_annexb(&avcc, &mut out));
        assert_eq!(out, [0, 0, 0, 1, 0x65, 0x88, 0, 0, 0, 1, 0x41, 0, 0, 0, 1, 0x09, 0xf0]);
        assert!(!avcc_to_annexb(&[0, 0, 0, 9, 1], &mut Vec::new()));
    }
}