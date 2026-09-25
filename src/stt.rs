//! Embedded transcription via whisper-rs (statically linked whisper.cpp).
//!
//! No sidecar binary to ship: the model weights still download on first
//! run (see [`crate::models`]), but STT itself lives inside the exe.
//! Token timestamps are enabled; words prefer token timing with the same
//! even-split fallback as the old sidecar path.

use std::path::Path;

use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

use crate::whisper::{Segment, Transcription, Word};

pub struct SttOptions {
    pub model: String,
    pub lang: String,
    pub threads: usize,
}

fn read_pcm16_mono(wav: &Path) -> anyhow::Result<Vec<f32>> {
    let mut reader = hound::WavReader::open(wav)?;
    let spec = reader.spec();
    if spec.sample_rate != 16000 {
        anyhow::bail!(
            "expected 16kHz WAV (pipeline extracts at 16kHz), got {}Hz",
            spec.sample_rate
        );
    }
    if spec.bits_per_sample != 16 {
        anyhow::bail!("expected 16-bit WAV, got {}-bit", spec.bits_per_sample);
    }
    let samples: Vec<i16> = reader.samples::<i16>().collect::<Result<Vec<_>, _>>()?;
    let mut pcm = vec![0.0f32; samples.len()];
    whisper_rs::convert_integer_to_float_audio(&samples, &mut pcm)
        .map_err(|e| anyhow::anyhow!("pcm convert failed: {e:?}"))?;
    if spec.channels == 2 {
        let mut mono = vec![0.0f32; pcm.len() / 2];
        whisper_rs::convert_stereo_to_mono_audio(&pcm, &mut mono)
            .map_err(|e| anyhow::anyhow!("stereo->mono failed: {e:?}"))?;
        pcm = mono;
    } else if spec.channels != 1 {
        anyhow::bail!("expected mono/stereo WAV, got {} channels", spec.channels);
    }
    Ok(pcm)
}

/// Transcribe in-process. `t_scale` converts whisper token/segment time
/// units to seconds (segments are centiseconds per whisper.cpp; token
/// t0/t1 are calibrated against sidecar output — see tests).
pub fn transcribe_embedded(wav: &Path, opts: &SttOptions) -> anyhow::Result<Transcription> {
    if !wav.is_file() {
        anyhow::bail!("Audio not found: {}", wav.display());
    }
    let model_path = crate::models::require(&opts.model)?;
    let pcm = read_pcm16_mono(wav)?;

    let ctx = WhisperContext::new_with_params(&model_path, WhisperContextParameters::default())
        .map_err(|e| anyhow::anyhow!("whisper init failed: {e:?}"))?;
    let mut state = ctx
        .create_state()
        .map_err(|e| anyhow::anyhow!("whisper state failed: {e:?}"))?;

    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    params.set_n_threads(opts.threads as std::ffi::c_int);
    params.set_language(Some(opts.lang.as_str()));
    params.set_token_timestamps(true);
    params.set_print_progress(false);

    state
        .full(params, &pcm)
        .map_err(|e| anyhow::anyhow!("whisper inference failed: {e:?}"))?;

    let n = state.full_n_segments();
    let mut words = Vec::new();
    let mut segments = Vec::new();
    for i in 0..n {
        let Some(seg) = state.get_segment(i) else {
            continue;
        };
        let from = seg.start_timestamp() as f64 / 100.0;
        let to = seg.end_timestamp() as f64 / 100.0;
        let text = seg
            .to_str_lossy()
            .map(|c| c.trim().to_string())
            .unwrap_or_default();
        if text.is_empty() {
            continue;
        }
        segments.push(Segment {
            s: from,
            e: to,
            text: text.clone(),
        });

        // Prefer token timing. Tokens are word pieces: WordBuilder joins
        // them into real words and skips special tokens ([_BEG_], [_TT_*]…).
        let mut wb = crate::whisper::WordBuilder::default();
        for j in 0..seg.n_tokens() {
            let Some(tok) = seg.get_token(j) else {
                continue;
            };
            let Ok(raw) = tok.to_bytes() else {
                continue;
            };
            let d = tok.token_data();
            // t0/t1 < 0 means "no timing for this token".
            let timing =
                (d.t0 >= 0 && d.t1 >= 0).then(|| (d.t0 as f64 / 100.0, d.t1 as f64 / 100.0));
            wb.push(raw, timing, Some(((d.p * 1000.0).round() / 1000.0) as f64));
        }
        let tw = wb.finish();
        if !tw.is_empty() {
            words.extend(tw);
            continue;
        }
        let parts: Vec<&str> = text.split_whitespace().collect();
        let count = parts.len().max(1);
        let dur = (to - from).max(0.0);
        for (k, w) in parts.iter().enumerate() {
            words.push(Word {
                w: w.to_string(),
                s: ((from + dur * k as f64 / count as f64) * 1000.0).round() / 1000.0,
                e: ((from + dur * (k + 1) as f64 / count as f64) * 1000.0).round() / 1000.0,
                conf: None,
            });
        }
    }

    Ok(Transcription {
        words,
        segments,
        language: opts.lang.clone(),
        model: opts.model.clone(),
    })
}
