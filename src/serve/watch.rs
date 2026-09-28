//! Watch folder: a video dropped into the watched folder starts a job
//! with the saved watch options once its size stops changing. What is
//! already in the folder when watching starts (or the folder changes)
//! stays put; files that arrive while the app is closed run on the next
//! start. State lives in `watch.json` next to the settings.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use super::{start_job, AppState, Event, ServerMsg};

const POLL: Duration = Duration::from_secs(5);
const VIDEO: &[&str] = &["mp4", "mov", "mkv", "webm", "m4v", "avi"];

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct Seen {
    dir: String,
    on: bool,
    files: BTreeSet<String>,
}

/// Videos directly in `dir` with their sizes.
fn videos(dir: &Path) -> Vec<(String, u64)> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return vec![];
    };
    rd.filter_map(|e| e.ok())
        .filter_map(|e| {
            let p = e.path();
            let ext = p.extension()?.to_str()?.to_ascii_lowercase();
            let m = e.metadata().ok()?;
            (m.is_file() && VIDEO.contains(&ext.as_str()))
                .then(|| (p.display().to_string(), m.len()))
        })
        .collect()
}

/// New files whose size held since the last poll (they are marked seen);
/// `pending` carries this poll's sizes to the next.
fn step(
    seen: &mut Seen,
    pending: &mut HashMap<String, u64>,
    files: &[(String, u64)],
) -> Vec<String> {
    let mut ready = Vec::new();
    let mut next = HashMap::new();
    for (p, size) in files {
        if seen.files.contains(p) {
            continue;
        }
        if *size > 0 && pending.get(p) == Some(size) {
            seen.files.insert(p.clone());
            ready.push(p.clone());
        } else {
            next.insert(p.clone(), *size);
        }
    }
    *pending = next;
    ready.sort();
    ready
}

fn save(path: &Path, seen: &Seen) {
    if let Ok(t) = serde_json::to_string_pretty(seen) {
        let _ = std::fs::write(path, t);
    }
}

pub(super) async fn run(st: Arc<AppState>) {
    let path = st.data_dir.join("watch.json");
    let mut seen: Seen = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let mut pending: HashMap<String, u64> = HashMap::new();
    loop {
        tokio::time::sleep(POLL).await;
        let s = st.settings.lock().await.clone();
        let dir = match (&s.watch_dir, s.watch_on) {
            (Some(d), true) if Path::new(d).is_dir() => d.clone(),
            _ => {
                if seen.on {
                    seen.on = false;
                    save(&path, &seen);
                }
                pending.clear();
                continue;
            }
        };
        let files = videos(Path::new(&dir));
        if seen.dir != dir || !seen.on {
            // Watching starts now: what's already there stays put.
            seen = Seen {
                dir,
                on: true,
                files: files.iter().map(|f| f.0.clone()).collect(),
            };
            save(&path, &seen);
            pending.clear();
            tracing::info!(
                "watching {} ({} file(s) already there)",
                seen.dir,
                seen.files.len()
            );
            continue;
        }
        let ready = step(&mut seen, &mut pending, &files);
        if ready.is_empty() {
            continue;
        }
        save(&path, &seen);
        for f in ready {
            let name = Path::new(&f)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let (tone, title, body) = match start_job(&st, f.clone(), s.watch_options.clone()).await
            {
                Ok(_) => {
                    tracing::info!("watch folder: started {f}");
                    ("info", "Watch folder", format!("Started {name}"))
                }
                Err(e) => {
                    tracing::warn!("watch folder: {f}: {e}");
                    ("error", "Watch folder", format!("{name}: {e}"))
                }
            };
            let _ = st.bus.send(ServerMsg::Ev {
                ev: Event::Toast {
                    tone: tone.into(),
                    title: title.into(),
                    body,
                },
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_start_once_their_size_settles() {
        let mut seen = Seen {
            dir: "d".into(),
            on: true,
            files: ["d/old.mp4".to_string()].into(),
        };
        let mut pending = HashMap::new();
        let f = |p: &str, n: u64| (p.to_string(), n);
        // First sight: wait a poll.
        assert!(step(
            &mut seen,
            &mut pending,
            &[f("d/old.mp4", 9), f("d/new.mp4", 100)]
        )
        .is_empty());
        // Still growing: wait again.
        assert!(step(&mut seen, &mut pending, &[f("d/new.mp4", 250)]).is_empty());
        // Settled: go, once.
        assert_eq!(
            step(&mut seen, &mut pending, &[f("d/new.mp4", 250)]),
            ["d/new.mp4"]
        );
        assert!(step(&mut seen, &mut pending, &[f("d/new.mp4", 250)]).is_empty());
        // Empty files never start.
        step(&mut seen, &mut pending, &[f("d/empty.mp4", 0)]);
        assert!(step(&mut seen, &mut pending, &[f("d/empty.mp4", 0)]).is_empty());
    }

    #[test]
    fn only_videos_are_listed() {
        let dir = std::env::temp_dir().join(format!("dc-watch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.MP4"), b"x").unwrap();
        std::fs::write(dir.join("b.mp4.part"), b"x").unwrap();
        std::fs::write(dir.join("c.txt"), b"x").unwrap();
        let v = videos(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(v.len(), 1);
        assert!(v[0].0.ends_with("a.MP4"));
    }
}
