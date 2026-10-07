/// BGRA color bars plus a sine tone, used to prove LAN egress without Discord.
pub fn color_bars(width: u32, height: u32, frame_index: u64) -> Vec<u8> {
    let width = width.max(1);
    let height = height.max(1);
    let mut pixels = vec![0u8; (width * height * 4) as usize];
    let colors: [[u8; 4]; 8] = [
        [255, 255, 255, 255],
        [0, 255, 255, 255],
        [255, 255, 0, 255],
        [0, 255, 0, 255],
        [255, 0, 255, 255],
        [0, 0, 255, 255],
        [255, 0, 0, 255],
        [0, 0, 0, 255],
    ];
    let bar = (width / 8).max(1);
    let shift = (frame_index % width as u64) as u32;
    for y in 0..height {
        for x in 0..width {
            let shifted = (x + shift) % width;
            let idx = (shifted / bar).min(7) as usize;
            let offset = ((y * width + x) * 4) as usize;
            pixels[offset..offset + 4].copy_from_slice(&colors[idx]);
        }
    }
    pixels
}

/// Interleaved stereo i16 sine. `phase` is in radians and is advanced by the caller.
pub fn sine_tone(samples: usize, sample_rate: u32, frequency_hz: f32, phase: &mut f32) -> Vec<i16> {
    let sample_rate = sample_rate.max(1) as f32;
    let mut pcm = Vec::with_capacity(samples * 2);
    let step = std::f32::consts::TAU * frequency_hz / sample_rate;
    for _ in 0..samples {
        let sample = (*phase).sin();
        let value = (sample * 0.2 * i16::MAX as f32) as i16;
        pcm.push(value);
        pcm.push(value);
        *phase += step;
        if *phase > std::f32::consts::TAU {
            *phase -= std::f32::consts::TAU;
        }
    }
    pcm
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_bars_have_bgra_size() {
        let frame = color_bars(16, 8, 0);
        assert_eq!(frame.len(), 16 * 8 * 4);
        assert_eq!(&frame[0..4], &[255, 255, 255, 255]);
    }

    #[test]
    fn sine_tone_is_stereo() {
        let mut phase = 0.0;
        let pcm = sine_tone(48, 48_000, 440.0, &mut phase);
        assert_eq!(pcm.len(), 96);
        assert!(phase > 0.0);
    }
}
