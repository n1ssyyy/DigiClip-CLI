//! Emphasis punch-ins: loud words get a brief directed zoom.
//!
//! The pipeline scans clip audio once (ebur128 momentary loudness), maps
//! peaks to words, and hands source-clock punch windows to `plan_tracks`,
//! which eases them into the raw targets as zoom bumps — the whole
//! existing path (glides, dolly, captions) rides along unchanged.

use crate::timeline::{tight, Keep};
use crate::whisper::Word;

/// Depth of the punch (window scale 1-depth at the peak): ~1.2x zoom.
pub const PUNCH_DEPTH: f64 = 0.18;

/// Parse ebur128 stderr lines into (t seconds, momentary LUFS).
/// Expected form: `... t: 0.499938 ... M: -17.2 ...`.
pub fn parse_ebur128(stderr: &str) -> Vec<(f64, f64)> {
    let mut out = Vec::new();
    for line in stderr.lines() {
        let Some(ti) = line.find("t:") else { continue };
        let Some(mi) = line.find("M:") else { continue };
        let t: Option<f64> = line[ti + 2..]
            .split_whitespace()
            .next()
            .and_then(|s| s.parse().ok());
        let m: Option<f64> = line[mi + 2..]
            .split_whitespace()
            .next()
            .and_then(|s| s.parse().ok());
        if let (Some(t), Some(m)) = (t, m) {
            if m.is_finite() && t.is_finite() {
                out.push((t, m));
            }
        }
    }
    out
}

/// Pick emphasis peaks: 0.5s-smoothed local maxima (±0.3s) standing `db`
/// above their ±1.5s median, greedy by height with 3s spacing.
/// Returns (t, height) in time order.
pub fn peaks(m: &[(f64, f64)], db: f64) -> Vec<(f64, f64)> {
    if m.len() < 10 {
        return vec![];
    }
    // Boxcar smooth (~0.5s at 10Hz metering).
    let sm: Vec<f64> = m
        .iter()
        .enumerate()
        .map(|(i, _)| {
            let (mut s, mut n) = (0.0, 0);
            for j in i.saturating_sub(2)..=(i + 2).min(m.len() - 1) {
                s += m[j].1;
                n += 1;
            }
            s / n as f64
        })
        .collect();
    let mut cand: Vec<(f64, f64)> = Vec::new();
    for i in 0..m.len() {
        let lo = i.saturating_sub(3);
        let hi = (i + 3).min(m.len() - 1);
        if !(lo..=hi).all(|j| sm[i] >= sm[j]) {
            continue;
        }
        let wlo = i.saturating_sub(15);
        let whi = (i + 15).min(m.len() - 1);
        let mut win: Vec<f64> = sm[wlo..=whi].to_vec();
        win.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let med = win[win.len() / 2];
        if sm[i] > -60.0 && sm[i] - med >= db {
            cand.push((m[i].0, sm[i]));
        }
    }
    // Greedy by height, 3s spacing, back to time order.
    cand.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    let mut taken: Vec<(f64, f64)> = Vec::new();
    for c in cand {
        if taken.iter().all(|t| (t.0 - c.0).abs() >= 3.0) {
            taken.push(c);
        }
    }
    taken.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    taken
}

/// Map peaks to word windows (word.s-0.15, word.e+0.45): skips fillers,
/// short words and words outside `kept` (tightened away). Top `max_n`
/// by height. Returns (start, end) in source clock.
pub fn windows(
    peaks: &[(f64, f64)],
    words: &[Word],
    kept: Option<&[Keep]>,
    max_n: usize,
) -> Vec<(f64, f64)> {
    let mut by_h: Vec<(f64, f64)> = peaks.to_vec();
    by_h.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    let mut out: Vec<(f64, f64, f64)> = Vec::new(); // (s, e, h)
    for (t, h) in by_h {
        if out.len() >= max_n {
            break;
        }
        let Some(w) = words.iter().find(|w| w.s <= t && t <= w.e) else {
            continue;
        };
        let c: String =
            w.w.trim()
                .trim_matches(|c: char| !c.is_alphanumeric())
                .to_lowercase();
        if c.len() < 3 || crate::scorer::is_hook_filler(&w.w) {
            continue;
        }
        if let Some(k) = kept {
            if tight(w.s, k).is_none() {
                continue;
            }
        }
        let (s, e) = ((w.s - 0.15).max(0.0), w.e + 0.45);
        if out
            .iter()
            .all(|(os, oe, _)| s >= *oe + 1.0 || e <= *os - 1.0)
        {
            out.push((s, e, h));
        }
    }
    out.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    out.into_iter().map(|(s, e, _)| (s, e)).collect()
}

/// Scan [a, b) of source audio, returning source-clock (t, momentary LUFS).
/// One fast slice extract + one metering pass; any failure errors (caller
/// treats punch as best-effort and continues without it).
pub fn scan(
    ffmpeg: &std::path::Path,
    source: &std::path::Path,
    a: f64,
    b: f64,
) -> anyhow::Result<Vec<(f64, f64)>> {
    let len = (b - a).max(0.5);
    let wav = std::env::temp_dir().join(format!(
        "digiclip-punch-{}-{:.0}.wav",
        std::process::id(),
        a * 100.0
    ));
    let run = |args: &[String]| -> anyhow::Result<std::process::Output> {
        Ok(std::process::Command::new(ffmpeg).args(args).output()?)
    };
    let s = |v: &dyn ToString| v.to_string();
    run(&[
        s(&"-y"),
        s(&"-hide_banner"),
        s(&"-loglevel"),
        s(&"error"),
        s(&"-ss"),
        s(&a),
        s(&"-t"),
        s(&len),
        s(&"-i"),
        s(&source.display()),
        s(&"-ac"),
        s(&"1"),
        s(&"-ar"),
        s(&"16000"),
        s(&"-c:a"),
        s(&"pcm_s16le"),
        s(&wav.display()),
    ])?;
    let metered = run(&[
        s(&"-hide_banner"),
        s(&"-i"),
        s(&wav.display()),
        s(&"-af"),
        s(&"ebur128"),
        s(&"-f"),
        s(&"null"),
        s(&"-"),
    ])?;
    let _ = std::fs::remove_file(&wav);
    if !metered.status.success() {
        anyhow::bail!("ebur128 meter failed");
    }
    let mut m = parse_ebur128(&String::from_utf8_lossy(&metered.stderr));
    for (t, _) in m.iter_mut() {
        *t += a; // slice offset -> source clock
    }
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tw(text: &str) -> Vec<Word> {
        text.split_whitespace()
            .enumerate()
            .map(|(i, w)| Word {
                w: w.into(),
                s: i as f64 * 0.5,
                e: i as f64 * 0.5 + 0.45,
                conf: Some(0.9),
            })
            .collect()
    }

    #[test]
    fn parses_ebur128_lines() {
        let err = "[Parsed_ebur128_0 @ x] t: 0.499938  TARGET:-23 LUFS    M: -17.2 S:-120.7     I: -18.5 LUFS       LRA:\nnoise\n[Parsed_ebur128_0 @ x] t: 0.599938  TARGET:-23 LUFS    M:-120.7 S:-120.7     I: -70.0 LUFS       LRA:";
        let m = parse_ebur128(err);
        assert_eq!(m.len(), 2);
        assert!((m[0].0 - 0.499938).abs() < 1e-9 && (m[0].1 + 17.2).abs() < 1e-9);
    }

    #[test]
    fn peaks_find_the_loud_word() {
        // 10s @10Hz: quiet bed -30, loud bump -12 around t=5.
        let m: Vec<(f64, f64)> = (0..100)
            .map(|i| {
                let t = i as f64 * 0.1;
                let v = if (t - 5.0).abs() < 0.4 { -12.0 } else { -30.0 };
                (t, v)
            })
            .collect();
        let p = peaks(&m, 6.0);
        assert_eq!(p.len(), 1, "one emphasis peak: {p:?}");
        assert!((p[0].0 - 5.0).abs() < 0.6, "near the bump: {:?}", p[0]);
    }

    #[test]
    fn windows_skip_fillers_and_cap_count() {
        let words = tw("this is um REALLY the point");
        // bump on "um" (t=1.2) and "REALLY" (t=1.7).
        let p = vec![(1.2, -10.0), (1.7, -11.0)];
        let w = windows(&p, &words, None, 5);
        assert_eq!(w.len(), 1, "filler peak dropped: {w:?}");
        assert!((w[0].0 - 1.35).abs() < 1e-9 && (w[0].1 - 2.4).abs() < 1e-9);
        let w1 = windows(&p, &words, None, 0);
        assert!(w1.is_empty());
    }
}
