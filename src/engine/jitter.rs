//! Receive side of the voice pipeline: one jitter buffer per speaker, and the mixer that
//! renders all speakers into one stereo frame with distance falloff and constant-power pan.
//!
//! Pure module, no I/O: the game tick pushes decoded Opus frames in, the audio path pulls
//! mixed stereo frames out.

pub use crate::engine::protocol::Pose;
use std::collections::BTreeMap;

/// Samples per 20 ms frame at 48 kHz mono.
pub const FRAME_SAMPLES: usize = 960;
/// Sample rate of the pipeline.
pub const SAMPLE_RATE: u32 = 48_000;
/// Target jitter buffer delay: 2 frames (40 ms).
pub const TARGET_DELAY_FRAMES: u64 = 2;
/// Reorder window: how many packets ahead of the play head may arrive out of order.
pub const REORDER_WINDOW: u64 = 8;

/// One speaker's jitter buffer. Frames are keyed by the sender's packet counter.
#[derive(Clone, Debug, Default)]
pub struct JitterBuffer {
    /// counter -> frame (encoded Opus bytes; the decoder is owned by the consumer).
    pending: BTreeMap<u64, Vec<u8>>,
    /// Next counter the play head will emit.
    next: u64,
    init: bool,
}

/// What the play head produced for one frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Frame {
    /// A real packet's payload.
    Packet(Vec<u8>),
    /// No packet in time: the decoder runs PLC on an empty packet.
    Plc,
}

impl JitterBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Pushes a packet. Returns false when the counter is behind the play head minus the
    /// reorder window (too late to ever play) — the caller's replay window should already
    /// have dropped duplicates.
    pub fn push(&mut self, counter: u64, frame: Vec<u8>) -> bool {
        if self.init {
            let horizon = self.next.saturating_sub(REORDER_WINDOW);
            if counter < horizon {
                return false;
            }
            if counter < self.next {
                // Late but inside the reorder window: keep it, the head may still want it.
                self.pending.insert(counter, frame);
                return true;
            }
        }
        self.pending.insert(counter, frame);
        true
    }

    /// Arms the buffer at the first packet seen: the play head starts `TARGET_DELAY_FRAMES`
    /// behind it so a small burst can be reordered.
    fn arm(&mut self, counter: u64) {
        self.next = counter.saturating_sub(TARGET_DELAY_FRAMES);
        self.init = true;
    }

    /// Pops the frame due now. When nothing is due but the buffer is armed, emits `Plc` (the
    /// decoder conceals the loss) — a frame is never dropped silently.
    pub fn pop_due(&mut self) -> Option<Frame> {
        if !self.init {
            let (&first, _) = self.pending.first_key_value()?;
            self.arm(first);
        }
        let due = self.next;
        self.next += 1;
        match self.pending.remove(&due) {
            Some(frame) => Some(Frame::Packet(frame)),
            None => Some(Frame::Plc),
        }
    }

    /// Drops everything (peer left).
    pub fn clear(&mut self) {
        self.pending.clear();
        self.init = false;
        self.next = 0;
    }

    /// Buffered packets not yet played.
    pub fn len(&self) -> usize {
        self.pending.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

/// Distance gain: 1 at the camera, smoothstep falloff to 0 at `max_distance`.
pub fn distance_gain(distance: f32, max_distance: f32) -> f32 {
    if !distance.is_finite() || max_distance <= 0.0 {
        return 0.0;
    }
    let t = (distance / max_distance).clamp(0.0, 1.0);
    // smoothstep: 1 - (3t^2 - 2t^3)
    1.0 - t * t * (3.0 - 2.0 * t)
}

/// Constant-power pan from the angle between the direction to the speaker and the camera's
/// yaw. `angle` in radians, 0 = straight ahead, positive = to the right. Returns (left, right).
/// The pan position follows `0.5 + 0.5*sin(angle)`, so ahead and directly behind are centred
/// and full left/right is at ±90°; `l^2 + r^2 = 1` everywhere.
pub fn constant_power_pan(angle: f32) -> (f32, f32) {
    let mut a = angle.rem_euclid(std::f32::consts::TAU);
    if a > std::f32::consts::PI {
        a -= std::f32::consts::TAU;
    }
    let pos = (0.5 + 0.5 * a.sin()).clamp(0.0, 1.0);
    let theta = pos * std::f32::consts::FRAC_PI_2;
    (theta.cos(), theta.sin())
}

/// Direction from the camera to a pose, and its horizontal angle relative to `camera_yaw`
/// (degrees, Minecraft convention: yaw 0 = +Z/south, increasing clockwise seen from above).
pub fn relative_angle(camera: &Pose, speaker: &Pose) -> (f32, f32) {
    let dx = speaker.x - camera.x;
    let dz = speaker.z - camera.z;
    let distance = (dx * dx + dz * dz).sqrt();
    // World bearing of the speaker from the camera, in degrees, 0 = +Z.
    let bearing = (-dx).atan2(dz).to_degrees(); // Minecraft yaw convention
    let mut rel = bearing - camera.yaw;
    rel = rel.rem_euclid(360.0);
    if rel > 180.0 {
        rel -= 360.0;
    }
    (distance, rel)
}

/// Mixes one 20 ms stereo frame from every speaker's jitter buffer.
///
/// `speakers`: per-speaker (jitter buffer, last pose, personal volume 0..2, muted).
/// `out` receives FRAME_SAMPLES stereo pairs, pre-zeroed. `output_volume` is the global
/// 0..2 gain. Returns how many speakers contributed a real packet this frame.
pub fn mix_frame(
    speakers: &mut [(JitterBuffer, Pose, f32, bool)],
    camera: &Pose,
    max_distance: f32,
    output_volume: f32,
    decode: &mut dyn FnMut(&[u8]) -> [f32; FRAME_SAMPLES],
    out: &mut [[f32; 2]; FRAME_SAMPLES],
) -> usize {
    let mut contributed = 0;
    for (jitter, pose, volume, muted) in speakers.iter_mut() {
        if *muted {
            jitter.pop_due();
            continue;
        }
        let (distance, rel_deg) = relative_angle(camera, pose);
        let gain = distance_gain(distance, max_distance)
            * volume.clamp(0.0, 2.0)
            * output_volume.clamp(0.0, 2.0);
        if gain <= 0.0 {
            jitter.pop_due();
            continue;
        }
        let (pan_l, pan_r) = constant_power_pan(rel_deg.to_radians());
        let frame = jitter.pop_due();
        let pcm = match frame {
            Some(Frame::Packet(bytes)) => {
                contributed += 1;
                decode(&bytes)
            }
            Some(Frame::Plc) => decode(&[]),
            None => continue,
        };
        let gl = gain * pan_l;
        let gr = gain * pan_r;
        for (o, s) in out.iter_mut().zip(pcm.iter()) {
            o[0] += s * gl;
            o[1] += s * gr;
        }
    }
    contributed
}

/// Linear resampler from `from_rate` to 48 kHz, keeping fractional position between calls.
#[derive(Clone, Debug, Default)]
pub struct Resampler {
    frac: f64,
    prev: f32,
    has_prev: bool,
}

impl Resampler {
    /// Pushes `input` at `from_rate` Hz; returns the 48 kHz samples. The previous call's last
    /// sample is kept so the interpolation is continuous across calls.
    pub fn resample(&mut self, input: &[f32], from_rate: u32) -> Vec<f32> {
        if input.is_empty() {
            return Vec::new();
        }
        let from_rate = from_rate.max(1) as f64;
        let step = from_rate / SAMPLE_RATE as f64;
        if !self.has_prev {
            self.prev = input[0];
            self.has_prev = true;
        }
        // buf[0] is the previous call's last sample; buf[1..] is `input`.
        let mut buf = Vec::with_capacity(input.len() + 1);
        buf.push(self.prev);
        buf.extend_from_slice(input);
        self.prev = input[input.len() - 1];
        let mut pos = 1.0 + self.frac;
        let mut out = Vec::new();
        while pos < buf.len() as f64 {
            let i = (pos as usize).min(buf.len() - 2);
            let frac = (pos - i as f64).clamp(0.0, 1.0) as f32;
            out.push(buf[i] + (buf[i + 1] - buf[i]) * frac);
            pos += step;
        }
        // Carry the next output time into the next call (buf[1] there is this call's last
        // input sample, so positions restart at 1.0 + frac).
        self.frac = pos - buf.len() as f64;
        out
    }
}

/// Frames a sample stream into 20 ms chunks of [`FRAME_SAMPLES`], keeping the remainder.
#[derive(Clone, Debug, Default)]
pub struct Framer {
    buf: Vec<f32>,
}

impl Framer {
    pub fn push(&mut self, samples: &[f32]) -> Vec<[f32; FRAME_SAMPLES]> {
        self.buf.extend_from_slice(samples);
        let mut frames = Vec::new();
        while self.buf.len() >= FRAME_SAMPLES {
            let frame: [f32; FRAME_SAMPLES] =
                self.buf[..FRAME_SAMPLES].try_into().expect("frame size");
            frames.push(frame);
            self.buf.drain(..FRAME_SAMPLES);
        }
        frames
    }
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }
}

/// RMS level of a frame, for the voice-activation gate.
pub fn rms(frame: &[f32]) -> f32 {
    if frame.is_empty() {
        return 0.0;
    }
    let sum: f32 = frame.iter().map(|s| s * s).sum();
    (sum / frame.len() as f32).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jitter_reorders_and_conceals() {
        let mut j = JitterBuffer::new();
        assert!(j.push(10, b"a".to_vec()));
        // First pop arms the buffer 2 frames behind 10 -> due 8, 9 are Plc, 10 is "a".
        assert_eq!(j.pop_due(), Some(Frame::Plc));
        assert_eq!(j.pop_due(), Some(Frame::Plc));
        assert_eq!(j.pop_due(), Some(Frame::Packet(b"a".to_vec())));
        // Out-of-order arrival inside the window (due 11 is missing -> Plc, then 12, 13).
        assert!(j.push(13, b"d".to_vec()));
        assert!(j.push(12, b"c".to_vec()));
        assert_eq!(j.pop_due(), Some(Frame::Plc));
        assert_eq!(j.pop_due(), Some(Frame::Packet(b"c".to_vec())));
        assert_eq!(j.pop_due(), Some(Frame::Packet(b"d".to_vec())));
        assert_eq!(j.pop_due(), Some(Frame::Plc));
        // Far too late: rejected.
        assert!(!j.push(1, b"old".to_vec()));
    }

    #[test]
    fn distance_gain_smoothstep() {
        assert_eq!(distance_gain(0.0, 32.0), 1.0);
        assert_eq!(distance_gain(32.0, 32.0), 0.0);
        assert_eq!(distance_gain(100.0, 32.0), 0.0);
        let half = distance_gain(16.0, 32.0);
        assert!(
            (half - 0.5).abs() < 1e-6,
            "smoothstep midpoint is 0.5, got {half}"
        );
        assert!(distance_gain(8.0, 32.0) > 0.5);
        assert_eq!(distance_gain(f32::NAN, 32.0), 0.0);
    }

    #[test]
    fn pan_is_constant_power_and_centred() {
        let (l, r) = constant_power_pan(0.0);
        assert!((l - r).abs() < 1e-6);
        assert!((l * l + r * r - 1.0).abs() < 1e-6);
        let (l, r) = constant_power_pan(std::f32::consts::FRAC_PI_2);
        assert!(r > 0.99 && l < 0.01);
        let (l, r) = constant_power_pan(-std::f32::consts::FRAC_PI_2);
        assert!(l > 0.99 && r < 0.01);
        // Directly behind: centred again, not silent.
        let (l, r) = constant_power_pan(std::f32::consts::PI);
        assert!((l - r).abs() < 1e-6);
        assert!((l * l + r * r - 1.0).abs() < 1e-6);
    }

    #[test]
    fn relative_angle_and_distance() {
        let cam = Pose {
            x: 0.0,
            y: 0.0,
            z: 0.0,
            yaw: 0.0,
        };
        let ahead = Pose {
            x: 0.0,
            y: 0.0,
            z: 10.0,
            yaw: 0.0,
        }; // +Z = yaw 0
        let (d, rel) = relative_angle(&cam, &ahead);
        assert!((d - 10.0).abs() < 1e-4);
        assert!(rel.abs() < 1e-3, "ahead is rel 0, got {rel}");
        let right = Pose {
            x: 10.0,
            y: 0.0,
            z: 0.0,
            yaw: 0.0,
        }; // +X = yaw -90
        let (_, rel) = relative_angle(&cam, &right);
        assert!((rel - (-90.0)).abs() < 1e-2, "right is -90, got {rel}");
    }

    #[test]
    fn mix_applies_gain_pan_and_plc() {
        let cam = Pose {
            x: 0.0,
            y: 0.0,
            z: 0.0,
            yaw: 0.0,
        };
        let speaker = Pose {
            x: 0.0,
            y: 0.0,
            z: 5.0,
            yaw: 0.0,
        };
        let mut j = JitterBuffer::new();
        j.push(10, vec![0xAB]);
        let mut speakers = [(j, speaker, 1.0f32, false)];
        let mut out = [[0.0f32; 2]; FRAME_SAMPLES];
        // PLC (empty packet) decodes to silence here; a real packet to a constant 0.5.
        let mut decode = |bytes: &[u8]| -> [f32; FRAME_SAMPLES] {
            if bytes.is_empty() {
                [0.0; FRAME_SAMPLES]
            } else {
                [0.5; FRAME_SAMPLES]
            }
        };
        // First two pops are Plc (arming), third is the packet.
        mix_frame(&mut speakers, &cam, 32.0, 1.0, &mut decode, &mut out);
        assert!(out.iter().all(|p| p[0] == 0.0 && p[1] == 0.0));
        mix_frame(&mut speakers, &cam, 32.0, 1.0, &mut decode, &mut out);
        mix_frame(&mut speakers, &cam, 32.0, 1.0, &mut decode, &mut out);
        let gain = distance_gain(5.0, 32.0);
        let (pl, pr) = constant_power_pan(0.0);
        assert!((out[0][0] - 0.5 * gain * pl).abs() < 1e-6);
        assert!((out[0][1] - 0.5 * gain * pr).abs() < 1e-6);
        // Muted speaker contributes nothing but still drains.
        let mut j2 = JitterBuffer::new();
        j2.push(10, vec![0xCD]);
        let mut speakers2 = [(j2, speaker, 1.0f32, true)];
        let mut out2 = [[0.0f32; 2]; FRAME_SAMPLES];
        mix_frame(&mut speakers2, &cam, 32.0, 1.0, &mut decode, &mut out2);
        assert!(out2.iter().all(|p| p[0] == 0.0 && p[1] == 0.0));
    }

    #[test]
    fn resampler_halves_96k_to_48k() {
        let mut r = Resampler::default();
        let input: Vec<f32> = (0..960).map(|i| i as f32).collect();
        let out = r.resample(&input, 96_000);
        assert_eq!(out.len(), 480);
        assert!((out[0] - 0.0).abs() < 1e-3);
        assert!((out[1] - 2.0).abs() < 1e-3);
    }

    #[test]
    fn resampler_upsamples_24k_to_48k() {
        let mut r = Resampler::default();
        let input: Vec<f32> = (0..480).map(|i| i as f32).collect();
        let out = r.resample(&input, 24_000);
        assert_eq!(out.len(), 960);
        assert!((out[1] - 0.5).abs() < 1e-3);
    }

    #[test]
    fn resampler_is_continuous_across_calls() {
        let mut r = Resampler::default();
        let a: Vec<f32> = (0..480).map(|i| i as f32).collect();
        let b: Vec<f32> = (480..960).map(|i| i as f32).collect();
        let mut out = r.resample(&a, 24_000);
        out.extend(r.resample(&b, 24_000));
        assert_eq!(out.len(), 1920);
        // Monotonic ramp stays monotonic.
        assert!(out.windows(2).all(|w| w[1] >= w[0] - 1e-3));
    }

    #[test]
    fn framer_splits_and_keeps_remainder() {
        let mut f = Framer::default();
        let samples = vec![1.0f32; FRAME_SAMPLES + 100];
        let frames = f.push(&samples);
        assert_eq!(frames.len(), 1);
        assert!(frames[0].iter().all(|&s| s == 1.0));
        assert_eq!(f.buffered(), 100);
        let frames = f.push(&vec![2.0; FRAME_SAMPLES - 100]);
        assert_eq!(frames.len(), 1);
        assert_eq!(f.buffered(), 0);
    }

    #[test]
    fn rms_of_silence_and_full_scale() {
        assert_eq!(rms(&[0.0; 960]), 0.0);
        assert!((rms(&[1.0; 960]) - 1.0).abs() < 1e-6);
        assert!((rms(&[0.5; 960]) - 0.5).abs() < 1e-6);
    }
}
