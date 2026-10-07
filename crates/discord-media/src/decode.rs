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
    let mut bgra = vec![0u8; width * height * 4];
    for row in 0..height {
        for col in 0..width {
            let y = y_plane.get(row * y_stride + col).copied().unwrap_or(0) as i32;
            let u = u_plane.get((row / 2) * u_stride + (col / 2)).copied().unwrap_or(128) as i32;
            let v = v_plane.get((row / 2) * v_stride + (col / 2)).copied().unwrap_or(128) as i32;
            let (b, g, r) = yuv_to_rgb(y, u, v);
            let offset = (row * width + col) * 4;
            bgra[offset] = b;
            bgra[offset + 1] = g;
            bgra[offset + 2] = r;
            bgra[offset + 3] = 255;
        }
    }
    bgra
}

fn yuv_to_rgb(y: i32, u: i32, v: i32) -> (u8, u8, u8) {
    let c = y - 16;
    let d = u - 128;
    let e = v - 128;
    let r = (298 * c + 409 * e + 128) >> 8;
    let g = (298 * c - 100 * d - 208 * e + 128) >> 8;
    let b = (298 * c + 516 * d + 128) >> 8;
    (clamp_u8(b), clamp_u8(g), clamp_u8(r))
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
        let (b, g, r) = yuv_to_rgb(16, 128, 128);
        assert!(b < 8 && g < 8 && r < 8);
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
