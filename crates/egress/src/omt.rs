use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use openmediatransport::types::{Codec, ColorSpace, FrameType, MediaFrame};
use openmediatransport::{Discovery, Sender};

use crate::error::EgressError;
use crate::{AudioPublisher, VideoPublisher};

const VIDEO: FrameType = FrameType(2);
const AUDIO: FrameType = FrameType(4);
const AUDIO_AND_VIDEO: FrameType = FrameType(6);

pub struct OmtPublisher {
    sender: Arc<Mutex<Sender>>,
    audio: openmediatransport::AudioIngress,
    discovery: Arc<Mutex<Discovery>>,
    name: String,
    stop: Arc<AtomicBool>,
    poll: Option<JoinHandle<()>>,
    last_discard_log: Option<Instant>,
    receiver_logged: bool,
}

impl OmtPublisher {
    pub fn open(name: &str, video: bool, audio: bool) -> Result<Self, EgressError> {
        let frame_types = match (video, audio) {
            (true, true) => AUDIO_AND_VIDEO,
            (true, false) => VIDEO,
            (false, true) => AUDIO,
            (false, false) => {
                return Err(EgressError::InvalidFrame(
                    "OMT source needs video, audio, or both",
                ))
            }
        };
        let sender = Sender::create(name, frame_types).map_err(|err| EgressError::Omt(err.to_string()))?;
        let port = sender.port();
        let audio_ingress = sender.audio_ingress();
        let mut discovery = Discovery::new().map_err(|err| EgressError::Omt(err.to_string()))?;
        discovery
            .register(name, port)
            .map_err(|err| EgressError::Omt(err.to_string()))?;
        tracing::info!(name, port, "OMT source registered");

        let sender = Arc::new(Mutex::new(sender));
        let discovery = Arc::new(Mutex::new(discovery));
        let stop = Arc::new(AtomicBool::new(false));
        let poll_sender = Arc::clone(&sender);
        let poll_stop = Arc::clone(&stop);
        let poll = thread::Builder::new()
            .name(format!("omt-{name}"))
            .spawn(move || {
                while !poll_stop.load(Ordering::Relaxed) {
                    if let Ok(mut sender) = poll_sender.lock() {
                        if let Err(err) = sender.poll_accept() {
                            tracing::debug!(%err, "OMT accept");
                        }
                        if let Err(err) = sender.poll_peer_metadata() {
                            tracing::debug!(%err, "OMT metadata");
                        }
                        sender.enable_audio_output();
                    }
                    thread::sleep(Duration::from_millis(20));
                }
            })
            .map_err(|err| EgressError::Omt(err.to_string()))?;

        Ok(Self {
            sender,
            audio: audio_ingress,
            discovery,
            name: name.to_owned(),
            stop,
            poll: Some(poll),
            last_discard_log: None,
            receiver_logged: false,
        })
    }
}

impl Drop for OmtPublisher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.poll.take() {
            let _ = handle.join();
        }
        if let Ok(mut discovery) = self.discovery.lock() {
            let _ = discovery.deregister(&self.name);
        }
        if let Ok(mut sender) = self.sender.lock() {
            sender.close();
        }
    }
}

impl VideoPublisher for OmtPublisher {
    fn publish_video(
        &mut self,
        bgra: &[u8],
        width: u32,
        height: u32,
        frame_rate: (u32, u32),
    ) -> Result<(), EgressError> {
        let expected = (width as usize)
            .saturating_mul(height as usize)
            .saturating_mul(4);
        if bgra.len() < expected || width == 0 || height == 0 {
            return Err(EgressError::InvalidFrame("BGRA buffer does not match dimensions"));
        }
        let frame = MediaFrame {
            frame_type: VIDEO,
            codec: Codec::Bgra.as_i32(),
            width: width as i32,
            height: height as i32,
            stride: (width * 4) as i32,
            frame_rate_n: frame_rate.0 as i32,
            frame_rate_d: frame_rate.1.max(1) as i32,
            aspect_ratio: width as f32 / height as f32,
            color_space: ColorSpace::Bt709,
            timestamp: timestamp_100ns(),
            data: bgra[..expected].to_vec(),
            ..MediaFrame::default()
        };
        let mut sender = self
            .sender
            .lock()
            .map_err(|_| EgressError::Omt("sender lock poisoned".into()))?;
        sender
            .send_video(frame)
            .map_err(|err| EgressError::Omt(err.to_string()))?;
        Ok(())
    }
}

impl AudioPublisher for OmtPublisher {
    fn publish_audio(
        &mut self,
        pcm: &[i16],
        sample_rate: u32,
        channels: u16,
    ) -> Result<(), EgressError> {
        if channels == 0 || sample_rate == 0 {
            return Err(EgressError::InvalidFrame("audio rate or channels is zero"));
        }
        let channels_usize = channels as usize;
        if pcm.len() % channels_usize != 0 {
            return Err(EgressError::InvalidFrame("PCM length is not a multiple of channels"));
        }
        let samples = pcm.len() / channels_usize;
        let planar = interleaved_i16_to_planar_f32(pcm, channels_usize);
        let frame = MediaFrame {
            frame_type: AUDIO,
            codec: Codec::Fpa1.as_i32(),
            sample_rate: sample_rate as i32,
            channels: channels as i32,
            samples_per_channel: samples as i32,
            timestamp: timestamp_100ns(),
            data: planar,
            ..MediaFrame::default()
        };
        let peers = {
            let mut sender = self
                .sender
                .lock()
                .map_err(|_| EgressError::Omt("sender lock poisoned".into()))?;
            sender.enable_audio_output();
            sender.connection_count()
        };
        self.audio
            .send(frame)
            .map_err(|err| EgressError::Omt(err.to_string()))?;
        if peers == 0 {
            self.receiver_logged = false;
            let audible = pcm.iter().any(|sample| sample.saturating_abs() > 32);
            let due = self
                .last_discard_log
                .map(|at| at.elapsed() >= Duration::from_secs(2))
                .unwrap_or(true);
            if audible && due {
                self.last_discard_log = Some(Instant::now());
                tracing::info!(
                    name = %self.name,
                    "OMT audio PCM arrived but no receiver is connected, so the frame is discarded"
                );
            }
        } else if !self.receiver_logged {
            self.receiver_logged = true;
            tracing::info!(name = %self.name, peers, "OMT audio receiver connected");
        }
        Ok(())
    }
}

fn timestamp_100ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as i64 / 100)
        .unwrap_or(0)
}

fn interleaved_i16_to_planar_f32(pcm: &[i16], channels: usize) -> Vec<u8> {
    let samples = pcm.len() / channels;
    let mut planar = vec![0.0f32; samples * channels];
    for (index, sample) in pcm.iter().enumerate() {
        let channel = index % channels;
        let sample_index = index / channels;
        planar[channel * samples + sample_index] = *sample as f32 / i16::MAX as f32;
    }
    let mut bytes = Vec::with_capacity(planar.len() * 4);
    for sample in planar {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    bytes
}
