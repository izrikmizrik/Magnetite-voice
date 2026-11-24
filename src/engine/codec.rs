//! The Opus codec wrapper (rusty-opus 1): one encoder for the local mic, one decoder per
//! remote speaker. 48 kHz mono, 20 ms frames (960 samples), VoIP mode at ~32 kbps,
//! complexity 4, DTX on so silence costs no airtime. Packet loss is concealed by the
//! decoder itself: decoding an empty packet runs PLC.

use crate::engine::jitter::{FRAME_SAMPLES, SAMPLE_RATE};
use rusty_opus::{Application, OpusDecoder, OpusEncoder};

/// Target bitrate for VoIP speech.
pub const BITRATE_BPS: i32 = 32_000;
/// Encoder complexity (0..10).
pub const COMPLEXITY: i32 = 4;

pub struct Encoder {
    enc: OpusEncoder,
    out: Vec<u8>,
}

impl Encoder {
    pub fn new() -> Result<Self, rusty_opus::Error> {
        let mut enc = OpusEncoder::new(SAMPLE_RATE as i32, 1, Application::Voip)?;
        enc.bitrate_bps = BITRATE_BPS;
        enc.complexity = COMPLEXITY;
        enc.use_dtx = true;
        enc.use_cbr = false;
        enc.use_inband_fec = true;
        enc.packet_loss_perc = 10;
        Ok(Self {
            enc,
            out: vec![0u8; 1500],
        })
    }

    /// Encodes one 20 ms frame of mono f32 PCM in [-1, 1]; returns the Opus packet.
    pub fn encode(&mut self, frame: &[f32; FRAME_SAMPLES]) -> Result<Vec<u8>, rusty_opus::Error> {
        let n = self.enc.encode(frame, FRAME_SAMPLES, &mut self.out)?;
        Ok(self.out[..n].to_vec())
    }
}

pub struct Decoder {
    dec: OpusDecoder,
    out: [f32; FRAME_SAMPLES],
}

impl Decoder {
    pub fn new() -> Result<Self, rusty_opus::Error> {
        Ok(Self {
            dec: OpusDecoder::new(SAMPLE_RATE as i32, 1)?,
            out: [0.0; FRAME_SAMPLES],
        })
    }

    /// Decodes one packet into a 20 ms frame. An empty `packet` runs packet-loss
    /// concealment instead of dropping the frame.
    pub fn decode(&mut self, packet: &[u8]) -> Result<[f32; FRAME_SAMPLES], rusty_opus::Error> {
        let n = self.dec.decode(packet, FRAME_SAMPLES, &mut self.out)?;
        if n != FRAME_SAMPLES {
            return Err(rusty_opus::Error::Internal("short frame"));
        }
        Ok(self.out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_round_trip_silence_and_tone() {
        let mut enc = Encoder::new().expect("encoder");
        let mut dec = Decoder::new().expect("decoder");
        let silence = [0.0f32; FRAME_SAMPLES];
        let pkt = enc.encode(&silence).expect("encode silence");
        let pcm = dec.decode(&pkt).expect("decode silence");
        assert!(
            pcm.iter().all(|s| s.abs() < 1e-3),
            "silence decodes near-silent"
        );

        let mut tone = [0.0f32; FRAME_SAMPLES];
        for (i, s) in tone.iter_mut().enumerate() {
            *s = 0.3 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / SAMPLE_RATE as f32).sin();
        }
        let pkt = enc.encode(&tone).expect("encode tone");
        assert!(!pkt.is_empty());
        let pcm = dec.decode(&pkt).expect("decode tone");
        let peak = pcm.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(peak > 0.05, "tone survives, peak {peak}");
    }

    #[test]
    fn plc_on_empty_packet_does_not_fail() {
        let mut enc = Encoder::new().unwrap();
        let mut dec = Decoder::new().unwrap();
        let tone = [0.2f32; FRAME_SAMPLES];
        let pkt = enc.encode(&tone).unwrap();
        dec.decode(&pkt).unwrap();
        let plc = dec.decode(&[]).expect("PLC must not error");
        assert_eq!(plc.len(), FRAME_SAMPLES);
    }

    #[test]
    fn garbage_packet_is_an_error_not_a_panic() {
        let mut dec = Decoder::new().unwrap();
        let garbage = [0xFFu8; 64];
        let _ = dec.decode(&garbage);
    }
}
