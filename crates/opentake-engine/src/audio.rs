//! Audio device discovery and stream setup (cpal).

use anyhow::{Context, Result, anyhow, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, Device, SampleFormat, Stream, StreamConfig};
use rtrb::{Consumer, Producer};
use serde::{Deserialize, Serialize};

use crate::mixer::Mixer;

#[derive(Debug, Clone, Serialize)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    pub is_default: bool,
    pub default_channels: u16,
    pub max_channels: u16,
    pub default_sample_rate: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeviceList {
    pub host: String,
    pub inputs: Vec<DeviceInfo>,
    pub outputs: Vec<DeviceInfo>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AudioConfig {
    /// Input device name (substring match) or id. `None` = system default.
    pub input_device: Option<String>,
    /// Output device name (substring match) or id. `None` = system default.
    pub output_device: Option<String>,
    pub sample_rate: Option<u32>,
    /// Device buffer size in frames. `None` = device default.
    pub buffer_size: Option<u32>,
    /// Run output only (no recording / monitoring).
    #[serde(default)]
    pub disable_input: bool,
}

pub fn list_devices() -> Result<DeviceList> {
    let host = cpal::default_host();
    let default_in = host.default_input_device().and_then(|d| d.id().ok());
    let default_out = host.default_output_device().and_then(|d| d.id().ok());

    let describe = |d: &Device, input: bool, default: &Option<cpal::DeviceId>| -> Option<DeviceInfo> {
        let id = d.id().ok()?;
        let name = d.description().map(|desc| desc.name().to_string()).unwrap_or_else(|_| d.to_string());
        let default_cfg = if input { d.default_input_config() } else { d.default_output_config() }.ok()?;
        let max_channels = if input {
            d.supported_input_configs().ok().into_iter().flatten().map(|c| c.channels()).max()
        } else {
            d.supported_output_configs().ok().into_iter().flatten().map(|c| c.channels()).max()
        };
        Some(DeviceInfo {
            is_default: default.as_ref() == Some(&id),
            id: id.to_string(),
            name,
            default_channels: default_cfg.channels(),
            max_channels: max_channels.unwrap_or(default_cfg.channels()),
            default_sample_rate: default_cfg.sample_rate(),
        })
    };

    Ok(DeviceList {
        host: host.id().name().to_string(),
        inputs: host.input_devices()?.filter_map(|d| describe(&d, true, &default_in)).collect(),
        outputs: host.output_devices()?.filter_map(|d| describe(&d, false, &default_out)).collect(),
    })
}

fn find_device(host: &cpal::Host, query: &Option<String>, input: bool) -> Result<Device> {
    let Some(q) = query else {
        let d = if input { host.default_input_device() } else { host.default_output_device() };
        return d.ok_or_else(|| anyhow!("no default {} device", if input { "input" } else { "output" }));
    };
    let devices: Vec<Device> = if input { host.input_devices()?.collect() } else { host.output_devices()?.collect() };
    let ql = q.to_lowercase();
    devices
        .iter()
        .find(|d| d.id().is_ok_and(|id| id.to_string() == *q))
        .or_else(|| {
            devices.iter().find(|d| d.description().is_ok_and(|desc| desc.name().to_lowercase().contains(&ql)))
        })
        .cloned()
        .ok_or_else(|| anyhow!("device not found: {q}"))
}

/// Running device streams. Dropping this stops audio.
pub struct AudioStreams {
    _input: Option<Stream>,
    _output: Stream,
    pub sample_rate: u32,
    pub input_channels: usize,
    pub output_channels: usize,
    pub input_name: Option<String>,
    pub output_name: String,
}

pub struct ResolvedConfig {
    input: Option<(Device, StreamConfig)>,
    output: (Device, StreamConfig),
}

impl ResolvedConfig {
    pub fn sample_rate(&self) -> u32 {
        self.output.1.sample_rate
    }

    pub fn input_channels(&self) -> usize {
        self.input.as_ref().map_or(0, |(_, c)| c.channels as usize)
    }
}

/// Pick devices and stream configs. Input uses all channels the device offers at the
/// output's sample rate, so any interface input can be recorded.
pub fn resolve(config: &AudioConfig) -> Result<ResolvedConfig> {
    let host = cpal::default_host();
    let out_dev = find_device(&host, &config.output_device, false)?;
    let out_default = out_dev.default_output_config().context("output device has no default config")?;
    if out_default.sample_format() != SampleFormat::F32 {
        bail!("output device sample format {} is not supported yet (need f32)", out_default.sample_format());
    }
    let sample_rate = config.sample_rate.unwrap_or(out_default.sample_rate());
    let buffer_size = config.buffer_size.map_or(BufferSize::Default, BufferSize::Fixed);
    let out_cfg = StreamConfig { channels: out_default.channels(), sample_rate, buffer_size };

    let input = if config.disable_input {
        None
    } else {
        match find_device(&host, &config.input_device, true) {
            Ok(dev) => {
                let best = dev
                    .supported_input_configs()?
                    .filter(|c| c.sample_format() == SampleFormat::F32 && c.contains_rate(sample_rate))
                    .max_by_key(|c| c.channels());
                match best {
                    Some(c) => Some((dev, StreamConfig { channels: c.channels(), sample_rate, buffer_size })),
                    None => {
                        tracing::warn!("input device has no f32 config at {sample_rate} Hz; running without input");
                        None
                    }
                }
            }
            Err(e) if config.input_device.is_none() => {
                tracing::warn!("{e}; running without input");
                None
            }
            Err(e) => return Err(e),
        }
    };

    Ok(ResolvedConfig { input, output: (out_dev, out_cfg) })
}

/// Start streams. Input samples flow through a ring buffer into the output callback,
/// which drives the mixer.
pub fn start(resolved: ResolvedConfig, mut mixer: Mixer) -> Result<AudioStreams> {
    let ResolvedConfig { input, output: (out_dev, out_cfg) } = resolved;
    let sample_rate = out_cfg.sample_rate;
    let in_channels = input.as_ref().map_or(0, |(_, c)| c.channels as usize);
    let out_channels = out_cfg.channels as usize;

    // ~0.5 s of interleaved input between the two callbacks.
    let ring_len = (in_channels * sample_rate as usize / 2).max(1);
    let (mut in_tx, mut in_rx): (Producer<f32>, Consumer<f32>) = rtrb::RingBuffer::new(ring_len);

    let input_stream = match &input {
        Some((dev, cfg)) => {
            let stream = dev.build_input_stream(
                *cfg,
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    if let Ok(chunk) = in_tx.write_chunk_uninit(data.len().min(in_tx.slots())) {
                        chunk.fill_from_iter(data.iter().copied());
                    }
                },
                |e| tracing::warn!("input stream: {e}"),
                None,
            )?;
            Some(stream)
        }
        None => None,
    };

    let mut in_buf = vec![0.0f32; 16_384 * in_channels.max(1)];
    let output_stream = out_dev.build_output_stream(
        out_cfg,
        move |out: &mut [f32], _: &cpal::OutputCallbackInfo| {
            let frames = out.len() / out_channels;
            let wanted = (frames * in_channels).min(in_buf.len());
            let mut got = 0;
            if in_channels > 0 {
                // Keep latency bounded: if input has run far ahead, drop the excess.
                let excess = in_rx.slots().saturating_sub(wanted * 2);
                let excess = excess - excess % in_channels;
                if excess > 0 {
                    if let Ok(c) = in_rx.read_chunk(excess) {
                        c.commit_all();
                    }
                }
                let avail = in_rx.slots().min(wanted);
                let avail = avail - avail % in_channels;
                if let Ok(c) = in_rx.read_chunk(avail) {
                    let (a, b) = c.as_slices();
                    in_buf[..a.len()].copy_from_slice(a);
                    in_buf[a.len()..a.len() + b.len()].copy_from_slice(b);
                    got = a.len() + b.len();
                    c.commit_all();
                }
            }
            mixer.process(&in_buf[..got], in_channels, out, out_channels);
        },
        |e| tracing::warn!("output stream: {e}"),
        None,
    )?;

    if let Some(s) = &input_stream {
        s.play()?;
    }
    output_stream.play()?;

    let name = |d: &Device| d.description().map(|x| x.name().to_string()).unwrap_or_else(|_| d.to_string());
    Ok(AudioStreams {
        input_name: input.as_ref().map(|(d, _)| name(d)),
        output_name: name(&out_dev),
        _input: input_stream,
        _output: output_stream,
        sample_rate,
        input_channels: in_channels,
        output_channels: out_channels,
    })
}
