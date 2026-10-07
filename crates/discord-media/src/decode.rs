use std::ffi::c_void;
use std::ptr::NonNull;

use openh264::decoder::Decoder;
use openh264::formats::YUVSource;
use opus::{Channels, Decoder as OpusNative};
use rav1d::include::dav1d::data::Dav1dData;
use rav1d::include::dav1d::dav1d::{Dav1dContext, Dav1dSettings};
use rav1d::include::dav1d::headers::{DAV1D_PIXEL_LAYOUT_I400, DAV1D_PIXEL_LAYOUT_I420, DAV1D_PIXEL_LAYOUT_I422, DAV1D_PIXEL_LAYOUT_I444};
use rav1d::include::dav1d::picture::Dav1dPicture;
use rav1d::src::lib::{dav1d_close, dav1d_data_unref, dav1d_data_wrap, dav1d_default_settings, dav1d_get_picture, dav1d_open, dav1d_picture_unref, dav1d_send_data};
use rav1d::src::send_sync_non_null::SendSyncNonNull;
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

pub struct Av1Decoder {
    ctx: Option<Dav1dContext>,
}

// The context is an internally reference-counted pointer. The connection task
// owns it and tokio only moves that task between threads.
unsafe impl Send for Av1Decoder {}

impl Av1Decoder {
    pub fn new() -> Result<Self, DecodeError> {
        let mut settings = unsafe {
            let mut settings = std::mem::MaybeUninit::<Dav1dSettings>::uninit();
            dav1d_default_settings(NonNull::new_unchecked(settings.as_mut_ptr()));
            settings.assume_init()
        };
        // One worker thread hits a rav1d bug: a bad frame unwraps a missing
        // frame header and aborts the process. Two threads take the path that
        // returns the error instead.
        settings.n_threads = 2;
        settings.max_frame_delay = 1;
        let mut ctx = None;
        let status = unsafe { dav1d_open(NonNull::new(&mut ctx), NonNull::new(&mut settings)) };
        if status.0 != 0 || ctx.is_none() {
            return Err(DecodeError::Av1(format!("decoder open failed ({})", status.0)));
        }
        Ok(Self { ctx })
    }

    pub fn decode(&mut self, obu: &[u8]) -> Result<Option<(Vec<u8>, u32, u32)>, DecodeError> {
        if obu.is_empty() {
            return Ok(None);
        }
        let mut held = None;
        if !self.send(obu)? {
            held = self.pull()?;
            if !self.send(obu)? {
                return Err(DecodeError::Av1("decoder queue stayed full".into()));
            }
        }
        Ok(self.pull()?.or(held))
    }

    fn send(&mut self, obu: &[u8]) -> Result<bool, DecodeError> {
        let bytes = obu.to_vec();
        let len = bytes.len();
        let mut data = Dav1dData::default();
        let boxed = Box::new(bytes);
        let ptr = NonNull::new(boxed.as_ptr() as *mut u8).ok_or_else(|| DecodeError::Av1("empty obu".into()))?;
        let cookie = SendSyncNonNull::from_box(boxed).cast::<c_void>();
        let wrapped = unsafe { dav1d_data_wrap(NonNull::new(&mut data), Some(ptr), len, Some(free_obu), Some(cookie)) };
        if wrapped.0 != 0 {
            unsafe { free_obu(ptr.as_ptr(), Some(cookie)) };
            return Err(DecodeError::Av1(format!("wrap failed ({})", wrapped.0)));
        }
        let status = unsafe { dav1d_send_data(self.ctx.clone(), NonNull::new(&mut data)) };
        if status.0 == 0 {
            return Ok(true);
        }
        unsafe { dav1d_data_unref(NonNull::new(&mut data)) };
        if status.0 == -11 {
            return Ok(false);
        }
        Err(DecodeError::Av1(format!("send failed ({})", status.0)))
    }

    fn pull(&mut self) -> Result<Option<(Vec<u8>, u32, u32)>, DecodeError> {
        let mut picture = Dav1dPicture::default();
        let status = unsafe { dav1d_get_picture(self.ctx.clone(), NonNull::new(&mut picture)) };
        if status.0 == -11 {
            return Ok(None);
        }
        if status.0 != 0 {
            return Err(DecodeError::Av1(format!("picture failed ({})", status.0)));
        }
        let frame = picture_bgra(&picture);
        unsafe { dav1d_picture_unref(NonNull::new(&mut picture)) };
        frame
    }
}

impl Drop for Av1Decoder {
    fn drop(&mut self) {
        if self.ctx.is_some() {
            unsafe { dav1d_close(NonNull::new(&mut self.ctx)) };
        }
    }
}

unsafe extern "C" fn free_obu(_ptr: *const u8, cookie: Option<SendSyncNonNull<c_void>>) {
    if let Some(cookie) = cookie {
        drop(Box::from_raw(cookie.as_ptr().as_ptr() as *mut Vec<u8>));
    }
}

fn picture_bgra(picture: &Dav1dPicture) -> Result<Option<(Vec<u8>, u32, u32)>, DecodeError> {
    let width = picture.p.w;
    let height = picture.p.h;
    if width <= 0 || height <= 0 || width > 3840 || height > 2160 {
        return Ok(None);
    }
    let width = width as usize;
    let height = height as usize;
    let layout = picture.p.layout;
    let bpc = picture.p.bpc.max(8) as usize;
    let y = picture.data[0].ok_or_else(|| DecodeError::Av1("picture has no luma".into()))?;
    let stride_y = picture.stride[0];
    let (ss_x, ss_y) = match layout {
        DAV1D_PIXEL_LAYOUT_I400 => (0, 0),
        DAV1D_PIXEL_LAYOUT_I420 => (1, 1),
        DAV1D_PIXEL_LAYOUT_I422 => (1, 0),
        DAV1D_PIXEL_LAYOUT_I444 => (0, 0),
        other => return Err(DecodeError::Av1(format!("unsupported layout {other}"))),
    };
    let u = picture.data[1];
    let v = picture.data[2];
    let stride_uv = picture.stride[1];
    let mut bgra = vec![0u8; width * height * 4];
    for row in 0..height {
        for col in 0..width {
            let y_value = plane_sample(y.as_ptr(), stride_y, col, row, bpc) as i32;
            let (u_value, v_value) = if layout == DAV1D_PIXEL_LAYOUT_I400 {
                (128, 128)
            } else {
                let Some(u) = u else { return Err(DecodeError::Av1("picture has no chroma".into())); };
                let Some(v) = v else { return Err(DecodeError::Av1("picture has no chroma".into())); };
                (
                    plane_sample(u.as_ptr(), stride_uv, col >> ss_x, row >> ss_y, bpc) as i32,
                    plane_sample(v.as_ptr(), stride_uv, col >> ss_x, row >> ss_y, bpc) as i32,
                )
            };
            let (b, g, r) = yuv_to_rgb(y_value, u_value, v_value);
            let offset = (row * width + col) * 4;
            bgra[offset] = b;
            bgra[offset + 1] = g;
            bgra[offset + 2] = r;
            bgra[offset + 3] = 255;
        }
    }
    Ok(Some((bgra, width as u32, height as u32)))
}

fn plane_sample(data: *const c_void, stride: isize, x: usize, y: usize, bpc: usize) -> u8 {
    let row = unsafe { (data as *const u8).offset(stride * y as isize) };
    if bpc <= 8 {
        unsafe { *row.add(x) }
    } else {
        let sample = unsafe { *(row as *const u16).add(x) };
        (sample >> (bpc - 8)) as u8
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

fn read_leb128(data: &[u8]) -> Option<(usize, usize)> {
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
