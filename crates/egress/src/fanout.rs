use crate::error::EgressError;
use crate::ndi::NdiPublisher;
use crate::omt::OmtPublisher;
use crate::{AudioPublisher, VideoPublisher};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceMode {
    AudioOnly,
    VideoOnly,
    AudioAndVideo,
}

pub struct FanoutPublisher {
    omt: Option<OmtPublisher>,
    ndi: Option<NdiPublisher>,
}

impl FanoutPublisher {
    pub fn open(
        name: &str,
        mode: SourceMode,
        enable_omt: bool,
        enable_ndi: bool,
    ) -> Result<Self, EgressError> {
        if !enable_omt && !enable_ndi {
            return Err(EgressError::InvalidFrame("no egress transport enabled"));
        }
        let video = matches!(mode, SourceMode::VideoOnly | SourceMode::AudioAndVideo);
        let audio = matches!(mode, SourceMode::AudioOnly | SourceMode::AudioAndVideo);
        let omt = if enable_omt {
            Some(OmtPublisher::open(name, video, audio)?)
        } else {
            None
        };
        let ndi = if enable_ndi {
            Some(NdiPublisher::open(name, video, audio)?)
        } else {
            None
        };
        Ok(Self { omt, ndi })
    }
}

impl VideoPublisher for FanoutPublisher {
    fn publish_video(
        &mut self,
        bgra: &[u8],
        width: u32,
        height: u32,
        frame_rate: (u32, u32),
    ) -> Result<(), EgressError> {
        if let Some(publisher) = &mut self.omt {
            publisher.publish_video(bgra, width, height, frame_rate)?;
        }
        if let Some(publisher) = &mut self.ndi {
            publisher.publish_video(bgra, width, height, frame_rate)?;
        }
        Ok(())
    }
}

impl AudioPublisher for FanoutPublisher {
    fn publish_audio(
        &mut self,
        pcm: &[i16],
        sample_rate: u32,
        channels: u16,
    ) -> Result<(), EgressError> {
        if let Some(publisher) = &mut self.omt {
            publisher.publish_audio(pcm, sample_rate, channels)?;
        }
        if let Some(publisher) = &mut self.ndi {
            publisher.publish_audio(pcm, sample_rate, channels)?;
        }
        Ok(())
    }
}
