use std::collections::HashMap;
use std::ffi::c_void;
use std::io::{self, BufReader, BufWriter};
use std::ptr::NonNull;

use rav1d::include::dav1d::data::Dav1dData;
use rav1d::include::dav1d::dav1d::{Dav1dContext, Dav1dSettings};
use rav1d::include::dav1d::headers::{DAV1D_PIXEL_LAYOUT_I400, DAV1D_PIXEL_LAYOUT_I420, DAV1D_PIXEL_LAYOUT_I422, DAV1D_PIXEL_LAYOUT_I444};
use rav1d::include::dav1d::picture::Dav1dPicture;
use rav1d::src::lib::{dav1d_close, dav1d_data_unref, dav1d_data_wrap, dav1d_default_settings, dav1d_get_picture, dav1d_open, dav1d_picture_unref, dav1d_send_data};
use rav1d::src::send_sync_non_null::SendSyncNonNull;

use crate::av1_ipc::{read_message, write_message, Av1IpcError, Message, FLAG_ABORT, FLAG_BAD_ID, FLAG_HANG, KIND_DECODE, KIND_DECODE_ERROR, KIND_DROP, KIND_HELLO, KIND_NO_PICTURE, KIND_PICTURE, KIND_READY, KIND_RESET, MAX_HEIGHT, MAX_WIDTH, WORKER_ARG};
use crate::decode::{chroma_bilinear, yuv_to_rgb, YuvColor, DecodeError};

pub fn maybe_run_av1_worker() -> bool {
    if std::env::args().nth(1).as_deref() != Some(WORKER_ARG) {
        return false;
    }
    if let Err(err) = serve() {
        eprintln!("av1 worker stopped: {err}");
        std::process::exit(1);
    }
    std::process::exit(0);
}

fn serve() -> Result<(), Av1IpcError> {
    let mut input = BufReader::new(io::stdin());
    let mut output = BufWriter::new(io::stdout());
    let hello = read_message(&mut input)?;
    if hello.kind != KIND_HELLO {
        return Err(Av1IpcError::Protocol("expected hello".into()));
    }
    write_message(&mut output, &reply(&hello, KIND_READY, 0, 0, Vec::new()))?;
    let mut decoders: HashMap<u32, Av1Decoder> = HashMap::new();
    loop {
        let message = match read_message(&mut input) {
            Ok(message) => message,
            Err(_) => return Ok(()),
        };
        if message.flags & FLAG_ABORT != 0 {
            std::process::abort();
        }
        if message.flags & FLAG_HANG != 0 {
            loop {
                std::thread::sleep(std::time::Duration::from_secs(60));
            }
        }
        if message.flags & FLAG_BAD_ID != 0 {
            let mut bad = reply(&message, KIND_NO_PICTURE, 0, 0, Vec::new());
            bad.request_id = bad.request_id.wrapping_add(1);
            write_message(&mut output, &bad)?;
            continue;
        }
        match message.kind {
            KIND_DECODE => {
                if message.payload.is_empty() {
                    decoders.entry(message.ssrc).or_insert_with(Av1Decoder::open);
                    write_message(&mut output, &reply(&message, KIND_NO_PICTURE, 0, 0, Vec::new()))?;
                    continue;
                }
                let needs_header = decoders.get(&message.ssrc).is_none_or(|decoder| !decoder.has_sequence);
                if needs_header && !crate::decode::contains_av1_sequence_header(&message.payload) {
                    decoders.remove(&message.ssrc);
                    write_message(
                        &mut output,
                        &reply(&message, KIND_DECODE_ERROR, 0, 0, b"waiting for a sequence header".to_vec()),
                    )?;
                    continue;
                }
                let had_header = crate::decode::contains_av1_sequence_header(&message.payload);
                let decoded = {
                    let decoder = decoders.entry(message.ssrc).or_insert_with(Av1Decoder::open);
                    if decoder.ctx.is_none() {
                        Err(None)
                    } else {
                        let result = decoder.decode(&message.payload);
                        if had_header && result.is_ok() {
                            decoder.has_sequence = true;
                        }
                        result.map_err(Some)
                    }
                };
                match decoded {
                    Ok(Some((bgra, width, height))) => {
                        let fits = width > 0 && height > 0 && width <= MAX_WIDTH && height <= MAX_HEIGHT && bgra.len() == width as usize * height as usize * 4;
                        if fits {
                            write_message(&mut output, &reply(&message, KIND_PICTURE, width, height, bgra))?;
                        } else {
                            write_message(&mut output, &reply(&message, KIND_DECODE_ERROR, 0, 0, b"picture dimensions rejected".to_vec()))?;
                        }
                    }
                    Ok(None) => write_message(&mut output, &reply(&message, KIND_NO_PICTURE, 0, 0, Vec::new()))?,
                    Err(None) => {
                        decoders.remove(&message.ssrc);
                        write_message(&mut output, &reply(&message, KIND_DECODE_ERROR, 0, 0, b"decoder open failed".to_vec()))?;
                    }
                    Err(Some(err)) => {
                        decoders.remove(&message.ssrc);
                        let text = err.to_string();
                        write_message(&mut output, &reply(&message, KIND_DECODE_ERROR, 0, 0, text.into_bytes()))?;
                    }
                }
            }
            KIND_RESET | KIND_DROP => {
                decoders.remove(&message.ssrc);
                write_message(&mut output, &reply(&message, KIND_NO_PICTURE, 0, 0, Vec::new()))?;
            }
            _ => return Err(Av1IpcError::Protocol("unexpected request".into())),
        }
    }
}

fn reply(request: &Message, kind: u8, width: u32, height: u32, payload: Vec<u8>) -> Message {
    Message {
        kind,
        flags: 0,
        request_id: request.request_id,
        ssrc: request.ssrc,
        width,
        height,
        payload,
    }
}

struct Av1Decoder {
    ctx: Option<Dav1dContext>,
    has_sequence: bool,
}

impl Av1Decoder {
    fn open() -> Self {
        let mut settings = unsafe {
            let mut settings = std::mem::MaybeUninit::<Dav1dSettings>::uninit();
            dav1d_default_settings(NonNull::new_unchecked(settings.as_mut_ptr()));
            settings.assume_init()
        };
        let threads = std::thread::available_parallelism().map(|count| count.get()).unwrap_or(4);
        settings.n_threads = i32::try_from(threads.clamp(4, 8)).unwrap_or(4);
        settings.max_frame_delay = 1;
        let mut ctx = None;
        let status = unsafe { dav1d_open(NonNull::new(&mut ctx), NonNull::new(&mut settings)) };
        if status.0 != 0 {
            ctx = None;
        }
        Self { ctx, has_sequence: false }
    }

    fn decode(&mut self, obu: &[u8]) -> Result<Option<(Vec<u8>, u32, u32)>, DecodeError> {
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
    if width <= 0 || height <= 0 || width > MAX_WIDTH as i32 || height > MAX_HEIGHT as i32 {
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
    let cw = if ss_x == 0 { width } else { (width >> ss_x).max(1) };
    let ch = if ss_y == 0 { height } else { (height >> ss_y).max(1) };
    let color = picture_color(picture);
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
                    chroma_bilinear(col, row, ss_x as u32, ss_y as u32, cw, ch, |x, y| {
                        plane_sample(u.as_ptr(), stride_uv, x, y, bpc) as i32
                    }),
                    chroma_bilinear(col, row, ss_x as u32, ss_y as u32, cw, ch, |x, y| {
                        plane_sample(v.as_ptr(), stride_uv, x, y, bpc) as i32
                    }),
                )
            };
            let (b, g, r) = yuv_to_rgb(color, y_value, u_value, v_value);
            let offset = (row * width + col) * 4;
            bgra[offset] = b;
            bgra[offset + 1] = g;
            bgra[offset + 2] = r;
            bgra[offset + 3] = 255;
        }
    }
    Ok(Some((bgra, width as u32, height as u32)))
}

fn picture_color(picture: &Dav1dPicture) -> YuvColor {
    let Some(header) = picture.seq_hdr else {
        return YuvColor::hd_limited();
    };
    // The sequence header stays alive until dav1d_picture_unref.
    let header = unsafe { header.as_ref() };
    YuvColor::from_av1(header.mtrx, header.color_range)
}

fn plane_sample(data: *const c_void, stride: isize, x: usize, y: usize, bpc: usize) -> u8 {
    let row = unsafe { (data as *const u8).offset(stride * y as isize) };
    if bpc <= 8 {
        unsafe { *row.add(x) }
    } else {
        let sample = unsafe { *(row as *const u16).add(x) } as u32;
        let shift = bpc - 8;
        ((sample + (1 << (shift - 1))) >> shift).min(255) as u8
    }
}
