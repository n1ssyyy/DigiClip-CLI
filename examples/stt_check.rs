//! STT calibration: embedded whisper-rs vs sidecar ground truth.
//!
//! Run: `cargo run --example stt_check`
//! Transcribes the TTS sample wav in-process and compares against the
//! sidecar transcript.json (token timings) to validate the t0/t1 scale.

fn main() -> anyhow::Result<()> {
    let tmp = std::env::temp_dir();
    let wav = tmp.join("digiclip-e2e-full").join("audio.wav");
    let gt_path = tmp.join("digiclip-e2e-full").join("transcript.json");
    assert!(wav.is_file(), "run the e2e full pipeline first");
    assert!(gt_path.is_file(), "run the e2e full pipeline first");

    let opts = digiclip_rs::stt::SttOptions {
        model: "base.en".into(),
        lang: "en".into(),
        threads: 6,
    };
    let tr = digiclip_rs::stt::transcribe_embedded(&wav, &opts)?;
    println!(
        "embedded: {} words, {} segments",
        tr.words.len(),
        tr.segments.len()
    );

    let gt: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&gt_path)?)?;
    let gt_words: Vec<(String, f64, f64)> = gt["words"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|w| {
            Some((
                w["w"].as_str()?.to_string(),
                w["s"].as_f64()?,
                w["e"].as_f64()?,
            ))
        })
        .collect();
    println!("sidecar:  {} words", gt_words.len());

    // Compare first-word and last-word timings (same audio, same model).
    for (a, b) in tr.words.iter().zip(gt_words.iter()).take(5) {
        println!(
            "  emb {:<12} {:5.2}-{:<5.2} | side {:5.2}-{:<5.2}",
            a.w, a.s, a.e, b.1, b.2
        );
    }
    let emb_last = tr.words.last().map(|w| w.e).unwrap_or(0.0);
    let gt_last = gt_words.last().map(|w| w.2).unwrap_or(0.0);
    println!("last-word end: embedded={emb_last:.2}s sidecar={gt_last:.2}s");
    assert!(
        (emb_last - gt_last).abs() < 3.0,
        "timing scales disagree — check t0/t1 units"
    );
    // Text sanity: same speech, expect heavy word overlap.
    let emb_set: std::collections::HashSet<String> =
        tr.words.iter().map(|w| w.w.to_lowercase()).collect();
    let hits = gt_words
        .iter()
        .filter(|(w, _, _)| emb_set.contains(&w.to_lowercase()))
        .count();
    println!("word overlap: {hits}/{}", gt_words.len());
    assert!(hits * 2 > gt_words.len(), "transcripts disagree badly");
    println!("STT CHECK OK");
    Ok(())
}
