//! Jev (TypeSafe AI): `POST {base}/v1/systemone` with a bearer key.
//! `TYPESAFE_BASE_URL` overrides the base (tests point it at a mock).

use super::{Answer, Question};
use std::time::Duration;

const BASE: &str = "https://api.typesafe.ai";
const MODEL: &str = "jev-latest";

pub struct Client {
    base: String,
    key: String,
    pub model: String,
    http: reqwest::Client,
}

impl Client {
    pub fn new(key: String, model: Option<String>) -> Self {
        Self {
            base: std::env::var("TYPESAFE_BASE_URL")
                .ok()
                .filter(|b| !b.trim().is_empty())
                .unwrap_or_else(|| BASE.into())
                .trim_end_matches('/')
                .to_string(),
            key: key.trim().to_string(),
            model: model
                .filter(|m| !m.trim().is_empty())
                .unwrap_or_else(|| MODEL.into()),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_default(),
        }
    }

    pub async fn ask(
        &self,
        state: &str,
        questions: &[(&'static str, Question)],
    ) -> anyhow::Result<Vec<Answer>> {
        let body = request(state, &self.model, questions);
        let url = format!("{}/v1/systemone", self.base);
        let mut wait = Duration::from_millis(800);
        for attempt in 0..4 {
            let resp = self
                .http
                .post(&url)
                .bearer_auth(&self.key)
                .json(&body)
                .send()
                .await;
            let resp = match resp {
                Ok(r) => r,
                Err(e) if attempt < 3 => {
                    tracing::debug!("jev: {e} — retrying");
                    tokio::time::sleep(wait).await;
                    wait *= 2;
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            let status = resp.status().as_u16();
            match status {
                200..=299 => {
                    let v: serde_json::Value = resp.json().await?;
                    return parse(&v, questions);
                }
                401 | 403 => anyhow::bail!("Jev refused the key (HTTP {status})"),
                429 | 500..=599 if attempt < 3 => {
                    tokio::time::sleep(wait).await;
                    wait *= 2;
                }
                _ => {
                    let text = resp.text().await.unwrap_or_default();
                    anyhow::bail!(
                        "Jev HTTP {status}: {}",
                        text.chars().take(300).collect::<String>()
                    );
                }
            }
        }
        anyhow::bail!("Jev kept failing")
    }
}

fn request(state: &str, model: &str, questions: &[(&'static str, Question)]) -> serde_json::Value {
    let mut qs = serde_json::Map::new();
    for (name, q) in questions {
        let mut o = serde_json::json!({
            "type": q.kind(),
            "instructions": q.instructions(),
        });
        match q {
            Question::Choice { options, .. } => {
                let crit: serde_json::Map<String, serde_json::Value> = options
                    .iter()
                    .map(|(k, d)| {
                        (
                            k.clone(),
                            d.clone().map_or(serde_json::Value::Null, Into::into),
                        )
                    })
                    .collect();
                o["criteria"] = crit.into();
            }
            Question::Score { levels, .. } => o["criteria"] = serde_json::json!(levels),
            Question::Noul { .. } => {}
        }
        qs.insert(name.to_string(), o);
    }
    serde_json::json!({ "state": state, "model": model, "questions": qs })
}

fn parse(
    v: &serde_json::Value,
    questions: &[(&'static str, Question)],
) -> anyhow::Result<Vec<Answer>> {
    let answers = v
        .get("answers")
        .ok_or_else(|| anyhow::anyhow!("Jev reply has no answers"))?;
    questions
        .iter()
        .map(|(name, q)| {
            let a = answers
                .get(*name)
                .ok_or_else(|| anyhow::anyhow!("Jev skipped question [{name}]"))?;
            let num = |k: &str| a.get(k).and_then(|x| x.as_f64());
            let conf = num("confidence").unwrap_or(0.0);
            Ok(match q {
                Question::Choice { options, .. } => {
                    let c = a
                        .get("choice")
                        .and_then(|c| c.as_str())
                        .filter(|c| options.iter().any(|(k, _)| k == c))
                        .ok_or_else(|| anyhow::anyhow!("Jev gave no valid choice for [{name}]"))?;
                    Answer::Choice {
                        choice: c.into(),
                        confidence: conf,
                    }
                }
                Question::Score { levels, .. } => Answer::Score {
                    score: num("score")
                        .ok_or_else(|| anyhow::anyhow!("Jev gave no score for [{name}]"))?
                        .clamp(0.0, levels.len().saturating_sub(1) as f64),
                    confidence: conf,
                },
                Question::Noul { .. } => Answer::Noul {
                    p: num("noul")
                        .ok_or_else(|| anyhow::anyhow!("Jev gave no noul for [{name}]"))?
                        .clamp(0.0, 1.0),
                },
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn qs() -> Vec<(&'static str, Question)> {
        vec![
            (
                "hook",
                Question::score("How strong?", &["weak", "ok", "strong"]),
            ),
            ("alone", Question::noul("Stands alone.")),
            (
                "look",
                Question::choice("Which look?", &[("neon", "gaming"), ("minimal", "")]),
            ),
        ]
    }

    #[test]
    fn request_matches_the_api_shape() {
        let v = request("hello", "jev-latest", &qs());
        assert_eq!(v["model"], "jev-latest");
        assert_eq!(v["state"], "hello");
        assert_eq!(v["questions"]["hook"]["type"], "score");
        assert_eq!(v["questions"]["hook"]["criteria"][2], "strong");
        assert_eq!(v["questions"]["alone"]["type"], "noul");
        assert!(v["questions"]["alone"].get("criteria").is_none());
        assert_eq!(v["questions"]["look"]["criteria"]["neon"], "gaming");
        assert!(v["questions"]["look"]["criteria"]["minimal"].is_null());
    }

    #[test]
    fn replies_parse_in_question_order() {
        let v = serde_json::json!({
            "model": "jev-1.13.0",
            "answers": {
                "look": {"type": "choice", "choice": "neon", "confidence": 0.6},
                "alone": {"type": "noul", "noul": 0.91},
                "hook": {"type": "score", "score": 1.7, "confidence": 0.4}
            }
        });
        let a = parse(&v, &qs()).unwrap();
        assert_eq!(a[0].score(), Some(1.7));
        assert_eq!(a[1].noul(), Some(0.91));
        assert_eq!(a[2].choice(), Some("neon"));
        // A choice outside the options is an error, not a silent pick.
        let bad = serde_json::json!({"answers": {
            "hook": {"score": 1}, "alone": {"noul": 0.5}, "look": {"choice": "comic"}
        }});
        assert!(parse(&bad, &qs()).is_err());
    }
}
