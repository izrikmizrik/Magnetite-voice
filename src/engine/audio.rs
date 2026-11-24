//! The audio backend behind a trait, so the same engine runs everywhere:
//!
//! - wasm (and any build without `pc-audio`): [`MockAudio`] — the sandbox has no microphone
//!   API yet. When the platform adds the microphone hook to mod-API (plus its permission:
//!   `NSMicrophoneUsageDescription` is already in the macOS launcher's Info.plist,
//!   Android's `RECORD_AUDIO` comes with it), the hook's frames plug in at `MockAudio`'s
//!   marked spot below and the encoder/mixer start working over the relay.
//! - native PC with `pc-audio`: [`CpalAudio`] — cpal 0.16 capture/playback. The device
//!   callbacks only move samples into/out of ring buffers; resampling, framing, coding and
//!   mixing happen in the game tick, so no mod thread is spawned here.

use crate::engine::jitter::{FRAME_SAMPLES, SAMPLE_RATE};

/// What the engine needs from an audio backend.
pub trait AudioBackend: Send + Sync {
    /// Drains the captured mono samples (at [`AudioBackend::input_rate`]).
    fn drain_capture(&self) -> Vec<f32>;
    /// The capture rate in Hz.
    fn input_rate(&self) -> u32;
    /// Queues one mixed 20 ms stereo frame at 48 kHz.
    fn push_playback(&self, frame: &[[f32; 2]; FRAME_SAMPLES]);
}

/// The wasm / no-`pc-audio` backend: silence in, silence out. The relay transport and the
/// whole engine still run; only samples are missing.
pub struct MockAudio;

impl AudioBackend for MockAudio {
    fn drain_capture(&self) -> Vec<f32> {
        Vec::new()
    }
    fn input_rate(&self) -> u32 {
        SAMPLE_RATE
    }
    fn push_playback(&self, _frame: &[[f32; 2]; FRAME_SAMPLES]) {}
}

/// Opens the backend for this build: cpal on native with `pc-audio`, the mock elsewhere.
pub fn open(_input_name: Option<&str>, _output_name: Option<&str>) -> Box<dyn AudioBackend> {
    #[cfg(all(feature = "pc-audio", not(target_arch = "wasm32")))]
    {
        match CpalAudio::open(_input_name, _output_name) {
            Ok(audio) => Box::new(audio),
            Err(e) => {
                eprintln!("mvchat: no audio backend ({e}); running silent");
                Box::new(MockAudio)
            }
        }
    }
    #[cfg(not(all(feature = "pc-audio", not(target_arch = "wasm32"))))]
    {
        // TODO(mic-hook): when mod-API grows the microphone hook + permission, replace this
        // with a backend that reads the hook's frames (and plays back through
        // `Host::push_voice`, already present inside the game) instead of returning silence.
        Box::new(MockAudio)
    }
}

// ---------------------------------------------------------------------------
// cpal backend (native, feature pc-audio only)
// ---------------------------------------------------------------------------

#[cfg(all(feature = "pc-audio", not(target_arch = "wasm32")))]
mod cpal_backend {
    use super::{AudioBackend, FRAME_SAMPLES, SAMPLE_RATE};
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    type Capture = Arc<Mutex<VecDeque<f32>>>;
    type Playback = Arc<Mutex<VecDeque<[f32; 2]>>>;

    pub struct CpalAudio {
        // Held so the streams stay alive; dropped when the engine stops.
        #[allow(dead_code)]
        input: Option<cpal::Stream>,
        #[allow(dead_code)]
        output: Option<cpal::Stream>,
        capture: Capture,
        playback: Playback,
        input_rate: u32,
    }

    /// Lists input device names for the Settings tab.
    pub fn input_device_names() -> Vec<String> {
        let host = cpal::default_host();
        host.input_devices()
            .map(|devices| devices.filter_map(|d| d.name().ok()).collect())
            .unwrap_or_default()
    }

    /// Lists output device names for the Settings tab.
    pub fn output_device_names() -> Vec<String> {
        let host = cpal::default_host();
        host.output_devices()
            .map(|devices| devices.filter_map(|d| d.name().ok()).collect())
            .unwrap_or_default()
    }

    impl CpalAudio {
        pub fn open(input_name: Option<&str>, output_name: Option<&str>) -> Result<Self, String> {
            let host = cpal::default_host();

            let input_dev = match input_name {
                Some(name) => host
                    .input_devices()
                    .map_err(|e| e.to_string())?
                    .find(|d| d.name().map(|n| n == name).unwrap_or(false))
                    .ok_or_else(|| format!("input device {name:?} not found"))?,
                None => host.default_input_device().ok_or("no input device")?,
            };
            let in_supported = input_dev
                .default_input_config()
                .map_err(|e| e.to_string())?;
            let input_rate = in_supported.sample_rate().0;
            let in_channels = in_supported.channels().max(1);
            let in_config = in_supported.config();

            let capture: Capture =
                Arc::new(Mutex::new(VecDeque::with_capacity(SAMPLE_RATE as usize)));
            let capture_cb = capture.clone();
            let input = input_dev
                .build_input_stream::<f32, _, _>(
                    &in_config,
                    move |data: &[f32], _| {
                        let mut q = capture_cb.lock().unwrap();
                        let ch = in_channels as usize;
                        for frame in data.chunks(ch) {
                            let mono = frame.iter().sum::<f32>() / frame.len().max(1) as f32;
                            q.push_back(mono);
                        }
                        while q.len() > SAMPLE_RATE as usize {
                            q.pop_front();
                        }
                    },
                    |err| {
                        eprintln!("mvchat: input stream error: {err}");
                    },
                    None,
                )
                .map_err(|e| format!("build input stream: {e}"))?;
            input.play().map_err(|e| e.to_string())?;

            let output_dev = match output_name {
                Some(name) => host
                    .output_devices()
                    .map_err(|e| e.to_string())?
                    .find(|d| d.name().map(|n| n == name).unwrap_or(false))
                    .ok_or_else(|| format!("output device {name:?} not found"))?,
                None => host.default_output_device().ok_or("no output device")?,
            };
            let out_supported = output_dev
                .default_output_config()
                .map_err(|e| e.to_string())?;
            let output_rate = out_supported.sample_rate().0;
            let output_channels = out_supported.channels().max(1);
            let out_config = out_supported.config();

            let playback: Playback =
                Arc::new(Mutex::new(VecDeque::with_capacity(FRAME_SAMPLES * 8)));
            let playback_cb = playback.clone();
            let out_resampler = Arc::new(Mutex::new((
                crate::engine::jitter::Resampler::default(),
                crate::engine::jitter::Resampler::default(),
            )));
            let resampler_cb = out_resampler.clone();
            let output = output_dev
                .build_output_stream::<f32, _, _>(
                    &out_config,
                    move |data: &mut [f32], _| {
                        let ch = output_channels as usize;
                        let frames = data.len() / ch;
                        let want = frames as f64 * SAMPLE_RATE as f64 / output_rate as f64;
                        let mut l_in = Vec::with_capacity(frames + 8);
                        let mut r_in = Vec::with_capacity(frames + 8);
                        {
                            let mut q = playback_cb.lock().unwrap();
                            while q.len() < (want.ceil() as usize) + 2 && !q.is_empty() {
                                let [l, r] = q.pop_front().unwrap_or([0.0, 0.0]);
                                l_in.push(l);
                                r_in.push(r);
                            }
                            while q.len() > FRAME_SAMPLES * 16 {
                                q.pop_front();
                            }
                        }
                        let mut rs = resampler_cb.lock().unwrap();
                        let l_out = rs.0.resample(&l_in, SAMPLE_RATE);
                        let r_out = rs.1.resample(&r_in, SAMPLE_RATE);
                        for i in 0..frames {
                            let l = l_out.get(i).copied().unwrap_or(0.0);
                            let r = r_out.get(i).copied().unwrap_or(0.0);
                            for c in 0..ch {
                                data[i * ch + c] = if c == 0 {
                                    l
                                } else if c == 1 {
                                    r
                                } else {
                                    (l + r) * 0.5
                                };
                            }
                        }
                    },
                    |err| {
                        eprintln!("mvchat: output stream error: {err}");
                    },
                    None,
                )
                .map_err(|e| format!("build output stream: {e}"))?;
            output.play().map_err(|e| e.to_string())?;

            Ok(Self {
                input: Some(input),
                output: Some(output),
                capture,
                playback,
                input_rate,
            })
        }
    }

    impl AudioBackend for CpalAudio {
        fn drain_capture(&self) -> Vec<f32> {
            let mut q = self.capture.lock().unwrap();
            q.drain(..).collect()
        }
        fn input_rate(&self) -> u32 {
            self.input_rate
        }
        fn push_playback(&self, frame: &[[f32; 2]; FRAME_SAMPLES]) {
            let mut q = self.playback.lock().unwrap();
            for pair in frame.iter() {
                q.push_back(*pair);
            }
            while q.len() > FRAME_SAMPLES * 16 {
                q.pop_front();
            }
        }
    }
}

#[cfg(all(feature = "pc-audio", not(target_arch = "wasm32")))]
pub use cpal_backend::{CpalAudio, input_device_names, output_device_names};
#[cfg(not(all(feature = "pc-audio", not(target_arch = "wasm32"))))]
pub fn input_device_names() -> Vec<String> {
    Vec::new()
}
#[cfg(not(all(feature = "pc-audio", not(target_arch = "wasm32"))))]
pub fn output_device_names() -> Vec<String> {
    Vec::new()
}
