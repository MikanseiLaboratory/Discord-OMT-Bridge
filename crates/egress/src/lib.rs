mod error;
mod fanout;
mod frame;
mod ndi;
mod omt;

pub use error::EgressError;
pub use fanout::{FanoutPublisher, SourceMode};
pub use frame::{color_bars, sine_tone};

use std::time::Duration;

/// Publish uncompressed BGRA video.
pub trait VideoPublisher {
    fn publish_video(
        &mut self,
        bgra: &[u8],
        width: u32,
        height: u32,
        frame_rate: (u32, u32),
    ) -> Result<(), EgressError>;
}

/// Publish interleaved PCM s16 audio.
pub trait AudioPublisher {
    fn publish_audio(
        &mut self,
        pcm: &[i16],
        sample_rate: u32,
        channels: u16,
    ) -> Result<(), EgressError>;
}

/// Run a moving color bar and a 440 Hz tone on one LAN source.
pub fn run_colorbar(
    name: &str,
    enable_omt: bool,
    enable_ndi: bool,
    duration: Duration,
) -> Result<(), EgressError> {
    if !enable_omt && !enable_ndi {
        return Err(EgressError::InvalidFrame(
            "enable OMT, NDI, or both",
        ));
    }
    let mut publisher = FanoutPublisher::open(name, SourceMode::AudioAndVideo, enable_omt, enable_ndi)?;
    let width = 1280u32;
    let height = 720u32;
    let fps = 30u32;
    let frame_interval = Duration::from_secs_f64(1.0 / f64::from(fps));
    let audio_samples = (48_000 / fps) as usize;
    let mut phase = 0.0f32;
    let started = std::time::Instant::now();
    let mut frame_index = 0u64;
    tracing::info!(
        name,
        enable_omt,
        enable_ndi,
        ?duration,
        "publishing color bars"
    );
    while started.elapsed() < duration {
        let tick = std::time::Instant::now();
        let pixels = color_bars(width, height, frame_index);
        publisher.publish_video(&pixels, width, height, (fps, 1))?;
        let tone = sine_tone(audio_samples, 48_000, 440.0, &mut phase);
        publisher.publish_audio(&tone, 48_000, 2)?;
        frame_index += 1;
        let spent = tick.elapsed();
        if spent < frame_interval {
            std::thread::sleep(frame_interval - spent);
        }
    }
    tracing::info!(frames = frame_index, "color bar finished");
    Ok(())
}
