//! Just enough H.264 for recording: Annex-B NAL splitting and SPS picture size.

pub const NAL_SPS: u8 = 7;
pub const NAL_PPS: u8 = 8;
pub const NAL_AUD: u8 = 9;
const NAL_TYPE_MASK: u8 = 0x1f;
const MACROBLOCK: u32 = 16;
/// Profiles whose SPS carries chroma format, bit depths and scaling lists (H.264 7.3.2.1.1).
const HIGH_PROFILES: [u8; 12] = [100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134];
const CHROMA_444: u32 = 3;
const CHROMA_420: u32 = 1;
const CHROMA_422: u32 = 2;
/// Scaling lists: 6 of 4x4 then 2 (or 6 for 4:4:4) of 8x8.
const SCALING_LISTS_4X4: u32 = 6;
const SCALING_LISTS: u32 = 8;
const SCALING_LISTS_444: u32 = 12;
const SCALING_4X4_SIZE: u32 = 16;
const SCALING_8X8_SIZE: u32 = 64;
const SCALING_DEFAULT: i64 = 8;
const SCALING_MODULO: i64 = 256;

pub const fn nal_type(nal: &[u8]) -> Option<u8> {
    match nal.first() {
        Some(header) => Some(*header & NAL_TYPE_MASK),
        None => None,
    }
}

/// NAL units of an Annex-B stream (3- or 4-byte start codes), without start codes.
pub fn nal_units(stream: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= stream.len() {
        if stream.get(i..i + 3) == Some(&[0, 0, 1][..]) {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    starts
        .iter()
        .enumerate()
        .map(|(n, &start)| {
            let end = starts.get(n + 1).map_or(stream.len(), |next| next - 3);
            let nal = stream.get(start..end).unwrap_or_default();
            // A 4-byte start code leaves its leading zero on the previous unit.
            let trimmed = nal.iter().rposition(|&b| b != 0).map_or(0, |last| last + 1);
            nal.get(..trimmed).unwrap_or_default()
        })
        .filter(|nal| !nal.is_empty())
        .collect()
}

/// RBSP bit reader (emulation prevention bytes removed up front).
struct Bits {
    data: Vec<u8>,
    position: usize,
}

impl Bits {
    fn new(nal: &[u8]) -> Self {
        let mut data = Vec::with_capacity(nal.len());
        let mut zeros = 0;
        for &byte in nal {
            if zeros >= 2 && byte == 3 {
                zeros = 0;
                continue;
            }
            zeros = if byte == 0 { zeros + 1 } else { 0 };
            data.push(byte);
        }
        Self { data, position: 0 }
    }

    fn bit(&mut self) -> Option<u32> {
        let byte = self.data.get(self.position / 8)?;
        let bit = (byte >> (7 - self.position % 8)) & 1;
        self.position += 1;
        Some(u32::from(bit))
    }

    fn bits(&mut self, count: u32) -> Option<u32> {
        (0..count).try_fold(0, |acc, _| Some(acc << 1 | self.bit()?))
    }

    fn flag(&mut self) -> Option<bool> {
        Some(self.bit()? == 1)
    }

    /// Exp-Golomb `ue(v)`.
    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > u32::BITS {
                return None;
            }
        }
        Some((1u32 << zeros) - 1 + self.bits(zeros)?)
    }

    /// Exp-Golomb `se(v)`.
    fn se(&mut self) -> Option<i64> {
        let code = i64::from(self.ue()?);
        Some(if code % 2 == 1 { (code + 1) / 2 } else { -code / 2 })
    }
}

fn skip_scaling_list(bits: &mut Bits, size: u32) -> Option<()> {
    let (mut last, mut next) = (SCALING_DEFAULT, SCALING_DEFAULT);
    for _ in 0..size {
        if next != 0 {
            next = (last + bits.se()? + SCALING_MODULO) % SCALING_MODULO;
        }
        if next != 0 {
            last = next;
        }
    }
    Some(())
}

/// Displayed width and height from an SPS NAL unit (header byte included).
pub fn sps_size(sps: &[u8]) -> Option<(u32, u32)> {
    let mut bits = Bits::new(sps);
    bits.bits(u8::BITS)?; // NAL header
    let profile = u8::try_from(bits.bits(u8::BITS)?).ok()?;
    bits.bits(u8::BITS)?; // constraint flags
    bits.bits(u8::BITS)?; // level
    bits.ue()?; // sps id
    let (mut chroma, mut separate_planes) = (CHROMA_420, false);
    if HIGH_PROFILES.contains(&profile) {
        chroma = bits.ue()?;
        if chroma == CHROMA_444 {
            separate_planes = bits.flag()?;
        }
        bits.ue()?; // luma bit depth
        bits.ue()?; // chroma bit depth
        bits.flag()?; // transform bypass
        if bits.flag()? {
            let lists = if chroma == CHROMA_444 { SCALING_LISTS_444 } else { SCALING_LISTS };
            for list in 0..lists {
                if bits.flag()? {
                    skip_scaling_list(
                        &mut bits,
                        if list < SCALING_LISTS_4X4 { SCALING_4X4_SIZE } else { SCALING_8X8_SIZE },
                    )?;
                }
            }
        }
    }
    bits.ue()?; // log2 max frame num
    match bits.ue()? {
        0 => {
            bits.ue()?; // log2 max poc lsb
        }
        1 => {
            bits.flag()?;
            bits.se()?;
            bits.se()?;
            for _ in 0..bits.ue()? {
                bits.se()?;
            }
        }
        _ => {}
    }
    bits.ue()?; // max ref frames
    bits.flag()?; // gaps allowed
    let width_mbs = bits.ue()? + 1;
    let height_units = bits.ue()? + 1;
    let frame_mbs_only = bits.flag()?;
    if !frame_mbs_only {
        bits.flag()?; // mb adaptive frame field
    }
    bits.flag()?; // direct 8x8 inference
    let field_factor = if frame_mbs_only { 1 } else { 2 };
    let (mut width, mut height) = (width_mbs * MACROBLOCK, height_units * MACROBLOCK * field_factor);
    if bits.flag()? {
        let (left, right, top, bottom) = (bits.ue()?, bits.ue()?, bits.ue()?, bits.ue()?);
        let (unit_x, unit_y) = if separate_planes || chroma == 0 {
            (1, field_factor)
        } else {
            let sub_width = if chroma == CHROMA_420 || chroma == CHROMA_422 { 2 } else { 1 };
            let sub_height = if chroma == CHROMA_420 { 2 } else { 1 };
            (sub_width, sub_height * field_factor)
        };
        width = width.checked_sub(unit_x * (left + right))?;
        height = height.checked_sub(unit_y * (top + bottom))?;
    }
    Some((width, height))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds RBSP bits the way an encoder would.
    #[derive(Default)]
    struct Writer {
        bits: Vec<u8>,
    }

    impl Writer {
        fn bits(&mut self, value: u32, count: u32) -> &mut Self {
            for i in (0..count).rev() {
                self.bits.push(u8::from(value >> i & 1 == 1));
            }
            self
        }

        fn ue(&mut self, value: u32) -> &mut Self {
            let code = value + 1;
            let length = u32::BITS - code.leading_zeros();
            self.bits(0, length - 1).bits(code, length)
        }

        fn bytes(&mut self) -> Vec<u8> {
            self.bits(1, 1); // rbsp stop bit
            self.bits.chunks(8).map(|byte| byte.iter().enumerate().fold(0, |acc, (i, &b)| acc | b << (7 - i))).collect()
        }
    }

    const BASELINE: u32 = 66;
    const HIGH: u32 = 100;

    fn sps(profile: u32, width_mbs: u32, height_mbs: u32, crop_bottom: Option<u32>) -> Vec<u8> {
        let mut w = Writer::default();
        w.bits(0x67, 8).bits(profile, 8).bits(0, 8).bits(31, 8).ue(0);
        if profile == HIGH {
            w.ue(CHROMA_420).ue(0).ue(0).bits(0, 1).bits(0, 1);
        }
        w.ue(0).ue(2).ue(1).bits(0, 1).ue(width_mbs - 1).ue(height_mbs - 1).bits(1, 1).bits(1, 1);
        match crop_bottom {
            Some(bottom) => w.bits(1, 1).ue(0).ue(0).ue(0).ue(bottom),
            None => w.bits(0, 1),
        };
        w.bits(0, 1); // no VUI
        w.bytes()
    }

    #[test]
    fn sps_sizes_with_and_without_cropping() {
        assert_eq!(sps_size(&sps(BASELINE, 80, 45, None)), Some((1280, 720)));
        assert_eq!(sps_size(&sps(HIGH, 120, 68, Some(4))), Some((1920, 1080)));
    }

    #[test]
    fn annex_b_splits_on_both_start_code_lengths() {
        let stream = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 4];
        let nals = nal_units(&stream);
        assert_eq!(nals, [&[0x67, 1, 2][..], &[0x68, 3], &[0x65, 4]]);
        assert_eq!(nals.iter().map(|n| nal_type(n)).collect::<Vec<_>>(), [Some(NAL_SPS), Some(NAL_PPS), Some(5)]);
    }

    #[test]
    fn emulation_prevention_bytes_are_removed() {
        let mut bits = Bits::new(&[0, 0, 3, 1]);
        assert_eq!(bits.bits(24), Some(1));
    }
}
