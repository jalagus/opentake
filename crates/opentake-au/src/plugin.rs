//! An audio unit instance, split into a control-side handle and an audio-side processor.
//!
//! ```text
//!   AuPlugin (control thread)        AuProcessor (audio thread)
//!        \                                /
//!         +------ Arc<Instance> ---------+   AudioUnit, render context, parameters
//! ```
//!
//! An audio unit is one object: parameters and state are read and written on it directly,
//! which the AU API allows from any thread while it renders. All unsafe FFI stays in this
//! file.

use std::cell::UnsafeCell;
use std::ffi::c_void;
use std::mem::MaybeUninit;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use objc2_audio_toolbox::*;
use objc2_core_audio_types::*;
use objc2_core_foundation::{CFArray, CFData, CFPropertyList, CFPropertyListCreateData, CFPropertyListCreateWithData, CFPropertyListFormat, CFRetained, CFString};
use serde::Serialize;

use crate::{ComponentInfo, Error, Result, find_component};

/// How long to wait for an AUv3 extension that only loads asynchronously.
const ASYNC_LOAD_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Serialize)]
pub struct ParamInfo {
    pub id: u32,
    pub title: String,
    pub units: String,
    pub min: f32,
    pub max: f32,
    pub default_normalized: f64,
    /// 0 = continuous, otherwise number of discrete steps.
    pub step_count: i32,
    pub read_only: bool,
    /// Normalized values map logarithmically (frequencies and the like).
    #[serde(skip)]
    log: bool,
    #[serde(skip)]
    unit: AudioUnitParameterUnit,
    /// Names of the values of an indexed parameter.
    #[serde(skip)]
    value_strings: Vec<String>,
    #[serde(skip)]
    has_strings: bool,
}

impl ParamInfo {
    /// The plugin's value (min..max) as 0..1.
    pub fn normalize(&self, value: f32) -> f64 {
        let (min, max, v) = (self.min as f64, self.max as f64, value as f64);
        if max <= min {
            return 0.0;
        }
        let n = if self.log { (v / min).ln() / (max / min).ln() } else { (v - min) / (max - min) };
        n.clamp(0.0, 1.0)
    }

    pub fn denormalize(&self, normalized: f64) -> f32 {
        let (min, max, n) = (self.min as f64, self.max as f64, normalized.clamp(0.0, 1.0));
        let v = if self.log { min * (max / min).powf(n) } else { min + (max - min) * n };
        (if self.step_count > 0 { v.round() } else { v }) as f32
    }

    /// The value without units (shown next to it, as VST3 display strings are).
    fn format(&self, v: f32) -> String {
        if let Some(s) = self.value_strings.get((v - self.min).round().max(0.0) as usize) {
            return s.clone();
        }
        match self.unit {
            AudioUnitParameterUnit::Boolean => return (if v >= 0.5 { "On" } else { "Off" }).into(),
            AudioUnitParameterUnit::Hertz if v >= 1000.0 => return format!("{:.2}k", v / 1000.0),
            _ => {}
        }
        let digits = match v.abs() {
            _ if self.step_count > 0 => 0,
            a if a >= 100.0 => 0,
            a if a >= 10.0 => 1,
            a if a >= 1.0 || a == 0.0 => 2,
            _ => 3,
        };
        format!("{v:.digits$}")
    }
}

/// A note for instruments. `velocity` 0 = note-off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoteEvent {
    /// Sample offset within the block.
    pub offset: u32,
    pub pitch: u8,
    pub velocity: u8,
}

/// Musical/transport information, answered through the host callbacks.
#[derive(Debug, Clone, Copy, Default)]
pub struct Transport {
    pub playing: bool,
    pub recording: bool,
    pub tempo_bpm: f64,
    pub time_sig: (i32, i32),
    pub position_samples: i64,
}

/// What the render and host callbacks read. They only run inside `AudioUnitRender`, on the
/// thread that called it, so the processor writes it just before rendering.
struct RenderContext {
    /// Effect input channels (the processor's own buffers), `input_frames` long, and the
    /// side chain's (input element 1).
    input: [*mut f32; 2],
    key: [*mut f32; 2],
    input_frames: usize,
    transport: Transport,
    sample_rate: f64,
    transport_changed: bool,
}

impl RenderContext {
    fn beat(&self) -> f64 {
        self.transport.position_samples as f64 / self.sample_rate * self.transport.tempo_bpm / 60.0
    }

    fn beats_per_bar(&self) -> f64 {
        let (num, den) = self.transport.time_sig;
        (num.max(1) as f64 * 4.0 / den.max(1) as f64).max(0.25)
    }
}

#[derive(Clone, Copy, PartialEq)]
struct Setup {
    sample_rate: f64,
    max_block: usize,
    channels: usize,
}

struct Instance {
    unit: AudioUnit,
    /// Has an audio input (effects and music effects).
    effect: bool,
    /// Takes MIDI (instruments and music effects).
    midi: bool,
    /// An effect with a second input element, fed the side chain.
    side_chain: bool,
    /// Boxed so the address handed to the callbacks stays put.
    ctx: Box<UnsafeCell<RenderContext>>,
    /// The format the unit is initialized with, if it is.
    setup: Mutex<Option<Setup>>,
    params: Vec<ParamInfo>,
}

// The AU API is designed for a control thread (properties, parameters) and a render
// thread working on one instance; the render context is only touched on the render
// thread (see `RenderContext`).
unsafe impl Send for Instance {}
unsafe impl Sync for Instance {}

impl Drop for Instance {
    fn drop(&mut self) {
        unsafe {
            if self.setup.get_mut().is_ok_and(|s| s.is_some()) {
                AudioUnitUninitialize(self.unit);
            }
            AudioComponentInstanceDispose(self.unit);
        }
    }
}

impl Instance {
    /// Initialize the unit for a sample rate and block size, keeping the current setup if
    /// it matches. Returns the channel count: stereo, or mono for mono-only units.
    fn configure(&self, sample_rate: f64, max_block: usize) -> Result<usize> {
        let mut setup = self.setup.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(s) = *setup {
            if s.sample_rate == sample_rate && s.max_block == max_block {
                return Ok(s.channels);
            }
            unsafe { AudioUnitUninitialize(self.unit) };
            *setup = None;
        }
        unsafe {
            set_prop(self.unit, kAudioUnitProperty_MaximumFramesPerSlice, kAudioUnitScope_Global, 0, &(max_block as u32), "MaximumFramesPerSlice")?;
            let channels: usize = [2, 1]
                .into_iter()
                .find(|&ch| {
                    let format = stream_format(sample_rate, ch as u32);
                    let set = |scope| set_prop(self.unit, kAudioUnitProperty_StreamFormat, scope, 0, &format, "StreamFormat").is_ok();
                    set(kAudioUnitScope_Output) && (!self.effect || set(kAudioUnitScope_Input))
                })
                .ok_or(Error::Other("the audio unit supports neither stereo nor mono 32-bit float"))?;
            if self.side_chain {
                // The key in the same format; a unit that wants another keeps its own.
                let _ = set_prop(self.unit, kAudioUnitProperty_StreamFormat, kAudioUnitScope_Input, 1, &stream_format(sample_rate, channels as u32), "StreamFormat");
            }
            check(AudioUnitInitialize(self.unit), "AudioUnitInitialize")?;
            (*self.ctx.get()).sample_rate = sample_rate;
            *setup = Some(Setup { sample_rate, max_block, channels });
            Ok(channels)
        }
    }

    fn param(&self, id: u32) -> Option<&ParamInfo> {
        self.params.iter().find(|p| p.id == id)
    }

    /// Set a parameter from a normalized value; allowed while rendering.
    fn set_param(&self, id: u32, normalized: f64) {
        if let Some(p) = self.param(id) {
            unsafe { AudioUnitSetParameter(self.unit, id, kAudioUnitScope_Global, 0, p.denormalize(normalized), 0) };
        }
    }
}

/// Control-side handle: parameters, state, creating the processor.
pub struct AuPlugin {
    inst: Arc<Instance>,
    pub info: ComponentInfo,
}

impl AuPlugin {
    /// Load an audio unit and initialize it for `sample_rate` (re-initialized by
    /// [`create_processor`](Self::create_processor) if that differs).
    pub fn new(info: &ComponentInfo, sample_rate: f64, max_block: usize) -> Result<Self> {
        let comp = find_component(&info.id).ok_or_else(|| Error::NotFound(info.display_name()))?;
        let unit = unsafe { instantiate(comp)? };
        let ty = crate::parse_id(&info.id).map_or(0, |c| c[0]);
        let ctx = RenderContext { input: [std::ptr::null_mut(); 2], key: [std::ptr::null_mut(); 2], input_frames: 0, transport: Transport::default(), sample_rate, transport_changed: true };
        let mut inst = Instance {
            unit,
            effect: ty != kAudioUnitType_MusicDevice,
            midi: ty != kAudioUnitType_Effect,
            side_chain: false,
            ctx: Box::new(UnsafeCell::new(ctx)),
            setup: Mutex::new(None),
            params: Vec::new(),
        };
        unsafe {
            let user = inst.ctx.get() as *mut c_void;
            let callbacks = HostCallbackInfo {
                hostUserData: user,
                beatAndTempoProc: Some(beat_and_tempo),
                musicalTimeLocationProc: Some(musical_time_location),
                transportStateProc: Some(transport_state),
                transportStateProc2: Some(transport_state2),
            };
            // Optional for the unit; some don't take host callbacks.
            let _ = set_prop(unit, kAudioUnitProperty_HostCallbacks, kAudioUnitScope_Global, 0, &callbacks, "HostCallbacks");
            if inst.effect {
                let input = AURenderCallbackStruct { inputProc: Some(render_input), inputProcRefCon: user };
                set_prop(unit, kAudioUnitProperty_SetRenderCallback, kAudioUnitScope_Input, 0, &input, "SetRenderCallback")?;
                // A second input element is the side chain (e.g. Logic-style keyed dynamics).
                let elements: u32 = get_prop(unit, kAudioUnitProperty_ElementCount, kAudioUnitScope_Input, 0, "ElementCount").unwrap_or(1);
                inst.side_chain = elements >= 2 && set_prop(unit, kAudioUnitProperty_SetRenderCallback, kAudioUnitScope_Input, 1, &input, "SetRenderCallback").is_ok();
            }
        }
        inst.configure(sample_rate, max_block)?;
        // Some units only publish parameters once initialized.
        inst.params = unsafe { read_params(unit) };
        Ok(Self { inst: Arc::new(inst), info: info.clone() })
    }

    pub fn name(&self) -> &str {
        &self.info.name
    }

    pub fn is_instrument(&self) -> bool {
        self.info.instrument
    }

    pub fn parameters(&self) -> &[ParamInfo] {
        &self.inst.params
    }

    /// Current value, normalized 0..1.
    pub fn get_param(&self, id: u32) -> Option<f64> {
        let p = self.inst.param(id)?;
        let mut value = 0.0f32;
        let status = unsafe { AudioUnitGetParameter(self.inst.unit, id, kAudioUnitScope_Global, 0, NonNull::from(&mut value)) };
        (status == 0).then(|| p.normalize(value))
    }

    pub fn set_param(&self, id: u32, normalized: f64) {
        self.inst.set_param(id, normalized);
    }

    /// Human readable value without units, e.g. "0.250" (units are in [`ParamInfo`]).
    pub fn param_display(&self, id: u32, normalized: f64) -> Option<String> {
        let p = self.inst.param(id)?;
        let mut value = p.denormalize(normalized);
        if p.has_strings && p.value_strings.is_empty() {
            let mut query = AudioUnitParameterStringFromValue { inParamID: id, inValue: NonNull::from(&mut value), outString: std::ptr::null() };
            let got = unsafe { get_prop_into(self.inst.unit, kAudioUnitProperty_ParameterStringFromValue, kAudioUnitScope_Global, 0, &mut query) };
            if got.is_ok()
                && let Some(s) = NonNull::new(query.outString as *mut CFString)
            {
                // SAFETY: the unit hands the caller a +1 string.
                return Some(unsafe { CFRetained::from_raw(s) }.to_string());
            }
        }
        Some(p.format(value))
    }

    /// The unit's full state (its ClassInfo property list, binary plist bytes).
    pub fn get_state(&self) -> Result<Vec<u8>> {
        unsafe {
            let plist: *const CFPropertyList = get_prop(self.inst.unit, kAudioUnitProperty_ClassInfo, kAudioUnitScope_Global, 0, "ClassInfo")?;
            // SAFETY: getting ClassInfo returns a +1 reference.
            let plist = CFRetained::from_raw(NonNull::new(plist as *mut CFPropertyList).ok_or(Error::Other("the audio unit returned no state"))?);
            let data = CFPropertyListCreateData(None, Some(&plist), CFPropertyListFormat::BinaryFormat_v1_0, 0, std::ptr::null_mut())
                .ok_or(Error::Other("could not serialize the audio unit state"))?;
            Ok(data.to_vec())
        }
    }

    /// Restore [`get_state`](Self::get_state) bytes. A property list behind a host-specific
    /// header (as other hosts store it) is found and used too.
    pub fn set_state(&self, state: &[u8]) -> Result<()> {
        let find = |magic: &[u8]| state.windows(magic.len()).position(|w| w == magic);
        let start = find(b"bplist").or_else(|| find(b"<?xml")).unwrap_or(0);
        let data = CFData::from_bytes(&state[start..]);
        unsafe {
            let plist = CFPropertyListCreateWithData(None, Some(&data), 0, std::ptr::null_mut(), std::ptr::null_mut())
                .ok_or(Error::Other("the saved state is not a property list"))?;
            let ptr: *const CFPropertyList = &*plist;
            set_prop(self.inst.unit, kAudioUnitProperty_ClassInfo, kAudioUnitScope_Global, 0, &ptr, "ClassInfo")
        }
    }

    /// Whether the unit takes a side chain (an effect with a second input element), which
    /// [`AuProcessor::process_keyed`] feeds.
    pub fn has_side_chain(&self) -> bool {
        self.inst.side_chain
    }

    /// Prepare the unit for processing and return the audio-thread half.
    pub fn create_processor(&self, sample_rate: f64, max_block: usize) -> Result<AuProcessor> {
        let channels = self.inst.configure(sample_rate, max_block)?;
        let unit = self.inst.unit;
        // A new processor starts clean: no tails or voices from an earlier one.
        unsafe { AudioUnitReset(unit, kAudioUnitScope_Global, 0) };
        let latency: f64 = unsafe { get_prop(unit, kAudioUnitProperty_Latency, kAudioUnitScope_Global, 0, "Latency") }.unwrap_or(0.0);
        let empty = AudioBuffer { mNumberChannels: 1, mDataByteSize: 0, mData: std::ptr::null_mut() };
        Ok(AuProcessor {
            inst: self.inst.clone(),
            channels,
            max_block,
            input: (0..2).map(|_| vec![0.0; max_block]).collect(),
            key: (0..2).map(|_| vec![0.0; max_block]).collect(),
            output: (0..channels).map(|_| vec![0.0; max_block]).collect(),
            list: BufferList { count: 0, buffers: [empty; 2] },
            sample_time: 0.0,
            latency: (latency.max(0.0) * sample_rate).round() as u32,
            last: None,
        })
    }
}

/// `AudioBufferList` with room for two buffers (the C struct is variable-length).
#[repr(C)]
struct BufferList {
    count: u32,
    buffers: [AudioBuffer; 2],
}

/// Audio-thread half. `process` does not allocate or lock.
pub struct AuProcessor {
    inst: Arc<Instance>,
    channels: usize,
    max_block: usize,
    input: Vec<Vec<f32>>,
    /// The side chain (silence when not keyed).
    key: Vec<Vec<f32>>,
    output: Vec<Vec<f32>>,
    list: BufferList,
    sample_time: f64,
    latency: u32,
    /// Transport state after the last block (playing, expected next position).
    last: Option<(bool, i64)>,
}

// Moved to the audio thread after creation; the buffer pointers point into its own Vecs.
unsafe impl Send for AuProcessor {}

impl AuProcessor {
    pub fn latency_samples(&self) -> u32 {
        self.latency
    }

    pub fn set_param(&mut self, id: u32, normalized: f64) {
        self.inst.set_param(id, normalized);
    }

    /// Process `frames` samples (up to the max block). `inputs` feed an effect's input;
    /// the unit's output is written to every channel of `outputs`.
    pub fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]], frames: usize, transport: &Transport, notes: &[NoteEvent]) {
        self.process_keyed(inputs, None, outputs, frames, transport, notes);
    }

    /// [`process`](Self::process) with a side chain for the unit's second input element (if
    /// it has one; silence there when `key` is `None`).
    pub fn process_keyed(&mut self, inputs: &[&[f32]], key: Option<&[&[f32]]>, outputs: &mut [&mut [f32]], frames: usize, transport: &Transport, notes: &[NoteEvent]) {
        let frames = frames.min(self.max_block);
        if frames == 0 {
            return;
        }
        let unit = self.inst.unit;
        let copy = |bufs: &mut Vec<Vec<f32>>, from: &[&[f32]]| {
            for (ch, buf) in bufs.iter_mut().enumerate() {
                match from.get(ch).or(from.first()) {
                    Some(src) => buf[..frames].copy_from_slice(&src[..frames]),
                    None => buf[..frames].fill(0.0),
                }
            }
        };
        if self.inst.effect {
            copy(&mut self.input, inputs);
        }
        if self.inst.side_chain {
            copy(&mut self.key, key.unwrap_or(&[]));
        }
        let changed = self.last.is_none_or(|(playing, next)| playing != transport.playing || (playing && next != transport.position_samples));
        self.last = Some((transport.playing, transport.position_samples + frames as i64));
        // SAFETY: the callbacks read the context only during AudioUnitRender below.
        unsafe {
            let ctx = &mut *self.inst.ctx.get();
            ctx.input = [self.input[0].as_mut_ptr(), self.input[1].as_mut_ptr()];
            ctx.key = [self.key[0].as_mut_ptr(), self.key[1].as_mut_ptr()];
            ctx.input_frames = frames;
            ctx.transport = *transport;
            ctx.transport_changed = changed;
        }
        if self.inst.midi {
            for n in notes {
                let status = if n.velocity > 0 { 0x90 } else { 0x80 };
                let offset = n.offset.min(frames as u32 - 1);
                unsafe { MusicDeviceMIDIEvent(unit, status, n.pitch as u32, n.velocity as u32, offset) };
            }
        }
        self.list.count = self.channels as u32;
        for (b, out) in self.list.buffers.iter_mut().zip(&mut self.output) {
            *b = AudioBuffer { mNumberChannels: 1, mDataByteSize: (frames * 4) as u32, mData: out.as_mut_ptr().cast() };
        }
        let mut flags = AudioUnitRenderActionFlags(0);
        let mut stamp: AudioTimeStamp = unsafe { std::mem::zeroed() };
        stamp.mSampleTime = self.sample_time;
        stamp.mFlags = AudioTimeStampFlags::SampleTimeValid;
        let status = unsafe {
            AudioUnitRender(unit, &mut flags, NonNull::from(&stamp), 0, frames as u32, NonNull::from(&mut self.list).cast())
        };
        self.sample_time += frames as f64;
        for (ch, out) in outputs.iter_mut().enumerate() {
            // The unit may have pointed mData at its own buffer.
            let data = self.list.buffers[ch.min(self.channels - 1)].mData as *const f32;
            if status != 0 || data.is_null() {
                out[..frames].fill(0.0);
            } else {
                out[..frames].copy_from_slice(unsafe { std::slice::from_raw_parts(data, frames) });
            }
        }
    }
}

// ---------------------------------------------------------------- callbacks

/// Supplies an effect's input (element 0) and side chain (element 1) from the processor's
/// buffers.
unsafe extern "C-unwind" fn render_input(
    ref_con: NonNull<c_void>,
    _flags: NonNull<AudioUnitRenderActionFlags>,
    _stamp: NonNull<AudioTimeStamp>,
    bus: u32,
    frames: u32,
    io: *mut AudioBufferList,
) -> i32 {
    unsafe {
        let ctx = &*(ref_con.as_ptr() as *const RenderContext);
        if io.is_null() {
            return 0;
        }
        let n = (*io).mNumberBuffers as usize;
        let buffers = std::ptr::addr_of_mut!((*io).mBuffers) as *mut AudioBuffer;
        let frames = (frames as usize).min(ctx.input_frames);
        for i in 0..n {
            let b = &mut *buffers.add(i);
            let src = if bus == 1 { ctx.key[i.min(1)] } else { ctx.input[i.min(1)] };
            if src.is_null() {
                continue;
            }
            if b.mData.is_null() {
                b.mData = src.cast();
            } else {
                std::ptr::copy_nonoverlapping(src, b.mData as *mut f32, frames);
            }
            b.mDataByteSize = (frames * 4) as u32;
        }
    }
    0
}

/// # Safety
/// Called by the unit during render with the context pointer given in `HostCallbackInfo`.
unsafe fn context<'a>(user: *mut c_void) -> &'a RenderContext {
    unsafe { &*(user as *const RenderContext) }
}

unsafe fn put<T>(ptr: *mut T, value: T) {
    if !ptr.is_null() {
        unsafe { ptr.write(value) };
    }
}

unsafe extern "C-unwind" fn beat_and_tempo(user: *mut c_void, beat: *mut f64, tempo: *mut f64) -> i32 {
    unsafe {
        let ctx = context(user);
        put(beat, ctx.beat());
        put(tempo, ctx.transport.tempo_bpm);
    }
    0
}

unsafe extern "C-unwind" fn musical_time_location(user: *mut c_void, to_next_beat: *mut u32, num: *mut f32, den: *mut u32, downbeat: *mut f64) -> i32 {
    unsafe {
        let ctx = context(user);
        let beat = ctx.beat();
        let samples_per_beat = ctx.sample_rate * 60.0 / ctx.transport.tempo_bpm.max(1.0);
        let frac = beat - beat.floor();
        put(to_next_beat, if frac > 0.0 { ((1.0 - frac) * samples_per_beat) as u32 } else { 0 });
        put(num, ctx.transport.time_sig.0 as f32);
        put(den, ctx.transport.time_sig.1 as u32);
        let bar = ctx.beats_per_bar();
        put(downbeat, (beat / bar).floor() * bar);
    }
    0
}

unsafe extern "C-unwind" fn transport_state(user: *mut c_void, playing: *mut u8, changed: *mut u8, sample: *mut f64, cycling: *mut u8, start: *mut f64, end: *mut f64) -> i32 {
    unsafe { transport_state2(user, playing, std::ptr::null_mut(), changed, sample, cycling, start, end) }
}

#[allow(clippy::too_many_arguments)]
unsafe extern "C-unwind" fn transport_state2(
    user: *mut c_void,
    playing: *mut u8,
    recording: *mut u8,
    changed: *mut u8,
    sample: *mut f64,
    cycling: *mut u8,
    start: *mut f64,
    end: *mut f64,
) -> i32 {
    unsafe {
        let ctx = context(user);
        put(playing, ctx.transport.playing as u8);
        put(recording, ctx.transport.recording as u8);
        put(changed, ctx.transport_changed as u8);
        put(sample, ctx.transport.position_samples as f64);
        put(cycling, 0);
        put(start, 0.0);
        put(end, 0.0);
    }
    0
}

// ---------------------------------------------------------------- helpers

/// Create an instance. AUv3 extensions that require it are loaded asynchronously, waiting
/// here for the result (never call this on a thread the completion needs, e.g. while
/// blocking a main thread the extension calls back on — the wait times out then).
unsafe fn instantiate(comp: AudioComponent) -> Result<AudioUnit> {
    let mut desc: AudioComponentDescription = unsafe { std::mem::zeroed() };
    unsafe { AudioComponentGetDescription(comp, NonNull::from(&mut desc)) };
    if desc.componentFlags & AudioComponentFlags::RequiresAsyncInstantiation.bits() == 0 {
        let mut unit: AudioUnit = std::ptr::null_mut();
        check(unsafe { AudioComponentInstanceNew(comp, NonNull::from(&mut unit)) }, "AudioComponentInstanceNew")?;
        return if unit.is_null() { Err(Error::Other("the audio unit could not be created")) } else { Ok(unit) };
    }
    let (tx, rx) = std::sync::mpsc::sync_channel::<(usize, i32)>(1);
    let done = block2::RcBlock::new(move |unit: AudioComponentInstance, status: i32| {
        let _ = tx.try_send((unit as usize, status));
    });
    unsafe { AudioComponentInstantiate(comp, AudioComponentInstantiationOptions::empty(), &done) };
    let (unit, status) = rx.recv_timeout(ASYNC_LOAD_TIMEOUT).map_err(|_| Error::Other("timed out loading the audio unit"))?;
    check(status, "AudioComponentInstantiate")?;
    if unit == 0 { Err(Error::Other("the audio unit could not be created")) } else { Ok(unit as AudioUnit) }
}

fn stream_format(sample_rate: f64, channels: u32) -> AudioStreamBasicDescription {
    AudioStreamBasicDescription {
        mSampleRate: sample_rate,
        mFormatID: kAudioFormatLinearPCM,
        mFormatFlags: kAudioFormatFlagsNativeFloatPacked | kAudioFormatFlagIsNonInterleaved,
        mBytesPerPacket: 4,
        mFramesPerPacket: 1,
        mBytesPerFrame: 4,
        mChannelsPerFrame: channels,
        mBitsPerChannel: 32,
        mReserved: 0,
    }
}

fn check(status: i32, what: &'static str) -> Result<()> {
    if status == 0 { Ok(()) } else { Err(Error::Call(what, status)) }
}

/// # Safety
/// `T` must be the property's C type.
unsafe fn get_prop<T: Copy>(unit: AudioUnit, id: u32, scope: u32, element: u32, what: &'static str) -> Result<T> {
    let mut value = MaybeUninit::<T>::zeroed();
    unsafe {
        get_prop_into(unit, id, scope, element, &mut value).map_err(|s| Error::Call(what, s))?;
        Ok(value.assume_init())
    }
}

/// Get a property into `value` (which may carry input fields, as some queries do).
unsafe fn get_prop_into<T>(unit: AudioUnit, id: u32, scope: u32, element: u32, value: &mut T) -> std::result::Result<(), i32> {
    let mut size = size_of::<T>() as u32;
    let status = unsafe { AudioUnitGetProperty(unit, id, scope, element, NonNull::from(value).cast(), NonNull::from(&mut size)) };
    if status == 0 { Ok(()) } else { Err(status) }
}

/// # Safety
/// `T` must be the property's C type.
unsafe fn set_prop<T>(unit: AudioUnit, id: u32, scope: u32, element: u32, value: &T, what: &'static str) -> Result<()> {
    check(unsafe { AudioUnitSetProperty(unit, id, scope, element, (value as *const T).cast(), size_of::<T>() as u32) }, what)
}

/// Take a CF string the unit handed out, releasing it if it was handed over owned.
unsafe fn cf_string(s: *const CFString, owned: bool) -> Option<String> {
    let s = NonNull::new(s as *mut CFString)?;
    Some(if owned { unsafe { CFRetained::from_raw(s) }.to_string() } else { unsafe { s.as_ref() }.to_string() })
}

unsafe fn read_params(unit: AudioUnit) -> Vec<ParamInfo> {
    let mut size = 0u32;
    let status = unsafe { AudioUnitGetPropertyInfo(unit, kAudioUnitProperty_ParameterList, kAudioUnitScope_Global, 0, &mut size, std::ptr::null_mut()) };
    if status != 0 || size == 0 {
        return Vec::new();
    }
    let mut ids = vec![0u32; size as usize / 4];
    let status = unsafe {
        AudioUnitGetProperty(unit, kAudioUnitProperty_ParameterList, kAudioUnitScope_Global, 0, NonNull::from(&mut ids[..]).cast(), NonNull::from(&mut size))
    };
    if status != 0 {
        return Vec::new();
    }
    ids.truncate(size as usize / 4);
    ids.into_iter().filter_map(|id| unsafe { read_param(unit, id) }).collect()
}

unsafe fn read_param(unit: AudioUnit, id: u32) -> Option<ParamInfo> {
    type Flag = AudioUnitParameterOptions;
    let info: AudioUnitParameterInfo = unsafe { get_prop(unit, kAudioUnitProperty_ParameterInfo, kAudioUnitScope_Global, id, "ParameterInfo") }.ok()?;
    let flags = info.flags;
    let release = flags.contains(Flag::Flag_CFNameRelease);
    let c_name: Vec<u8> = info.name.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
    let cf_name = if flags.contains(Flag::Flag_HasCFNameString) { unsafe { cf_string(info.cfNameString, release) } } else { None };
    let title = cf_name.unwrap_or_else(|| String::from_utf8_lossy(&c_name).into_owned());
    let unit_kind = info.unit;
    use AudioUnitParameterUnit as U;
    let units = match unit_kind {
        U::CustomUnit => unsafe { cf_string(info.unitName, release) }.unwrap_or_default(),
        U::Percent | U::EqualPowerCrossfade => "%".into(),
        U::Seconds => "s".into(),
        U::Milliseconds => "ms".into(),
        U::SampleFrames => "samples".into(),
        U::Hertz => "Hz".into(),
        U::Decibels => "dB".into(),
        U::Cents | U::AbsoluteCents => "cents".into(),
        U::RelativeSemiTones => "semitones".into(),
        U::Phase | U::Degrees => "°".into(),
        U::Octaves => "octaves".into(),
        U::BPM => "BPM".into(),
        U::Beats => "beats".into(),
        U::Ratio => ":1".into(),
        _ => String::new(),
    };
    let discrete = matches!(unit_kind, U::Indexed | U::Boolean);
    let value_strings = if unit_kind == U::Indexed {
        unsafe { get_prop::<*const CFArray<CFString>>(unit, kAudioUnitProperty_ParameterValueStrings, kAudioUnitScope_Global, id, "ParameterValueStrings") }
            .ok()
            .and_then(|a| NonNull::new(a as *mut CFArray<CFString>))
            // SAFETY: the value strings come as a +1 array.
            .map(|a| unsafe { CFRetained::from_raw(a) }.iter().map(|s| s.to_string()).collect())
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let mut p = ParamInfo {
        id,
        title,
        units,
        min: info.minValue,
        max: info.maxValue,
        default_normalized: 0.0,
        step_count: if discrete { (info.maxValue - info.minValue).round() as i32 } else { 0 },
        read_only: !flags.contains(Flag::Flag_IsWritable),
        log: flags.contains(Flag::Flag_DisplayLogarithmic) && info.minValue > 0.0,
        unit: unit_kind,
        value_strings,
        has_strings: flags.contains(Flag::Flag_ValuesHaveStrings),
    };
    p.default_normalized = p.normalize(info.defaultValue);
    Some(p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::find;

    const SR: f64 = 48_000.0;
    const BLOCK: usize = 512;

    fn load(id: &str) -> AuPlugin {
        AuPlugin::new(&find(id).expect("built-in audio unit"), SR, BLOCK).unwrap()
    }

    fn render(p: &mut AuProcessor, input: f32, notes: &[NoteEvent], blocks: usize) -> Vec<f32> {
        let t = Transport { playing: true, tempo_bpm: 120.0, time_sig: (4, 4), ..Transport::default() };
        let inp = vec![input; BLOCK];
        let mut all = Vec::new();
        for i in 0..blocks {
            let (mut l, mut r) = (vec![0.0; BLOCK], vec![0.0; BLOCK]);
            let t = Transport { position_samples: (i * BLOCK) as i64, ..t };
            p.process(&[&inp, &inp], &mut [&mut l, &mut r], BLOCK, &t, if i == 0 { notes } else { &[] });
            all.extend(l);
        }
        all
    }

    #[test]
    fn effect_processes_with_params_state_and_latency() {
        let plugin = load("aufx:dely:appl");
        assert!(!plugin.is_instrument());
        let params = plugin.parameters();
        let mix = params.iter().find(|p| p.title.to_lowercase().contains("wet")).expect("a dry/wet parameter").clone();

        // Fully dry passes the input through; fully wet delays it, so the start is silent.
        plugin.set_param(mix.id, 0.0);
        let mut proc = plugin.create_processor(SR, BLOCK).unwrap();
        let dry = render(&mut proc, 0.5, &[], 4);
        assert!((dry[100] - 0.5).abs() < 0.05, "dry output follows the input: {}", dry[100]);
        plugin.set_param(mix.id, 1.0);
        assert!((plugin.get_param(mix.id).unwrap() - 1.0).abs() < 1e-3);
        assert!(plugin.param_display(mix.id, 1.0).is_some_and(|d| d.contains("100")));
        let mut proc = plugin.create_processor(SR, BLOCK).unwrap();
        let wet = render(&mut proc, 0.5, &[], 2);
        assert!(wet[10].abs() < 0.05, "wet output starts with the delay: {}", wet[10]);

        // State round trip into a second instance.
        let state = plugin.get_state().unwrap();
        assert!(state.starts_with(b"bplist"));
        let other = load("aufx:dely:appl");
        other.set_param(mix.id, 0.25);
        other.set_state(&state).unwrap();
        assert!((other.get_param(mix.id).unwrap() - 1.0).abs() < 1e-3, "state restores the mix");
        assert!(other.set_state(b"garbage").is_err());

        // The peak limiter looks ahead, which it reports as latency.
        let limiter = load("aufx:lmtr:appl");
        assert!(limiter.create_processor(SR, BLOCK).unwrap().latency_samples() > 0);
    }

    #[test]
    fn instrument_sounds_after_note_on() {
        let plugin = load("aumu:dls :appl");
        assert!(plugin.is_instrument());
        let mut proc = plugin.create_processor(SR, BLOCK).unwrap();
        assert!(render(&mut proc, 0.0, &[], 2).iter().all(|s| s.abs() < 1e-6), "silent without notes");
        let note = NoteEvent { offset: 64, pitch: 60, velocity: 100 };
        let out = render(&mut proc, 0.0, &[note], 20);
        assert!(out.iter().any(|s| s.abs() > 1e-3), "a note-on makes sound");
    }
}
