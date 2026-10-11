//! First-run setup: what DigiClip needs from the internet, fetched by the
//! engine itself.
//!
//! Once the engine is up it looks for the five things below and downloads
//! the ones that are missing, smallest first, one at a time. Nobody has to
//! ask, and a job never has to be the one that finds out:
//!
//! | id       | what                                   | size      |
//! |----------|----------------------------------------|-----------|
//! | `face`   | the face model smart framing follows   | 0.2 MB    |
//! | `ffmpeg` | ffmpeg + ffprobe, when none is found   | ~60-115 MB|
//! | `stt`    | the speech model picked in Settings    | 75 MB+    |
//! | `yt-dlp` | link import                            | ~18 MB    |
//! | `laya`   | the clip judge that runs on this PC    | 1.6 GB    |
//!
//! Laya is only fetched while the judge setting can use it ("auto" without a
//! Jev key, or "laya"), and never again once the creator removed it in
//! Settings (downloading it there brings it back).
//!
//! Every download goes through [`crate::download`], so a dropped connection
//! is resumed. When one still fails for good (offline), the pass ends, the
//! item says why, and the next pass tries again: after 20 s, then doubling
//! up to five minutes, or at once on `setup_retry` and on a settings change.
//!
//! The app hears about it as one `setup` event with every item's state, sent
//! whenever something changed (a `hello` carries the same list; `null` until
//! the first look at the disk is done).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Notify;

use super::{AppState, Event, ServerMsg, Settings, LAYA};
use crate::download::Hooks;

#[derive(Clone, Copy, Debug)]
enum Tool {
    Face,
    Ffmpeg,
    YtDlp,
}

impl Tool {
    const ALL: [Tool; 3] = [Tool::Face, Tool::Ffmpeg, Tool::YtDlp];

    fn id(self) -> &'static str {
        match self {
            Tool::Face => "face",
            Tool::Ffmpeg => "ffmpeg",
            Tool::YtDlp => "yt-dlp",
        }
    }

    fn size_mb(self) -> u64 {
        match self {
            Tool::Face => 1,
            Tool::Ffmpeg if cfg!(windows) => 115,
            Tool::Ffmpeg => 60,
            Tool::YtDlp if cfg!(windows) => 18,
            Tool::YtDlp => 35,
        }
    }

    /// Is it on this machine? Looks at the disk and PATH: call it off the
    /// async threads.
    fn ready(self) -> bool {
        match self {
            Tool::Face => crate::provision::yunet_ready(),
            Tool::Ffmpeg => crate::binaries::resolve("ffmpeg").is_some(),
            Tool::YtDlp => crate::fetch::tool().is_some(),
        }
    }
}

#[derive(Default, Clone)]
struct ToolRun {
    ready: bool,
    downloading: bool,
    pct: Option<u8>,
    error: Option<String>,
}

/// One thing setup looks after, as the app sees it.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub(super) struct Item {
    /// "face" | "ffmpeg" | "stt" | "yt-dlp" | "laya"
    id: &'static str,
    /// The speech model's id (`stt` only).
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    size_mb: u64,
    /// "ready" | "downloading" | "waiting" (downloading, and waiting for the
    /// connection to try again) | "failed" (it tries again later) |
    /// "missing" (its turn has not come yet)
    state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pct: Option<u8>,
    /// Why it waits or failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    why: Option<String>,
}

#[derive(Default)]
pub(super) struct Setup {
    tools: Mutex<HashMap<&'static str, ToolRun>>,
    /// Downloads waiting to try again (tool id or model id → why).
    waiting: Mutex<HashMap<String, String>>,
    /// The first look at the disk is done.
    checked: AtomicBool,
    /// Something an item is made of changed.
    dirty: Notify,
    /// Run a pass now.
    kick: Notify,
    /// What the app was last told.
    last: Mutex<Option<Vec<Item>>>,
}

impl Setup {
    /// Something changed: tell the app (soon, and only if it shows).
    pub(super) fn touch(&self) {
        self.dirty.notify_one();
    }

    /// Look again now (a retry, or a setting that changes what is needed).
    pub(super) fn kick(&self) {
        self.kick.notify_one();
        self.touch();
    }

    pub(super) fn set_waiting(&self, id: &str, why: Option<&str>) {
        let mut w = self.waiting.lock().unwrap_or_else(|e| e.into_inner());
        let changed = match why {
            Some(why) => w.insert(id.to_string(), why.to_string()).as_deref() != Some(why),
            None => w.remove(id).is_some(),
        };
        drop(w);
        if changed {
            self.touch();
        }
    }

    fn update(&self, tool: Tool, f: impl FnOnce(&mut ToolRun)) {
        f(self
            .tools
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(tool.id())
            .or_default());
        self.touch();
    }
}

/// Removing Laya in Settings leaves this behind: setup does not fetch it
/// again until the creator downloads it there.
pub(super) fn laya_off() -> PathBuf {
    crate::models::dir().join("laya.off")
}

/// Can the judge setting use Laya at all?
fn can_use_laya(s: &Settings) -> bool {
    match s.decider.as_str() {
        "laya" => true,
        // "auto" judges with Jev when there is a key, else with Laya.
        "auto" => s.jev_key.as_ref().is_none_or(|k| k.trim().is_empty()),
        _ => false,
    }
}

fn wants_laya(s: &Settings) -> bool {
    can_use_laya(s) && !laya_off().exists()
}

fn state_of(ready: bool, downloading: bool, waiting: bool, failed: bool) -> &'static str {
    match (ready, downloading, waiting, failed) {
        (true, ..) => "ready",
        (_, true, true, _) => "waiting",
        (_, true, ..) => "downloading",
        (.., true) => "failed",
        _ => "missing",
    }
}

/// Every item's state. `None` until the first look at the disk is done.
pub(super) async fn items(st: &AppState) -> Option<Vec<Item>> {
    if !st.setup.checked.load(Ordering::SeqCst) {
        return None;
    }
    let (stt, want_laya) = {
        let s = st.settings.lock().await;
        (s.stt_model.clone(), wants_laya(&s))
    };
    let models = {
        let runs = st.model_runs.lock().await;
        st.models_state(&runs)
    };
    let tools = st
        .setup
        .tools
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let waiting = st
        .setup
        .waiting
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let tool = |t: Tool| {
        let r = tools.get(t.id()).cloned().unwrap_or_default();
        let wait = waiting.get(t.id());
        Item {
            id: t.id(),
            model: None,
            size_mb: t.size_mb(),
            state: state_of(r.ready, r.downloading, wait.is_some(), r.error.is_some()),
            pct: r.pct.filter(|_| r.downloading && !r.ready),
            why: wait.cloned().or(r.error).filter(|_| !r.ready),
        }
    };
    let model = |id: &'static str, mid: &str| {
        let m = models.get(mid)?;
        let wait = waiting.get(mid);
        Some(Item {
            id,
            model: (id == "stt").then(|| mid.to_string()),
            size_mb: m.size_mb,
            state: state_of(
                m.downloaded,
                m.downloading,
                wait.is_some(),
                m.error.is_some(),
            ),
            pct: m.progress.filter(|_| m.downloading && !m.downloaded),
            why: wait
                .cloned()
                .or_else(|| m.error.clone())
                .filter(|_| !m.downloaded),
        })
    };
    let mut out = vec![tool(Tool::Face), tool(Tool::Ffmpeg)];
    out.extend(model("stt", &stt));
    out.push(tool(Tool::YtDlp));
    if want_laya || models.get(LAYA).is_some_and(|m| m.downloaded) {
        out.extend(model(LAYA, LAYA));
    }
    Some(out)
}

/// Tell the app whenever the list changed (forever).
pub(super) async fn announce(st: Arc<AppState>) {
    loop {
        st.setup.dirty.notified().await;
        if let Some(now) = items(&st).await {
            let changed = {
                let mut last = st.setup.last.lock().unwrap_or_else(|e| e.into_inner());
                let changed = last.as_ref() != Some(&now);
                if changed {
                    *last = Some(now.clone());
                }
                changed
            };
            if changed {
                let _ = st.bus.send(ServerMsg::Ev {
                    ev: Event::Setup { setup: now },
                });
            }
        }
        // A burst of changes (percent ticks) is one message.
        tokio::time::sleep(Duration::from_millis(120)).await;
    }
}

/// Fetch whatever is missing, now and whenever something is (forever).
pub(super) async fn run(st: Arc<AppState>) {
    const FIRST: Duration = Duration::from_secs(20);
    const MAX: Duration = Duration::from_secs(300);
    let mut wait = FIRST;
    loop {
        if pass(&st).await {
            wait = FIRST;
            st.setup.kick.notified().await;
        } else {
            tokio::select! {
                _ = tokio::time::sleep(wait) => wait = (wait * 2).min(MAX),
                _ = st.setup.kick.notified() => wait = FIRST,
            }
        }
    }
}

/// One look at everything, downloading what is missing. `true` when nothing
/// is missing any more.
async fn pass(st: &Arc<AppState>) -> bool {
    let ready = tokio::task::spawn_blocking(|| Tool::ALL.map(|t| (t, t.ready())))
        .await
        .unwrap_or_else(|_| Tool::ALL.map(|t| (t, false)));
    for (t, r) in ready {
        st.setup.update(t, |x| {
            x.ready = r;
            if r {
                x.error = None;
            }
        });
    }
    st.setup.checked.store(true, Ordering::SeqCst);
    st.setup.touch();

    let is_ready = |t: Tool| ready.iter().any(|(x, r)| x.id() == t.id() && *r);
    let mut all = true;
    for t in [Tool::Face, Tool::Ffmpeg] {
        if !is_ready(t) {
            all &= fetch_tool(st, t).await;
        }
    }
    let stt = st.settings.lock().await.stt_model.clone();
    if !crate::models::is_downloaded(&stt) {
        all &= fetch_model(st, &stt).await;
    }
    if !is_ready(Tool::YtDlp) {
        all &= fetch_tool(st, Tool::YtDlp).await;
    }
    let want_laya = {
        let s = st.settings.lock().await;
        wants_laya(&s)
    };
    if want_laya && !crate::decide::laya::is_ready() {
        all &= fetch_model(st, LAYA).await;
    }
    all
}

async fn fetch_tool(st: &Arc<AppState>, tool: Tool) -> bool {
    st.setup.update(tool, |t| {
        t.downloading = true;
        t.pct = None;
        t.error = None;
    });
    let (stp, stw) = (st.clone(), st.clone());
    let last = std::sync::atomic::AtomicU8::new(u8::MAX);
    let progress = move |done: u64, total: u64| {
        let pct = done
            .saturating_mul(100)
            .checked_div(total)
            .unwrap_or(0)
            .min(100) as u8;
        if last.swap(pct, Ordering::Relaxed) != pct {
            stp.setup.update(tool, |t| t.pct = Some(pct));
        }
    };
    let status = move |why: Option<&str>| stw.setup.set_waiting(tool.id(), why);
    let hooks = Hooks {
        progress: Some(&progress),
        cancel: None,
        status: Some(&status),
    };
    let res = match tool {
        Tool::Face => crate::provision::ensure_yunet_with(hooks).await.map(drop),
        Tool::Ffmpeg => crate::provision::ensure_ffmpeg_with(hooks).await.map(drop),
        Tool::YtDlp => crate::fetch::ensure_tool(hooks).await.map(drop),
    };
    if let Err(e) = &res {
        tracing::warn!("setup: {} is not there yet: {e:#}", tool.id());
    }
    st.setup.set_waiting(tool.id(), None);
    st.setup.update(tool, |t| {
        t.downloading = false;
        t.pct = None;
        t.ready = res.is_ok();
        t.error = res
            .as_ref()
            .err()
            .map(|e| super::truncate(&e.to_string(), 300));
    });
    if res.is_ok() {
        // What health says about ffmpeg and the face model just changed.
        let st = st.clone();
        tokio::spawn(async move {
            if let Some(health) = st.health.fresh(&st.bus).await {
                let _ = st.bus.send(ServerMsg::Ev {
                    ev: Event::Health { health },
                });
            }
        });
    }
    res.is_ok()
}

/// A speech model or Laya, through the same run the Settings page starts
/// (so it shows there too). Waits for one that is already going.
async fn fetch_model(st: &Arc<AppState>, mid: &str) -> bool {
    if super::begin_model_download(st, mid).await.is_ok() {
        return super::download_model(st.clone(), mid.to_string(), false).await;
    }
    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let runs = st.model_runs.lock().await;
        if !runs.get(mid).is_some_and(|r| r.downloading) {
            return st
                .models_state(&runs)
                .get(mid)
                .is_some_and(|m| m.downloaded);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_item_says_the_most_useful_thing_about_itself() {
        assert_eq!(state_of(true, true, true, true), "ready");
        assert_eq!(state_of(false, true, true, false), "waiting");
        assert_eq!(state_of(false, true, false, true), "downloading");
        assert_eq!(state_of(false, false, false, true), "failed");
        assert_eq!(state_of(false, false, false, false), "missing");
    }

    #[test]
    fn laya_is_wanted_only_while_the_judge_can_use_it() {
        let mut s = Settings {
            decider: "auto".into(),
            jev_key: None,
            ..Settings::default()
        };
        assert!(can_use_laya(&s));
        s.jev_key = Some("  ".into());
        assert!(can_use_laya(&s));
        s.jev_key = Some("jev-key".into());
        assert!(!can_use_laya(&s));
        s.decider = "laya".into();
        assert!(can_use_laya(&s));
        s.decider = "off".into();
        assert!(!can_use_laya(&s));
        s.decider = "jev".into();
        assert!(!can_use_laya(&s));
    }
}
