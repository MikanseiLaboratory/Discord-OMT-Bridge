use openh264::decoder::Decoder;
use openh264::formats::YUVSource;
use opus::{Channels, Decoder as OpusNative};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DecodeError {
    #[error("opus: {0}")]
    Opus(String),
    #[error("h264: {0}")]
    H264(String),
    #[error("av1: {0}")]
    Av1(String),
}

pub struct OpusDecoder {
    decoder: OpusNative,
    output: Vec<i16>,
}

impl OpusDecoder {
    pub fn new() -> Result<Self, DecodeError> {
        let decoder = OpusNative::new(48_000, Channels::Stereo)
            .map_err(|err| DecodeError::Opus(err.to_string()))?;
        Ok(Self {
            decoder,
            output: vec![0; 5760 * 2],
        })
    }

    pub fn decode(&mut self, payload: &[u8]) -> Result<Vec<i16>, DecodeError> {
        if payload.is_empty() {
            return Ok(Vec::new());
        }
        let samples_per_channel = self
            .decoder
            .decode(payload, &mut self.output, false)
            .map_err(|err| DecodeError::Opus(err.to_string()))?;
        let interleaved = samples_per_channel.saturating_mul(2);
        Ok(self.output[..interleaved].to_vec())
    }
}

pub struct H264Decoder {
    decoder: Decoder,
}

impl H264Decoder {
    pub fn new() -> Result<Self, DecodeError> {
        let decoder = Decoder::new().map_err(|err| DecodeError::H264(err.to_string()))?;
        Ok(Self { decoder })
    }

    pub fn decode(&mut self, access_unit: &[u8]) -> Result<Option<(Vec<u8>, u32, u32)>, DecodeError> {
        let bitstream = ensure_start_code(access_unit);
        let Some(yuv) = self
            .decoder
            .decode(&bitstream)
            .map_err(|err| DecodeError::H264(err.to_string()))?
        else {
            return Ok(None);
        };
        let (width, height) = yuv.dimensions();
        if width == 0 || height == 0 {
            return Ok(None);
        }
        let bgra = yuv_to_bgra(&yuv);
        Ok(Some((bgra, width as u32, height as u32)))
    }
}

const OBU_HAS_SIZE: u8 = 0x02;
const OBU_HAS_EXTENSION: u8 = 0x04;

/// Reassembles AV1 RTP packets into the frame Discord's DAVE decryptor expects.
///
/// The packetizer removes OBU sizes. Discord puts those sizes back on every OBU
/// except the last, and leaves the DAVE trailer at the end of that last OBU.
#[derive(Debug, Default)]
pub struct Av1Assembler {
    obus: Vec<Vec<u8>>,
    tail: Vec<u8>,
}

impl Av1Assembler {
    pub fn push(&mut self, payload: &[u8], marker: bool) -> Option<Vec<u8>> {
        if payload.is_empty() {
            self.reset();
            return None;
        }
        let header = payload[0];
        let continues = header & 0x80 != 0;
        let unfinished = header & 0x40 != 0;
        let count = (header >> 4) & 0x03;
        let new_sequence = header & 0x08 != 0;
        if new_sequence && !continues {
            self.reset();
        }
        let mut elements = match split_obu_elements(&payload[1..], count) {
            Some(elements) if !elements.is_empty() => elements,
            _ => {
                self.reset();
                return None;
            }
        };
        if continues {
            let first = elements.remove(0);
            self.tail.extend_from_slice(&first);
            if !(elements.is_empty() && unfinished) {
                self.obus.push(std::mem::take(&mut self.tail));
            }
        } else if !self.tail.is_empty() {
            self.obus.push(std::mem::take(&mut self.tail));
        }
        let last_index = elements.len().saturating_sub(1);
        for (index, element) in elements.into_iter().enumerate() {
            if unfinished && index == last_index {
                self.tail.extend_from_slice(&element);
            } else {
                self.obus.push(element);
            }
        }
        if marker {
            return self.finish();
        }
        None
    }

    fn finish(&mut self) -> Option<Vec<u8>> {
        if !self.tail.is_empty() {
            self.obus.push(std::mem::take(&mut self.tail));
        }
        let frame = reconstruct_discord_frame(&self.obus);
        self.reset();
        (!frame.is_empty()).then_some(frame)
    }

    fn reset(&mut self) {
        self.obus.clear();
        self.tail.clear();
    }
}

fn split_obu_elements(mut rest: &[u8], count: u8) -> Option<Vec<Vec<u8>>> {
    let mut elements = Vec::new();
    if count == 0 {
        while !rest.is_empty() {
            let (size, used) = read_leb128(rest)?;
            rest = &rest[used..];
            if size > rest.len() {
                return None;
            }
            elements.push(rest[..size].to_vec());
            rest = &rest[size..];
        }
    } else {
        for index in 0..count {
            if index + 1 == count {
                if rest.is_empty() {
                    return None;
                }
                elements.push(rest.to_vec());
                rest = &[];
                break;
            }
            let (size, used) = read_leb128(rest)?;
            rest = &rest[used..];
            if size > rest.len() {
                return None;
            }
            elements.push(rest[..size].to_vec());
            rest = &rest[size..];
        }
        if let Some(last) = elements.last_mut() {
            last.extend_from_slice(rest);
        }
    }
    Some(elements)
}

fn reconstruct_discord_frame(obus: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    let last = obus.len().saturating_sub(1);
    for (index, obu) in obus.iter().enumerate() {
        let Some((header, extension, payload, trailing)) = split_obu(obu) else {
            continue;
        };
        if index == last {
            out.push(header & !OBU_HAS_SIZE);
            if let Some(extension) = extension {
                out.push(extension);
            }
            out.extend_from_slice(payload);
            out.extend_from_slice(trailing);
        } else {
            out.push(header | OBU_HAS_SIZE);
            if let Some(extension) = extension {
                out.push(extension);
            }
            write_leb128(payload.len(), &mut out);
            out.extend_from_slice(payload);
        }
    }
    out
}

fn split_obu(obu: &[u8]) -> Option<(u8, Option<u8>, &[u8], &[u8])> {
    let header = *obu.first()?;
    let mut cursor = 1;
    let extension = if header & OBU_HAS_EXTENSION != 0 {
        let extension = *obu.get(cursor)?;
        cursor += 1;
        Some(extension)
    } else {
        None
    };
    if header & OBU_HAS_SIZE != 0 {
        let (size, used) = read_leb128(obu.get(cursor..)?)?;
        cursor += used;
        let end = cursor.checked_add(size)?;
        if end > obu.len() {
            return None;
        }
        Some((header, extension, &obu[cursor..end], &obu[end..]))
    } else {
        Some((header, extension, &obu[cursor..], &[]))
    }
}

fn write_leb128(mut value: usize, out: &mut Vec<u8>) {
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
            break;
        }
    }
}

/// True when any sized OBU in the access unit is a sequence header.
/// A fresh rav1d context aborts if the first frame has no sequence header.
pub(crate) fn contains_av1_sequence_header(frame: &[u8]) -> bool {
    let mut index = 0usize;
    while index < frame.len() {
        let header = frame[index];
        let obu_type = (header >> 3) & 0x0f;
        if obu_type == 1 {
            return true;
        }
        index += 1;
        if header & 0x04 != 0 {
            if index >= frame.len() {
                return false;
            }
            index += 1;
        }
        if header & 0x02 == 0 {
            return false;
        }
        let Some((size, used)) = read_leb128(&frame[index..]) else {
            return false;
        };
        index = index.saturating_add(used).saturating_add(size);
    }
    false
}

pub(crate) fn read_leb128(data: &[u8]) -> Option<(usize, usize)> {
    let mut value = 0usize;
    for index in 0..8 {
        let byte = *data.get(index)?;
        value |= ((byte & 0x7f) as usize) << (index * 7);
        if byte & 0x80 == 0 {
            return Some((value, index + 1));
        }
    }
    None
}

fn ensure_start_code(access_unit: &[u8]) -> Vec<u8> {
    if access_unit.starts_with(&[0, 0, 1]) || access_unit.starts_with(&[0, 0, 0, 1]) {
        return access_unit.to_vec();
    }
    let mut framed = Vec::with_capacity(access_unit.len() + 4);
    framed.extend_from_slice(&[0, 0, 0, 1]);
    framed.extend_from_slice(access_unit);
    framed
}

fn yuv_to_bgra(yuv: &impl YUVSource) -> Vec<u8> {
    let (width, height) = yuv.dimensions();
    let (y_stride, u_stride, v_stride) = yuv.strides();
    let y_plane = yuv.y();
    let u_plane = yuv.u();
    let v_plane = yuv.v();
    let cw = (width / 2).max(1);
    let ch = (height / 2).max(1);
    let mut bgra = vec![0u8; width * height * 4];
    for row in 0..height {
        for col in 0..width {
            let y = y_plane.get(row * y_stride + col).copied().unwrap_or(0) as i32;
            let u = chroma_bilinear(col, row, 1, 1, cw, ch, |x, y| {
                u_plane.get(y * u_stride + x).copied().unwrap_or(128) as i32
            });
            let v = chroma_bilinear(col, row, 1, 1, cw, ch, |x, y| {
                v_plane.get(y * v_stride + x).copied().unwrap_or(128) as i32
            });
            let (b, g, r) = yuv_to_rgb(YuvColor::hd_limited(), y, u, v);
            let offset = (row * width + col) * 4;
            bgra[offset] = b;
            bgra[offset + 1] = g;
            bgra[offset + 2] = r;
            bgra[offset + 3] = 255;
        }
    }
    bgra
}

/// Color description from an AV1 sequence header. Unknown values stay on
/// limited-range BT.709, which is what ordinary Discord HD uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct YuvColor {
    matrix: YuvMatrix,
    full: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum YuvMatrix {
    Bt709,
    Bt601,
    Bt2020,
    Identity,
}

impl YuvColor {
    pub(crate) fn hd_limited() -> Self {
        Self {
            matrix: YuvMatrix::Bt709,
            full: false,
        }
    }

    pub(crate) fn from_av1(matrix: u32, color_range: u8) -> Self {
        let matrix = match matrix {
            0 => YuvMatrix::Identity,
            4 | 5 | 6 => YuvMatrix::Bt601,
            9 => YuvMatrix::Bt2020,
            _ => YuvMatrix::Bt709,
        };
        Self {
            matrix,
            full: color_range == 1,
        }
    }
}

pub(crate) fn yuv_to_rgb(color: YuvColor, y: i32, u: i32, v: i32) -> (u8, u8, u8) {
    if color.matrix == YuvMatrix::Identity {
        let plane = |sample: i32| {
            if color.full {
                sample
            } else {
                ((sample - 16) * 298 + 128) >> 8
            }
        };
        // AV1 identity stores G in Y, B in U, and R in V.
        return (clamp_u8(plane(u)), clamp_u8(plane(y)), clamp_u8(plane(v)));
    }
    let d = u - 128;
    let e = v - 128;
    let (r, g, b) = if color.full {
        let (re, gd, ge, bd) = match color.matrix {
            YuvMatrix::Bt601 => (359, 88, 183, 454),
            YuvMatrix::Bt2020 => (378, 42, 146, 482),
            YuvMatrix::Bt709 | YuvMatrix::Identity => (403, 48, 120, 475),
        };
        (
            y + ((re * e + 128) >> 8),
            y - ((gd * d + ge * e + 128) >> 8),
            y + ((bd * d + 128) >> 8),
        )
    } else {
        let c = y - 16;
        let (re, gd, ge, bd) = match color.matrix {
            YuvMatrix::Bt601 => (409, 100, 208, 516),
            YuvMatrix::Bt2020 => (430, 48, 167, 548),
            YuvMatrix::Bt709 | YuvMatrix::Identity => (459, 55, 136, 541),
        };
        (
            (298 * c + re * e + 128) >> 8,
            (298 * c - gd * d - ge * e + 128) >> 8,
            (298 * c + bd * d + 128) >> 8,
        )
    };
    (clamp_u8(b), clamp_u8(g), clamp_u8(r))
}

/// Chroma at one luma pixel. `ss_x` and `ss_y` are 0 (full) or 1 (half).
pub(crate) fn chroma_bilinear(x: usize, y: usize, ss_x: u32, ss_y: u32, cw: usize, ch: usize, mut sample: impl FnMut(usize, usize) -> i32) -> i32 {
    if cw == 0 || ch == 0 {
        return 128;
    }
    let (cx, fx) = chroma_phase(x, ss_x, cw);
    let (cy, fy) = chroma_phase(y, ss_y, ch);
    let cx1 = (cx + 1).min(cw - 1);
    let cy1 = (cy + 1).min(ch - 1);
    let top = sample(cx, cy) * (256 - fx) + sample(cx1, cy) * fx;
    let bottom = sample(cx, cy1) * (256 - fx) + sample(cx1, cy1) * fx;
    (top * (256 - fy) + bottom * fy + (1 << 15)) >> 16
}

fn chroma_phase(pos: usize, ss: u32, limit: usize) -> (usize, i32) {
    if ss == 0 || limit <= 1 {
        return (pos.min(limit.saturating_sub(1)), 0);
    }
    // MPEG siting: the chroma sample sits between luma pixels.
    let mut fp = pos as i32 * 128 + 64 - 128;
    if fp < 0 {
        fp = 0;
    }
    let max = ((limit as i32) - 1) << 8;
    if fp > max {
        fp = max;
    }
    ((fp >> 8) as usize, fp & 255)
}

fn clamp_u8(value: i32) -> u8 {
    value.clamp(0, 255) as u8
}

/// Discord sometimes delivers a plaintext Opus packet with a DAVE supplemental trailer.
pub fn strip_dave_supplemental(packet: &[u8]) -> Option<&[u8]> {
    if packet.len() < 4 {
        return None;
    }
    let padding = *packet.last()? as usize;
    if padding == 0 || padding >= packet.len() {
        return None;
    }
    let without_padding = &packet[..packet.len() - padding];
    if without_padding.len() < 3 {
        return None;
    }
    let end = without_padding.len();
    if without_padding[end - 2] != 0xFA || without_padding[end - 1] != 0xFA {
        return None;
    }
    let supplemental = without_padding[end - 3] as usize;
    if supplemental == 0 || supplemental > without_padding.len() {
        return None;
    }
    Some(&without_padding[..without_padding.len() - supplemental])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opus_decoder_is_available() {
        OpusDecoder::new().expect("opus decoder");
    }

    #[test]
    fn yuv_black_is_dark() {
        let (b, g, r) = yuv_to_rgb(YuvColor::hd_limited(), 16, 128, 128);
        assert!(b < 8 && g < 8 && r < 8);
    }

    #[test]
    fn full_range_gray_is_not_lifted_to_limited_black() {
        let (b, g, r) = yuv_to_rgb(YuvColor::from_av1(1, 1), 16, 128, 128);
        assert!((12..=20).contains(&b) && (12..=20).contains(&g) && (12..=20).contains(&r));
    }

    #[test]
    fn av1_color_description_selects_the_matrix() {
        assert_eq!(YuvColor::from_av1(2, 0), YuvColor::hd_limited());
        assert_eq!(
            YuvColor::from_av1(6, 0),
            YuvColor {
                matrix: YuvMatrix::Bt601,
                full: false
            }
        );
        assert_eq!(
            YuvColor::from_av1(9, 1),
            YuvColor {
                matrix: YuvMatrix::Bt2020,
                full: true
            }
        );
        let (b, g, r) = yuv_to_rgb(YuvColor::from_av1(0, 1), 10, 20, 30);
        assert_eq!((b, g, r), (20, 10, 30));
        let hd = yuv_to_rgb(YuvColor::hd_limited(), 126, 90, 200);
        let sd = yuv_to_rgb(YuvColor::from_av1(6, 0), 126, 90, 200);
        assert_ne!(hd, sd);
    }

    #[test]
    fn av1_packet_without_lengths_is_the_obu() {
        let mut frame = Av1Assembler::default();
        let obu = frame.push(&[0x10, 0x08, 0x0b], true).unwrap();
        assert_eq!(obu, [0x08, 0x0b]);
    }

    #[test]
    fn av1_packet_restores_sizes_except_the_last_obu() {
        let mut frame = Av1Assembler::default();
        let obu = frame
            .push(&[0x20, 0x02, 0x08, 0xaa, 0x30, 0xbb, 0xcc, 0xfa, 0xfa], true)
            .unwrap();
        assert_eq!(obu, [0x0a, 0x01, 0xaa, 0x30, 0xbb, 0xcc, 0xfa, 0xfa]);
    }

    #[test]
    fn av1_fragments_join_before_the_last_obu_is_closed() {
        let mut frame = Av1Assembler::default();
        assert!(frame.push(&[0x50, 0x08, 0xaa], false).is_none());
        let obu = frame.push(&[0x90, 0x11, 0x22], true).unwrap();
        assert_eq!(obu, [0x08, 0xaa, 0x11, 0x22]);
    }

    #[test]
    fn supplemental_trailer_is_removed() {
        let mut packet = b"opus".to_vec();
        packet.extend_from_slice(&[0; 9]);
        packet.extend_from_slice(&[12, 0xFA, 0xFA]);
        packet.push(1);
        let stripped = strip_dave_supplemental(&packet).unwrap();
        assert_eq!(stripped, b"opus");
    }
}
