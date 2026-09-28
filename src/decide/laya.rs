//! Laya (Convai Innovations, Apache-2.0), run locally: ModernBERT-large
//! plus Laya's decision head, from the ONNX export at
//! `huggingface.co/receptron/laya-onnx`. The request rendering ports
//! receptron/laya (MIT), which mirrors the checkpoint's reference code:
//! one sequence per question,
//! `[CLS] <type> question: <instructions> [SEP] [MASK] opt0 [MASK] opt1 … [SEP] <state> [SEP]`,
//! each option scored at its `[MASK]`, then calibrated by a temperature
//! chosen by question type and option count.

use super::{confidence, Answer, Question};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const REPO: &str = "https://huggingface.co/receptron/laya-onnx/resolve/main/";

/// The bundle, with each file's rough minimum size (a smaller file is a
/// broken download).
const FILES: &[(&str, u64)] = &[
    ("laya_config.json", 16),
    ("tokenizer/tokenizer_config.json", 16),
    ("tokenizer/tokenizer.json", 1 << 20),
    ("laya.onnx", 1 << 20),
    ("laya.onnx.data", 1 << 30),
];

/// Download size, for the model list.
pub const SIZE_MB: u64 = 1612;
const TOTAL_BYTES: u64 = 1_692_649_536;

/// `…/models/laya`.
pub fn dir() -> PathBuf {
    crate::models::dir().join("laya")
}

pub fn is_ready() -> bool {
    let d = dir();
    FILES
        .iter()
        .all(|(f, min)| std::fs::metadata(d.join(f)).is_ok_and(|m| m.is_file() && m.len() >= *min))
}

/// Fetch the missing bundle files (resumable). `progress` gets bytes over
/// the whole bundle.
pub async fn download(
    progress: Option<std::sync::Arc<crate::progress::ByteFn>>,
) -> anyhow::Result<()> {
    let d = dir();
    let mut base = 0u64;
    for (f, min) in FILES {
        let dest = d.join(f);
        let have = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
        if have >= *min {
            base += have;
            continue;
        }
        let offset = base;
        let progress = progress.clone();
        let hook = move |done: u64, _total: u64| {
            if let Some(p) = &progress {
                p((offset + done).min(TOTAL_BYTES), TOTAL_BYTES);
            }
        };
        crate::provision::download_to(&format!("{REPO}{f}"), &dest, "laya", Some(&hook)).await?;
        base += std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
    }
    if !is_ready() {
        anyhow::bail!("Laya bundle incomplete after download");
    }
    Ok(())
}

/// Delete the bundle.
pub fn remove() -> std::io::Result<()> {
    match std::fs::remove_dir_all(dir()) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        r => r,
    }
}

/// The loaded model, kept while jobs use it and a few idle minutes after,
/// so a queue of videos loads it once (it holds ~2 GB of RAM).
static SHARED: Mutex<Option<(Arc<Laya>, Instant)>> = Mutex::new(None);
const KEEP: Duration = Duration::from_secs(300);

/// The shared model, loading it if needed.
pub async fn shared() -> anyhow::Result<Arc<Laya>> {
    if let Some((l, used)) = SHARED.lock().ok().as_deref_mut().and_then(Option::as_mut) {
        *used = Instant::now();
        return Ok(l.clone());
    }
    let l = Arc::new(tokio::task::spawn_blocking(Laya::load).await??);
    if let Ok(mut g) = SHARED.lock() {
        *g = Some((l.clone(), Instant::now()));
    }
    tokio::spawn(async {
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
            let Ok(mut g) = SHARED.lock() else { break };
            match g.as_mut() {
                Some((l, used)) if Arc::strong_count(l) > 1 => *used = Instant::now(),
                Some((_, used)) if used.elapsed() > KEEP => {
                    *g = None;
                    tracing::info!("Laya unloaded (idle)");
                    break;
                }
                Some(_) => {}
                None => break,
            }
        }
    });
    Ok(l)
}

#[derive(Debug, Clone, serde::Deserialize)]
struct Config {
    max_len: usize,
    head_max_len: usize,
    temperature: [f64; 3],
    temperature_by_options: HashMap<String, f64>,
}

#[derive(Debug, Clone, Copy)]
struct Special {
    cls: u32,
    sep: u32,
    mask: u32,
    pad: u32,
}

const MASK_TOK: &str = "[MASK]";

pub struct Laya {
    session: Mutex<ort::session::Session>,
    tok: tokenizers::Tokenizer,
    cfg: Config,
    ids: Special,
}

fn qtype(q: &Question) -> usize {
    match q {
        Question::Choice { .. } => 0,
        Question::Score { .. } => 1,
        Question::Noul { .. } => 2,
    }
}

/// Option texts in label order. A noul is always `[false, true]`.
fn render_options(q: &Question) -> Vec<String> {
    match q {
        Question::Choice { options, .. } => options
            .iter()
            .map(|(k, d)| match d {
                Some(d) if !d.is_empty() => format!("{k}: {d}"),
                _ => k.clone(),
            })
            .collect(),
        Question::Score { levels, .. } => levels
            .iter()
            .enumerate()
            .map(|(i, c)| format!("level {i}: {c}"))
            .collect(),
        Question::Noul { .. } => vec![
            "false: no, the statement does not hold".into(),
            "true: yes, the statement holds".into(),
        ],
    }
}

fn size_bucket(k: usize) -> &'static str {
    match k {
        0..=2 => "2",
        3..=5 => "3-5",
        6..=10 => "6-10",
        _ => "11+",
    }
}

/// Token ids and the position of each option's `[MASK]`.
fn build_sequence(
    encode: &dyn Fn(&str) -> Vec<u32>,
    ids: Special,
    state: &str,
    q: &Question,
    max_len: usize,
    head_max_len: usize,
) -> (Vec<u32>, Vec<usize>) {
    let scrub = |s: &str| s.replace(MASK_TOK, " ");
    let mut head = encode(&format!(
        "{} question: {}",
        q.kind(),
        scrub(q.instructions())
    ));
    let mut opts: Vec<Vec<u32>> = render_options(q)
        .iter()
        .map(|o| {
            let mut v = vec![ids.mask];
            v.extend(encode(&format!(" {}", scrub(o))).into_iter().take(48));
            v
        })
        .collect();
    let total = |xs: &[Vec<u32>]| xs.iter().map(Vec::len).sum::<usize>();
    let mut budget = head_max_len as i64 - total(&opts) as i64;
    if budget < 16 {
        // Too many / too long options: shrink every option evenly.
        let per = ((head_max_len.saturating_sub(16)) / opts.len().max(1)).max(4);
        for o in &mut opts {
            o.truncate(per);
        }
        budget = head_max_len as i64 - total(&opts) as i64;
    }
    head.truncate(budget.max(8) as usize);
    let mut seq = vec![ids.cls];
    seq.extend(head);
    seq.push(ids.sep);
    let mut markers = Vec::with_capacity(opts.len());
    for o in opts {
        markers.push(seq.len());
        seq.extend(o);
    }
    seq.push(ids.sep);
    let room = max_len.saturating_sub(seq.len() + 1);
    seq.extend(encode(&scrub(state)).into_iter().take(room));
    seq.push(ids.sep);
    seq.truncate(max_len);
    markers.retain(|m| *m < max_len);
    (seq, markers)
}

fn softmax(z: &[f64]) -> Vec<f64> {
    let m = z.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let e: Vec<f64> = z.iter().map(|v| (v - m).exp()).collect();
    let s: f64 = e.iter().sum();
    e.iter().map(|v| v / s).collect()
}

impl Laya {
    /// Open the downloaded bundle on the CPU (ONNX Runtime's DirectML
    /// provider rejects this graph's reshapes).
    pub fn load() -> anyhow::Result<Self> {
        let d = dir();
        if !is_ready() {
            anyhow::bail!("Laya bundle not downloaded ({})", d.display());
        }
        let cfg: Config =
            serde_json::from_str(&std::fs::read_to_string(d.join("laya_config.json"))?)?;
        let tok = tokenizers::Tokenizer::from_file(d.join("tokenizer").join("tokenizer.json"))
            .map_err(|e| anyhow::anyhow!("laya tokenizer: {e}"))?;
        let id = |t: &str| {
            tok.token_to_id(t)
                .ok_or_else(|| anyhow::anyhow!("laya tokenizer has no {t}"))
        };
        let ids = Special {
            cls: id("[CLS]")?,
            sep: id("[SEP]")?,
            mask: id(MASK_TOK)?,
            pad: id("[PAD]")?,
        };
        let t0 = std::time::Instant::now();
        let session = ort::session::Session::builder()?
            .with_optimization_level(ort::session::builder::GraphOptimizationLevel::Level3)
            .map_err(|e| anyhow::anyhow!("laya session: {e:?}"))?
            .commit_from_file(d.join("laya.onnx"))?;
        tracing::info!("Laya loaded in {:.1}s", t0.elapsed().as_secs_f64());
        Ok(Self {
            session: Mutex::new(session),
            tok,
            cfg,
            ids,
        })
    }

    fn encode(&self, text: &str) -> Vec<u32> {
        self.tok
            .encode(text, false)
            .map(|e| e.get_ids().to_vec())
            .unwrap_or_default()
    }

    /// Answer every question about `state` in one forward pass. Blocking.
    pub fn ask(
        &self,
        state: &str,
        questions: &[(&'static str, Question)],
    ) -> anyhow::Result<Vec<Answer>> {
        anyhow::ensure!(!questions.is_empty(), "laya: no questions");
        let enc = |t: &str| self.encode(t);
        let items: Vec<(Vec<u32>, Vec<usize>)> = questions
            .iter()
            .map(|(name, q)| {
                let (seq, markers) = build_sequence(
                    &enc,
                    self.ids,
                    state,
                    q,
                    self.cfg.max_len,
                    self.cfg.head_max_len,
                );
                anyhow::ensure!(
                    markers.len() == render_options(q).len(),
                    "laya: options of [{name}] do not fit"
                );
                Ok((seq, markers))
            })
            .collect::<anyhow::Result<_>>()?;
        let n = items.len();
        let l = items.iter().map(|i| i.0.len()).max().unwrap_or(1);
        let k = items.iter().map(|i| i.1.len()).max().unwrap_or(1);
        let mut input_ids = vec![self.ids.pad as i64; n * l];
        let mut attention = vec![0i64; n * l];
        let mut marker_pos = vec![0i64; n * k];
        let mut marker_mask = vec![false; n * k];
        let mut qtypes = vec![0i64; n];
        for (r, ((seq, markers), (_, q))) in items.iter().zip(questions).enumerate() {
            for (j, v) in seq.iter().enumerate() {
                input_ids[r * l + j] = *v as i64;
                attention[r * l + j] = 1;
            }
            for (j, m) in markers.iter().enumerate() {
                marker_pos[r * k + j] = *m as i64;
                marker_mask[r * k + j] = true;
            }
            qtypes[r] = qtype(q) as i64;
        }
        use ort::value::Tensor;
        let inputs = ort::inputs![
            "input_ids" => Tensor::from_array(([n, l], input_ids))?,
            "attention_mask" => Tensor::from_array(([n, l], attention))?,
            "marker_pos" => Tensor::from_array(([n, k], marker_pos))?,
            "marker_mask" => Tensor::from_array(([n, k], marker_mask))?,
            "qtype" => Tensor::from_array(([n], qtypes))?,
        ];
        let logits: Vec<f32> = {
            let mut s = self
                .session
                .lock()
                .map_err(|_| anyhow::anyhow!("laya session poisoned"))?;
            let out = s.run(inputs)?;
            out["logits"].try_extract_tensor::<f32>()?.1.to_vec()
        };
        anyhow::ensure!(logits.len() >= n * k, "laya: short logits");
        Ok(items
            .iter()
            .zip(questions)
            .enumerate()
            .map(|(r, ((_, markers), (_, q)))| {
                let kq = markers.len();
                let qt = qtype(q);
                let key = format!("{}:{}", q.kind(), size_bucket(kq));
                let temp = self
                    .cfg
                    .temperature_by_options
                    .get(&key)
                    .copied()
                    .unwrap_or(self.cfg.temperature[qt]);
                let z: Vec<f64> = logits[r * k..r * k + kq]
                    .iter()
                    .map(|v| *v as f64 / temp)
                    .collect();
                let p = softmax(&z);
                answer(q, &p)
            })
            .collect())
    }
}

fn answer(q: &Question, p: &[f64]) -> Answer {
    match q {
        Question::Choice { options, .. } => {
            let best = p
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.total_cmp(b.1))
                .map_or(0, |(i, _)| i);
            Answer::Choice {
                choice: options[best].0.clone(),
                confidence: confidence(p),
            }
        }
        Question::Score { .. } => Answer::Score {
            score: p.iter().enumerate().map(|(i, v)| i as f64 * v).sum(),
            confidence: confidence(p),
        },
        Question::Noul { .. } => Answer::Noul {
            p: p.get(1).copied().unwrap_or(0.0),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDS: Special = Special {
        cls: 1,
        sep: 2,
        mask: 3,
        pad: 0,
    };

    /// One id per character (10 + byte), so lengths are easy to reason about.
    fn enc(t: &str) -> Vec<u32> {
        t.bytes().map(|b| 10 + b as u32).collect()
    }

    #[test]
    fn sequence_layout_matches_the_reference() {
        let q = Question::noul("ok?");
        let (seq, markers) = build_sequence(&enc, IDS, "state", &q, 512, 192);
        let head = enc("noul question: ok?");
        assert_eq!(seq[0], IDS.cls);
        assert_eq!(&seq[1..1 + head.len()], head.as_slice());
        assert_eq!(seq[1 + head.len()], IDS.sep);
        assert_eq!(markers.len(), 2);
        assert_eq!(markers[0], head.len() + 2);
        for m in &markers {
            assert_eq!(seq[*m], IDS.mask);
        }
        // Options are " "-prefixed and capped at 48 tokens.
        let o0 = enc(" false: no, the statement does not hold");
        assert_eq!(
            &seq[markers[0] + 1..markers[0] + 1 + o0.len().min(48)],
            &o0[..o0.len().min(48)]
        );
        assert!(seq.ends_with(&[&enc("state")[..], &[IDS.sep]].concat()));
    }

    #[test]
    fn long_state_is_cut_to_max_len_and_masks_scrubbed() {
        let q = Question::score("rate", &["low", "high"]);
        let state = format!("{}[MASK]", "x".repeat(2000));
        let (seq, markers) = build_sequence(&enc, IDS, &state, &q, 256, 192);
        assert_eq!(seq.len(), 256);
        assert_eq!(*seq.last().unwrap(), IDS.sep);
        // Only the option markers are [MASK]; the state's literal one is gone.
        assert_eq!(
            seq.iter().filter(|&&t| t == IDS.mask).count(),
            markers.len()
        );
    }

    #[test]
    fn many_options_shrink_evenly() {
        let opts: Vec<(String, &str)> = (0..30)
            .map(|i| (format!("option-{i}"), "a long description of this option"))
            .collect();
        let refs: Vec<(&str, &str)> = opts.iter().map(|(k, d)| (k.as_str(), *d)).collect();
        let q = Question::choice("which", &refs);
        let (_, markers) = build_sequence(&enc, IDS, "s", &q, 512, 192);
        assert_eq!(markers.len(), 30);
    }

    #[test]
    fn answers_follow_the_distribution() {
        let q = Question::score("r", &["a", "b", "c"]);
        let a = answer(&q, &[0.0, 0.5, 0.5]);
        assert_eq!(a.score(), Some(1.5));
        let c = Question::choice("r", &[("x", ""), ("y", "")]);
        assert_eq!(answer(&c, &[0.2, 0.8]).choice(), Some("y"));
        assert_eq!(answer(&Question::noul("n"), &[0.3, 0.7]).noul(), Some(0.7));
        let p = softmax(&[1.0, 1.0]);
        assert!((p[0] - 0.5).abs() < 1e-9);
        assert_eq!(size_bucket(2), "2");
        assert_eq!(size_bucket(8), "6-10");
    }

    /// Real model, when the bundle is on this machine:
    /// `cargo test --lib laya_real -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn laya_real() {
        let l = Laya::load().unwrap();
        let qs = vec![
            (
                "urgent",
                Question::noul("Does this message convey urgency?"),
            ),
            (
                "topic",
                Question::choice(
                    "What is this about?",
                    &[
                        ("billing", "payments, invoices"),
                        ("bug", "software defects"),
                        ("praise", "compliments"),
                    ],
                ),
            ),
        ];
        let t = std::time::Instant::now();
        let a = l
            .ask(
                "Help! My payouts have been failing for 3 days and customers are angry.",
                &qs,
            )
            .unwrap();
        eprintln!("{a:?} in {:?}", t.elapsed());
        assert!(a[0].noul().unwrap() > 0.5);
        assert_eq!(a[1].choice(), Some("billing"));
    }
}
