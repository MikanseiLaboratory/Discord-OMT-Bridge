use crate::error::EgressError;
use crate::{AudioPublisher, VideoPublisher};

#[cfg(feature = "ndi")]
mod imp {
    use super::*;
    use grafton_ndi::{AudioFrame, PixelFormat, Sender, SenderOptions, VideoFrame, NDI};

    pub struct NdiPublisher {
        sender: Sender,
        _ndi: NDI,
    }

    impl NdiPublisher {
        pub fn open(name: &str) -> Result<Self, EgressError> {
            let ndi = NDI::new().map_err(|err| EgressError::Ndi(err.to_string()))?;
            let options = SenderOptions::builder(name)
                .clock_video(false)
                .clock_audio(false)
                .build();
            let sender = Sender::new(&ndi, &options).map_err(|err| EgressError::Ndi(err.to_string()))?;
            tracing::info!(name, "NDI source published");
            Ok(Self { sender, _ndi: ndi })
        }
    }

    impl VideoPublisher for NdiPublisher {
        fn publish_video(
            &mut self,
            bgra: &[u8],
            width: u32,
            height: u32,
            frame_rate: (u32, u32),
        ) -> Result<(), EgressError> {
            let mut frame = VideoFrame::builder()
                .resolution(width, height)
                .pixel_format(PixelFormat::BGRA)
                .frame_rate(frame_rate.0.max(1), frame_rate.1.max(1))
                .build()
                .map_err(|err| EgressError::Ndi(err.to_string()))?;
            let expected = (width as usize)
                .saturating_mul(height as usize)
                .saturating_mul(4);
            if bgra.len() < expected {
                return Err(EgressError::InvalidFrame("BGRA buffer is short"));
            }
            frame
                .data_mut()
                .get_mut(..expected)
                .ok_or(EgressError::InvalidFrame("NDI video buffer is short"))?
                .copy_from_slice(&bgra[..expected]);
            self.sender.send_video(&frame);
            Ok(())
        }
    }

    impl AudioPublisher for NdiPublisher {
        fn publish_audio(
            &mut self,
            pcm: &[i16],
            sample_rate: u32,
            channels: u16,
        ) -> Result<(), EgressError> {
            if channels == 0 || pcm.len() % channels as usize != 0 {
                return Err(EgressError::InvalidFrame("invalid PCM layout"));
            }
            let samples = (pcm.len() / channels as usize) as u32;
            let data: Vec<f32> = pcm.iter().map(|sample| *sample as f32 / i16::MAX as f32).collect();
            let frame = AudioFrame::builder()
                .sample_rate(sample_rate)
                .channels(channels as u32)
                .samples(samples)
                .data(data)
                .build()
                .map_err(|err| EgressError::Ndi(err.to_string()))?;
            self.sender.send_audio(&frame);
            Ok(())
        }
    }
}

#[cfg(not(feature = "ndi"))]
mod imp {
    use super::*;

    pub struct NdiPublisher;

    impl NdiPublisher {
        pub fn open(_name: &str) -> Result<Self, EgressError> {
            Err(EgressError::Ndi(
                "rebuild with the `ndi` feature and an installed NDI 6 SDK".into(),
            ))
        }
    }

    impl VideoPublisher for NdiPublisher {
        fn publish_video(
            &mut self,
            _bgra: &[u8],
            _width: u32,
            _height: u32,
            _frame_rate: (u32, u32),
        ) -> Result<(), EgressError> {
            Err(EgressError::Ndi("NDI support is not compiled in".into()))
        }
    }

    impl AudioPublisher for NdiPublisher {
        fn publish_audio(
            &mut self,
            _pcm: &[i16],
            _sample_rate: u32,
            _channels: u16,
        ) -> Result<(), EgressError> {
            Err(EgressError::Ndi("NDI support is not compiled in".into()))
        }
    }
}

pub use imp::NdiPublisher;
