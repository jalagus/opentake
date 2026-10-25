//! Automation: envelopes that move a track's fader, pan, mute or a plug-in parameter over
//! time. Values are normalized 0..1 — the fader's travel for volume (so a curve feels like
//! moving the fader), left..right for pan, on/off for mute, the plug-in's own normalized
//! value for parameters.

use serde::{Deserialize, Serialize};

/// Fader range: -∞ at the bottom, +6 dB at the top, 0 dB near the top like hardware faders.
pub const FADER_MIN_DB: f32 = -60.0;
pub const FADER_MAX_DB: f32 = 6.0;

/// Fader position (0..1) for a linear gain.
pub fn gain_to_pos(gain: f32) -> f32 {
    let db = gain_to_db(gain);
    if db <= FADER_MIN_DB { 0.0 } else { ((db - FADER_MIN_DB) / (FADER_MAX_DB - FADER_MIN_DB)).min(1.0).powf(1.6) }
}

/// Linear gain for a fader position (0..1).
pub fn pos_to_gain(pos: f32) -> f32 {
    if pos <= 0.0 {
        return 0.0;
    }
    db_to_gain(FADER_MIN_DB + pos.min(1.0).powf(1.0 / 1.6) * (FADER_MAX_DB - FADER_MIN_DB))
}

pub fn gain_to_db(gain: f32) -> f32 {
    if gain <= 0.0 { f32::NEG_INFINITY } else { 20.0 * gain.log10() }
}

pub fn db_to_gain(db: f32) -> f32 {
    if db <= FADER_MIN_DB { 0.0 } else { 10f32.powf(db / 20.0) }
}

/// What a lane moves.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum AutomationTarget {
    Volume,
    Pan,
    Mute,
    /// A plug-in parameter: the slot's persistent id (`PluginSlot::id`, which survives
    /// reordering, undo and saving) and the parameter id.
    Param { slot: u32, param: u32 },
}

impl AutomationTarget {
    /// Mute switches: its envelope jumps instead of ramping.
    pub fn stepped(self) -> bool {
        self == AutomationTarget::Mute
    }

    /// The value in this target's own terms for a normalized one (volume: dB, pan: -1..1,
    /// mute: 0/1, parameters: unchanged).
    pub fn to_units(self, v: f32) -> f32 {
        match self {
            AutomationTarget::Volume => gain_to_db(pos_to_gain(v)),
            AutomationTarget::Pan => v * 2.0 - 1.0,
            AutomationTarget::Mute => (v >= 0.5) as u8 as f32,
            AutomationTarget::Param { .. } => v,
        }
    }

    pub fn from_units(self, u: f32) -> f32 {
        match self {
            AutomationTarget::Volume => gain_to_pos(db_to_gain(u)),
            AutomationTarget::Pan => (u.clamp(-1.0, 1.0) + 1.0) / 2.0,
            _ => u.clamp(0.0, 1.0),
        }
    }

    /// The value as text (parameters as a percentage; the engine knows their own display).
    pub fn format(self, v: f32) -> String {
        match self {
            AutomationTarget::Volume => {
                let db = self.to_units(v);
                if db <= FADER_MIN_DB { "-∞ dB".into() } else { format!("{db:+.1} dB") }
            }
            AutomationTarget::Pan => match (self.to_units(v) * 100.0).round() as i32 {
                0 => "C".into(),
                p if p < 0 => format!("L{}", -p),
                p => format!("R{p}"),
            },
            AutomationTarget::Mute => if v >= 0.5 { "Muted" } else { "On" }.into(),
            AutomationTarget::Param { .. } => format!("{:.0}%", v * 100.0),
        }
    }
}

/// How the envelope gets from a point to the next one.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Curve {
    #[default]
    Linear,
    /// Stay at this point's value until the next point (a step).
    Hold,
    /// Ease in and out (S-curve).
    Smooth,
    /// Move quickly at first, then settle.
    FastStart,
    /// Start slowly, then move quickly.
    FastEnd,
}

impl Curve {
    pub const ALL: [Curve; 5] = [Curve::Linear, Curve::Hold, Curve::Smooth, Curve::FastStart, Curve::FastEnd];

    pub fn label(self) -> &'static str {
        match self {
            Curve::Linear => "Linear",
            Curve::Hold => "Hold (Step)",
            Curve::Smooth => "Smooth",
            Curve::FastStart => "Fast Start",
            Curve::FastEnd => "Fast End",
        }
    }

    /// Fraction of the way from one value to the next at `f` (0..1) of the time.
    fn shape(self, f: f64) -> f64 {
        match self {
            Curve::Linear => f,
            Curve::Hold => 0.0,
            Curve::Smooth => (1.0 - (f * std::f64::consts::PI).cos()) / 2.0,
            Curve::FastStart => 1.0 - (1.0 - f) * (1.0 - f),
            Curve::FastEnd => f * f,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct AutomationPoint {
    pub time_secs: f64,
    /// Normalized 0..1 (see the module docs).
    pub value: f32,
    /// Shape of the segment from this point to the next.
    #[serde(default, skip_serializing_if = "is_linear")]
    pub curve: Curve,
}

fn is_linear(c: &Curve) -> bool {
    *c == Curve::Linear
}

impl AutomationPoint {
    pub fn new(time_secs: f64, value: f32) -> Self {
        Self { time_secs: time_secs.max(0.0), value: value.clamp(0.0, 1.0), curve: Curve::Linear }
    }
}

/// Logic's automation modes. Read plays the envelope; Touch and Latch also record control
/// moves while playing (Touch until the control is released, Latch until the transport
/// stops); Off ignores the lane.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AutomationMode {
    Off,
    #[default]
    Read,
    Touch,
    Latch,
}

impl AutomationMode {
    pub const ALL: [AutomationMode; 4] = [AutomationMode::Read, AutomationMode::Touch, AutomationMode::Latch, AutomationMode::Off];

    pub fn label(self) -> &'static str {
        match self {
            AutomationMode::Off => "Off",
            AutomationMode::Read => "Read",
            AutomationMode::Touch => "Touch",
            AutomationMode::Latch => "Latch",
        }
    }

    pub fn writes(self) -> bool {
        matches!(self, AutomationMode::Touch | AutomationMode::Latch)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AutomationLane {
    pub target: AutomationTarget,
    /// Sorted by time.
    #[serde(default)]
    pub points: Vec<AutomationPoint>,
    /// Drawn under the track when the track shows its automation.
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub shown: bool,
    #[serde(default)]
    pub mode: AutomationMode,
}

fn yes() -> bool {
    true
}

fn is_true(b: &bool) -> bool {
    *b
}

/// Envelope value at `t` (seconds): the first point's value before it, the last one's after
/// it, the segment's curve in between. `None` without points.
pub fn envelope_at(points: &[AutomationPoint], t: f64, stepped: bool) -> Option<f32> {
    let i = points.partition_point(|p| p.time_secs <= t);
    let (a, b) = match (i.checked_sub(1).map(|k| &points[k]), points.get(i)) {
        (Some(a), Some(b)) => (a, b),
        (Some(p), None) | (None, Some(p)) => return Some(p.value),
        (None, None) => return None,
    };
    let span = b.time_secs - a.time_secs;
    if stepped || span <= 0.0 {
        return Some(a.value);
    }
    let f = a.curve.shape(((t - a.time_secs) / span).clamp(0.0, 1.0));
    Some(a.value + (b.value - a.value) * f as f32)
}

impl AutomationLane {
    pub fn new(target: AutomationTarget) -> Self {
        Self { target, points: Vec::new(), shown: true, mode: AutomationMode::Read }
    }

    /// Plays (moves its target): not off, and has points.
    pub fn is_active(&self) -> bool {
        self.mode != AutomationMode::Off && !self.points.is_empty()
    }

    pub fn value_at(&self, t: f64) -> Option<f32> {
        envelope_at(&self.points, t, self.target.stepped())
    }

    /// Add a point, keeping the points sorted (after any already at that time). Returns its index.
    pub fn insert(&mut self, point: AutomationPoint) -> usize {
        let p = AutomationPoint::new(point.time_secs, point.value);
        let i = self.points.partition_point(|q| q.time_secs <= p.time_secs);
        self.points.insert(i, AutomationPoint { curve: point.curve, ..p });
        i
    }

    /// Replace all points (sorted, values clamped).
    pub fn set_points(&mut self, points: impl IntoIterator<Item = AutomationPoint>) {
        self.points = points.into_iter().map(|p| AutomationPoint { curve: p.curve, ..AutomationPoint::new(p.time_secs, p.value) }).collect();
        self.points.sort_by(|a, b| a.time_secs.total_cmp(&b.time_secs));
    }

    /// Remove the points in `from..=to` (seconds).
    pub fn clear_range(&mut self, from: f64, to: f64) {
        self.points.retain(|p| p.time_secs < from || p.time_secs > to);
    }

    /// Move point `index` to `time`/`value`, kept between its neighbours so the order (and
    /// the index) stays. Returns false if there's no such point.
    pub fn move_point(&mut self, index: usize, time: f64, value: f32) -> bool {
        if index >= self.points.len() {
            return false;
        }
        let lo = index.checked_sub(1).map_or(0.0, |k| self.points[k].time_secs);
        let hi = self.points.get(index + 1).map_or(f64::INFINITY, |p| p.time_secs);
        let p = &mut self.points[index];
        p.time_secs = time.clamp(lo, hi.max(lo));
        p.value = value.clamp(0.0, 1.0);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pt(t: f64, v: f32, curve: Curve) -> AutomationPoint {
        AutomationPoint { time_secs: t, value: v, curve }
    }

    #[test]
    fn evaluates_curves_holds_and_the_ends() {
        let mut lane = AutomationLane::new(AutomationTarget::Volume);
        assert_eq!(lane.value_at(1.0), None);
        lane.set_points([pt(3.0, 0.0, Curve::Linear), pt(1.0, 1.0, Curve::Linear), pt(5.0, 1.0, Curve::Hold), pt(7.0, 0.2, Curve::Linear)]);
        assert_eq!(lane.points.iter().map(|p| p.time_secs).collect::<Vec<_>>(), vec![1.0, 3.0, 5.0, 7.0], "sorted");
        assert_eq!(lane.value_at(0.0), Some(1.0), "first value before the first point");
        assert_eq!(lane.value_at(2.0), Some(0.5), "linear");
        assert_eq!(lane.value_at(6.9), Some(1.0), "hold until the next point");
        assert_eq!(lane.value_at(7.0), Some(0.2));
        assert_eq!(lane.value_at(99.0), Some(0.2), "last value after the end");
        // Shaped segments meet their end values; fast start is ahead of linear, fast end behind.
        for (curve, mid) in [(Curve::Smooth, 0.5), (Curve::FastStart, 0.75), (Curve::FastEnd, 0.25)] {
            let l = AutomationLane { points: vec![pt(0.0, 0.0, curve), pt(2.0, 1.0, Curve::Linear)], ..AutomationLane::new(AutomationTarget::Pan) };
            assert!((l.value_at(1.0).unwrap() - mid).abs() < 1e-6, "{curve:?}");
            assert_eq!((l.value_at(0.0), l.value_at(2.0)), (Some(0.0), Some(1.0)));
        }
        // Mute switches at the point, never in between.
        let mute = AutomationLane { points: vec![pt(0.0, 0.0, Curve::Linear), pt(2.0, 1.0, Curve::Linear)], ..AutomationLane::new(AutomationTarget::Mute) };
        assert_eq!((mute.value_at(1.99), mute.value_at(2.0)), (Some(0.0), Some(1.0)));
    }

    #[test]
    fn inserting_moving_and_clearing_points() {
        let mut lane = AutomationLane::new(AutomationTarget::Pan);
        assert_eq!(lane.insert(AutomationPoint::new(2.0, 0.5)), 0);
        assert_eq!(lane.insert(AutomationPoint::new(1.0, 1.5)), 0, "earlier point goes first");
        assert_eq!(lane.points[0].value, 1.0, "values are clamped");
        assert_eq!(lane.insert(AutomationPoint::new(2.0, 0.1)), 2, "same time: after the existing one");
        // A jump: the later of two points at the same time wins from there on.
        assert_eq!(lane.value_at(2.0), Some(0.1));
        lane.insert(AutomationPoint::new(4.0, 0.0));
        assert!(lane.move_point(1, 9.0, 0.3), "kept before its right neighbour");
        assert_eq!(lane.points[1].time_secs, 2.0);
        assert!(lane.move_point(3, 0.5, 0.3));
        assert_eq!(lane.points[3].time_secs, 2.0, "kept after its left neighbour");
        lane.clear_range(1.5, 2.5);
        assert_eq!(lane.points.len(), 1);
        assert!(!lane.move_point(5, 0.0, 0.0));
    }

    #[test]
    fn fader_mapping_and_units() {
        assert!((gain_to_pos(1.0) - 0.858).abs() < 0.01, "0 dB near the top, like hardware faders");
        for g in [0.1f32, 0.5, 1.0, 1.9] {
            assert!((pos_to_gain(gain_to_pos(g)) - g).abs() < 1e-4);
        }
        assert_eq!(pos_to_gain(0.0), 0.0);
        let vol = AutomationTarget::Volume;
        assert!((vol.to_units(vol.from_units(-12.0)) + 12.0).abs() < 1e-3);
        assert_eq!(vol.format(gain_to_pos(1.0)), "+0.0 dB");
        assert_eq!(AutomationTarget::Pan.format(0.25), "L50");
        assert_eq!(AutomationTarget::Pan.from_units(1.0), 1.0);
    }

    #[test]
    fn lanes_serialize_compactly_and_old_files_load() {
        let mut lane = AutomationLane::new(AutomationTarget::Param { slot: 3, param: 7 });
        lane.insert(AutomationPoint::new(1.0, 0.5));
        let json = serde_json::to_string(&lane).unwrap();
        assert_eq!(json, r#"{"target":{"kind":"param","slot":3,"param":7},"points":[{"time_secs":1.0,"value":0.5}],"mode":"read"}"#);
        let back: AutomationLane = serde_json::from_str(r#"{"target":{"kind":"volume"}}"#).unwrap();
        assert!(back.shown && back.mode == AutomationMode::Read && back.points.is_empty());
    }
}
