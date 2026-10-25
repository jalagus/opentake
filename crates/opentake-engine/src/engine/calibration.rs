//! Recording latency: the round trip the device reports, corrected by a per-setup offset
//! that a loopback measurement (or the user) sets. Offsets are kept in a small JSON file,
//! keyed by input, output, sample rate and buffer size, since each of those changes it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;

use super::Engine;
use crate::latency;
use crate::mixer::{Command, LATENCY_UNKNOWN};

/// Per-setup recording offsets (milliseconds), optionally backed by a file.
#[derive(Debug, Default)]
pub(super) struct LatencyStore {
    path: Option<PathBuf>,
    offsets: BTreeMap<String, f64>,
}

impl LatencyStore {
    fn load(path: &Path) -> Self {
        let offsets = match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                tracing::warn!("ignoring invalid latency file {}: {e}", path.display());
                BTreeMap::new()
            }),
            Err(_) => BTreeMap::new(),
        };
        Self { path: Some(path.to_path_buf()), offsets }
    }

    pub(super) fn get(&self, setup: &str) -> f64 {
        self.offsets.get(setup).copied().unwrap_or(0.0)
    }

    fn set(&mut self, setup: &str, ms: f64) -> Result<()> {
        if ms == 0.0 {
            self.offsets.remove(setup);
        } else {
            self.offsets.insert(setup.to_string(), ms);
        }
        let Some(path) = &self.path else { return Ok(()) };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(&self.offsets)?).with_context(|| format!("saving {}", path.display()))
    }
}

/// Identifies a device setup for its stored offset.
pub(super) fn setup_key(input: Option<&str>, output: &str, sample_rate: u32, buffer: Option<u32>) -> String {
    let buffer = buffer.map_or("default".to_string(), |b| b.to_string());
    format!("{} → {output} @ {sample_rate} Hz, buffer {buffer}", input.unwrap_or("no input"))
}

#[derive(Debug, Clone, Serialize)]
pub struct LatencyReport {
    /// The measured round trip, output to input (milliseconds).
    pub round_trip_ms: f64,
    /// What the device reports (milliseconds; 0 if it doesn't).
    pub reported_ms: f64,
    /// The correction now applied to recordings with this setup (milliseconds).
    pub record_offset_ms: f64,
    /// How clearly the test signal stood out (above ~2 is reliable).
    pub clarity: f64,
    pub input_peak_db: f64,
    pub setup: String,
}

impl Engine {
    /// Keep per-setup recording offsets in this file (loaded now, saved on change).
    pub fn set_latency_store(&mut self, path: &Path) {
        self.latency_store = LatencyStore::load(path);
    }

    /// Manual correction added to the measured interface round trip when placing
    /// recordings (milliseconds; positive moves recordings earlier). Remembered for the
    /// current device setup.
    pub fn set_record_offset(&mut self, ms: f64) -> Result<()> {
        if !ms.is_finite() || ms.abs() > 500.0 {
            bail!("record offset must be within ±500 ms");
        }
        let ms = (ms * 100.0).round() / 100.0;
        self.record_offset_ms = ms;
        if let Some(setup) = self.audio.as_ref().map(|a| a.setup.clone()) {
            self.latency_store.set(&setup, ms)?;
        }
        Ok(())
    }

    pub fn record_offset_ms(&self) -> f64 {
        self.record_offset_ms
    }

    /// Measure the true round trip by playing a test signal (replacing the mix for about
    /// two seconds) and finding it on `input_channel`, which must hear the output: a cable
    /// from an output to that input, or speakers near a microphone. Sets the recording
    /// offset for this setup to the difference from what the device reports.
    pub fn measure_latency(&mut self, input_channel: usize) -> Result<LatencyReport> {
        if self.shared.recording.load(Ordering::Relaxed) {
            bail!("can't measure latency while recording");
        }
        let audio = self.audio.as_ref().ok_or_else(|| anyhow!("audio is not running; call start_audio first"))?;
        if audio.input_channels == 0 {
            bail!("no input device is open");
        }
        if input_channel >= audio.input_channels {
            bail!("input channel {} doesn't exist (the input has {})", input_channel + 1, audio.input_channels);
        }
        let sr = audio.sample_rate as f64;
        let setup = audio.setup.clone();
        let len = latency::capture_len(sr);
        while audio.probe_results.try_recv().is_ok() {} // a stale result from a timed-out run
        self.send(Command::Stop)?;
        self.send(Command::Probe { channel: input_channel, buf: Box::new(Vec::with_capacity(len)) })?;
        let audio = self.audio.as_ref().expect("checked above");
        let captured = audio
            .probe_results
            .recv_timeout(Duration::from_secs_f64(len as f64 / sr + 3.0))
            .map_err(|_| anyhow!("the audio device didn't run the measurement"))?;
        let found = latency::find(&captured, sr)?;
        // Recordings use the reported round trip plus the offset (nothing reported = 0).
        let reported = match self.shared.io_latency.load(Ordering::Relaxed) {
            LATENCY_UNKNOWN => 0,
            v => v,
        };
        let ms = |samples: f64| (samples * 1000.0 / sr * 100.0).round() / 100.0;
        self.set_record_offset(ms(found.lag as f64 - reported as f64))?;
        Ok(LatencyReport {
            round_trip_ms: ms(found.lag as f64),
            reported_ms: ms(reported as f64),
            record_offset_ms: self.record_offset_ms,
            clarity: (found.clarity * 10.0).round() / 10.0,
            input_peak_db: (found.input_peak_db * 10.0).round() / 10.0,
            setup,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offsets_are_stored_per_setup() {
        let path = std::env::temp_dir().join(format!("opentake-latency-{}/latency.json", std::process::id()));
        let mut store = LatencyStore::load(&path);
        let a = setup_key(Some("Mic"), "Speakers", 48_000, Some(256));
        let b = setup_key(Some("Mic"), "Speakers", 48_000, None);
        store.set(&a, 12.5).unwrap();
        let store = LatencyStore::load(&path);
        assert_eq!((store.get(&a), store.get(&b)), (12.5, 0.0));
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }
}
