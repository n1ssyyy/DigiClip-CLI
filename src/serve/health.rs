//! The engine's health, probed in the background and kept in the server state.
//!
//! A probe starts ffmpeg and asks for the GPU (nvidia-smi, a WMI query); on a
//! cold or busy Windows PC that takes seconds, sometimes more than a minute.
//! So nothing that answers a client waits for one: a `hello` or an MCP status
//! reads the last result (or nothing, before the first probe finished) and, if
//! it is old, asks for a refresh that runs on its own and is pushed as an
//! `Event::Health` when it changed.
//!
//! One probe at a time. A background request while one runs is skipped (its
//! result is pushed anyway). A caller that needs a result newer than its own
//! request (`fresh`) queues exactly one follow-up probe, which every such
//! caller shares.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{broadcast, watch};

use super::{Event, Health, ServerMsg};

/// A stored result older than this is refreshed in the background by the next
/// `hello` / status.
pub(super) const STALE: Duration = Duration::from_secs(60);

type Probe = Arc<dyn Fn() -> Health + Send + Sync>;

#[derive(Default)]
struct State {
    last: Option<(Health, Instant)>,
    /// A runner task is probing right now.
    running: bool,
    /// A caller wants a probe that starts after its request: run one more
    /// when the current one ends.
    pending: bool,
    /// Sequence number of the latest probe started.
    started: u64,
}

struct Shared {
    probe: Probe,
    state: Mutex<State>,
    /// Sequence number of the latest probe finished.
    done: watch::Sender<u64>,
}

#[derive(Clone)]
pub(super) struct HealthCache {
    shared: Arc<Shared>,
}

impl Default for HealthCache {
    fn default() -> Self {
        Self::with_probe(super::probe_health)
    }
}

impl HealthCache {
    pub(super) fn with_probe(probe: impl Fn() -> Health + Send + Sync + 'static) -> Self {
        Self {
            shared: Arc::new(Shared {
                probe: Arc::new(probe),
                state: Mutex::new(State::default()),
                done: watch::channel(0).0,
            }),
        }
    }

    /// The last result, with no probe and no waiting. `None` before the first
    /// probe finished.
    pub(super) fn last(&self) -> Option<Health> {
        let s = self.shared.state.lock().unwrap();
        s.last.as_ref().map(|(h, _)| h.clone())
    }

    /// What a `hello` or a status answers with: the last result at once, and
    /// a background refresh started if there is none or it is old.
    pub(super) fn known(&self, bus: &broadcast::Sender<ServerMsg>) -> Option<Health> {
        let (last, current) = {
            let s = self.shared.state.lock().unwrap();
            match &s.last {
                Some((h, at)) => (Some(h.clone()), at.elapsed() < STALE),
                None => (None, false),
            }
        };
        if !current {
            self.refresh_in_background(bus);
        }
        last
    }

    /// Start a probe unless one is running (then this joins it: its result
    /// is pushed when it ends). Returns whether a new probe started. The
    /// result is stored and, when it differs from the stored one or is the
    /// first, broadcast as `Event::Health`.
    pub(super) fn refresh_in_background(&self, bus: &broadcast::Sender<ServerMsg>) -> bool {
        {
            let mut s = self.shared.state.lock().unwrap();
            if s.running {
                return false;
            }
            s.running = true;
            s.started += 1;
        }
        self.spawn_runner(bus.clone());
        true
    }

    /// A result from a probe that started after this call (not one already
    /// running). Callers at the same time share one follow-up probe. `None`
    /// if the probe itself failed.
    pub(super) async fn fresh(&self, bus: &broadcast::Sender<ServerMsg>) -> Option<Health> {
        let (want, start) = {
            let mut s = self.shared.state.lock().unwrap();
            if s.running {
                s.pending = true;
                (s.started + 1, false)
            } else {
                s.running = true;
                s.started += 1;
                (s.started, true)
            }
        };
        let mut done = self.shared.done.subscribe();
        if start {
            self.spawn_runner(bus.clone());
        }
        done.wait_for(|d| *d >= want).await.ok()?;
        self.last()
    }

    fn spawn_runner(&self, bus: broadcast::Sender<ServerMsg>) {
        let me = self.clone();
        tokio::spawn(async move {
            loop {
                let probe = me.shared.probe.clone();
                let probed = tokio::task::spawn_blocking(move || probe()).await;
                let mut s = me.shared.state.lock().unwrap();
                let Ok(h) = probed else {
                    tracing::warn!("health probe failed");
                    s.running = false;
                    // Callers waiting for a follow-up probe are released too
                    // (they answer with what is stored), not left hanging.
                    if std::mem::take(&mut s.pending) {
                        s.started += 1;
                    }
                    let seq = s.started;
                    drop(s);
                    me.shared.done.send_replace(seq);
                    return;
                };
                let changed = s.last.as_ref().map(|(p, _)| p) != Some(&h);
                s.last = Some((h.clone(), Instant::now()));
                let again = std::mem::take(&mut s.pending);
                let finished = s.started;
                if again {
                    s.started += 1;
                } else {
                    s.running = false;
                }
                drop(s);
                me.shared.done.send_replace(finished);
                if changed {
                    let _ = bus.send(ServerMsg::Ev {
                        ev: Event::Health { health: h },
                    });
                }
                if !again {
                    return;
                }
            }
        });
    }
}

#[cfg(test)]
impl HealthCache {
    /// A cache whose probe answers at once with a fixed, fake result.
    pub(super) fn instant() -> Self {
        Self::with_probe(|| Health::fake("instant"))
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;

    /// A cache whose probe waits for a `()` on the returned sender, counts
    /// how many probes started and tags each result `probe-<n>`.
    pub(in crate::serve) fn held_probe() -> (HealthCache, Arc<AtomicUsize>, mpsc::Sender<()>) {
        let (release, wait) = mpsc::channel::<()>();
        let wait = Mutex::new(wait);
        let calls = Arc::new(AtomicUsize::new(0));
        let n = calls.clone();
        let cache = HealthCache::with_probe(move || {
            let me = n.fetch_add(1, Ordering::SeqCst) + 1;
            let _ = wait.lock().unwrap().recv();
            Health::fake(&format!("probe-{me}"))
        });
        (cache, calls, release)
    }

    async fn until(what: &str, mut f: impl FnMut() -> bool) {
        for _ in 0..500 {
            if f() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("never happened: {what}");
    }

    fn bus() -> (broadcast::Sender<ServerMsg>, broadcast::Receiver<ServerMsg>) {
        broadcast::channel(16)
    }

    #[tokio::test]
    async fn two_refreshes_while_one_runs_probe_once() {
        let (cache, calls, release) = held_probe();
        let (tx, mut rx) = bus();
        assert!(cache.refresh_in_background(&tx));
        until("the probe started", || calls.load(Ordering::SeqCst) == 1).await;
        // While it runs: skipped, and `known` answers at once with nothing.
        assert!(!cache.refresh_in_background(&tx));
        assert!(!cache.refresh_in_background(&tx));
        assert!(cache.known(&tx).is_none());
        assert!(cache.last().is_none());
        release.send(()).unwrap();
        let ServerMsg::Ev {
            ev: Event::Health { health },
        } = rx.recv().await.unwrap()
        else {
            panic!("expected the health event");
        };
        assert_eq!(health.version, "probe-1");
        assert_eq!(cache.last().unwrap().version, "probe-1");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // Fresh result stored: no new probe from `known`, no second event.
        assert_eq!(cache.known(&tx).unwrap().version, "probe-1");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn fresh_callers_share_one_follow_up_probe() {
        let (cache, calls, release) = held_probe();
        let (tx, _rx) = bus();
        assert!(cache.refresh_in_background(&tx));
        until("the probe started", || calls.load(Ordering::SeqCst) == 1).await;
        let a = tokio::spawn({
            let (c, t) = (cache.clone(), tx.clone());
            async move { c.fresh(&t).await }
        });
        let b = tokio::spawn({
            let (c, t) = (cache.clone(), tx.clone());
            async move { c.fresh(&t).await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        // Neither is satisfied by the probe that was already running.
        assert!(!a.is_finished() && !b.is_finished());
        release.send(()).unwrap();
        release.send(()).unwrap();
        let (a, b) = (a.await.unwrap().unwrap(), b.await.unwrap().unwrap());
        assert_eq!(
            (a.version.as_str(), b.version.as_str()),
            ("probe-2", "probe-2")
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        // Idle again: the next refresh starts a new probe.
        assert!(cache.refresh_in_background(&tx));
        release.send(()).unwrap();
        until("the third probe", || calls.load(Ordering::SeqCst) == 3).await;
    }

    #[tokio::test]
    async fn an_unchanged_result_is_not_pushed_again() {
        let cache = HealthCache::instant();
        let (tx, mut rx) = bus();
        assert!(cache.fresh(&tx).await.is_some());
        assert!(matches!(
            rx.try_recv(),
            Ok(ServerMsg::Ev {
                ev: Event::Health { .. }
            })
        ));
        assert!(cache.fresh(&tx).await.is_some());
        assert!(rx.try_recv().is_err());
    }
}
