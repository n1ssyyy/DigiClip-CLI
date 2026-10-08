//! Virtual camera planner: per-sample framing targets → one camera rect
//! per output frame.
//!
//! The whole clip is known before the first frame renders, so the camera
//! is planned offline like an editor would, not chased like a live
//! follower:
//!
//! 1. **Evidence.** Real sightings only: placeholders (dropouts, post-cut
//!    settles) carry no information and are bridged, never copied.
//!    Speaker handoffs are moved back to the new speaker's first word.
//! 2. **Dense desired signal.** Linear between sightings, stepped at
//!    framing changes, median-filtered against spikes, with a confidence
//!    weight that fades inside long gaps.
//! 3. **Snaps.** Some moves are cuts, not pans: switches between people
//!    who don't share a frame (landing on the first word), layout changes
//!    (close-up <-> letterboxed wide), and any reframe near an edit
//!    jump-cut (the edit hides it). Framings that would only blink on
//!    screen before a cut are skipped.
//! 4. **Path.** Between snaps each channel (pan, tilt, zoom) is the
//!    smoothest path that keeps the subject inside a dead band around the
//!    framing ([`solve_band`]): detection jitter and fidgeting cost
//!    nothing so the camera stays locked off; a subject that walks is
//!    followed by one continuous, eased move that starts early and lands
//!    softly — never stop-and-go. A stiff safe band caps how far a fast
//!    subject may lead the camera.
//!
//! Emphasis punch-ins are a separate layer ([`apply_punches`]): a fast
//! eased zoom anchored on the speaker's face (the face stays put on
//! screen, the frame tightens around it), held through the phrase, then
//! released — never mixed into the framing path, so they can't be mistaken
//! for subject motion.

use crate::compose::Rect;
use crate::look::CameraFeel;

/// What a target frames. Drives move choice and punch eligibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// One tracked speaker.
    Subject,
    /// A talking group sharing one window.
    Group,
    /// Nobody on camera: the whole frame, letterboxed.
    Wide,
    /// A fixed framing (center crop, VLM focus, external plan).
    Fixed,
}

/// One framing sample on the OUTPUT clock.
#[derive(Debug, Clone)]
pub struct Target {
    pub t: f64,
    /// Desired framing (source px).
    pub rect: Rect,
    /// Subject point (face center); punch-ins zoom around it.
    pub ax: f64,
    pub ay: f64,
    pub kind: Kind,
    /// Discrete framing change starts here (handoff, group edge).
    pub cut: bool,
    /// Placeholder, not evidence (dropout hold, silence hold, post-cut
    /// settle): the planner fills it from real samples around it.
    pub weak: bool,
}

/// One planned camera frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pose {
    pub rect: Rect,
    pub ax: f64,
    pub ay: f64,
    pub kind: Kind,
}

/// Planner tuning. Bands are fractions of the current window.
#[derive(Debug, Clone)]
pub struct CamCfg {
    /// Dead band: the subject moving within ±band of the framing costs
    /// nothing, so detection jitter and fidgeting never move the camera
    /// (x: fraction of window width, y: of window height).
    pub band_x: f64,
    pub band_y: f64,
    /// Zoom dead band (|ln size ratio|).
    pub band_z: f64,
    /// Safe band: the camera may trail a fast subject, but never by more
    /// than this (a stiff constraint, not a preference).
    pub safe_x: f64,
    pub safe_y: f64,
    /// Response time (s): the shortest period the camera reproduces for
    /// pans, tilts and zooms. Longer reads calmer; shorter follows tighter.
    pub pan_s: f64,
    pub tilt_s: f64,
    pub zoom_s: f64,
    /// Response time of the punch anchor (the face the zoom holds still).
    pub anchor_s: f64,
    /// Spike-killing median window on the desired signal (s).
    pub median_s: f64,
    /// Handoffs this far apart (fraction of window width) cut, not pan.
    pub switch_cut_frac: f64,
    /// Changes between a crop and the letterboxed wide cut instead of
    /// morphing (a layout change, like a multicam edit — not a zoom).
    pub layout_cut: bool,
    /// Reframes within this distance of an edit jump-cut snap onto it (s).
    pub jump_win_s: f64,
    /// Handoff cuts snap to an utterance onset within this lookback (s).
    pub onset_back_s: f64,
    /// Handoff glide duration = clamp(base + per * distance, min, max) (s).
    pub glide_base_s: f64,
    pub glide_per_s: f64,
    pub glide_min_s: f64,
    pub glide_max_s: f64,
    /// A framing must be on screen at least this long, or it is skipped
    /// (a blink of a framing right before a cut reads as a glitch) (s).
    pub min_show_s: f64,
    /// One framing per shot: the camera picks it and holds it (cuts between
    /// shots still reframe). Off = the planned path.
    pub hold_shot: bool,
}

impl CamCfg {
    /// Tuning for a Look's camera feel. `Smooth` (and no feel at all) is the
    /// default, untouched.
    ///
    /// - `Steady`: dead bands x1.8, safe bands x1.25, pan/tilt/zoom response
    ///   x1.5, handoff glides x1.3 longer.
    /// - `Lively`: dead bands x0.5, safe bands x0.8, response x0.5, glides
    ///   x0.7.
    /// - `Locked`: the default tuning, holding one framing per shot.
    pub fn for_feel(feel: Option<CameraFeel>) -> Self {
        let mut c = Self::default();
        let (band, safe, resp, glide) = match feel {
            Some(CameraFeel::Steady) => (1.8, 1.25, 1.5, 1.3),
            Some(CameraFeel::Lively) => (0.5, 0.8, 0.5, 0.7),
            Some(CameraFeel::Locked) => {
                c.hold_shot = true;
                return c;
            }
            Some(CameraFeel::Smooth) | None => return c,
        };
        c.band_x *= band;
        c.band_y *= band;
        c.band_z *= band;
        c.safe_x *= safe;
        c.safe_y *= safe;
        c.pan_s *= resp;
        c.tilt_s *= resp;
        c.zoom_s *= resp;
        c.glide_base_s *= glide;
        c.glide_per_s *= glide;
        c.glide_min_s *= glide;
        c.glide_max_s *= glide;
        c
    }
}

impl Default for CamCfg {
    fn default() -> Self {
        Self {
            band_x: 0.05,
            band_y: 0.04,
            band_z: 0.06,
            safe_x: 0.2,
            safe_y: 0.14,
            pan_s: 1.3,
            tilt_s: 1.8,
            zoom_s: 2.6,
            anchor_s: 0.35,
            median_s: 0.5,
            switch_cut_frac: 0.5,
            layout_cut: true,
            jump_win_s: 0.5,
            onset_back_s: 1.2,
            glide_base_s: 0.5,
            glide_per_s: 0.6,
            glide_min_s: 0.6,
            glide_max_s: 1.2,
            min_show_s: 0.8,
            hold_shot: false,
        }
    }
}

/// Everything the planner needs, on the output clock.
pub struct PlanInput<'a> {
    pub targets: &'a [Target],
    pub frames: usize,
    pub fps: f64,
    pub src_w: f64,
    pub src_h: f64,
    /// Shot boundaries (s): the camera snaps and re-plans at each.
    pub hards: &'a [f64],
    /// Edit jump-cuts (s): continuity is kept, but reframes hide here.
    pub jumps: &'a [f64],
    /// Utterance onsets (s): word starts after a pause; switches land here.
    pub onsets: &'a [f64],
    /// Output canvas: its aspect sets the fallback (base) framing.
    pub canvas: crate::compose::Canvas,
}

/// Channel vector: center x/y, ln width, ln height, punch anchor.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Ch {
    cx: f64,
    cy: f64,
    lw: f64,
    lh: f64,
    ax: f64,
    ay: f64,
}

impl Ch {
    fn of(r: &Rect, ax: f64, ay: f64) -> Self {
        Ch {
            cx: r.cx(),
            cy: r.cy(),
            lw: r.w.max(2.0).ln(),
            lh: r.h.max(2.0).ln(),
            ax,
            ay,
        }
    }
    fn rect(&self) -> Rect {
        Rect::from_center(self.cx, self.cy, self.lw.exp(), self.lh.exp())
    }
    fn lerp(&self, o: &Ch, u: f64) -> Ch {
        let l = |a: f64, b: f64| a + (b - a) * u;
        Ch {
            cx: l(self.cx, o.cx),
            cy: l(self.cy, o.cy),
            lw: l(self.lw, o.lw),
            lh: l(self.lh, o.lh),
            ax: l(self.ax, o.ax),
            ay: l(self.ay, o.ay),
        }
    }
    /// Band-normalized deviation of `o` from this framing (>1 = outside).
    fn dev(&self, o: &Ch, cfg: &CamCfg) -> f64 {
        let (w, h) = (self.lw.exp(), self.lh.exp());
        ((o.cx - self.cx).abs() / (cfg.band_x * w))
            .max((o.cy - self.cy).abs() / (cfg.band_y * h))
            .max((o.lw - self.lw).abs() / cfg.band_z)
            .max((o.lh - self.lh).abs() / cfg.band_z)
    }
    /// Move distance in "screens" (drives glide time).
    fn dist(&self, o: &Ch) -> f64 {
        let w = self.lw.exp().max(o.lw.exp());
        let h = self.lh.exp().max(o.lh.exp());
        ((o.cx - self.cx).abs() / w)
            .max((o.cy - self.cy).abs() / h)
            .max((o.lw - self.lw).abs() / 0.35)
            .max((o.lh - self.lh).abs() / 0.35)
    }
    fn get(&self, i: usize) -> f64 {
        [self.cx, self.cy, self.lw, self.lh, self.ax, self.ay][i]
    }
    fn set(&mut self, i: usize, v: f64) {
        match i {
            0 => self.cx = v,
            1 => self.cy = v,
            2 => self.lw = v,
            3 => self.lh = v,
            4 => self.ax = v,
            _ => self.ay = v,
        }
    }
}

/// Smootherstep: zero velocity and acceleration at both ends.
pub fn smootherstep(u: f64) -> f64 {
    let u = u.clamp(0.0, 1.0);
    u * u * u * (u * (u * 6.0 - 15.0) + 10.0)
}

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = v.len();
    if n == 0 {
        0.0
    } else if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

fn median_ch(xs: &[Ch]) -> Ch {
    let col = |f: fn(&Ch) -> f64| median(&mut xs.iter().map(f).collect::<Vec<_>>());
    Ch {
        cx: col(|c| c.cx),
        cy: col(|c| c.cy),
        lw: col(|c| c.lw),
        lh: col(|c| c.lh),
        ax: col(|c| c.ax),
        ay: col(|c| c.ay),
    }
}

/// Plan one camera pose per output frame.
pub fn plan(inp: &PlanInput, cfg: &CamCfg) -> Vec<Pose> {
    let n = inp.frames;
    if n == 0 {
        return Vec::new();
    }
    let fps = inp.fps.max(1.0);
    let base = inp.canvas.base_rect(inp.src_w, inp.src_h);
    let fallback = Pose {
        rect: base,
        ax: base.cx(),
        ay: base.cy(),
        kind: Kind::Fixed,
    };
    let frame_of = |t: f64| ((t * fps).round().max(0.0) as usize).min(n);
    // Shot boundaries (frame index + exact time, sorted, unique, interior).
    // Targets belong to shots by exact time: the last sample before a cut
    // must never leak into the next shot (it would open on the old shot's
    // framing — e.g. stay wide until the new face is confirmed).
    let mut bounds: Vec<(usize, f64)> = inp
        .hards
        .iter()
        .map(|&t| (frame_of(t), t))
        .filter(|&(f, _)| f > 0 && f < n)
        .collect();
    bounds.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    bounds.dedup_by_key(|b| b.0);
    let jump_frames: Vec<usize> = inp.jumps.iter().map(|&t| frame_of(t)).collect();
    let onset_frames: Vec<usize> = inp.onsets.iter().map(|&t| frame_of(t)).collect();

    let mut out: Vec<Pose> = Vec::with_capacity(n);
    let mut starts = vec![(0usize, f64::NEG_INFINITY)];
    starts.extend(bounds.iter().copied());
    for (si, &(f0, t0)) in starts.iter().enumerate() {
        let (f1, t1) = starts.get(si + 1).copied().unwrap_or((n, f64::INFINITY));
        if f1 <= f0 {
            continue;
        }
        let tg: Vec<&Target> = inp
            .targets
            .iter()
            .filter(|t| t.t >= t0 - 1e-6 && t.t < t1 - 1e-6)
            .collect();
        let shot =
            plan_shot(&tg, f0, f1, fps, cfg, &jump_frames, &onset_frames).unwrap_or_else(|| {
                // No targets inside: hold whatever framed just before (or base).
                let prev = inp
                    .targets
                    .iter()
                    .rev()
                    .find(|t| t.t < t0)
                    .map(|t| Pose {
                        rect: t.rect,
                        ax: t.ax,
                        ay: t.ay,
                        kind: t.kind,
                    })
                    .unwrap_or(fallback);
                vec![prev; f1 - f0]
            });
        out.extend(shot);
    }
    for p in out.iter_mut() {
        p.rect = p.rect.clamped(inp.src_w, inp.src_h);
    }
    out
}

/// One piece of framing evidence on the shot's clock.
#[derive(Clone, Copy)]
struct Ev {
    t: f64,
    ch: Ch,
    kind: Kind,
    cut: bool,
}

/// Plan frames `[f0, f1)` of one shot. None when it has no targets.
#[allow(clippy::too_many_arguments)]
fn plan_shot(
    tg: &[&Target],
    f0: usize,
    f1: usize,
    fps: f64,
    cfg: &CamCfg,
    jump_frames: &[usize],
    onset_frames: &[usize],
) -> Option<Vec<Pose>> {
    if tg.is_empty() {
        return None;
    }
    let len = f1 - f0;
    let local = |f: usize| f.checked_sub(f0).filter(|&k| k < len);
    // --- 1. evidence ---------------------------------------------------------
    // Real sightings only; placeholders (dropouts, post-cut settle) carry
    // no information, the solver bridges them. A shot made only of
    // placeholders still uses them.
    let strong: Vec<&Target> = {
        let s: Vec<&Target> = tg.iter().copied().filter(|t| !t.weak).collect();
        if s.is_empty() {
            tg.to_vec()
        } else {
            s
        }
    };
    // A framing needs a few sightings behind it. A run (samples between
    // framing changes) of fewer than three face sightings is a transient
    // (a stray detection, a head passing through) and would otherwise win
    // a whole shot or force a blink cut; synthetic framings (wide, fixed)
    // are exempt.
    let strong: Vec<&Target> = {
        let mut runs: Vec<Vec<&Target>> = Vec::new();
        for (i, s) in strong.iter().enumerate() {
            let new_run = i == 0 || s.cut || strong[i - 1].kind != s.kind;
            if new_run {
                runs.push(Vec::new());
            }
            if let Some(r) = runs.last_mut() {
                r.push(s);
            }
        }
        let solid =
            |r: &Vec<&Target>| r.len() >= 3 || !matches!(r[0].kind, Kind::Subject | Kind::Group);
        if runs.iter().any(solid) {
            runs.into_iter().filter(solid).flatten().collect()
        } else {
            strong
        }
    };
    // Handoffs land on the new speaker's first word: detection confirms a
    // turn ~0.3–0.8 s late, the words say when it really began. Samples
    // between that onset and the confirmation still show the old framing,
    // so they are dropped.
    let onsets_t: Vec<f64> = onset_frames
        .iter()
        .filter_map(|&o| local(o))
        .map(|o| (f0 + o) as f64 / fps)
        .collect();
    let mut ev: Vec<Ev> = Vec::with_capacity(strong.len());
    let mut last_change = f64::NEG_INFINITY;
    for (i, s) in strong.iter().enumerate() {
        let mut e = Ev {
            t: s.t,
            ch: Ch::of(&s.rect, s.ax, s.ay),
            kind: s.kind,
            cut: i > 0 && s.cut,
        };
        let changes = e.cut || ev.last().is_some_and(|p: &Ev| p.kind != e.kind);
        if e.cut && e.kind == Kind::Subject {
            if let Some(o) = onsets_t
                .iter()
                .copied()
                .filter(|&o| o + cfg.onset_back_s >= s.t && o <= s.t + 0.15)
                .filter(|&o| o > last_change + 0.45)
                .reduce(f64::max)
            {
                while ev.len() > 1 && ev.last().is_some_and(|p| p.t >= o) {
                    ev.pop();
                }
                e.t = o.min(s.t);
            }
        }
        if changes {
            last_change = e.t;
        }
        ev.push(e);
    }

    // --- 2. dense desired signal ----------------------------------------------
    // Linear between sightings, stepped at framing changes, with a
    // confidence weight that fades inside long gaps (the solver, not a
    // straight line, decides how to bridge a dropout).
    let mut desired: Vec<Ch> = Vec::with_capacity(len);
    let mut weight: Vec<f64> = Vec::with_capacity(len);
    let mut kinds: Vec<Kind> = Vec::with_capacity(len);
    let mut change = vec![false; len];
    {
        let mut i = 0usize;
        for k in 0..len {
            let t = (f0 + k) as f64 / fps;
            while i + 1 < ev.len() && ev[i + 1].t <= t + 1e-9 {
                i += 1;
            }
            let a = ev[i];
            let (ch, w) = if t < a.t - 1e-9 {
                (a.ch, 0.02) // before the first sighting
            } else if i + 1 >= ev.len() {
                let w = if t - a.t < 0.35 { 1.0 } else { 0.02 };
                (a.ch, w)
            } else {
                let b = ev[i + 1];
                let gap = b.t - a.t;
                let w = if gap <= 0.35 {
                    1.0
                } else {
                    let near = (t - a.t).min(b.t - t);
                    (-(near - 0.1).max(0.0) / 0.35).exp().max(0.02)
                };
                if b.cut || b.kind != a.kind {
                    (a.ch, w) // step: hold until the change lands
                } else {
                    let u = ((t - a.t) / gap.max(1e-9)).clamp(0.0, 1.0);
                    (a.ch.lerp(&b.ch, u), w)
                }
            };
            desired.push(ch);
            weight.push(w);
            kinds.push(a.kind);
        }
        for k in 1..len {
            if kinds[k] != kinds[k - 1] {
                change[k] = true;
            }
        }
        for e in ev.iter().skip(1).filter(|e| e.cut) {
            let k = ((e.t * fps).round() as usize).saturating_sub(f0);
            if k > 0 && k < len {
                change[k] = true;
            }
        }
    }
    // Spike-killing median, never across a discrete change.
    let half = ((cfg.median_s * fps / 2.0).round() as usize).max(1);
    let run_bounds = |change: &[bool]| {
        let mut seg_start = vec![0usize; len];
        let mut seg_end = vec![len; len];
        let mut s = 0;
        for k in 0..len {
            if change[k] {
                s = k;
            }
            seg_start[k] = s;
        }
        let mut e = len;
        for k in (0..len).rev() {
            seg_end[k] = e;
            if change[k] {
                e = k;
            }
        }
        (seg_start, seg_end)
    };
    let mut desired: Vec<Ch> = {
        let (ss, se) = run_bounds(&change);
        (0..len)
            .map(|k| {
                let lo = k.saturating_sub(half).max(ss[k]);
                let hi = (k + half + 1).min(se[k]);
                median_ch(&desired[lo..hi])
            })
            .collect()
    };

    // Locked off: the shot's one framing is what the evidence says most of
    // the time (the median of its dominant kind), held for every frame.
    if cfg.hold_shot {
        return Some(hold_pose(&desired, &kinds, &weight));
    }

    // --- 3. snaps ----------------------------------------------------------------
    // Discrete moves the camera cuts instead of panning:
    // - speaker switches between people who don't share a frame (no
    //   whip-pan across the table);
    // - layout changes (close-up <-> letterboxed wide);
    // - any reframe near an edit jump-cut (the edit hides it).
    let mut snaps: Vec<usize> = Vec::new();
    for k in 1..len {
        if !change[k] {
            continue;
        }
        let (from, to) = (desired[k - 1], desired[k]);
        let (fk, tk) = (kinds[k - 1], kinds[k]);
        let far = fk == Kind::Subject
            && tk == Kind::Subject
            && (to.cx - from.cx).abs() > cfg.switch_cut_frac * from.lw.exp().max(to.lw.exp());
        let layout = cfg.layout_cut && (fk == Kind::Wide) != (tk == Kind::Wide);
        if far || layout {
            snaps.push(k);
        }
    }
    let jw = ((cfg.jump_win_s * fps).round() as usize).max(1);
    for j in jump_frames.iter().filter_map(|&j| local(j)) {
        if j == 0 || snaps.iter().any(|&s| s.abs_diff(j) <= jw) {
            // A snap nearby moves onto the jump-cut instead.
            if let Some(s) = snaps.iter_mut().find(|s| s.abs_diff(j) <= jw && **s != j) {
                let (a, b) = ((*s).min(j), (*s).max(j));
                if *s > j {
                    let v = (desired[*s], kinds[*s]);
                    for k in a..b {
                        (desired[k], kinds[k]) = v;
                    }
                } else if a > 0 {
                    let v = (desired[a - 1], kinds[a - 1]);
                    for k in a..b {
                        (desired[k], kinds[k]) = v;
                    }
                }
                *s = j;
            }
            continue;
        }
        let lo = j.saturating_sub(jw);
        let hi = (j + jw).min(len);
        let l = median_ch(&desired[lo..j]);
        let r = median_ch(&desired[j..hi]);
        if l.dev(&r, cfg) > 2.0 {
            // Both sides settle onto their framing right at the cut.
            for d in &mut desired[lo..j] {
                if l.dev(d, cfg) > 1.0 {
                    *d = l;
                }
            }
            for d in &mut desired[j..hi] {
                if r.dev(d, cfg) > 1.0 {
                    *d = r;
                }
            }
            snaps.push(j);
        }
    }
    snaps.sort_unstable();
    snaps.dedup();
    // No blink framings: a framing that would be on screen shorter than
    // `min_show` is skipped — the previous one rides out to the next snap
    // (or shot end), and a shot opening on a blink opens on what follows.
    let min_show = ((cfg.min_show_s * fps).round() as usize).max(1);
    let mut kept: Vec<usize> = Vec::new();
    for (i, &s) in snaps.iter().enumerate() {
        let next = snaps.get(i + 1).copied().unwrap_or(len);
        let prev = kept.last().copied().unwrap_or(0);
        if next - s < min_show {
            let v = (desired[s - 1], kinds[s - 1]);
            for k in s..next {
                (desired[k], kinds[k]) = v;
                change[k] = false;
            }
        } else if s - prev < min_show && prev == 0 {
            let v = (desired[s], kinds[s]);
            for k in 0..s {
                (desired[k], kinds[k]) = v;
                change[k] = false;
            }
        } else {
            kept.push(s);
        }
    }
    let snaps = kept;
    // A soft change (near handoff, group edge) that would only just start
    // before a snap is skipped too: the snap takes the camera past it.
    for k in 1..len {
        if !change[k] || snaps.contains(&k) {
            continue;
        }
        let next = snaps.iter().copied().find(|&s| s > k).unwrap_or(len);
        if next - k < min_show {
            let v = (desired[k - 1], kinds[k - 1]);
            for j in k..next {
                (desired[j], kinds[j]) = v;
                change[j] = false;
            }
        }
    }

    // --- 4. solve the path ------------------------------------------------------
    // Per piece (between snaps and soft changes) and channel: the smoothest
    // path (least acceleration) that keeps the subject inside the dead
    // band, fitted offline to the whole clip — no lag, no stop-and-go.
    let soft: Vec<usize> = (1..len)
        .filter(|&k| change[k] && !snaps.contains(&k))
        .collect();
    let lam = |t: f64| (t * fps / std::f64::consts::TAU).powi(4);
    let mut path = desired.clone();
    let mut cuts = vec![0usize];
    cuts.extend(snaps.iter().copied());
    cuts.extend(soft.iter().copied());
    cuts.sort_unstable();
    cuts.dedup();
    cuts.push(len);
    for w in cuts.windows(2) {
        let (a, b) = (w[0], w[1]);
        if b <= a {
            continue;
        }
        let d = &desired[a..b];
        let wt = &weight[a..b];
        for c in 0..6 {
            let dc: Vec<f64> = d.iter().map(|x| x.get(c)).collect();
            if dc.windows(2).all(|p| p[0] == p[1]) {
                continue; // constant is exact already (and the common case)
            }
            let (band, safe, t): (Vec<f64>, Vec<f64>, f64) = match c {
                0 => (
                    d.iter().map(|x| cfg.band_x * x.lw.exp()).collect(),
                    d.iter().map(|x| cfg.safe_x * x.lw.exp()).collect(),
                    cfg.pan_s,
                ),
                1 => (
                    d.iter().map(|x| cfg.band_y * x.lh.exp()).collect(),
                    d.iter().map(|x| cfg.safe_y * x.lh.exp()).collect(),
                    cfg.tilt_s,
                ),
                2 | 3 => (vec![cfg.band_z; b - a], vec![0.3; b - a], cfg.zoom_s),
                _ => (vec![0.0; b - a], vec![f64::INFINITY; b - a], cfg.anchor_s),
            };
            let still = if c == 2 || c == 3 { 2e-3 } else { 0.75 };
            let sol = solve_band(&dc, wt, &band, &safe, lam(t), still);
            for (k, v) in sol.into_iter().enumerate() {
                path[a + k].set(c, v);
            }
        }
    }
    // --- 5. glides ---------------------------------------------------------------
    // A soft change (a handoff between neighbors, a group forming or
    // breaking up) is a deliberate move from one settled framing to the
    // next: a smootherstep dolly (zero velocity and acceleration at both
    // ends — no wind-up, no overshoot) timed by distance, starting a beat
    // before the change. Never across a snap.
    let solved = path.clone();
    for &k in &soft {
        let lo = cuts.iter().copied().filter(|&c| c < k).max().unwrap_or(0);
        let hi = cuts.iter().copied().find(|&c| c > k).unwrap_or(len);
        let (from, to) = (solved[k - 1], solved[k]);
        let dist = from.dist(&to);
        let dur = ((cfg.glide_base_s + cfg.glide_per_s * dist)
            .clamp(cfg.glide_min_s, cfg.glide_max_s)
            * fps)
            .round() as usize;
        let s0 = k.saturating_sub(dur * 3 / 10).max(lo);
        let s1 = (s0 + dur.max(2)).min(hi);
        if s1 <= s0 + 1 {
            continue;
        }
        for (j, p) in path.iter_mut().enumerate().take(s1).skip(s0) {
            let u = smootherstep((j - s0) as f64 / (s1 - s0 - 1) as f64);
            let (l, r) = if j < k {
                (solved[j], solved[k])
            } else {
                (solved[k - 1], solved[j])
            };
            *p = l.lerp(&r, u);
        }
    }
    Some(
        path.iter()
            .zip(kinds.iter())
            .map(|(c, &kind)| Pose {
                rect: c.rect(),
                ax: c.ax,
                ay: c.ay,
                kind,
            })
            .collect(),
    )
}

/// The single pose a locked-off shot holds: the most common kind of
/// framing in the shot, at the median of its (confident) desired values.
fn hold_pose(desired: &[Ch], kinds: &[Kind], weight: &[f64]) -> Vec<Pose> {
    let count = |k: Kind| kinds.iter().filter(|&&x| x == k).count();
    // Ties go to the first kind seen, so the choice is stable.
    let kind = kinds
        .iter()
        .copied()
        .fold((kinds[0], count(kinds[0])), |best, k| {
            let n = count(k);
            if n > best.1 {
                (k, n)
            } else {
                best
            }
        })
        .0;
    let of_kind = |confident: bool| -> Vec<Ch> {
        desired
            .iter()
            .zip(kinds)
            .zip(weight)
            .filter(|((_, &k), &w)| k == kind && (!confident || w >= 0.5))
            .map(|((d, _), _)| *d)
            .collect()
    };
    let mut pool = of_kind(true);
    if pool.is_empty() {
        pool = of_kind(false);
    }
    let ch = median_ch(&pool);
    vec![
        Pose {
            rect: ch.rect(),
            ax: ch.ax,
            ay: ch.ay,
            kind,
        };
        desired.len()
    ]
}

/// Stiffness of the safe band relative to a full-confidence sighting.
const SAFE_W: f64 = 40.0;
/// Pull toward the exact framing inside the dead band (relative weight):
/// only picks *where* a still camera rests (the average framing), far too
/// weak to move it.
const REST_W: f64 = 0.002;
/// Velocity penalty relative to the acceleration penalty's scale: among
/// equally smooth paths prefer the stillest (a camera at rest stays at
/// rest instead of creeping inside the band).
const DRAG: f64 = 0.05;

/// Dead-band smoothing path: the minimizer of
///
/// ```text
///   Σ w·dist(c, [d-band, d+band])²  +  SAFE_W·w·dist(c, [d-safe, d+safe])²
///     + REST_W·w·(c-d)²  +  lam·Σ (Δ²c)²  +  DRAG·√lam·Σ (Δc)²
/// ```
///
/// Inside the band the data costs (next to) nothing, so the path is as
/// still as it can be; only a subject that leaves the band bends it, as
/// late and as gently as the band allows. The safe band caps how far a
/// fast subject may lead the camera. `lam = (T·fps/2π)⁴` makes `T` seconds
/// the shortest period the camera reproduces.
///
/// The cost is piecewise quadratic, so it is solved exactly by active-set
/// Newton: guess which frames sit outside each band, solve that quadratic
/// (one banded Cholesky, O(n)), repeat until the guess is self-consistent
/// (a handful of rounds). A path spanning less than `still` is locked off
/// exactly. Pure (unit-tested).
fn solve_band(d: &[f64], w: &[f64], band: &[f64], safe: &[f64], lam: f64, still: f64) -> Vec<f64> {
    let n = d.len();
    if n < 4 {
        return d.to_vec();
    }
    let lam1 = DRAG * lam.sqrt();
    // Side of each band a frame sits on: -1 below, 0 inside, +1 above.
    let side = |c: f64, d: f64, b: f64| -> i8 {
        if c > d + b {
            1
        } else if c < d - b {
            -1
        } else {
            0
        }
    };
    let solve = |act: &[(i8, i8)]| {
        let mut diag = vec![0.0; n];
        let mut rhs = vec![0.0; n];
        for k in 0..n {
            let (a, s) = act[k];
            diag[k] += REST_W * w[k];
            rhs[k] += REST_W * w[k] * d[k];
            if a != 0 {
                diag[k] += w[k];
                rhs[k] += w[k] * (d[k] + a as f64 * band[k]);
            }
            if s != 0 && safe[k].is_finite() {
                diag[k] += SAFE_W * w[k];
                rhs[k] += SAFE_W * w[k] * (d[k] + s as f64 * safe[k]);
            }
        }
        Banded::whittaker(&diag, lam, lam1).cholesky().solve(&rhs)
    };
    // Start from the plain smoother (every frame pulled to its framing),
    // then let frames inside the band go free.
    let full: Vec<f64> = {
        let diag: Vec<f64> = w.iter().map(|&x| x.max(1e-6)).collect();
        let rhs: Vec<f64> = (0..n).map(|k| diag[k] * d[k]).collect();
        Banded::whittaker(&diag, lam, lam1).cholesky().solve(&rhs)
    };
    let act_of = |c: &[f64]| -> Vec<(i8, i8)> {
        (0..n)
            .map(|k| (side(c[k], d[k], band[k]), side(c[k], d[k], safe[k])))
            .collect()
    };
    let mut act = act_of(&full);
    let mut c = solve(&act);
    for _ in 0..60 {
        let next = act_of(&c);
        if next == act {
            break;
        }
        act = next;
        c = solve(&act);
    }
    let (lo, hi) = c
        .iter()
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), &v| {
            (lo.min(v), hi.max(v))
        });
    if hi - lo < still {
        let mean = c.iter().sum::<f64>() / n as f64;
        return vec![mean; n];
    }
    c
}

/// Symmetric positive-definite pentadiagonal matrix (bandwidth 2), stored
/// as its lower band: `a[k] = [A(k,k-2), A(k,k-1), A(k,k)]`.
struct Banded {
    a: Vec<[f64; 3]>,
}

impl Banded {
    /// `diag(w) + lam·D2ᵀD2 + lam1·D1ᵀD1`, Dk the k-th difference operator.
    fn whittaker(w: &[f64], lam: f64, lam1: f64) -> Self {
        let n = w.len();
        let mut a: Vec<[f64; 3]> = w.iter().map(|&x| [0.0, 0.0, x]).collect();
        for r in 0..n.saturating_sub(1) {
            a[r][2] += lam1;
            a[r + 1][2] += lam1;
            a[r + 1][1] -= lam1;
        }
        for r in 0..n.saturating_sub(2) {
            let co = [1.0, -2.0, 1.0];
            for i in 0..3 {
                for j in 0..=i {
                    // (r+i, r+j), lower triangle: offset i-j.
                    a[r + i][2 - (i - j)] += lam * co[i] * co[j];
                }
            }
        }
        // A tiny ridge keeps all-zero-weight stretches solvable.
        for row in a.iter_mut() {
            row[2] += 1e-9;
        }
        Banded { a }
    }

    /// In-place banded Cholesky: A = L·Lᵀ (L stored in the same layout).
    fn cholesky(mut self) -> Self {
        let n = self.a.len();
        for k in 0..n {
            for off in (0..=2usize).rev() {
                // L(k, k-off)
                let Some(j) = k.checked_sub(off) else {
                    continue;
                };
                let mut s = self.a[k][2 - off];
                for m in (k.saturating_sub(2)).max(j.saturating_sub(2))..j {
                    s -= self.a[k][2 - (k - m)] * self.a[j][2 - (j - m)];
                }
                if off == 0 {
                    self.a[k][2] = s.max(1e-12).sqrt();
                } else {
                    self.a[k][2 - off] = s / self.a[j][2];
                }
            }
        }
        self
    }

    /// Solve L·Lᵀ·x = b.
    fn solve(&self, b: &[f64]) -> Vec<f64> {
        let n = b.len();
        let mut y = vec![0.0; n];
        for k in 0..n {
            let mut s = b[k];
            for off in 1..=2usize {
                if let Some(j) = k.checked_sub(off) {
                    s -= self.a[k][2 - off] * y[j];
                }
            }
            y[k] = s / self.a[k][2];
        }
        let mut x = vec![0.0; n];
        for k in (0..n).rev() {
            let mut s = y[k];
            for off in 1..=2usize {
                if k + off < n {
                    s -= self.a[k + off][2 - off] * x[k + off];
                }
            }
            x[k] = s / self.a[k][2];
        }
        x
    }
}

/// Emphasis punch-in tuning.
#[derive(Debug, Clone)]
pub struct PunchCfg {
    /// Peak zoom factor (1.18 = 18% tighter).
    pub zoom: f64,
    /// Ease-in time (s): fast, ease-out curve — it should read as a punch.
    pub attack_s: f64,
    /// Ease-back time (s): gentler, so the release never reads as a bounce.
    pub release_s: f64,
    /// The camera must be still this long around a punch (s).
    pub guard_s: f64,
}

impl Default for PunchCfg {
    fn default() -> Self {
        Self {
            zoom: 1.18,
            attack_s: 0.2,
            release_s: 0.45,
            guard_s: 0.25,
        }
    }
}

/// Punch tuning for a Look: `peak` (1.0..1.4, the Look's `camera.punch`)
/// replaces the default peak zoom; absent keeps the default. Either way the
/// peak is held to `res_cap` (the resolution floor: no mush on low-res
/// input). A peak of 1.0 is no visible punch.
pub fn punch_cfg(peak: Option<f64>, res_cap: f64) -> PunchCfg {
    let mut cfg = PunchCfg::default();
    if let Some(p) = peak {
        cfg.zoom = p;
    }
    cfg.zoom = cfg.zoom.min(res_cap);
    cfg
}

/// Punch envelope 0..1 at `t` for a window `[a, b]` (attack from `a`,
/// hold to `b`, release after). Pure (unit-tested).
pub fn punch_env(t: f64, a: f64, b: f64, cfg: &PunchCfg) -> f64 {
    if t < a || t > b + cfg.release_s {
        return 0.0;
    }
    if t < a + cfg.attack_s {
        let u = (t - a) / cfg.attack_s;
        return 1.0 - (1.0 - u).powi(3); // ease-out cubic
    }
    if t <= b {
        return 1.0;
    }
    let u = (t - b) / cfg.release_s;
    0.5 * (1.0 + (std::f64::consts::PI * u).cos()) // ease-in-out sine
}

/// Layer emphasis punch-ins (`[a, b]` windows, output clock) onto a
/// planned path. Only single-speaker framings punch, and only while the
/// camera is otherwise still (never on top of a move or a cut). The zoom
/// anchors on the face: it keeps its screen position while the frame
/// tightens around it. Returns how many punches landed.
pub fn apply_punches(
    poses: &mut [Pose],
    windows: &[(f64, f64)],
    fps: f64,
    src_w: f64,
    src_h: f64,
    cfg: &PunchCfg,
) -> usize {
    let n = poses.len();
    if n == 0 || windows.is_empty() || cfg.zoom <= 1.0 {
        return 0; // (a peak of 1.0 is no punch at all)
    }
    // Visibly moving: panning faster than 6% of the frame per second, or
    // zooming faster than 4%/s. A slow drift can carry a punch (the zoom
    // is anchored on the face); a pan or a cut can't.
    let moving: Vec<bool> = (0..n)
        .map(|k| {
            if k == 0 {
                return false;
            }
            let (a, b) = (poses[k - 1].rect, poses[k].rect);
            (b.cx() - a.cx()).abs() / a.w.max(1.0) * fps > 0.06
                || (b.cy() - a.cy()).abs() / a.h.max(1.0) * fps > 0.06
                || (b.w / a.w.max(1.0)).ln().abs() * fps > 0.04
                || poses[k].kind != poses[k - 1].kind
        })
        .collect();
    let guard = (cfg.guard_s * fps).round() as usize;
    let frame = |t: f64| ((t * fps).round().max(0.0) as usize).min(n);
    let mut landed = 0;
    let mut env = vec![0.0f64; n];
    for &(a, b) in windows {
        let (k0, k1) = (frame(a), frame(b + cfg.release_s));
        if k1 <= k0 {
            continue;
        }
        let lo = k0.saturating_sub(guard);
        let hi = (k1 + guard).min(n);
        let ok = (lo..hi).all(|k| !moving[k] && poses[k].kind == Kind::Subject);
        if !ok {
            continue;
        }
        for (k, e) in env.iter_mut().enumerate().take(k1.min(n)).skip(k0) {
            *e = e.max(punch_env(k as f64 / fps, a, b, cfg));
        }
        landed += 1;
    }
    for (p, &e) in poses.iter_mut().zip(env.iter()) {
        if e <= 0.0 {
            continue;
        }
        let m = 1.0 + (cfg.zoom - 1.0) * e;
        let r = p.rect;
        let z = Rect {
            x: p.ax - (p.ax - r.x) / m,
            y: p.ay - (p.ay - r.y) / m,
            w: r.w / m,
            h: r.h / m,
        };
        p.rect = z.clamped(src_w, src_h);
    }
    landed
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: f64 = 640.0;
    const H: f64 = 360.0;
    const FPS: f64 = 30.0;

    /// A 9:16 window of height `h` centered at (cx, cy).
    fn win(cx: f64, cy: f64, h: f64) -> Rect {
        Rect::from_center(cx, cy, h * 9.0 / 16.0, h)
    }

    fn tgt(t: f64, cx: f64) -> Target {
        Target {
            t,
            rect: win(cx, 180.0, 360.0),
            ax: cx,
            ay: 150.0,
            kind: Kind::Subject,
            cut: false,
            weak: false,
        }
    }

    fn run(
        targets: &[Target],
        secs: f64,
        hards: &[f64],
        jumps: &[f64],
        onsets: &[f64],
    ) -> Vec<Pose> {
        plan(
            &PlanInput {
                targets,
                frames: (secs * FPS) as usize,
                fps: FPS,
                src_w: W,
                src_h: H,
                hards,
                jumps,
                onsets,
                canvas: crate::compose::Canvas::TALL,
            },
            &CamCfg::default(),
        )
    }

    fn cxs(p: &[Pose]) -> Vec<f64> {
        p.iter().map(|q| q.rect.cx()).collect()
    }

    fn max_step(v: &[f64]) -> f64 {
        v.windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0, f64::max)
    }

    /// Deterministic pseudo-noise in [-1, 1].
    fn noise(i: usize) -> f64 {
        let x = ((i as u64).wrapping_mul(2654435761) % 1000) as f64 / 500.0;
        x - 1.0
    }

    #[test]
    fn detection_jitter_never_moves_the_camera() {
        // ±8px box noise at 15 Hz around a still speaker (dead zone ~20px).
        let tg: Vec<Target> = (0..150)
            .map(|i| tgt(i as f64 / 15.0, 300.0 + 8.0 * noise(i)))
            .collect();
        let p = run(&tg, 10.0, &[], &[], &[]);
        assert_eq!(p.len(), 300);
        let first = p[0].rect;
        assert!(
            p.iter().all(|q| q.rect == first),
            "camera must be locked off"
        );
        assert!(
            (first.cx() - 300.0).abs() < 4.0,
            "centered on the speaker: {}",
            first.cx()
        );
    }

    #[test]
    fn brief_excursions_are_ignored() {
        // Speaker leans out for 0.2s and back: no reframe.
        let tg: Vec<Target> = (0..90)
            .map(|i| {
                let t = i as f64 / 15.0;
                tgt(
                    t,
                    if (2.0..2.2).contains(&t) {
                        380.0
                    } else {
                        300.0
                    },
                )
            })
            .collect();
        let p = run(&tg, 6.0, &[], &[], &[]);
        let v = cxs(&p);
        assert!(
            v.iter().all(|x| (x - v[0]).abs() < 1e-9),
            "flap moved the camera"
        );
    }

    #[test]
    fn near_handoff_glides_smoothly_and_lands_exactly() {
        // Two speakers 90px apart (window ~202px): glide, not cut.
        let mut tg: Vec<Target> = (0..45).map(|i| tgt(i as f64 / 15.0, 260.0)).collect();
        for i in 45..120 {
            let mut t = tgt(i as f64 / 15.0, 350.0);
            t.cut = i == 45;
            tg.push(t);
        }
        let p = run(&tg, 8.0, &[], &[], &[]);
        let v = cxs(&p);
        // Smootherstep peaks at 1.875x the mean speed: 90px over ~19 frames.
        assert!(max_step(&v) < 10.0, "glide, not a jump: {}", max_step(&v));
        assert!((v[0] - 260.0).abs() < 1.0 && (v.last().unwrap() - 350.0).abs() < 1.0);
        // Eased: the first and last moving steps are gentle.
        let moving: Vec<usize> = (1..v.len())
            .filter(|&k| (v[k] - v[k - 1]).abs() > 1e-6)
            .collect();
        let (a, b) = (moving[0], *moving.last().unwrap());
        assert!((v[a] - v[a - 1]).abs() < 1.5 && (v[b] - v[b - 1]).abs() < 1.5);
        assert!(b - a >= 12, "a real glide takes time: {} frames", b - a);
    }

    #[test]
    fn far_handoff_cuts_on_the_first_word() {
        // Speakers across the table (~330px apart): cut, timed to the onset
        // at 2.6s although detection confirmed the switch at 3.0s.
        let mut tg: Vec<Target> = (0..45).map(|i| tgt(i as f64 / 15.0, 150.0)).collect();
        for i in 45..120 {
            let mut t = tgt(i as f64 / 15.0, 480.0);
            t.cut = i == 45;
            tg.push(t);
        }
        let p = run(&tg, 8.0, &[], &[], &[1.0, 2.6]);
        let v = cxs(&p);
        let jumps: Vec<usize> = (1..v.len())
            .filter(|&k| (v[k] - v[k - 1]).abs() > 1e-6)
            .collect();
        assert_eq!(jumps.len(), 1, "exactly one cut, no glide: {jumps:?}");
        assert_eq!(jumps[0], 78, "cut lands on the onset frame (2.6s)");
    }

    #[test]
    fn walking_subject_is_followed_smoothly() {
        // Presenter walks 300px in 6s (50px/s), then stops.
        let tg: Vec<Target> = (0..150)
            .map(|i| {
                let t = i as f64 / 15.0;
                tgt(t, 150.0 + 50.0 * (t - 1.0).clamp(0.0, 6.0) + 3.0 * noise(i))
            })
            .collect();
        let p = run(&tg, 10.0, &[], &[], &[]);
        let v = cxs(&p);
        assert!(max_step(&v) < 6.0, "no lurches: {}", max_step(&v));
        // Never loses the subject: stays within ~a dead zone and a half.
        for (k, x) in v.iter().enumerate() {
            let t = k as f64 / FPS;
            let subj = 150.0 + 50.0 * (t - 1.0).clamp(0.0, 6.0);
            assert!(
                (x - subj).abs() < 45.0,
                "t={t:.2} cam {x:.0} vs subject {subj:.0}"
            );
        }
        assert!(
            (v.last().unwrap() - 450.0).abs() < 6.0,
            "settles on the subject"
        );
    }

    #[test]
    fn shot_cuts_snap_and_never_glide_across() {
        let mut tg: Vec<Target> = (0..45).map(|i| tgt(i as f64 / 15.0, 250.0)).collect();
        tg.extend((45..90).map(|i| tgt(i as f64 / 15.0, 330.0)));
        let p = run(&tg, 6.0, &[3.0], &[], &[]);
        let v = cxs(&p);
        assert!(
            v[..90].iter().all(|x| (x - 250.0).abs() < 1.0),
            "pre-cut holds"
        );
        assert!(
            v[90..].iter().all(|x| (x - 330.0).abs() < 1.0),
            "post-cut holds"
        );
    }

    #[test]
    fn reframes_hide_inside_nearby_jump_cuts() {
        // The subject drifts at 3.2s; an edit jump-cut sits at 3.0s.
        let tg: Vec<Target> = (0..90)
            .map(|i| {
                let t = i as f64 / 15.0;
                tgt(t, if t < 3.2 { 250.0 } else { 320.0 })
            })
            .collect();
        let p = run(&tg, 6.0, &[], &[3.0], &[]);
        let v = cxs(&p);
        let moving: Vec<usize> = (1..v.len())
            .filter(|&k| (v[k] - v[k - 1]).abs() > 1e-6)
            .collect();
        assert_eq!(moving, vec![90], "one snap exactly on the jump-cut");
    }

    #[test]
    fn glides_never_overrun_the_next_cut() {
        // Near handoff at 2.0s (glide), then a far one at 2.3s (cut): the
        // glide has no room, so the camera holds and cuts straight across.
        let mut tg: Vec<Target> = (0..30).map(|i| tgt(i as f64 / 15.0, 200.0)).collect();
        for i in 30..35 {
            let mut t = tgt(i as f64 / 15.0, 270.0);
            t.cut = i == 30;
            tg.push(t);
        }
        for i in 35..90 {
            let mut t = tgt(i as f64 / 15.0, 520.0);
            t.cut = i == 35;
            tg.push(t);
        }
        let p = run(&tg, 6.0, &[], &[], &[]);
        let v = cxs(&p);
        let moving: Vec<usize> = (1..v.len())
            .filter(|&k| (v[k] - v[k - 1]).abs() > 1e-6)
            .collect();
        assert_eq!(moving.len(), 1, "one clean cut, no half glide: {moving:?}");
        assert!((v[moving[0] - 1] - 200.0).abs() < 1.0 && (v[moving[0]] - 520.0).abs() < 1.0);
    }

    #[test]
    fn settle_placeholders_start_on_the_real_speaker() {
        // First 0.4s are weak placeholders (post-cut settle at center).
        let tg: Vec<Target> = (0..60)
            .map(|i| {
                let t = i as f64 / 15.0;
                let mut x = tgt(t, if t < 0.4 { 320.0 } else { 200.0 });
                x.weak = t < 0.4;
                x
            })
            .collect();
        let p = run(&tg, 4.0, &[], &[], &[]);
        assert!(
            cxs(&p).iter().all(|x| (x - 200.0).abs() < 1.0),
            "no settle glide"
        );
    }

    #[test]
    fn new_shot_opens_on_its_own_framing() {
        // Wide until a shot cut at 2.0 s; the new face is only confirmed
        // 0.45 s later (post-cut settle placeholders in between).
        let wide = |t: f64| Target {
            t,
            rect: Rect {
                x: 0.0,
                y: 0.0,
                w: W,
                h: H,
            },
            ax: W / 2.0,
            ay: H / 2.0,
            kind: Kind::Wide,
            cut: false,
            weak: false,
        };
        let mut tg: Vec<Target> = (0..30).map(|i| wide(i as f64 / 15.0)).collect();
        tg.push(wide(1.999)); // chunk-end sample just before the cut
        for i in 30..37 {
            let mut t = tgt(i as f64 / 15.0, 320.0);
            t.weak = true;
            tg.push(t);
        }
        for i in 37..90 {
            tg.push(tgt(i as f64 / 15.0, 420.0));
        }
        let p = run(&tg, 6.0, &[2.0], &[], &[]);
        assert_eq!(p[59].kind, Kind::Wide);
        assert_eq!(p[60].kind, Kind::Subject, "cut lands on the shot boundary");
        assert!((p[60].rect.cx() - 420.0).abs() < 1.0, "{}", p[60].rect.cx());
    }

    #[test]
    fn no_blink_framing_right_before_a_shot_cut() {
        // Speaker A, a far handoff to B 0.3 s before a hard cut, then a new
        // shot: B's framing would show for 9 frames — it must be skipped.
        let mut tg: Vec<Target> = (0..45).map(|i| tgt(i as f64 / 15.0, 150.0)).collect();
        for i in 45..50 {
            let mut t = tgt(i as f64 / 15.0, 500.0);
            t.cut = i == 45;
            tg.push(t);
        }
        for i in 50..90 {
            tg.push(tgt(i as f64 / 15.0, 320.0));
        }
        let p = run(&tg, 6.0, &[50.0 / 15.0], &[], &[]);
        let cut = 100;
        assert!(
            p[..cut].iter().all(|q| (q.rect.cx() - 150.0).abs() < 1.0),
            "old framing rides out to the cut"
        );
        assert!((p[cut].rect.cx() - 320.0).abs() < 1.0);
    }

    #[test]
    fn wide_cuts_between_layouts() {
        let mut tg: Vec<Target> = (0..30).map(|i| tgt(i as f64 / 15.0, 300.0)).collect();
        for i in 30..90 {
            tg.push(Target {
                t: i as f64 / 15.0,
                rect: Rect {
                    x: 0.0,
                    y: 0.0,
                    w: W,
                    h: H,
                },
                ax: W / 2.0,
                ay: H / 2.0,
                kind: Kind::Wide,
                cut: i == 30,
                weak: false,
            });
        }
        let p = run(&tg, 6.0, &[], &[], &[]);
        let ws: Vec<f64> = p.iter().map(|q| q.rect.w).collect();
        assert!((ws[0] - 202.5).abs() < 1.0 && (ws.last().unwrap() - W).abs() < 1.0);
        // Close-up -> letterboxed wide is a layout change: one clean cut on
        // the change frame, no morph frames with half-grown blur bars.
        let steps: Vec<usize> = (1..ws.len())
            .filter(|&k| (ws[k] - ws[k - 1]).abs() > 0.5)
            .collect();
        assert_eq!(steps, vec![60], "one cut at the change: {steps:?}");
        assert_eq!(p[59].kind, Kind::Subject);
        assert_eq!(p[60].kind, Kind::Wide);
    }

    #[test]
    fn punch_envelope_attacks_fast_holds_and_releases() {
        let c = PunchCfg::default();
        assert_eq!(punch_env(0.99, 1.0, 2.0, &c), 0.0);
        assert!(punch_env(1.1, 1.0, 2.0, &c) > 0.8, "fast attack");
        assert_eq!(punch_env(1.5, 1.0, 2.0, &c), 1.0);
        let r = punch_env(2.2, 1.0, 2.0, &c);
        assert!(r > 0.3 && r < 1.0, "gentle release: {r}");
        assert_eq!(punch_env(2.5, 1.0, 2.0, &c), 0.0);
    }

    #[test]
    fn punch_zooms_around_the_face() {
        let tg: Vec<Target> = (0..90).map(|i| tgt(i as f64 / 15.0, 300.0)).collect();
        let mut p = run(&tg, 6.0, &[], &[], &[]);
        let before = p[60];
        let n = apply_punches(&mut p, &[(1.8, 2.6)], FPS, W, H, &PunchCfg::default());
        assert_eq!(n, 1);
        let z = p[66]; // 2.2s: fully punched
        assert!((before.rect.w / z.rect.w - 1.18).abs() < 1e-6);
        // The face keeps its relative screen position.
        let rel = |q: &Pose| ((q.ax - q.rect.x) / q.rect.w, (q.ay - q.rect.y) / q.rect.h);
        let (a, b) = (rel(&before), rel(&z));
        assert!((a.0 - b.0).abs() < 1e-9 && (a.1 - b.1).abs() < 1e-9);
        assert_eq!(p[10].rect, before.rect, "outside the window: untouched");
    }

    #[test]
    fn punches_skip_moving_cameras() {
        let mut tg: Vec<Target> = (0..45).map(|i| tgt(i as f64 / 15.0, 250.0)).collect();
        for i in 45..90 {
            let mut t = tgt(i as f64 / 15.0, 350.0);
            t.cut = i == 45;
            tg.push(t);
        }
        let mut p = run(&tg, 6.0, &[], &[], &[]);
        let n = apply_punches(&mut p, &[(2.9, 3.4)], FPS, W, H, &PunchCfg::default());
        assert_eq!(n, 0, "never punch during a glide");
    }

    fn run_cfg(cfg: &CamCfg, targets: &[Target], secs: f64, hards: &[f64]) -> Vec<Pose> {
        plan(
            &PlanInput {
                targets,
                frames: (secs * FPS) as usize,
                fps: FPS,
                src_w: W,
                src_h: H,
                hards,
                jumps: &[],
                onsets: &[],
                canvas: crate::compose::Canvas::TALL,
            },
            cfg,
        )
    }

    /// A speaker who sways and drifts around the room (slow wander plus a
    /// quicker lean), with detection noise.
    fn wanderer() -> Vec<Target> {
        (0..300)
            .map(|i| {
                let t = i as f64 / 15.0;
                let cx = 300.0
                    + 38.0 * (std::f64::consts::TAU * t / 7.0).sin()
                    + 16.0 * (std::f64::consts::TAU * t / 2.3).sin()
                    + 3.0 * noise(i);
                tgt(t, cx)
            })
            .collect()
    }

    /// Total distance the window centre travels.
    fn travel(p: &[Pose]) -> f64 {
        p.windows(2)
            .map(|w| (w[1].rect.cx() - w[0].rect.cx()).abs())
            .sum()
    }

    #[test]
    fn smooth_and_absent_feel_are_exactly_todays_camera() {
        let tg = wanderer();
        let today = run(&tg, 20.0, &[], &[], &[]);
        assert!(today.windows(2).any(|w| w[0].rect != w[1].rect), "it moves");
        for feel in [None, Some(CameraFeel::Smooth)] {
            let cfg = CamCfg::for_feel(feel);
            assert_eq!(format!("{cfg:?}"), format!("{:?}", CamCfg::default()));
            assert!(run_cfg(&cfg, &tg, 20.0, &[]) == today, "{feel:?}");
        }
    }

    #[test]
    fn feel_sets_how_much_the_camera_moves() {
        let tg = wanderer();
        let steady = travel(&run_cfg(
            &CamCfg::for_feel(Some(CameraFeel::Steady)),
            &tg,
            20.0,
            &[],
        ));
        let smooth = travel(&run_cfg(&CamCfg::default(), &tg, 20.0, &[]));
        let lively = travel(&run_cfg(
            &CamCfg::for_feel(Some(CameraFeel::Lively)),
            &tg,
            20.0,
            &[],
        ));
        assert!(steady < smooth, "steady {steady:.0} < smooth {smooth:.0}");
        assert!(smooth < lively, "smooth {smooth:.0} < lively {lively:.0}");
        // Clearly apart, not a rounding difference.
        assert!(
            steady < smooth * 0.85 && lively > smooth * 1.15,
            "{steady:.0} {smooth:.0} {lively:.0}"
        );
    }

    #[test]
    fn steady_answers_a_lean_later_and_lively_sooner() {
        // The speaker shifts 40px at 4s and stays there. The camera's top
        // speed: steady eases over slowly, lively gets there briskly.
        let tg: Vec<Target> = (0..150)
            .map(|i| {
                let t = i as f64 / 15.0;
                tgt(t, if t < 4.0 { 300.0 } else { 340.0 })
            })
            .collect();
        let at = |feel| {
            max_step(&cxs(&run_cfg(
                &CamCfg::for_feel(Some(feel)),
                &tg,
                10.0,
                &[],
            )))
        };
        let (steady, smooth, lively) = (
            at(CameraFeel::Steady),
            at(CameraFeel::Smooth),
            at(CameraFeel::Lively),
        );
        assert!(
            steady < smooth && smooth < lively,
            "{steady:.1} {smooth:.1} {lively:.1}"
        );
    }

    #[test]
    fn locked_holds_one_framing_per_shot_and_cuts_between_shots() {
        let tg = wanderer();
        let p = run_cfg(
            &CamCfg::for_feel(Some(CameraFeel::Locked)),
            &tg,
            20.0,
            &[10.0],
        );
        assert_eq!(p.len(), 600);
        assert!(p[..300].iter().all(|q| *q == p[0]), "shot 1 is one pose");
        assert!(p[300..].iter().all(|q| *q == p[300]), "shot 2 is one pose");
        // Each shot's framing sits on where the speaker mostly was.
        assert!((p[0].rect.cx() - 300.0).abs() < 25.0, "{}", p[0].rect.cx());
        assert!((p[300].rect.cx() - 300.0).abs() < 25.0);
        // Without a cut the whole clip is one framing.
        let one = run_cfg(&CamCfg::for_feel(Some(CameraFeel::Locked)), &tg, 20.0, &[]);
        assert!(one.iter().all(|q| *q == one[0]));
        // A walking speaker still gets two different framings across a cut.
        let mut walk: Vec<Target> = (0..150).map(|i| tgt(i as f64 / 15.0, 220.0)).collect();
        walk.extend((150..300).map(|i| tgt(i as f64 / 15.0, 420.0)));
        let w = run_cfg(
            &CamCfg::for_feel(Some(CameraFeel::Locked)),
            &walk,
            20.0,
            &[10.0],
        );
        assert!((w[0].rect.cx() - 220.0).abs() < 1.0 && (w[599].rect.cx() - 420.0).abs() < 1.0);
    }

    #[test]
    fn locked_holds_the_dominant_kind_of_a_mixed_shot() {
        let wide = |t: f64| Target {
            t,
            rect: Rect {
                x: 0.0,
                y: 0.0,
                w: W,
                h: H,
            },
            ax: W / 2.0,
            ay: H / 2.0,
            kind: Kind::Wide,
            cut: false,
            weak: false,
        };
        let mut tg: Vec<Target> = (0..30).map(|i| wide(i as f64 / 15.0)).collect();
        for i in 30..120 {
            let mut t = tgt(i as f64 / 15.0, 300.0);
            t.cut = i == 30;
            tg.push(t);
        }
        let p = run_cfg(&CamCfg::for_feel(Some(CameraFeel::Locked)), &tg, 8.0, &[]);
        assert!(p.iter().all(|q| *q == p[0]));
        assert_eq!(p[0].kind, Kind::Subject);
        assert!((p[0].rect.cx() - 300.0).abs() < 1.0);
    }

    #[test]
    fn punch_look_sets_the_peak_and_absent_is_today() {
        // Absent: exactly the old construction.
        let cap = 1.5;
        let today = PunchCfg {
            zoom: PunchCfg::default().zoom.min(cap),
            ..PunchCfg::default()
        };
        assert_eq!(format!("{:?}", punch_cfg(None, cap)), format!("{today:?}"));
        assert_eq!(
            punch_cfg(None, 1.1).zoom,
            1.1,
            "resolution floor still holds"
        );
        assert_eq!(punch_cfg(Some(1.3), cap).zoom, 1.3);
        assert_eq!(punch_cfg(Some(1.4), 1.25).zoom, 1.25);
        // The peak is what the camera reaches.
        let tg: Vec<Target> = (0..90).map(|i| tgt(i as f64 / 15.0, 300.0)).collect();
        let base = run(&tg, 6.0, &[], &[], &[]);
        for peak in [1.1, 1.3, 1.4] {
            let mut p = base.clone();
            let n = apply_punches(
                &mut p,
                &[(1.8, 2.6)],
                FPS,
                W,
                H,
                &punch_cfg(Some(peak), 2.0),
            );
            assert_eq!(n, 1);
            assert!(
                (base[66].rect.w / p[66].rect.w - peak).abs() < 1e-6,
                "{peak}"
            );
        }
        // 1.0 is no visible punch: the path is untouched.
        let mut p = base.clone();
        apply_punches(&mut p, &[(1.8, 2.6)], FPS, W, H, &punch_cfg(Some(1.0), 2.0));
        assert!(p == base);
        // Absent through the planner = the old default call.
        let (mut a, mut b) = (base.clone(), base.clone());
        apply_punches(&mut a, &[(1.8, 2.6)], FPS, W, H, &punch_cfg(None, 9.0));
        apply_punches(&mut b, &[(1.8, 2.6)], FPS, W, H, &PunchCfg::default());
        assert!(a == b);
    }
}
