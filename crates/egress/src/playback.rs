use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, SampleRate, Stream, StreamConfig};


const RING_SECONDS: u32 = 1;

#[derive(Clone, Debug)]
pub struct ListedDevice {
    pub name: String,
    pub host: &'static str,
    pub channels: u16,
}

#[derive(Clone, Debug)]
pub enum SpeakerTarget {
    /// One stereo stream on the default host (WASAPI on Windows).
    Device { name: String },
    /// A channel range on the shared ASIO device. `start` is 1-based.
    Asio { name: String, start: u16, count: u16 },
}

pub fn asio_available() -> bool {
    cfg!(all(windows, feature = "asio"))
}

pub fn list_output_devices() -> Vec<ListedDevice> {
    let mut listed = list_host("device", &cpal::default_host());
    listed.extend(asio_devices());
    listed
}

fn asio_devices() -> Vec<ListedDevice> {
    #[cfg(all(windows, feature = "asio"))]
    if let Some(host) = asio_host() {
        return list_host("asio", &host);
    }
    Vec::new()
}

pub struct PlaybackEngine {
    tx: Sender<Command>,
    thread: Option<JoinHandle<()>>,
}

impl PlaybackEngine {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("playback".into())
            .spawn(move || worker(rx))
            .ok();
        Self { tx, thread }
    }

    pub fn bind(&self, id: &str, target: SpeakerTarget) -> Result<(), String> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.tx
            .send(Command::Bind {
                id: id.to_owned(),
                target,
                reply: reply_tx,
            })
            .map_err(|_| "音声出力が停止しています".to_owned())?;
        reply_rx.recv().map_err(|_| "音声出力が停止しています".to_owned())?
    }

    pub fn unbind(&self, id: &str) {
        let _ = self.tx.send(Command::Unbind { id: id.to_owned() });
    }

    pub fn write(&self, id: &str, pcm: &[i16], sample_rate: u32, channels: u16) {
        if pcm.is_empty() || sample_rate == 0 || channels == 0 {
            return;
        }
        let _ = self.tx.send(Command::Pcm {
            id: id.to_owned(),
            pcm: pcm.to_vec(),
            sample_rate,
            channels,
        });
    }
}

impl Drop for PlaybackEngine {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Stop);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

enum Command {
    Bind {
        id: String,
        target: SpeakerTarget,
        reply: SyncSender<Result<(), String>>,
    },
    Unbind {
        id: String,
    },
    Pcm {
        id: String,
        pcm: Vec<i16>,
        sample_rate: u32,
        channels: u16,
    },
    Stop,
}

struct StereoQueue {
    rate: u32,
    samples: VecDeque<f32>,
}

struct AsioTrack {
    start: usize,
    count: usize,
    rate: u32,
    samples: VecDeque<f32>,
}

struct AsioMix {
    tracks: HashMap<String, AsioTrack>,
}

struct EngineState {
    wasapi: HashMap<String, StereoQueue>,
    asio: HashMap<String, AsioMix>,
}

struct Worker {
    wasapi_streams: HashMap<String, Stream>,
    asio_streams: HashMap<String, Stream>,
    state: Arc<Mutex<EngineState>>,
}

fn worker(rx: Receiver<Command>) {
    let mut worker = Worker {
        wasapi_streams: HashMap::new(),
        asio_streams: HashMap::new(),
        state: Arc::new(Mutex::new(EngineState {
            wasapi: HashMap::new(),
            asio: HashMap::new(),
        })),
    };
    while let Ok(command) = rx.recv() {
        match command {
            Command::Stop => break,
            Command::Bind { id, target, reply } => {
                let _ = reply.send(worker.bind(id, target));
            }
            Command::Unbind { id } => worker.unbind(&id),
            Command::Pcm { id, pcm, sample_rate, channels } => worker.push(&id, &pcm, sample_rate, channels),
        }
    }
}

impl Worker {
    fn bind(&mut self, id: String, target: SpeakerTarget) -> Result<(), String> {
        self.unbind(&id);
        match target {
            SpeakerTarget::Device { name } => self.bind_wasapi(id, &name),
            SpeakerTarget::Asio { name, start, count } => self.bind_asio(id, &name, start, count),
        }
    }

    fn bind_wasapi(&mut self, id: String, name: &str) -> Result<(), String> {
        let host = cpal::default_host();
        let device = output_device(&host, name)?;
        let (config, format) = stream_config(&device, 2)?;
        let rate = config.sample_rate.0;
        let channels = config.channels as usize;
        {
            let mut state = self.state.lock().unwrap_or_else(|err| err.into_inner());
            state.wasapi.insert(
                id.clone(),
                StereoQueue {
                    rate,
                    samples: VecDeque::new(),
                },
            );
        }
        let stream = match open_wasapi_stream(&device, &config, format, channels, Arc::clone(&self.state), id.clone()) {
            Ok(stream) => stream,
            Err(err) => {
                let mut state = self.state.lock().unwrap_or_else(|poison| poison.into_inner());
                state.wasapi.remove(&id);
                return Err(err);
            }
        };
        self.wasapi_streams.insert(id, stream);
        Ok(())
    }

    fn bind_asio(&mut self, id: String, name: &str, start: u16, count: u16) -> Result<(), String> {
        #[cfg(not(all(windows, feature = "asio")))]
        {
            let _ = (id, name, start, count);
            return Err("このビルドには ASIO が含まれていません".into());
        }
        #[cfg(all(windows, feature = "asio"))]
        {
            let host = asio_host().ok_or_else(|| "ASIO ホストを開けません。ASIO SDK 付きでビルドしてください".to_owned())?;
            let device = output_device(&host, name)?;
            let resolved = device.name().map_err(|err| err.to_string())?;
            let (config, format) = stream_config(&device, 0)?;
            let rate = config.sample_rate.0;
            let channels = config.channels as usize;
            let start = start.max(1) as usize - 1;
            let count = count.max(1) as usize;
            if start >= channels {
                return Err(format!("ASIO の開始チャンネルがデバイスの {channels} チャンネルを超えています"));
            }
            let mut created = false;
            {
                let mut state = self.state.lock().unwrap_or_else(|err| err.into_inner());
                let mix = state.asio.entry(resolved.clone()).or_insert_with(|| {
                    created = true;
                    AsioMix { tracks: HashMap::new() }
                });
                mix.tracks.insert(
                    id,
                    AsioTrack {
                        start,
                        count,
                        rate,
                        samples: VecDeque::new(),
                    },
                );
            }
            if created {
                match open_asio_stream(&device, &config, format, channels, Arc::clone(&self.state), resolved.clone()) {
                    Ok(stream) => self.asio_streams.insert(resolved, stream),
                    Err(err) => {
                        let mut state = self.state.lock().unwrap_or_else(|poison| poison.into_inner());
                        state.asio.remove(&resolved);
                        return Err(err);
                    }
                };
            }
            Ok(())
        }
    }

    fn unbind(&mut self, id: &str) {
        self.wasapi_streams.remove(id);
        let mut state = self.state.lock().unwrap_or_else(|err| err.into_inner());
        state.wasapi.remove(id);
        let mut empty_device = None;
        for (device, mix) in &mut state.asio {
            if mix.tracks.remove(id).is_some() && mix.tracks.is_empty() {
                empty_device = Some(device.clone());
                break;
            }
        }
        if let Some(device) = empty_device {
            state.asio.remove(&device);
            drop(state);
            self.asio_streams.remove(&device);
        }
    }

    fn push(&mut self, id: &str, pcm: &[i16], sample_rate: u32, channels: u16) {
        let stereo = pcm_to_stereo(pcm, channels);
        if stereo.is_empty() {
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(|err| err.into_inner());
        if let Some(queue) = state.wasapi.get_mut(id) {
            let converted = resample_stereo(&stereo, sample_rate, queue.rate);
            push_stereo(&mut queue.samples, &converted, ring_capacity(queue.rate));
            return;
        }
        for mix in state.asio.values_mut() {
            if let Some(track) = mix.tracks.get_mut(id) {
                let converted = resample_stereo(&stereo, sample_rate, track.rate);
                push_stereo(&mut track.samples, &converted, ring_capacity(track.rate));
                return;
            }
        }
    }
}

fn list_host(host_name: &'static str, host: &cpal::Host) -> Vec<ListedDevice> {
    let Ok(devices) = host.output_devices() else {
        return Vec::new();
    };
    devices
        .filter_map(|device| {
            let name = device.name().ok()?;
            let channels = device.default_output_config().map(|config| config.channels()).unwrap_or(2);
            Some(ListedDevice { name, host: host_name, channels })
        })
        .collect()
}

fn output_device(host: &cpal::Host, name: &str) -> Result<cpal::Device, String> {
    if name.is_empty() {
        return host.default_output_device().ok_or_else(|| "再生デバイスがありません".to_owned());
    }
    let mut devices = host.output_devices().map_err(|err| err.to_string())?;
    devices
        .find(|device| device.name().ok().as_deref() == Some(name))
        .ok_or_else(|| format!("再生デバイスが見つかりません: {name}"))
}

fn stream_config(device: &cpal::Device, channels: u16) -> Result<(StreamConfig, SampleFormat), String> {
    let default = device.default_output_config().map_err(|err| err.to_string())?;
    let want = if channels == 0 { default.channels() } else { channels };
    if let Ok(ranges) = device.supported_output_configs() {
        for range in ranges {
            if range.channels() != want || range.sample_format() != SampleFormat::F32 {
                continue;
            }
            let max = range.max_sample_rate().0;
            let min = range.min_sample_rate().0;
            let rate = if (min..=max).contains(&48_000) { 48_000 } else { default.sample_rate().0.clamp(min, max) };
            let supported = range.with_sample_rate(SampleRate(rate));
            return Ok((supported.into(), SampleFormat::F32));
        }
    }
    Ok((default.config(), default.sample_format()))
}

fn open_wasapi_stream(
    device: &cpal::Device,
    config: &StreamConfig,
    format: SampleFormat,
    channels: usize,
    state: Arc<Mutex<EngineState>>,
    id: String,
) -> Result<Stream, String> {
    let stream = match format {
        SampleFormat::F32 => device.build_output_stream(
            config,
            move |data: &mut [f32], _| fill_wasapi(&state, &id, data, channels),
            |err| tracing::warn!(%err, "playback stream"),
            None,
        ),
        SampleFormat::I16 => {
            let state = Arc::clone(&state);
            device.build_output_stream(
                config,
                move |data: &mut [i16], _| {
                    let mut float = vec![0.0f32; data.len()];
                    fill_wasapi(&state, &id, &mut float, channels);
                    for (dest, sample) in data.iter_mut().zip(float) {
                        *dest = (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                    }
                },
                |err| tracing::warn!(%err, "playback stream"),
                None,
            )
        }
        other => return Err(format!("再生デバイスのサンプル形式 {other:?} には対応していません")),
    };
    let stream = stream.map_err(|err| err.to_string())?;
    stream.play().map_err(|err| err.to_string())?;
    Ok(stream)
}

#[cfg_attr(not(all(windows, feature = "asio")), allow(dead_code))]
fn open_asio_stream(
    device: &cpal::Device,
    config: &StreamConfig,
    format: SampleFormat,
    channels: usize,
    state: Arc<Mutex<EngineState>>,
    device_name: String,
) -> Result<Stream, String> {
    let stream = match format {
        SampleFormat::F32 => device.build_output_stream(
            config,
            move |data: &mut [f32], _| fill_asio(&state, &device_name, data, channels),
            |err| tracing::warn!(%err, "ASIO stream"),
            None,
        ),
        SampleFormat::I16 => {
            let state = Arc::clone(&state);
            device.build_output_stream(
                config,
                move |data: &mut [i16], _| {
                    let mut float = vec![0.0f32; data.len()];
                    fill_asio(&state, &device_name, &mut float, channels);
                    for (dest, sample) in data.iter_mut().zip(float) {
                        *dest = (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                    }
                },
                |err| tracing::warn!(%err, "ASIO stream"),
                None,
            )
        }
        other => return Err(format!("ASIO のサンプル形式 {other:?} には対応していません")),
    };
    let stream = stream.map_err(|err| err.to_string())?;
    stream.play().map_err(|err| err.to_string())?;
    Ok(stream)
}

fn fill_wasapi(state: &Mutex<EngineState>, id: &str, output: &mut [f32], channels: usize) {
    let channels = channels.max(1);
    let mut guard = state.lock().unwrap_or_else(|err| err.into_inner());
    let Some(queue) = guard.wasapi.get_mut(id) else {
        output.fill(0.0);
        return;
    };
    let frames = output.len() / channels;
    for frame in 0..frames {
        let left = queue.samples.pop_front().unwrap_or(0.0);
        let right = queue.samples.pop_front().unwrap_or(0.0);
        let base = frame * channels;
        output[base] = left;
        if channels > 1 {
            output[base + 1] = right;
        }
        for channel in 2..channels {
            output[base + channel] = 0.0;
        }
    }
}

#[cfg_attr(not(all(windows, feature = "asio")), allow(dead_code))]
fn fill_asio(state: &Mutex<EngineState>, device: &str, output: &mut [f32], channels: usize) {
    output.fill(0.0);
    if channels == 0 {
        return;
    }
    let mut guard = state.lock().unwrap_or_else(|err| err.into_inner());
    let Some(mix) = guard.asio.get_mut(device) else {
        return;
    };
    let frames = output.len() / channels;
    for frame in 0..frames {
        for track in mix.tracks.values_mut() {
            let left = track.samples.pop_front().unwrap_or(0.0);
            let right = track.samples.pop_front().unwrap_or(0.0);
            mix_stereo_into(output, channels, frame, track.start, track.count, left, right);
        }
    }
}

fn ring_capacity(rate: u32) -> usize {
    (rate.max(1) as usize).saturating_mul(2).saturating_mul(RING_SECONDS as usize)
}

fn push_stereo(samples: &mut VecDeque<f32>, pcm: &[f32], capacity: usize) {
    samples.extend(pcm.iter().copied());
    let overflow = samples.len().saturating_sub(capacity);
    if overflow > 0 {
        let drop_n = overflow + (overflow % 2);
        let drop_n = drop_n.min(samples.len());
        samples.drain(0..drop_n);
    }
}

fn pcm_to_stereo(pcm: &[i16], channels: u16) -> Vec<f32> {
    let channels = channels.max(1) as usize;
    let scale = i16::MAX as f32;
    let mut out = Vec::with_capacity(pcm.len() / channels * 2);
    for frame in pcm.chunks_exact(channels) {
        let left = frame[0] as f32 / scale;
        let right = if channels == 1 { left } else { frame[1] as f32 / scale };
        out.push(left);
        out.push(right);
    }
    out
}

fn resample_stereo(input: &[f32], in_rate: u32, out_rate: u32) -> Vec<f32> {
    if input.len() < 2 || in_rate == 0 || out_rate == 0 {
        return Vec::new();
    }
    if in_rate == out_rate {
        return input.to_vec();
    }
    let in_frames = input.len() / 2;
    let out_frames = ((in_frames as u64 * u64::from(out_rate)) / u64::from(in_rate)).max(1) as usize;
    let mut out = Vec::with_capacity(out_frames * 2);
    for frame in 0..out_frames {
        let position = frame as f64 * f64::from(in_rate) / f64::from(out_rate);
        let index = (position.floor() as usize).min(in_frames - 1);
        let next = (index + 1).min(in_frames - 1);
        let frac = (position - index as f64) as f32;
        for channel in 0..2 {
            let start = input[index * 2 + channel];
            let end = input[next * 2 + channel];
            out.push(start + (end - start) * frac);
        }
    }
    out
}

pub fn mix_stereo_into(output: &mut [f32], channels: usize, frame: usize, start: usize, count: usize, left: f32, right: f32) {
    if channels == 0 || count == 0 || start >= channels {
        return;
    }
    let base = frame.saturating_mul(channels);
    for offset in 0..count {
        let dest = start + offset;
        if dest >= channels || base + dest >= output.len() {
            break;
        }
        let sample = match offset {
            0 => left,
            1 => right,
            _ => 0.0,
        };
        output[base + dest] = (output[base + dest] + sample).clamp(-1.0, 1.0);
    }
}

#[cfg(all(windows, feature = "asio"))]
fn asio_host() -> Option<cpal::Host> {
    cpal::host_from_id(cpal::HostId::Asio).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asio_range_writes_only_its_channels() {
        let mut output = vec![0.0; 8];
        mix_stereo_into(&mut output, 4, 0, 2, 2, 0.5, -0.25);
        mix_stereo_into(&mut output, 4, 1, 0, 2, 0.1, 0.2);
        assert_eq!(output, vec![0.0, 0.0, 0.5, -0.25, 0.1, 0.2, 0.0, 0.0]);
    }

    #[test]
    fn overlapping_asio_ranges_mix() {
        let mut output = vec![0.0; 2];
        mix_stereo_into(&mut output, 2, 0, 0, 2, 0.4, 0.4);
        mix_stereo_into(&mut output, 2, 0, 0, 2, 0.4, 0.4);
        assert_eq!(output, vec![0.8, 0.8]);
    }

    #[test]
    fn same_rate_is_copied() {
        let input = vec![0.0, 1.0, 0.5, -0.5];
        assert_eq!(resample_stereo(&input, 48_000, 48_000), input);
    }
}
