//! Disk writer for recordings. The audio thread pushes raw interleaved input into a
//! ring buffer; this thread splits it per armed track and writes mono WAV files.

use std::fs::File;
use std::io::BufWriter;
use std::path::PathBuf;
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, bounded, unbounded};
use rtrb::Consumer;

#[derive(Debug, Clone)]
pub struct RecordTarget {
    pub track_id: u64,
    pub channel: usize,
    pub path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct RecordedFile {
    pub track_id: u64,
    pub path: PathBuf,
    pub frames: u64,
}

enum Msg {
    Start { targets: Vec<RecordTarget>, sample_rate: u32, reply: Sender<Result<()>> },
    Stop { reply: Sender<Vec<RecordedFile>> },
    Shutdown,
}

struct Active {
    target: RecordTarget,
    writer: hound::WavWriter<BufWriter<File>>,
    frames: u64,
}

pub struct Recorder {
    tx: Sender<Msg>,
    handle: Option<JoinHandle<()>>,
}

impl Recorder {
    pub fn spawn(consumer: Consumer<f32>, in_channels: usize) -> Self {
        let (tx, rx) = unbounded();
        let handle = std::thread::Builder::new()
            .name("daw-recorder".into())
            .spawn(move || run(rx, consumer, in_channels))
            .expect("spawn recorder thread");
        Self { tx, handle: Some(handle) }
    }

    pub fn start(&self, targets: Vec<RecordTarget>, sample_rate: u32) -> Result<()> {
        let (reply, rx) = bounded(1);
        self.tx.send(Msg::Start { targets, sample_rate, reply })?;
        rx.recv()?
    }

    /// Flush and close all files. Call after the audio thread has stopped recording.
    pub fn stop(&self) -> Result<Vec<RecordedFile>> {
        let (reply, rx) = bounded(1);
        self.tx.send(Msg::Stop { reply })?;
        Ok(rx.recv()?)
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Shutdown);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn run(rx: Receiver<Msg>, mut consumer: Consumer<f32>, in_channels: usize) {
    let mut active: Vec<Active> = Vec::new();
    loop {
        match rx.recv_timeout(Duration::from_millis(5)) {
            Ok(Msg::Start { targets, sample_rate, reply }) => {
                drain(&mut consumer, in_channels, &mut []); // discard anything stale
                let opened = open_all(targets, sample_rate);
                let result = opened.map(|a| active = a);
                let _ = reply.send(result);
            }
            Ok(Msg::Stop { reply }) => {
                drain(&mut consumer, in_channels, &mut active);
                let files = active
                    .drain(..)
                    .filter_map(|a| {
                        let Active { target, writer, frames } = a;
                        match writer.finalize() {
                            Ok(()) => Some(RecordedFile { track_id: target.track_id, path: target.path, frames }),
                            Err(e) => {
                                tracing::error!("finalizing {}: {e}", target.path.display());
                                None
                            }
                        }
                    })
                    .collect();
                let _ = reply.send(files);
            }
            Ok(Msg::Shutdown) | Err(RecvTimeoutError::Disconnected) => return,
            Err(RecvTimeoutError::Timeout) => {}
        }
        drain(&mut consumer, in_channels, &mut active);
    }
}

fn open_all(targets: Vec<RecordTarget>, sample_rate: u32) -> Result<Vec<Active>> {
    let spec = hound::WavSpec { channels: 1, sample_rate, bits_per_sample: 32, sample_format: hound::SampleFormat::Float };
    targets
        .into_iter()
        .map(|target| {
            if let Some(dir) = target.path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let writer = hound::WavWriter::create(&target.path, spec)
                .with_context(|| format!("creating {}", target.path.display()))?;
            Ok(Active { target, writer, frames: 0 })
        })
        .collect()
}

fn drain(consumer: &mut Consumer<f32>, in_channels: usize, active: &mut [Active]) {
    if in_channels == 0 {
        return;
    }
    let available = consumer.slots();
    let take = available - available % in_channels;
    if take == 0 {
        return;
    }
    let Ok(chunk) = consumer.read_chunk(take) else { return };
    let (a, b) = chunk.as_slices();
    // A frame may straddle the ring's wrap point, so index across both slices.
    let sample = |i: usize| if i < a.len() { a[i] } else { b[i - a.len()] };
    for frame in 0..take / in_channels {
        for rec in active.iter_mut() {
            let ch = rec.target.channel.min(in_channels - 1);
            if rec.writer.write_sample(sample(frame * in_channels + ch)).is_ok() {
                rec.frames += 1;
            }
        }
    }
    chunk.commit_all();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_interleaved_input_per_target() {
        let dir = std::env::temp_dir().join(format!("opentake-rec-{}", std::process::id()));
        let (mut tx, rx) = rtrb::RingBuffer::new(4096);
        let rec = Recorder::spawn(rx, 2);
        let path = dir.join("ch1.wav");
        rec.start(vec![RecordTarget { track_id: 1, channel: 1, path: path.clone() }], 48_000).unwrap();
        for i in 0..1000 {
            tx.push(0.0).unwrap();
            tx.push(i as f32 / 1000.0).unwrap();
        }
        std::thread::sleep(Duration::from_millis(20));
        let files = rec.stop().unwrap();
        assert_eq!(files[0].frames, 1000);
        let samples: Vec<f32> = hound::WavReader::open(&path).unwrap().samples().map(Result::unwrap).collect();
        assert_eq!(samples[500], 0.5);
        std::fs::remove_dir_all(&dir).ok();
    }
}
