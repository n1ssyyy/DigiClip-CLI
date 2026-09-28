//! In-process audio analysis on the extracted 16 kHz mono WAV.
//!
//! One pass builds a 10 ms energy envelope that two consumers share:
//! - cut placement — every edit (clip edges, pause/filler removals) lands
//!   in the quietest instant between words instead of on whisper's word
//!   stamps, which drift ±50–150 ms and would clip consonants;
//! - emphasis detection — 400 ms loudness windows (momentary-loudness
//!   style) for punch-ins, with no extra ffmpeg extract + ebur128 pass.
//!
//! A one-pole high-pass (~100 Hz) runs first so rumble, hum and DC never
//! read as "sound" in the quiet-point search.

use std::path::Path;

/// 16 kHz mono energy envelope, one mean-square value per 10 ms hop.
#[derive(Debug, Clone)]
pub struct Envelope {
    /// Mean-square per hop, full scale = 1.0.
    ms: Vec<f32>,
}

/// Seconds per envelope frame.
pub const HOP: f64 = 0.01;

impl Envelope {
    /// Stream the WAV (never loads a long podcast's samples at once).
    pub fn load(wav: &Path) -> anyhow::Result<Self> {
        let mut reader = hound::WavReader::open(wav)?;
        let spec = reader.spec();
        if spec.bits_per_sample != 16 || spec.sample_format != hound::SampleFormat::Int {
            anyhow::bail!(
                "envelope wants 16-bit PCM, got {}-bit",
                spec.bits_per_sample
            );
        }
        let ch = spec.channels.max(1) as usize;
        let mut acc = Accum::new(spec.sample_rate);
        let mut frame_sum = 0.0f32;
        let mut n = 0usize;
        for s in reader.samples::<i16>() {
            frame_sum += s? as f32 / 32768.0;
            n += 1;
            if n == ch {
                acc.push(frame_sum / ch as f32);
                frame_sum = 0.0;
                n = 0;
            }
        }
        Ok(acc.finish())
    }

    /// Build from float samples (tests, in-memory audio).
    pub fn from_samples(samples: &[f32], rate: u32) -> Self {
        let mut acc = Accum::new(rate);
        for &s in samples {
            acc.push(s);
        }
        acc.finish()
    }

    pub fn is_empty(&self) -> bool {
        self.ms.is_empty()
    }

    /// Envelope length in seconds.
    pub fn duration(&self) -> f64 {
        self.ms.len() as f64 * HOP
    }

    fn frame(&self, t: f64) -> usize {
        ((t / HOP).floor().max(0.0) as usize).min(self.ms.len().saturating_sub(1))
    }

    /// Mean power over [a, b) in dBFS (-100 floor).
    pub fn db(&self, a: f64, b: f64) -> f64 {
        if self.ms.is_empty() {
            return -100.0;
        }
        let (i, j) = (
            self.frame(a),
            self.frame(b.max(a + HOP)).max(self.frame(a) + 1),
        );
        let j = j.min(self.ms.len());
        let i = i.min(j.saturating_sub(1));
        let mean = self.ms[i..j].iter().map(|&v| v as f64).sum::<f64>() / (j - i).max(1) as f64;
        to_db(mean)
    }

    /// The quietest instant in `[a, b]`: center of the 30 ms window with
    /// the least energy. Near-ties (within 1.5 dB) go to the one closest to
    /// `pref`, so on true silence the cut stays where the planner wanted it.
    /// Returns `pref` clamped into range when the span is too short to
    /// search or the envelope is empty.
    pub fn quietest(&self, a: f64, b: f64, pref: f64) -> f64 {
        let pref_c = pref.clamp(a.min(b), b.max(a));
        if self.ms.len() < 3 || b - a < 3.0 * HOP {
            return pref_c;
        }
        let i0 = self.frame(a);
        let i1 = self.frame(b).min(self.ms.len() - 1);
        if i1 < i0 + 2 {
            return pref_c;
        }
        // Windows [k, k+3) with k+3 <= i1+1.
        let win = |k: usize| (self.ms[k] + self.ms[k + 1] + self.ms[k + 2]) as f64 / 3.0;
        let cands: Vec<(usize, f64)> = (i0..=(i1 - 2)).map(|k| (k, to_db(win(k)))).collect();
        let floor = cands.iter().map(|c| c.1).fold(f64::INFINITY, f64::min);
        let center = |k: usize| (k as f64 + 1.5) * HOP;
        cands
            .iter()
            .filter(|c| c.1 <= floor + 1.5)
            .map(|c| center(c.0))
            .min_by(|x, y| {
                (x - pref_c)
                    .abs()
                    .partial_cmp(&(y - pref_c).abs())
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .unwrap_or(pref_c)
            .clamp(a.min(b), b.max(a))
    }

    /// Momentary-loudness style series over `[a, b]`: 400 ms mean power in
    /// dBFS every 100 ms, stamped at the window CENTER (ebur128 stamps the
    /// window end, which put punches a word late).
    pub fn loudness(&self, a: f64, b: f64) -> Vec<(f64, f64)> {
        let mut out = Vec::new();
        if self.ms.is_empty() {
            return out;
        }
        let mut t = a.max(0.2);
        let end = b.min(self.duration() - 0.2);
        while t <= end + 1e-9 {
            out.push((t, self.db(t - 0.2, t + 0.2)));
            t += 0.1;
        }
        out
    }
}

fn to_db(ms: f64) -> f64 {
    (10.0 * (ms + 1e-10).log10()).max(-100.0)
}

/// Streaming high-pass + mean-square accumulator (10 ms hops).
struct Accum {
    per_hop: usize,
    alpha: f32,
    prev_x: f32,
    prev_y: f32,
    sum: f32,
    n: usize,
    ms: Vec<f32>,
}

impl Accum {
    fn new(rate: u32) -> Self {
        let rate = rate.max(1000);
        Self {
            per_hop: (rate as f64 * HOP).round() as usize,
            // One-pole high-pass at ~100 Hz.
            alpha: (-2.0 * std::f64::consts::PI * 100.0 / rate as f64).exp() as f32,
            prev_x: 0.0,
            prev_y: 0.0,
            sum: 0.0,
            n: 0,
            ms: Vec::new(),
        }
    }

    fn push(&mut self, x: f32) {
        let y = self.alpha * (self.prev_y + x - self.prev_x);
        self.prev_x = x;
        self.prev_y = y;
        self.sum += y * y;
        self.n += 1;
        if self.n == self.per_hop {
            self.ms.push(self.sum / self.n as f32);
            self.sum = 0.0;
            self.n = 0;
        }
    }

    fn finish(mut self) -> Envelope {
        if self.n > 0 {
            self.ms.push(self.sum / self.n as f32);
        }
        Envelope { ms: self.ms }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 16 kHz: 1 kHz tone at `amp` over the given spans, silence elsewhere.
    fn tone(len_s: f64, spans: &[(f64, f64, f32)]) -> Vec<f32> {
        let n = (len_s * 16000.0) as usize;
        (0..n)
            .map(|i| {
                let t = i as f64 / 16000.0;
                spans
                    .iter()
                    .find(|(a, b, _)| t >= *a && t < *b)
                    .map(|(_, _, amp)| amp * (2.0 * std::f32::consts::PI * 1000.0 * t as f32).sin())
                    .unwrap_or(0.0)
            })
            .collect()
    }

    #[test]
    fn quietest_finds_the_gap_between_words() {
        // Two "words" with a 120 ms gap at 1.00-1.12.
        let env = Envelope::from_samples(&tone(2.0, &[(0.2, 1.0, 0.5), (1.12, 1.8, 0.5)]), 16000);
        let q = env.quietest(0.8, 1.4, 0.9);
        assert!(q > 1.0 && q < 1.12, "cut must land in the gap, got {q}");
    }

    #[test]
    fn quietest_prefers_pref_on_flat_silence() {
        let env = Envelope::from_samples(&tone(2.0, &[]), 16000);
        let q = env.quietest(0.5, 1.5, 1.2);
        assert!(
            (q - 1.2).abs() < 0.03,
            "flat silence keeps the planned cut, got {q}"
        );
        // Degenerate spans return pref clamped.
        assert!((env.quietest(1.0, 1.01, 3.0) - 1.01).abs() < 1e-9);
    }

    #[test]
    fn loudness_peaks_where_it_is_loud_and_is_centered() {
        let env = Envelope::from_samples(
            // First matching span wins: the loud burst over a quiet bed.
            &tone(4.0, &[(2.0, 2.4, 0.8), (0.0, 4.0, 0.05)]),
            16000,
        );
        let l = env.loudness(0.5, 3.5);
        let peak = l.iter().cloned().fold(
            (0.0, f64::NEG_INFINITY),
            |a, b| if b.1 > a.1 { b } else { a },
        );
        assert!(
            (peak.0 - 2.2).abs() < 0.11,
            "center-stamped peak, got {:?}",
            peak
        );
        assert!(peak.1 > env.db(0.5, 1.5) + 15.0);
    }

    #[test]
    fn highpass_ignores_dc_and_rumble() {
        // Big DC offset: must still read as silence.
        let dc: Vec<f32> = vec![0.4; 16000];
        let env = Envelope::from_samples(&dc, 16000);
        assert!(
            env.db(0.3, 1.0) < -60.0,
            "DC reads silent: {}",
            env.db(0.3, 1.0)
        );
    }
}
