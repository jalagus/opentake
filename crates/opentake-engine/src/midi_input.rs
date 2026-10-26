//! MIDI input from hardware and virtual ports (midir: CoreMIDI, WinMM, ALSA).
//!
//! Each open port has its own lock-free ring: the port's callback stamps a message with the
//! time it was played and pushes it, never blocking; the audio thread drains every ring at
//! the start of each device callback (`Mixer::gather_midi`). A port that goes away simply
//! stops delivering; its ring is dropped (off the audio thread) when it's closed or rescanned.
//!
//! Timing: everything uses one monotonic clock ([`now_ns`]). midir's own timestamps count
//! from a backend-specific point (CoreMIDI host time, WinMM: since the port opened, ALSA: a
//! queue), so each connection learns the offset between its stamps and our clock from the
//! smallest arrival delay seen ([`StampClock`]); that keeps the device's timing and drops the
//! jitter of callback delivery. The audio callback reads the same clock when it starts and
//! [`BlockClock`] maps a message played during the previous callback period to the same
//! relative frame of this callback. Live notes therefore play a constant one buffer late,
//! without jitter; with no timing (tests, or a host that gives none) they land at frame 0.
//! Notes, control changes, pitch bend and channel pressure are used (omni: every channel);
//! polyphonic aftertouch, program changes and sysex are ignored.

use std::sync::OnceLock;
use std::time::Instant;

use anyhow::{Result, anyhow};
use opentake_project::Controller;
use rtrb::{Consumer, Producer};
use serde::{Deserialize, Serialize};

/// Messages a port can buffer between two audio callbacks.
pub const PORT_RING: usize = 1024;
/// Ports the audio thread reads at once.
pub const MAX_PORTS: usize = 32;

/// A short MIDI message (sysex is filtered out) and when it was played (see [`now_ns`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RawMidi {
    pub time_ns: i64,
    pub len: u8,
    pub data: [u8; 3],
}

impl RawMidi {
    pub fn new(time_ns: i64, bytes: &[u8]) -> Option<Self> {
        let mut data = [0; 3];
        (1..=3).contains(&bytes.len()).then(|| {
            data[..bytes.len()].copy_from_slice(bytes);
            Self { time_ns, len: bytes.len() as u8, data }
        })
    }

    pub fn bytes(&self) -> &[u8] {
        &self.data[..self.len as usize]
    }
}

/// Nanoseconds on the clock MIDI arrivals and audio callbacks share. Lock-free after the
/// first call (which the engine makes before audio starts).
pub fn now_ns() -> i64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_nanos() as i64
}

/// What the instruments act on (omni: every channel).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MidiMsg {
    NoteOn { pitch: u8, velocity: u8 },
    NoteOff { pitch: u8 },
    /// A controller at its raw value: 0..=127, pitch bend 0..=16383 (8192 = centre). The
    /// sustain pedal is CC64 (down from 64).
    Control(Controller, u16),
    /// All Notes Off (CC123) or All Sound Off (CC120).
    AllNotesOff,
}

/// Parse a message; `running` is the port's running status (the last channel status byte),
/// for devices that leave repeated status bytes out.
pub fn parse(bytes: &[u8], running: &mut u8) -> Option<MidiMsg> {
    let (&first, _) = bytes.split_first()?;
    let (status, data) = match first {
        0x80..=0xEF => {
            *running = first;
            (first, &bytes[1..])
        }
        0xF0..=0xF7 => {
            *running = 0; // system common messages cancel running status
            return None;
        }
        0xF8..=0xFF => return None, // real-time: clock, active sensing...
        _ if *running != 0 => (*running, bytes),
        _ => return None,
    };
    let byte = |i: usize| data.get(i).map(|b| b & 0x7F);
    match status & 0xF0 {
        0x90 => match (byte(0)?, byte(1)?) {
            (pitch, 0) => Some(MidiMsg::NoteOff { pitch }),
            (pitch, velocity) => Some(MidiMsg::NoteOn { pitch, velocity }),
        },
        0x80 => Some(MidiMsg::NoteOff { pitch: byte(0)? }),
        0xB0 => match (byte(0)?, byte(1)?) {
            (120 | 123, _) => Some(MidiMsg::AllNotesOff),
            (121.., _) => None, // other channel mode messages
            (cc, v) => Some(MidiMsg::Control(Controller::Cc(cc), v as u16)),
        },
        0xD0 => Some(MidiMsg::Control(Controller::ChannelPressure, byte(0)? as u16)),
        0xE0 => Some(MidiMsg::Control(Controller::PitchBend, byte(0)? as u16 | (byte(1)? as u16) << 7)),
        _ => None, // polyphonic aftertouch, program change
    }
}

/// Maps one connection's timestamps (µs from some backend-specific point) onto [`now_ns`].
/// The offset is the smallest `arrival - stamp` seen: the least-delayed delivery. It may
/// creep up by 1 ms per second, so drift between the two clocks is followed.
#[derive(Debug, Default)]
pub struct StampClock {
    offset_ns: Option<i64>,
    last_arrival: i64,
}

impl StampClock {
    pub fn map(&mut self, stamp_us: u64, arrival_ns: i64) -> i64 {
        let stamp = stamp_us as i64 * 1000;
        let skew = arrival_ns - stamp;
        let offset = match self.offset_ns {
            Some(o) => (o + (arrival_ns - self.last_arrival).max(0) / 1000).min(skew),
            None => skew,
        };
        self.offset_ns = Some(offset);
        self.last_arrival = arrival_ns;
        stamp + offset
    }
}

/// Maps MIDI times onto frames of the current audio callback. The callback's start time
/// is smoothed (each one is predicted from the last plus its length, then nudged towards the
/// time read), so scheduling jitter of the callback doesn't move the notes. Messages played
/// during the period before this callback land at the same relative frame in it.
#[derive(Debug)]
pub struct BlockClock {
    start_ns: Option<f64>,
    frames: usize,
    ns_per_frame: f64,
}

impl BlockClock {
    pub fn new(sample_rate: u32) -> Self {
        Self { start_ns: None, frames: 0, ns_per_frame: 1e9 / sample_rate.max(1) as f64 }
    }

    /// A callback of `frames` started at `now` (`None`: no timing, every message at frame 0).
    pub fn begin(&mut self, now: Option<i64>, frames: usize) {
        self.start_ns = now.map(|now| {
            let now = now as f64;
            match self.start_ns {
                Some(prev) => {
                    let predicted = prev + self.frames as f64 * self.ns_per_frame;
                    // A stall or a changed buffer size: start over from the time read.
                    let slack = (self.frames.max(frames) as f64 * self.ns_per_frame).max(2e6);
                    if (now - predicted).abs() > slack { now } else { predicted + (now - predicted) * 0.05 }
                }
                None => now,
            }
        });
        self.frames = frames;
    }

    /// The frame of this callback for a message played at `time_ns`.
    pub fn offset(&self, time_ns: i64) -> u32 {
        let Some(start) = self.start_ns else { return 0 };
        let window = start - self.frames as f64 * self.ns_per_frame;
        let frame = ((time_ns as f64 - window) / self.ns_per_frame).floor();
        frame.clamp(0.0, self.frames.saturating_sub(1) as f64) as u32
    }

    /// How much later than played a message plays (frames): a callback's length with timing,
    /// else half of one on average.
    pub fn delay(&self) -> usize {
        if self.start_ns.is_some() { self.frames } else { self.frames / 2 }
    }
}

/// Which ports to listen to.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MidiSelection {
    /// Every port, including ones plugged in later (on a rescan).
    #[default]
    All,
    /// These ports, by name.
    Ports(Vec<String>),
}

impl MidiSelection {
    pub fn includes(&self, name: &str) -> bool {
        match self {
            Self::All => true,
            Self::Ports(names) => names.iter().any(|n| n == name),
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct MidiPortInfo {
    pub name: String,
    /// Chosen in the selection.
    pub enabled: bool,
    /// Connected and delivering to the engine.
    pub open: bool,
}

/// The MIDI input ports there are now.
pub fn port_names() -> Result<Vec<String>> {
    let input = midir::MidiInput::new("OpenTake").map_err(|e| anyhow!("MIDI is unavailable: {e}"))?;
    Ok(input.ports().iter().filter_map(|p| input.port_name(p).ok()).collect())
}

/// Open MIDI input connections (closed when dropped).
#[derive(Default)]
pub struct Connections {
    open: Vec<(String, midir::MidiInputConnection<()>)>,
}

impl Connections {
    pub fn names(&self) -> Vec<String> {
        self.open.iter().map(|(n, _)| n.clone()).collect()
    }

    /// Match the open ports to `selection` and to what's plugged in: close ports no longer
    /// wanted or present, open new ones and hand each new ring's reading end to `attach`.
    /// Returns the ports there are, and problems opening any.
    pub fn sync(&mut self, selection: &MidiSelection, mut attach: impl FnMut(Consumer<RawMidi>) -> Result<()>) -> Result<(Vec<String>, Vec<String>)> {
        let names = port_names()?;
        self.open.retain(|(n, _)| selection.includes(n) && names.contains(n));
        let mut errors = Vec::new();
        for name in names.iter().filter(|n| selection.includes(n)) {
            if self.open.iter().any(|(n, _)| n == name) {
                continue;
            }
            match open(name) {
                Ok((conn, rx)) => {
                    attach(rx)?;
                    self.open.push((name.clone(), conn));
                }
                Err(e) => errors.push(format!("{name}: {e:#}")),
            }
        }
        Ok((names, errors))
    }
}

/// Connect to port `name`; its messages go into a new ring.
fn open(name: &str) -> Result<(midir::MidiInputConnection<()>, Consumer<RawMidi>)> {
    let mut input = midir::MidiInput::new("OpenTake").map_err(|e| anyhow!("{e}"))?;
    input.ignore(midir::Ignore::All);
    let port = input.ports().into_iter().find(|p| input.port_name(p).is_ok_and(|n| n == name)).ok_or_else(|| anyhow!("port is gone"))?;
    let (tx, rx) = rtrb::RingBuffer::new(PORT_RING);
    let conn = input.connect(&port, "OpenTake In", receiver(tx), ()).map_err(|e| anyhow!("{e}"))?;
    Ok((conn, rx))
}

/// The port callback: stamp and push, never block (a full ring drops the message).
fn receiver(mut tx: Producer<RawMidi>) -> impl FnMut(u64, &[u8], &mut ()) + Send + 'static {
    let mut clock = StampClock::default();
    move |stamp, bytes, _| {
        let time = clock.map(stamp, now_ns());
        if let Some(m) = RawMidi::new(time, bytes) {
            let _ = tx.push(m);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_notes_controllers_and_running_status() {
        let mut rs = 0;
        assert_eq!(parse(&[0x90, 60, 100], &mut rs), Some(MidiMsg::NoteOn { pitch: 60, velocity: 100 }));
        assert_eq!(parse(&[0x93, 60, 0], &mut rs), Some(MidiMsg::NoteOff { pitch: 60 }), "velocity 0 is a note-off");
        assert_eq!(parse(&[0x80, 61, 64], &mut rs), Some(MidiMsg::NoteOff { pitch: 61 }));
        assert_eq!(parse(&[0xB0, 64, 127], &mut rs), Some(MidiMsg::Control(Controller::SUSTAIN, 127)));
        assert_eq!(parse(&[0xB0, 123, 0], &mut rs), Some(MidiMsg::AllNotesOff));
        assert_eq!(parse(&[0xB0, 120, 0], &mut rs), Some(MidiMsg::AllNotesOff));
        assert_eq!(parse(&[0xB0, 121, 0], &mut rs), None, "reset all controllers is a mode message");
        assert_eq!(parse(&[0xB1, 1, 50], &mut rs), Some(MidiMsg::Control(Controller::MOD_WHEEL, 50)));
        assert_eq!(parse(&[0xE0, 0, 64], &mut rs), Some(MidiMsg::Control(Controller::PitchBend, 8192)), "centre");
        assert_eq!(parse(&[0xE0, 0x7F, 0x7F], &mut rs), Some(MidiMsg::Control(Controller::PitchBend, 16383)));
        assert_eq!(parse(&[0xD0, 90], &mut rs), Some(MidiMsg::Control(Controller::ChannelPressure, 90)));
        assert_eq!(parse(&[0xA0, 60, 90], &mut rs), None, "polyphonic aftertouch is ignored");
        // Running status: data bytes reuse the last status; real-time bytes don't cancel it.
        parse(&[0x90, 60, 100], &mut rs);
        assert_eq!(parse(&[0xF8], &mut rs), None);
        assert_eq!(parse(&[62, 90], &mut rs), Some(MidiMsg::NoteOn { pitch: 62, velocity: 90 }));
        assert_eq!(parse(&[62, 0], &mut rs), Some(MidiMsg::NoteOff { pitch: 62 }));
        assert_eq!(parse(&[0xF2, 0, 0], &mut rs), None);
        assert_eq!(parse(&[62, 90], &mut rs), None, "system common cancels running status");
        assert_eq!(parse(&[0x90, 60], &mut rs), None, "truncated");
    }

    #[test]
    fn stamps_map_onto_the_shared_clock_without_delivery_jitter() {
        let mut c = StampClock::default();
        // Stamps 1 ms apart, delivered 3 ms, 1 ms and 5 ms late (µs stamps, ns arrivals).
        assert_eq!(c.map(10_000, 13_000_000), 13_000_000);
        assert_eq!(c.map(11_000, 12_000_000), 12_000_000, "a faster delivery lowers the offset");
        assert_eq!(c.map(12_000, 18_000_000), 13_006_000, "a late delivery keeps its own time (plus the creep)");
    }

    #[test]
    fn messages_land_a_constant_buffer_late() {
        // 48 kHz, 480-frame callbacks (10 ms) starting at 100 ms, 110 ms (+0.3 ms jitter)...
        let mut b = BlockClock::new(48_000);
        assert_eq!((b.offset(5), b.delay()), (0, 0), "no timing yet");
        b.begin(Some(100_000_000), 480);
        b.begin(Some(110_300_000), 480);
        // The smoothed start barely moves with the jitter: a note played 2.5 ms into the
        // previous period lands 120 frames in, give or take a frame.
        let o = b.offset(102_500_000);
        assert!((119..=121).contains(&o), "{o}");
        assert_eq!(b.offset(50_000_000), 0, "older messages clamp to the start");
        assert_eq!(b.offset(200_000_000), 479, "and newer ones to the end");
        assert_eq!(b.delay(), 480);
        // A stall resynchronises to the time read.
        b.begin(Some(500_000_000), 480);
        assert_eq!(b.offset(495_000_000), 240);
        b.begin(None, 256);
        assert_eq!((b.offset(495_000_000), b.delay()), (0, 128));
    }

    #[test]
    fn selection_matches_names() {
        assert!(MidiSelection::All.includes("Keys"));
        let s = MidiSelection::Ports(vec!["Keys".into()]);
        assert!(s.includes("Keys") && !s.includes("Pads"));
        assert_eq!(serde_json::to_string(&MidiSelection::All).unwrap(), "\"all\"");
    }
}
