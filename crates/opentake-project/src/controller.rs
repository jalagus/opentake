//! MIDI controller data of a MIDI region: pitch bend, control changes and channel pressure.
//!
//! Controllers are stored as discrete events (as MIDI sends them), not curves: each holds its
//! value until the next one. Editors draw them as steps and generate events at a sensible
//! density when a curve is drawn. Values are normalized: pitch bend -1..1 (0 = centre,
//! ±1 = the instrument's bend range), everything else 0..1 (127 = 1).

use serde::{Deserialize, Serialize};

/// Which controller a lane holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Controller {
    PitchBend,
    /// Control change 0..=127 (1 = mod wheel, 7 = volume, 10 = pan, 11 = expression, 64 = sustain).
    Cc(u8),
    /// Channel pressure (aftertouch).
    ChannelPressure,
}

/// Every controller kind fits a table this long (see [`Controller::slot`]).
pub const CONTROLLER_SLOTS: usize = 130;
/// Raw pitch-bend centre (14-bit).
pub const BEND_CENTRE: u16 = 8192;

impl Controller {
    pub const MOD_WHEEL: Self = Self::Cc(1);
    pub const VOLUME: Self = Self::Cc(7);
    pub const PAN: Self = Self::Cc(10);
    pub const EXPRESSION: Self = Self::Cc(11);
    pub const SUSTAIN: Self = Self::Cc(64);

    /// Index into a table of every controller: CCs 0..=127, then pitch bend and pressure.
    pub fn slot(self) -> usize {
        match self {
            Self::Cc(n) => (n & 0x7F) as usize,
            Self::PitchBend => 128,
            Self::ChannelPressure => 129,
        }
    }

    pub fn from_slot(slot: usize) -> Self {
        match slot {
            128 => Self::PitchBend,
            129 => Self::ChannelPressure,
            n => Self::Cc(n as u8 & 0x7F),
        }
    }

    pub fn is_bipolar(self) -> bool {
        self == Self::PitchBend
    }

    /// The raw MIDI value: 0..=16383 for pitch bend (8192 = centre), else 0..=127.
    pub fn raw(self, value: f32) -> u16 {
        match self {
            Self::PitchBend => ((value.clamp(-1.0, 1.0) + 1.0) * 8192.0).round().min(16383.0) as u16,
            _ => (value.clamp(0.0, 1.0) * 127.0).round() as u16,
        }
    }

    /// The normalized value of a raw MIDI value.
    pub fn from_raw(self, raw: u16) -> f32 {
        match self {
            Self::PitchBend => (raw.min(16383) as f32 - 8192.0) / 8192.0,
            _ => raw.min(127) as f32 / 127.0,
        }
    }

    /// What the controller rests at when nothing has moved it (sent when playback starts
    /// before a lane's first event), if it has a well-known neutral value.
    pub fn neutral(self) -> Option<f32> {
        match self {
            Self::PitchBend | Self::ChannelPressure | Self::MOD_WHEEL | Self::SUSTAIN => Some(0.0),
            Self::EXPRESSION => Some(1.0),
            _ => None,
        }
    }

    pub fn name(self) -> String {
        match self {
            Self::PitchBend => "Pitch Bend".into(),
            Self::ChannelPressure => "Channel Pressure".into(),
            Self::MOD_WHEEL => "Mod Wheel (CC1)".into(),
            Self::VOLUME => "Volume (CC7)".into(),
            Self::PAN => "Pan (CC10)".into(),
            Self::EXPRESSION => "Expression (CC11)".into(),
            Self::SUSTAIN => "Sustain (CC64)".into(),
            Self::Cc(n) => format!("CC{n}"),
        }
    }
}

/// A controller value at a time (seconds from the region start; value normalized).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ControllerEvent {
    pub time_secs: f64,
    pub value: f32,
}

/// One controller's events in a region, sorted by time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ControllerLane {
    pub kind: Controller,
    pub events: Vec<ControllerEvent>,
}

impl ControllerLane {
    pub fn new(kind: Controller, events: Vec<ControllerEvent>) -> Self {
        let mut lane = Self { kind, events };
        lane.normalize();
        lane
    }

    /// Sort by time, clamp values to the kind's range and drop exact duplicates (an event at
    /// the same time as the next one, or repeating the value before it).
    pub fn normalize(&mut self) {
        let (lo, hi) = if self.kind.is_bipolar() { (-1.0, 1.0) } else { (0.0, 1.0) };
        for e in &mut self.events {
            e.value = e.value.clamp(lo, hi);
            e.time_secs = e.time_secs.max(0.0);
        }
        self.events.sort_by(|a, b| a.time_secs.total_cmp(&b.time_secs));
        // Same time: the later one wins.
        let mut out: Vec<ControllerEvent> = Vec::with_capacity(self.events.len());
        for e in self.events.drain(..) {
            match out.last_mut() {
                Some(last) if (last.time_secs - e.time_secs).abs() < 1e-9 => *last = e,
                _ => out.push(e),
            }
        }
        out.dedup_by(|b, a| a.value == b.value);
        self.events = out;
    }

    /// The value in effect at `t` (the last event at or before it).
    pub fn value_at(&self, t: f64) -> Option<f32> {
        let i = self.events.partition_point(|e| e.time_secs <= t + 1e-9);
        i.checked_sub(1).map(|i| self.events[i].value)
    }

    /// Split at `cut`: events from `cut` on move to the returned lane, shifted to start at 0
    /// and led by the value in effect at the cut, so the right part starts where the left
    /// left off.
    pub fn split_off(&mut self, cut: f64) -> ControllerLane {
        let at = self.events.partition_point(|e| e.time_secs < cut - 1e-9);
        let mut right: Vec<ControllerEvent> = self.events.split_off(at).into_iter().map(|e| ControllerEvent { time_secs: (e.time_secs - cut).max(0.0), ..e }).collect();
        if right.first().is_none_or(|e| e.time_secs > 1e-9)
            && let Some(last) = self.events.last()
        {
            right.insert(0, ControllerEvent { time_secs: 0.0, value: last.value });
        }
        ControllerLane { kind: self.kind, events: right }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(t: f64, v: f32) -> ControllerEvent {
        ControllerEvent { time_secs: t, value: v }
    }

    #[test]
    fn raw_values_round_trip() {
        for raw in [0, 1, 4096, 8192, 12000, 16383] {
            assert_eq!(Controller::PitchBend.raw(Controller::PitchBend.from_raw(raw)), raw);
        }
        assert_eq!(Controller::PitchBend.raw(0.0), BEND_CENTRE);
        assert_eq!(Controller::PitchBend.raw(1.0), 16383);
        for raw in 0..=127 {
            assert_eq!(Controller::MOD_WHEEL.raw(Controller::MOD_WHEEL.from_raw(raw)), raw);
        }
        for slot in 0..CONTROLLER_SLOTS {
            assert_eq!(Controller::from_slot(slot).slot(), slot);
        }
    }

    #[test]
    fn lanes_sort_clamp_and_drop_duplicates() {
        let lane = ControllerLane::new(Controller::SUSTAIN, vec![ev(1.0, 1.0), ev(0.0, 0.0), ev(0.5, 2.0), ev(0.5, 1.0), ev(2.0, 0.0)]);
        assert_eq!(lane.events, vec![ev(0.0, 0.0), ev(0.5, 1.0), ev(2.0, 0.0)], "same time: last wins; repeats go");
        assert_eq!((lane.value_at(0.7), lane.value_at(-1.0)), (Some(1.0), None));
        let bend = ControllerLane::new(Controller::PitchBend, vec![ev(0.0, -3.0)]);
        assert_eq!(bend.events[0].value, -1.0);
    }

    #[test]
    fn split_carries_the_value_into_the_right_part() {
        let mut left = ControllerLane::new(Controller::PitchBend, vec![ev(0.0, 0.0), ev(1.0, 0.5), ev(3.0, -0.5)]);
        let right = left.split_off(2.0);
        assert_eq!(left.events, vec![ev(0.0, 0.0), ev(1.0, 0.5)]);
        assert_eq!(right.events, vec![ev(0.0, 0.5), ev(1.0, -0.5)]);
        // An event right at the cut needs no carried value.
        let mut l = ControllerLane::new(Controller::MOD_WHEEL, vec![ev(0.0, 0.2), ev(2.0, 0.4)]);
        assert_eq!(l.split_off(2.0).events, vec![ev(0.0, 0.4)]);
    }
}
