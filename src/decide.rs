//! System One decisions: typed questions about a piece of text answered
//! with calibrated probabilities in one fast pass, instead of generated
//! prose. Two backends speak the same request shape:
//!
//! - **Jev** (TypeSafe AI): hosted, needs an API key, any language.
//! - **Laya** (Convai Innovations, Apache-2.0): a 421M ModernBERT model run
//!   locally through ONNX Runtime once its bundle is downloaded (English).
//!
//! The pipeline uses them where a yes/no, a score or a pick from a list is
//! the whole answer: ranking clip candidates, "why this clip", focus
//! checks and the caption look (see [`clips`]).

pub mod clips;
pub mod jev;
pub mod laya;

use std::sync::Arc;

/// One typed question.
#[derive(Debug, Clone)]
pub enum Question {
    /// Pick one option: `(key, description)`.
    Choice {
        instructions: String,
        options: Vec<(String, Option<String>)>,
    },
    /// Place the state on an ordered rubric (index 0 = lowest).
    Score {
        instructions: String,
        levels: Vec<String>,
    },
    /// Is the statement true? (`noul` in both APIs.)
    Noul { instructions: String },
}

impl Question {
    pub fn noul(instructions: &str) -> Self {
        Self::Noul {
            instructions: instructions.into(),
        }
    }

    pub fn score(instructions: &str, levels: &[&str]) -> Self {
        Self::Score {
            instructions: instructions.into(),
            levels: levels.iter().map(|l| l.to_string()).collect(),
        }
    }

    pub fn choice(instructions: &str, options: &[(&str, &str)]) -> Self {
        Self::Choice {
            instructions: instructions.into(),
            options: options
                .iter()
                .map(|(k, d)| (k.to_string(), (!d.is_empty()).then(|| d.to_string())))
                .collect(),
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            Self::Choice { .. } => "choice",
            Self::Score { .. } => "score",
            Self::Noul { .. } => "noul",
        }
    }

    fn instructions(&self) -> &str {
        match self {
            Self::Choice { instructions, .. }
            | Self::Score { instructions, .. }
            | Self::Noul { instructions } => instructions,
        }
    }
}

/// The typed answer to one [`Question`].
#[derive(Debug, Clone, PartialEq)]
pub enum Answer {
    Choice {
        choice: String,
        confidence: f64,
    },
    /// Expected level, `0..levels-1`.
    Score {
        score: f64,
        confidence: f64,
    },
    /// P(true).
    Noul {
        p: f64,
    },
}

impl Answer {
    pub fn noul(&self) -> Option<f64> {
        match self {
            Self::Noul { p } => Some(*p),
            _ => None,
        }
    }

    pub fn score(&self) -> Option<f64> {
        match self {
            Self::Score { score, .. } => Some(*score),
            _ => None,
        }
    }

    pub fn choice(&self) -> Option<&str> {
        match self {
            Self::Choice { choice, .. } => Some(choice),
            _ => None,
        }
    }
}

/// `--decider`: which System One model judges clips.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Pick {
    /// Jev when a key is set, else Laya when downloaded and the talk is
    /// English, else none.
    Auto,
    /// TypeSafe's hosted Jev (needs `--jev-key` or `JEV_API_KEY`).
    Jev,
    /// Local Laya on the CPU (English; downloads ~1.6 GB on first use).
    Laya,
    /// Keep the LLM or heuristic picks as they are.
    Off,
}

pub enum Decider {
    Jev(jev::Client),
    Laya(Arc<laya::Laya>),
}

impl Decider {
    /// `jev:jev-latest` / `laya`, for logs and the pick source.
    pub fn label(&self) -> String {
        match self {
            Self::Jev(c) => format!("jev:{}", c.model),
            Self::Laya(_) => "laya".into(),
        }
    }

    /// Answer `questions` about `state`, in order.
    pub async fn ask(
        &self,
        state: &str,
        questions: &[(&'static str, Question)],
    ) -> anyhow::Result<Vec<Answer>> {
        match self {
            Self::Jev(c) => c.ask(state, questions).await,
            Self::Laya(l) => {
                let (l, state, qs) = (l.clone(), state.to_string(), questions.to_vec());
                tokio::task::spawn_blocking(move || l.ask(&state, &qs)).await?
            }
        }
    }

    /// The decider `pick` asks for, or none. An explicit `laya` downloads
    /// the bundle (~1.6 GB, once) when it is missing; `auto` never
    /// downloads and skips Laya for talks that aren't English.
    pub async fn resolve(
        pick: Pick,
        jev_key: Option<String>,
        jev_model: Option<String>,
        lang: &str,
    ) -> Option<Self> {
        let key = jev_key
            .filter(|k| !k.trim().is_empty())
            .or_else(|| std::env::var("JEV_API_KEY").ok())
            .filter(|k| !k.trim().is_empty());
        let english = lang.is_empty() || lang.starts_with("en");
        let load_laya = || async {
            match laya::shared().await {
                Ok(l) => Some(Self::Laya(l)),
                Err(e) => {
                    tracing::warn!("Laya failed to load ({e:#}) — no System One judging");
                    None
                }
            }
        };
        match pick {
            Pick::Off => None,
            Pick::Jev => match key {
                Some(k) => Some(Self::Jev(jev::Client::new(k, jev_model))),
                None => {
                    tracing::warn!("--decider jev needs --jev-key or JEV_API_KEY — skipping");
                    None
                }
            },
            Pick::Laya => {
                if !laya::is_ready() {
                    tracing::info!("Laya bundle missing — downloading once (~1.6 GB)…");
                    if let Err(e) = laya::download(None).await {
                        tracing::warn!("Laya download failed ({e:#}) — no System One judging");
                        return None;
                    }
                }
                if !english {
                    tracing::warn!(
                        "Laya is trained on English; judging a [{lang}] talk may be rough"
                    );
                }
                load_laya().await
            }
            Pick::Auto => {
                if let Some(k) = key {
                    Some(Self::Jev(jev::Client::new(k, jev_model)))
                } else if laya::is_ready() && english {
                    load_laya().await
                } else {
                    None
                }
            }
        }
    }
}

/// `1 - normalized entropy`: 1 = certain, 0 = uniform (Jev's confidence).
pub(crate) fn confidence(p: &[f64]) -> f64 {
    let k = p.len();
    if k < 2 {
        return 1.0;
    }
    let ent: f64 = p.iter().map(|x| -x * x.max(1e-12).ln()).sum();
    1.0 - ent / (k as f64).ln()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confidence_spans_certain_to_uniform() {
        assert!((confidence(&[1.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!(confidence(&[0.5, 0.5]).abs() < 1e-9);
        assert!(confidence(&[0.9, 0.05, 0.05]) > 0.5);
        assert_eq!(confidence(&[1.0]), 1.0);
    }

    #[test]
    fn answers_read_back_by_type() {
        assert_eq!(Answer::Noul { p: 0.7 }.noul(), Some(0.7));
        assert_eq!(Answer::Noul { p: 0.7 }.score(), None);
        let c = Answer::Choice {
            choice: "neon".into(),
            confidence: 0.4,
        };
        assert_eq!(c.choice(), Some("neon"));
        assert_eq!(Question::noul("x").kind(), "noul");
        assert_eq!(
            Question::choice("pick", &[("a", ""), ("b", "bee")]).kind(),
            "choice"
        );
    }
}
