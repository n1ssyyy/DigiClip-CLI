//! Virtual camera planner: per-sample framing targets → one camera rect
//! per output frame.
//!
//! The whole clip is known before the first frame renders, so the camera
//! is planned offline like an editor would, not chased like a live
//! follower:
//!
//! 1. **Dense desired signal.** Targets (8–15 Hz detections) become a
//!    per-frame signal: linear between samples, stepped at framing changes,
//!    placeholders (dropouts, silence holds, post-cut settles) filled from
//!    the nearest real evidence, then median-filtered to kill spikes.
//! 2. **Holds (dead zone).** The camera locks off and stays put while the
//!    subject moves inside a dead zone around the framing. Only a
//!    *sustained* exit (or a confirmed handoff) starts a new hold, whose
//!    pose is the median of everything it covers. A locked camera is the
//!    most professional shot there is; it also means detection noise can
//!    never become camera shake.
//! 3. **Moves.** Between holds the camera either *cuts* or *glides*:
//!    - reframes near an edit jump-cut snap onto the cut (the edit hides
//!      them);
//!    - speaker switches between people who don't share a frame cut on the
//!      new speaker's first word (no whip-pan across the table);
//!    - layout changes (close-up <-> letterboxed wide) cut too;
//!    - everything else glides on a smootherstep ease (zero velocity and
//!      acceleration at both ends) timed by distance, and starts slightly
//!      *before* the need (offline anticipation instead of lag).
//! 4. A light zero-phase smoothing rounds chained moves; shot cuts and
//!    snaps are never smoothed across.
//!
//! Emphasis punch-ins are a separate layer ([`apply_punches`]): a fast
//! eased zoom anchored on the speaker's face (the face stays put on
//! screen, the frame tightens around it), held through the phrase, then
//! released — never mixed into the framing path, so they can't be mistaken
//! for subject motion.

use crate::compose::Rect;

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

/// Planner tuning. Dead zones are fractions of the current window.
#[derive(Debug, Clone)]
pub struct CamCfg {
    /// Horizontal dead zone (fraction of window width).
    pub dz_x: f64,
    /// Vertical dead zone (fraction of window height).
    pub dz_y: f64,
    /// Zoom dead zone (|ln size ratio|).
    pub dz_z: f64,
    /// A drift must stay outside the dead zone this long to reframe (s).
    pub persist_s: f64,
    /// Lookahead used to pick a new hold's provisional pose (s).
    pub look_s: f64,
    /// Spike-killing median window on the desired signal (s).
    pub median_s: f64,
    /// Handoffs this far apart (fraction of window width) cut, not glide.
    pub switch_cut_frac: f64,
    /// Changes between a crop and the letterboxed wide cut instead of
    /// morphing (a layout change, like a multicam edit — not a zoom).
    pub layout_cut: bool,
    /// Reframes within this distance of an edit jump-cut snap onto it (s).
    pub jump_win_s: f64,
    /// Glide duration = clamp(base + per * distance, min, max) (s).
    pub glide_base_s: f64,
    pub glide_per_s: f64,
    pub glide_min_s: f64,
    pub glide_max_s: f64,
    /// Handoff moves start this long before the new speaker's onset (s).
    pub lead_s: f64,
    /// Zero-phase smoothing sigma applied to the final path (s).
    pub smooth_s: f64,
    /// Handoff cuts snap to an utterance onset within this lookback (s).
    pub onset_back_s: f64,
    /// Drift reframes closer together than this chain into one continuous
    /// follow (a walking presenter) instead of stop-and-go moves (s).
    pub track_hold_s: f64,
    /// Smoothing sigma of that continuous follow (s).
    pub track_sigma_s: f64,
}

impl Default for CamCfg {
    fn default() -> Self {
        // Dead zones follow AutoFlip's static-by-default bias (hold while
        // the subject stays within ~±12% of the window); switches farther
        // than half a window cut rather than whip-pan; glides are
        // smootherstep over 0.45–1.0 s by distance.
        Self {
            dz_x: 0.12,
            dz_y: 0.08,
            dz_z: 0.12,
            persist_s: 0.45,
            look_s: 0.8,
            median_s: 0.33,
            switch_cut_frac: 0.5,
            layout_cut: true,
            jump_win_s: 0.5,
            glide_base_s: 0.4,
            glide_per_s: 0.5,
            glide_min_s: 0.45,
            glide_max_s: 1.0,
            lead_s: 0.1,
            smooth_s: 0.08,
            onset_back_s: 1.2,
            track_hold_s: 1.6,
            track_sigma_s: 0.4,
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
}

/// Channel vector: center x/y, ln width, ln height.
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
    /// Dead-zone-normalized deviation of `o` from this framing (>1 = out).
    fn dev(&self, o: &Ch, cfg: &CamCfg) -> f64 {
        let (w, h) = (self.lw.exp(), self.lh.exp());
        ((o.cx - self.cx).abs() / (cfg.dz_x * w))
            .max((o.cy - self.cy).abs() / (cfg.dz_y * h))
            .max((o.lw - self.lw).abs() / cfg.dz_z)
            .max((o.lh - self.lh).abs() / cfg.dz_z)
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
    let base = crate::compose::base_rect(inp.src_w, inp.src_h);
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
    // --- 1. dense desired signal ------------------------------------------
    // Strong samples only; weak ones (placeholders) are filled from
    // neighbors. A shot made only of placeholders still uses them.
    let strong: Vec<&Target> = {
        let s: Vec<&Target> = tg.iter().copied().filter(|t| !t.weak).collect();
        if s.is_empty() {
            tg.to_vec()
        } else {
            s
        }
    };
    let kind_at: Vec<Kind>;
    let mut desired: Vec<Ch> = Vec::with_capacity(len);
    // Frame index where a discrete change begins (handoff/kind change).
    let mut change = vec![false; len];
    {
        let mut kinds = Vec::with_capacity(len);
        let mut i = 0usize;
        for k in 0..len {
            let t = (f0 + k) as f64 / fps;
            while i + 1 < strong.len() && strong[i + 1].t <= t + 1e-9 {
                i += 1;
            }
            let a = strong[i];
            let ch_a = Ch::of(&a.rect, a.ax, a.ay);
            let ch = if t <= a.t || i + 1 >= strong.len() {
                ch_a
            } else {
                let b = strong[i + 1];
                if b.cut || b.kind != a.kind {
                    ch_a // step: hold until the change lands
                } else {
                    let u = ((t - a.t) / (b.t - a.t).max(1e-9)).clamp(0.0, 1.0);
                    ch_a.lerp(&Ch::of(&b.rect, b.ax, b.ay), u)
                }
            };
            desired.push(ch);
            kinds.push(a.kind);
        }
        for k in 1..len {
            if kinds[k] != kinds[k - 1] {
                change[k] = true;
            }
        }
        // Handoff flags: the first frame at/after each strong cut sample.
        for s in strong.iter().skip(1).filter(|s| s.cut) {
            let k = ((s.t * fps).round() as usize).saturating_sub(f0);
            if k > 0 && k < len {
                change[k] = true;
            }
        }
        kind_at = kinds;
    }
    // Spike-killing median, never across a discrete change.
    let half = ((cfg.median_s * fps / 2.0).round() as usize).max(1);
    let desired: Vec<Ch> = {
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
        (0..len)
            .map(|k| {
                let lo = k.saturating_sub(half).max(seg_start[k]);
                let hi = (k + half + 1).min(seg_end[k]);
                median_ch(&desired[lo..hi])
            })
            .collect()
    };

    // --- 2. holds ----------------------------------------------------------
    let persist = ((cfg.persist_s * fps).round() as usize).max(1);
    let look = ((cfg.look_s * fps).round() as usize).max(1);
    let next_change = |k: usize| (k + 1..len).find(|&j| change[j]).unwrap_or(len);
    let provisional = |k: usize| median_ch(&desired[k..(k + look).min(next_change(k)).max(k + 1)]);
    // (start frame, triggered by a discrete change?)
    let mut holds: Vec<(usize, bool)> = vec![(0, false)];
    let mut p = provisional(0);
    let mut kind = kind_at[0];
    let mut run = 0usize;
    let mut k = 1;
    while k < len {
        let outside = kind_at[k] != kind || p.dev(&desired[k], cfg) > 1.0;
        if change[k] && outside {
            holds.push((k, true));
            p = provisional(k);
            kind = kind_at[k];
            run = 0;
        } else if outside {
            run += 1;
            if run >= persist {
                let e = k + 1 - run;
                holds.push((e, false));
                p = provisional(e);
                kind = kind_at[e];
                run = 0;
                k = e;
            }
        } else {
            run = 0;
        }
        k += 1;
    }
    // Final pose per hold: median of everything it covers.
    let mut hp: Vec<(usize, bool, Ch, Kind)> = holds
        .iter()
        .enumerate()
        .map(|(i, &(s, trig))| {
            let e = holds.get(i + 1).map(|h| h.0).unwrap_or(len);
            (s, trig, median_ch(&desired[s..e.max(s + 1)]), kind_at[s])
        })
        .collect();
    // Merge near-identical neighbors (a move that wouldn't read as one).
    let mut merged: Vec<(usize, bool, Ch, Kind)> = Vec::with_capacity(hp.len());
    for h in hp.drain(..) {
        if let Some(last) = merged.last() {
            if last.3 == h.3 && last.2.dev(&h.2, cfg) < 0.35 {
                continue;
            }
        }
        merged.push(h);
    }

    // --- 3. moves -------------------------------------------------------------
    struct Move {
        at: usize,
        /// Triggered by a discrete change (handoff, framing class flip).
        trig: bool,
        snap: bool,
        start: usize,
        dur: usize,
    }
    let local = |f: usize| f.checked_sub(f0).filter(|&k| k < len);
    let mut moves: Vec<Move> = Vec::new();
    let mut last_at = 0usize;
    for i in 1..merged.len() {
        let (mut at, trig, to, to_kind) = merged[i];
        let (_, _, from, from_kind) = merged[i - 1];
        let d = from.dist(&to);
        // Handoffs land on the new speaker's first word (detection confirms
        // a turn ~0.3–0.8s late; the words say when it really began).
        if trig && to_kind == Kind::Subject {
            let t_at = at;
            let back = (cfg.onset_back_s * fps) as usize;
            if let Some(o) = onset_frames
                .iter()
                .filter_map(|&o| local(o))
                .filter(|&o| o + back >= t_at && o <= t_at + (0.15 * fps) as usize)
                .filter(|&o| o > last_at + persist)
                .max()
            {
                at = o;
            }
        }
        // An edit jump-cut close by hides the reframe: snap onto it.
        let jw = (cfg.jump_win_s * fps).round() as usize;
        let jump = jump_frames
            .iter()
            .filter_map(|&j| local(j))
            .filter(|&j| j > last_at && j + jw >= at && j <= at + jw)
            .min_by_key(|&j| (j as i64 - at as i64).abs());
        let far_switch = trig
            && from_kind == Kind::Subject
            && to_kind == Kind::Subject
            && (to.cx - from.cx).abs() > cfg.switch_cut_frac * from.lw.exp().max(to.lw.exp());
        let layout = cfg.layout_cut && trig && (from_kind == Kind::Wide) != (to_kind == Kind::Wide);
        let (at, snap) = match jump {
            Some(j) => (j, true),
            None => (at, far_switch || layout),
        };
        let dur = ((cfg.glide_base_s + cfg.glide_per_s * d).clamp(cfg.glide_min_s, cfg.glide_max_s)
            * fps)
            .round() as usize;
        let lead = if trig {
            (cfg.lead_s * fps).round() as usize
        } else {
            dur * 35 / 100 // drift: center the move on the exit
        };
        let start = at.saturating_sub(lead).max(last_at + 1).min(at);
        merged[i].0 = at;
        moves.push(Move {
            at,
            trig,
            snap,
            start,
            dur: dur.max(1),
        });
        last_at = at;
    }
    // A glide must land before the next snap. A framing that would barely
    // be on screen before the cut is skipped (a move straight into a cut
    // reads as a twitch; the cut takes the camera past it); otherwise the
    // glide is squeezed to fit.
    let min_show = (0.8 * fps).round() as usize;
    // Same at the shot's end: the shot cut reframes anyway, so a change
    // that would show for a blink before it (a handoff snap 0.3 s before
    // the edit cuts away) is dropped and the current framing rides out.
    for i in 0..moves.len() {
        if len.saturating_sub(moves[i].at) < min_show {
            merged[i + 1].2 = merged[i].2;
            merged[i + 1].3 = merged[i].3;
        } else if moves[i].start + moves[i].dur > len {
            moves[i].dur = (len - moves[i].start).max(1);
        }
    }
    for i in 0..moves.len().saturating_sub(1) {
        if moves[i].snap || !moves[i + 1].snap {
            continue;
        }
        let room = moves[i + 1].at.saturating_sub(moves[i].start);
        if room >= moves[i].dur {
            continue;
        }
        if moves[i + 1].at.saturating_sub(moves[i].at) >= min_show {
            moves[i].dur = room.max(1);
        } else {
            merged[i + 1].2 = merged[i].2;
            merged[i + 1].3 = merged[i].3;
        }
    }

    // --- 4. render the path ------------------------------------------------------
    let mut path: Vec<Ch> = Vec::with_capacity(len);
    let mut kinds: Vec<Kind> = Vec::with_capacity(len);
    for i in 0..merged.len() {
        let s = merged[i].0;
        let e = merged.get(i + 1).map(|h| h.0).unwrap_or(len);
        for _ in s..e.max(s) {
            path.push(merged[i].2);
            kinds.push(merged[i].3);
        }
    }
    path.truncate(len);
    kinds.truncate(len);
    while path.len() < len {
        path.push(*path.last().unwrap_or(&merged[0].2));
        kinds.push(*kinds.last().unwrap_or(&merged[0].3));
    }
    // Snap points split the smoothing; glides overwrite their span.
    let mut snaps = vec![false; len];
    for (i, m) in moves.iter().enumerate() {
        let to = merged[i + 1].2;
        if m.snap {
            if m.at < len {
                snaps[m.at] = true;
            }
            continue;
        }
        let from = if m.start > 0 {
            path[m.start - 1]
        } else {
            path[0]
        };
        // Never past the next snap (it owns the frames from its cut on).
        let cap = moves
            .get(i + 1)
            .filter(|n| n.snap)
            .map(|n| n.at)
            .unwrap_or(len);
        let end = (m.start + m.dur).min(len).min(cap);
        for k in m.start..end {
            let u = (k + 1 - m.start) as f64 / m.dur as f64;
            path[k] = from.lerp(&to, smootherstep(u));
        }
        // The move's own tail belongs to the new hold.
        let next = moves.get(i + 1).map(|n| n.start).unwrap_or(len);
        for k in end..next.min(len) {
            path[k] = to;
        }
        // Framing class flips mid-glide (a dolly lands as its target kind).
        for k in m.start..end.min(len) {
            kinds[k] = merged[i + 1].3;
        }
    }
    // --- 5. continuous follow ----------------------------------------------------
    // Drift reframes chained closer than `track_hold_s` mean the subject
    // itself is moving (a walking presenter). Stop-and-go holds would read
    // as a nervous operator, so that stretch follows the zero-phase
    // smoothed subject path instead, blended in and out.
    let hold_gap = (cfg.track_hold_s * fps).round() as usize;
    let drift = |m: &Move| !m.snap && !m.trig;
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < moves.len() {
        if !drift(&moves[i]) {
            i += 1;
            continue;
        }
        let mut j = i;
        while j + 1 < moves.len()
            && drift(&moves[j + 1])
            && moves[j + 1].at - moves[j].at < hold_gap
        {
            j += 1;
        }
        if j > i {
            let a = moves[i].start;
            let b = (moves[j].start + moves[j].dur).min(len);
            // Never follow across a discrete framing change.
            let b = (a + 1..b).find(|&k| change[k] || snaps[k]).unwrap_or(b);
            spans.push((a, b));
        }
        i = j + 1;
    }
    if !spans.is_empty() {
        let follow = smooth_segments(&desired, &change, cfg.track_sigma_s * fps);
        let ramp = ((0.5 * fps).round() as usize).max(1);
        for (a, b) in spans {
            let n = b - a;
            let ramp = ramp.min(n / 2).max(1);
            for k in a..b {
                let edge = (k - a + 1).min(b - k) as f64 / ramp as f64;
                let u = edge.clamp(0.0, 1.0);
                let w = u * u * (3.0 - 2.0 * u);
                path[k] = path[k].lerp(&follow[k], w);
            }
        }
    }

    let path = smooth_segments(&path, &snaps, cfg.smooth_s * fps);
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

/// Zero-phase Gaussian smoothing per snap-delimited segment (edges
/// replicate, so holds stay exactly still).
fn smooth_segments(path: &[Ch], snaps: &[bool], sigma: f64) -> Vec<Ch> {
    if sigma < 0.5 || path.len() < 3 {
        return path.to_vec();
    }
    let r = (sigma * 3.0).ceil() as isize;
    let w: Vec<f64> = (-r..=r)
        .map(|i| (-(i * i) as f64 / (2.0 * sigma * sigma)).exp())
        .collect();
    let wsum: f64 = w.iter().sum();
    let mut out = path.to_vec();
    let mut s = 0;
    let n = path.len();
    while s < n {
        let mut e = s + 1;
        while e < n && !snaps[e] {
            e += 1;
        }
        let seg = &path[s..e];
        // Constant segments are exact already (and the common case).
        if seg.windows(2).any(|p| p[0] != p[1]) {
            for k in 0..seg.len() {
                let mut acc = [0.0f64; 6];
                for (j, wj) in w.iter().enumerate() {
                    let idx =
                        (k as isize + j as isize - r).clamp(0, seg.len() as isize - 1) as usize;
                    let c = &seg[idx];
                    for (a, v) in acc.iter_mut().zip([c.cx, c.cy, c.lw, c.lh, c.ax, c.ay]) {
                        *a += wj * v;
                    }
                }
                let a = acc.map(|v| v / wsum);
                out[s + k] = Ch {
                    cx: a[0],
                    cy: a[1],
                    lw: a[2],
                    lh: a[3],
                    ax: a[4],
                    ay: a[5],
                };
            }
        }
        s = e;
    }
    out
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
    if n == 0 || windows.is_empty() {
        return 0;
    }
    let moving: Vec<bool> = (0..n)
        .map(|k| k > 0 && poses[k].rect != poses[k - 1].rect)
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
}
