//! A VST3 plugin instance, split into a control-side handle and an audio-side processor.
//!
//! ```text
//!   Vst3Plugin (control thread)      Vst3Processor (audio thread)
//!        \                                /
//!         +------ Arc<Instance> ---------+   component, controller, module
//! ```

use std::ffi::c_void;
use std::sync::Arc;

use serde::Serialize;
use vst3::Steinberg::Vst::*;
use vst3::Steinberg::*;
use vst3::{ComPtr, ComWrapper};

use crate::editor::{Edit, EditQueue, Vst3Editor};
use crate::host::{ComponentHandler, EventList, HostApp, MIDI_CONTROLLERS, MemoryStream, ParamChanges, from_wstring};
use crate::module::{ClassInfo, Module};
use crate::{Error, Result};

#[derive(Debug, Clone, Serialize)]
pub struct ParamInfo {
    pub id: u32,
    pub title: String,
    pub units: String,
    pub default_normalized: f64,
    /// 0 = continuous, otherwise number of discrete steps.
    pub step_count: i32,
    pub can_automate: bool,
    pub read_only: bool,
    pub is_bypass: bool,
}

/// A note event for instrument plugins. `velocity` 0 means note-off.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NoteEvent {
    /// Sample offset within the block.
    pub offset: u32,
    pub pitch: u8,
    /// 0..1; 0 = note-off.
    pub velocity: f32,
}

/// Musical/transport information passed to the plugin each block.
#[derive(Debug, Clone, Copy, Default)]
pub struct Transport {
    pub playing: bool,
    pub recording: bool,
    pub tempo_bpm: f64,
    pub time_sig: (i32, i32),
    pub position_samples: i64,
}

pub(crate) struct Instance {
    /// Generation of the processor that currently owns activation. A processor only
    /// deactivates the plugin on drop if no newer processor has taken over (this lets a
    /// plugin be re-activated, e.g. by undo, while the old processor is still being freed).
    activation: std::sync::Mutex<u64>,
    component: ComPtr<IComponent>,
    pub(crate) controller: Option<ComPtr<IEditController>>,
    separate_controller: bool,
    connections: Option<(ComPtr<IConnectionPoint>, ComPtr<IConnectionPoint>)>,
    /// Set on the controller: edits made in the plug-in's editor land in `edits`.
    _handler: ComWrapper<ComponentHandler>,
    edits: Arc<EditQueue>,
    _host: ComWrapper<HostApp>,
    // Must be dropped last so the binary outlives every COM object above.
    _module: Arc<Module>,
}

impl Drop for Instance {
    fn drop(&mut self) {
        unsafe {
            if let Some((c, e)) = &self.connections {
                c.disconnect(e.as_ptr());
                e.disconnect(c.as_ptr());
            }
            if let Some(ctrl) = &self.controller {
                ctrl.setComponentHandler(std::ptr::null_mut());
            }
            if self.separate_controller {
                if let Some(ctrl) = &self.controller {
                    ctrl.terminate();
                }
            }
            self.component.terminate();
        }
    }
}

/// A VST3 plug-in's saved state: the component's and the edit controller's chunks, as a
/// `.vstpreset` holds them ("Comp" and "Cont").
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginState {
    pub component: Vec<u8>,
    pub controller: Option<Vec<u8>>,
}

/// Control-side handle: parameters, state, instantiating the processor.
pub struct Vst3Plugin {
    inst: Arc<Instance>,
    pub class: ClassInfo,
    params: Vec<ParamInfo>,
}

// VST3 objects are created on the control thread and the processor is moved to the
// audio thread, which is the threading model the SDK expects for IAudioProcessor.
unsafe impl Send for Vst3Plugin {}

impl Vst3Plugin {
    pub fn new(module: Arc<Module>, class: ClassInfo) -> Result<Self> {
        unsafe {
            let host = ComWrapper::new(HostApp);
            let host_unknown = host.to_com_ptr::<FUnknown>().ok_or(Error::Com("host FUnknown"))?;

            let component: ComPtr<IComponent> = create(module.factory(), &class.tuid, &IComponent_iid)
                .ok_or(Error::Instantiate(class.name.clone()))?;
            check(component.initialize(host_unknown.as_ptr()), "IComponent::initialize")?;

            // Controller: either a separate class or implemented by the component itself.
            let mut ctrl_cid: TUID = [0; 16];
            let (controller, separate_controller) = if component.getControllerClassId(&mut ctrl_cid) == kResultOk
                && ctrl_cid.iter().any(|&b| b != 0)
            {
                let ctrl: Option<ComPtr<IEditController>> = create(module.factory(), &ctrl_cid, &IEditController_iid);
                if let Some(c) = &ctrl {
                    check(c.initialize(host_unknown.as_ptr()), "IEditController::initialize")?;
                }
                (ctrl, true)
            } else {
                (component.cast::<IEditController>(), false)
            };

            let connections = match (&controller, separate_controller) {
                (Some(ctrl), true) => {
                    match (component.cast::<IConnectionPoint>(), ctrl.cast::<IConnectionPoint>()) {
                        (Some(c), Some(e)) => {
                            c.connect(e.as_ptr());
                            e.connect(c.as_ptr());
                            Some((c, e))
                        }
                        _ => None,
                    }
                }
                _ => None,
            };

            let edits = Arc::new(EditQueue::default());
            let handler = ComWrapper::new(ComponentHandler { edits: edits.clone() });
            if let (Some(ctrl), Some(h)) = (&controller, handler.as_com_ref::<IComponentHandler>()) {
                ctrl.setComponentHandler(h.as_ptr());
            }
            let inst = Arc::new(Instance { activation: std::sync::Mutex::new(0), component, controller, separate_controller, connections, _handler: handler, edits, _host: host, _module: module });

            // Sync the controller with the component's initial state.
            if let Some(ctrl) = &inst.controller {
                if let Ok(state) = get_component_state(&inst.component) {
                    let stream = ComWrapper::new(MemoryStream::from_bytes(state));
                    if let Some(s) = stream.to_com_ptr::<IBStream>() {
                        ctrl.setComponentState(s.as_ptr());
                    }
                }
            }

            let params = read_params(inst.controller.as_ref());
            Ok(Self { inst, class, params })
        }
    }

    pub fn name(&self) -> &str {
        &self.class.name
    }

    /// Whether this is an instrument (takes note events, produces audio).
    pub fn is_instrument(&self) -> bool {
        self.class.is_instrument()
    }

    pub fn parameters(&self) -> &[ParamInfo] {
        &self.params
    }

    pub fn get_param(&self, id: u32) -> Option<f64> {
        let ctrl = self.inst.controller.as_ref()?;
        Some(unsafe { ctrl.getParamNormalized(id) })
    }

    /// Human readable value, e.g. "-6.0 dB".
    pub fn param_display(&self, id: u32, normalized: f64) -> Option<String> {
        let ctrl = self.inst.controller.as_ref()?;
        let mut s: String128 = [0; 128];
        unsafe { (ctrl.getParamStringByValue(id, normalized, &mut s) == kResultOk).then(|| from_wstring(&s)) }
    }

    /// Updates the controller's view of a parameter. The processor must be told
    /// separately via [`Vst3Processor::queue_param`].
    pub fn set_param_on_controller(&self, id: u32, normalized: f64) {
        if let Some(ctrl) = &self.inst.controller {
            unsafe { ctrl.setParamNormalized(id, normalized) };
        }
    }

    /// The plug-in's full state: the component's (`IComponent::getState`: what it
    /// processes with) and the edit controller's (`IEditController::getState`: editor-side
    /// settings, often none). The latter is `None` when the plug-in has no controller or
    /// declines to save one.
    pub fn save_state(&self) -> Result<PluginState> {
        unsafe {
            let component = get_component_state(&self.inst.component)?;
            let controller = self.inst.controller.as_ref().and_then(|ctrl| {
                let stream = ComWrapper::new(MemoryStream::default());
                let s = stream.to_com_ptr::<IBStream>()?;
                (ctrl.getState(s.as_ptr()) == kResultOk).then(|| stream.bytes())
            });
            Ok(PluginState { component, controller })
        }
    }

    /// Restore a saved state, in the order the VST3 spec gives: `IComponent::setState`, then
    /// the controller's `setComponentState` with the same bytes and its own `setState`. Call
    /// it before the processor is set up (also allowed later, from this thread). A component
    /// that refuses the state is an error; a controller that does is only logged (it then
    /// keeps showing its defaults until the parameters are set).
    pub fn load_state(&self, state: &PluginState) -> Result<()> {
        // Each call reads a fresh stream from the start: plug-ins may seek or read to the end.
        let stream = |bytes: &[u8]| ComWrapper::new(MemoryStream::from_bytes(bytes.to_vec()));
        unsafe {
            let comp = stream(&state.component);
            let s = comp.to_com_ptr::<IBStream>().ok_or(Error::Com("IBStream"))?;
            check(self.inst.component.setState(s.as_ptr()), "IComponent::setState")?;
            if let Some(ctrl) = &self.inst.controller {
                let comp = stream(&state.component);
                let s = comp.to_com_ptr::<IBStream>().ok_or(Error::Com("IBStream"))?;
                if let Err(e) = check(ctrl.setComponentState(s.as_ptr()), "IEditController::setComponentState") {
                    tracing::warn!("{}: {e}", self.class.name);
                }
                if let Some(bytes) = &state.controller {
                    let own = stream(bytes);
                    let s = own.to_com_ptr::<IBStream>().ok_or(Error::Com("IBStream"))?;
                    if let Err(e) = check(ctrl.setState(s.as_ptr()), "IEditController::setState") {
                        tracing::warn!("{}: {e}", self.class.name);
                    }
                }
            }
        }
        // A plug-in may answer with `restartComponent` (values changed): that's this restore,
        // not an edit to record.
        for edit in self.inst.edits.take().into_iter().filter(|e| !matches!(e, Edit::Reload)) {
            self.inst.edits.push(edit);
        }
        Ok(())
    }

    /// Whether the plug-in has a side-chain input (an aux audio input bus), which
    /// [`Vst3Processor::process_keyed`] feeds.
    pub fn has_side_chain(&self) -> bool {
        unsafe { side_chain_bus(&self.inst.component).is_some() }
    }

    /// Whether the plug-in may have an editor of its own here (it has an edit controller
    /// and this platform has editor windows); [`open_editor`](Self::open_editor) tells for sure.
    pub fn has_editor(&self) -> bool {
        self.inst.controller.is_some() && crate::window::platform_type().is_some()
    }

    /// Open the plug-in's editor in a native window titled `title`. Main thread only.
    pub fn open_editor(&self, title: &str) -> Result<Vst3Editor> {
        crate::editor::open(self.inst.clone(), title)
    }

    /// Parameter edits made in the plug-in's editor since the last call.
    pub fn take_edits(&self) -> Vec<Edit> {
        self.inst.edits.take()
    }

    /// Prepare the plugin for processing and return the audio-thread half.
    pub fn create_processor(&self, sample_rate: f64, max_block: usize, channels: usize) -> Result<Vst3Processor> {
        Vst3Processor::new(self.inst.clone(), sample_rate, max_block, channels)
    }
}

/// Audio-thread half. `process` does not allocate.
pub struct Vst3Processor {
    inst: Arc<Instance>,
    processor: ComPtr<IAudioProcessor>,
    in_buses: Vec<usize>,
    out_buses: Vec<usize>,
    /// The side-chain input bus, activated when the plug-in has one.
    side_chain: Option<usize>,
    in_ptrs: Vec<*mut f32>,
    out_ptrs: Vec<*mut f32>,
    in_bufs: Vec<AudioBusBuffers>,
    out_bufs: Vec<AudioBusBuffers>,
    /// Silent buffers for channels the host doesn't feed and outputs it discards.
    scratch: Vec<Vec<f32>>,
    params_in: ComWrapper<ParamChanges>,
    params_out: ComWrapper<ParamChanges>,
    events_in: Option<ComWrapper<EventList>>,
    /// The parameter each MIDI controller drives (`IMidiMapping`), looked up once.
    midi_map: [Option<ParamID>; MIDI_CONTROLLERS],
    generation: u64,
    sample_rate: f64,
    max_block: usize,
}

unsafe impl Send for Vst3Processor {}

impl Vst3Processor {
    fn new(inst: Arc<Instance>, sample_rate: f64, max_block: usize, channels: usize) -> Result<Self> {
        unsafe {
            let component = &inst.component;
            let processor: ComPtr<IAudioProcessor> = component.cast().ok_or(Error::Com("IAudioProcessor"))?;

            if processor.canProcessSampleSize(SymbolicSampleSizes_::kSample32 as i32) != kResultOk {
                return Err(Error::Unsupported("32-bit float processing"));
            }

            let audio = MediaTypes_::kAudio as MediaType;
            let (input, output) = (BusDirections_::kInput as BusDirection, BusDirections_::kOutput as BusDirection);
            let n_in = component.getBusCount(audio, input).max(0) as usize;
            let n_out = component.getBusCount(audio, output).max(0) as usize;

            // Ask for our channel layout on the main buses; keep the plugin's choice otherwise.
            let wanted = if channels == 1 { SpeakerArr::kMono } else { SpeakerArr::kStereo };
            let mut in_arr: Vec<SpeakerArrangement> = (0..n_in).map(|i| bus_arrangement(&processor, input, i)).collect();
            let mut out_arr: Vec<SpeakerArrangement> = (0..n_out).map(|i| bus_arrangement(&processor, output, i)).collect();
            if let Some(a) = in_arr.first_mut() {
                *a = wanted;
            }
            if let Some(a) = out_arr.first_mut() {
                *a = wanted;
            }
            processor.setBusArrangements(in_arr.as_mut_ptr(), n_in as i32, out_arr.as_mut_ptr(), n_out as i32);

            // The main buses, plus the side chain's (fed silence when the slot isn't keyed).
            let side_chain = side_chain_bus(component);
            for (dir, n) in [(input, n_in), (output, n_out)] {
                for i in 0..n {
                    let on = i == 0 || (dir == input && Some(i) == side_chain);
                    component.activateBus(audio, dir, i as i32, on as TBool);
                }
            }
            // Instruments receive notes on their first event input bus.
            let event = MediaTypes_::kEvent as MediaType;
            let has_events = component.getBusCount(event, input) > 0;
            if has_events {
                component.activateBus(event, input, 0, 1);
            }
            let in_buses: Vec<usize> = (0..n_in).map(|i| bus_arrangement(&processor, input, i).count_ones() as usize).collect();
            let out_buses: Vec<usize> = (0..n_out).map(|i| bus_arrangement(&processor, output, i).count_ones() as usize).collect();

            let mut setup = ProcessSetup {
                processMode: ProcessModes_::kRealtime as i32,
                symbolicSampleSize: SymbolicSampleSizes_::kSample32 as i32,
                maxSamplesPerBlock: max_block as i32,
                sampleRate: sample_rate,
            };
            let generation = {
                let mut current = inst.activation.lock().unwrap_or_else(|p| p.into_inner());
                // A previous processor may still be active (being dropped elsewhere).
                processor.setProcessing(0);
                component.setActive(0);
                check(processor.setupProcessing(&mut setup), "IAudioProcessor::setupProcessing")?;
                check(component.setActive(1), "IComponent::setActive")?;
                processor.setProcessing(1);
                *current += 1;
                *current
            };

            let total_in: usize = in_buses.iter().sum();
            let total_out: usize = out_buses.iter().sum();
            let empty_bus = AudioBusBuffers {
                numChannels: 0,
                silenceFlags: 0,
                __field0: AudioBusBuffers__type0 { channelBuffers32: std::ptr::null_mut() },
            };

            Ok(Self {
                processor,
                in_ptrs: vec![std::ptr::null_mut(); total_in],
                out_ptrs: vec![std::ptr::null_mut(); total_out],
                in_bufs: vec![empty_bus; n_in],
                out_bufs: vec![empty_bus; n_out],
                scratch: (0..total_in + total_out).map(|_| vec![0.0; max_block]).collect(),
                in_buses,
                out_buses,
                side_chain,
                params_in: ComWrapper::new(ParamChanges::new()),
                params_out: ComWrapper::new(ParamChanges::new()),
                events_in: has_events.then(|| ComWrapper::new(EventList::new())),
                midi_map: midi_mapping(inst.controller.as_ref()),
                generation,
                sample_rate,
                max_block,
                inst,
            })
        }
    }

    pub fn latency_samples(&self) -> u32 {
        unsafe { self.processor.getLatencySamples() }
    }

    /// Queue a parameter change to be applied at the start of the next block.
    pub fn queue_param(&mut self, id: u32, normalized: f64) -> bool {
        self.params_in.push(id, normalized)
    }

    /// Queue a MIDI controller (VST3 numbering: CCs 0..=127, 128 channel pressure, 129
    /// pitch bend; value 0..1) at `offset` in the next block, as a change of the parameter
    /// the plug-in maps it to. False (dropped) when the plug-in maps no parameter to it.
    pub fn queue_midi_control(&mut self, offset: u32, controller: u8, normalized: f64) -> bool {
        crate::host::queue_midi_control(&self.params_in, &self.midi_map, offset as i32, controller, normalized)
    }

    /// Process `frames` samples. `inputs` feed the main input bus (repeated if the plugin
    /// wants more channels), and the main output bus is written to `outputs`.
    pub fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]], frames: usize, transport: &Transport, notes: &[NoteEvent]) {
        self.process_keyed(inputs, None, outputs, frames, transport, notes);
    }

    /// [`process`](Self::process) with a side-chain signal for the plug-in's aux input bus
    /// (if it has one; silence there when `key` is `None`).
    pub fn process_keyed(&mut self, inputs: &[&[f32]], key: Option<&[&[f32]]>, outputs: &mut [&mut [f32]], frames: usize, transport: &Transport, notes: &[NoteEvent]) {
        let frames = frames.min(self.max_block);
        let mut scratch = self.scratch.iter_mut();

        // Wire up channel pointers: main bus to the caller's buffers, the side chain to the
        // key, everything else to scratch.
        let mut k = 0;
        for (bus, &n) in self.in_buses.iter().enumerate() {
            let feed = if bus == 0 { Some(inputs) } else if Some(bus) == self.side_chain { key } else { None };
            for ch in 0..n {
                let s = scratch.next().expect("scratch sized for all channels");
                self.in_ptrs[k + ch] = match feed.and_then(|f| f.get(ch).or(f.first())) {
                    Some(buf) => buf.as_ptr() as *mut f32,
                    _ => {
                        s[..frames].fill(0.0);
                        s.as_mut_ptr()
                    }
                };
            }
            self.in_bufs[bus] = AudioBusBuffers {
                numChannels: n as i32,
                silenceFlags: 0,
                __field0: AudioBusBuffers__type0 { channelBuffers32: unsafe { self.in_ptrs.as_mut_ptr().add(k) } },
            };
            k += n;
        }
        let mut k = 0;
        for (bus, &n) in self.out_buses.iter().enumerate() {
            for ch in 0..n {
                let s = scratch.next().expect("scratch sized for all channels");
                self.out_ptrs[k + ch] = match outputs.get_mut(ch) {
                    Some(buf) if bus == 0 => buf.as_mut_ptr(),
                    _ => s.as_mut_ptr(),
                };
            }
            self.out_bufs[bus] = AudioBusBuffers {
                numChannels: n as i32,
                silenceFlags: 0,
                __field0: AudioBusBuffers__type0 { channelBuffers32: unsafe { self.out_ptrs.as_mut_ptr().add(k) } },
            };
            k += n;
        }

        use ProcessContext_::StatesAndFlags_::*;
        let mut ctx: ProcessContext = unsafe { std::mem::zeroed() };
        // The flag constants are u32 on some platforms and i32 on others.
        ctx.state = (kTempoValid | kTimeSigValid) as _;
        if transport.playing {
            ctx.state |= kPlaying as u32;
        }
        if transport.recording {
            ctx.state |= kRecording as u32;
        }
        ctx.sampleRate = self.sample_rate;
        ctx.projectTimeSamples = transport.position_samples;
        ctx.tempo = transport.tempo_bpm;
        ctx.timeSigNumerator = transport.time_sig.0;
        ctx.timeSigDenominator = transport.time_sig.1;
        ctx.projectTimeMusic = transport.position_samples as f64 / self.sample_rate * transport.tempo_bpm / 60.0;

        let events_ptr = match &self.events_in {
            Some(list) => {
                list.clear();
                for n in notes {
                    let mut e: Event = unsafe { std::mem::zeroed() };
                    e.sampleOffset = (n.offset as i32).min(frames as i32 - 1).max(0);
                    if n.velocity > 0.0 {
                        e.r#type = Event_::EventTypes_::kNoteOnEvent as u16;
                        e.__field0.noteOn = NoteOnEvent { channel: 0, pitch: n.pitch as i16, tuning: 0.0, velocity: n.velocity, length: 0, noteId: -1 };
                    } else {
                        e.r#type = Event_::EventTypes_::kNoteOffEvent as u16;
                        e.__field0.noteOff = NoteOffEvent { channel: 0, pitch: n.pitch as i16, velocity: 0.0, noteId: -1, tuning: 0.0 };
                    }
                    list.push(e);
                }
                list.as_com_ref::<IEventList>().map_or(std::ptr::null_mut(), |r| r.as_ptr())
            }
            None => std::ptr::null_mut(),
        };
        let params_in = self.params_in.as_com_ref::<IParameterChanges>().map_or(std::ptr::null_mut(), |r| r.as_ptr());
        let params_out = self.params_out.as_com_ref::<IParameterChanges>().map_or(std::ptr::null_mut(), |r| r.as_ptr());

        let mut data = ProcessData {
            processMode: ProcessModes_::kRealtime as i32,
            symbolicSampleSize: SymbolicSampleSizes_::kSample32 as i32,
            numSamples: frames as i32,
            numInputs: self.in_bufs.len() as i32,
            numOutputs: self.out_bufs.len() as i32,
            inputs: self.in_bufs.as_mut_ptr(),
            outputs: self.out_bufs.as_mut_ptr(),
            inputParameterChanges: params_in,
            outputParameterChanges: params_out,
            inputEvents: events_ptr,
            outputEvents: std::ptr::null_mut(),
            processContext: &mut ctx,
        };
        unsafe { self.processor.process(&mut data) };

        self.params_in.clear();
        self.params_out.clear();

        // If the plugin has fewer output channels than the caller, duplicate the last one.
        let produced = self.out_buses.first().copied().unwrap_or(0);
        if produced > 0 && produced < outputs.len() {
            let (src, rest) = outputs.split_at_mut(produced);
            for out in rest {
                out[..frames].copy_from_slice(&src[produced - 1][..frames]);
            }
        }
    }
}

/// Which parameter the plug-in drives with each MIDI controller (bus 0, channel 1).
fn midi_mapping(controller: Option<&ComPtr<IEditController>>) -> [Option<ParamID>; MIDI_CONTROLLERS] {
    let mut map = [None; MIDI_CONTROLLERS];
    if let Some(mapping) = controller.and_then(|c| c.cast::<IMidiMapping>()) {
        for (n, slot) in map.iter_mut().enumerate() {
            let mut id: ParamID = 0;
            if unsafe { mapping.getMidiControllerAssignment(0, 0, n as CtrlNumber, &mut id) } == kResultOk {
                *slot = Some(id);
            }
        }
    }
    map
}

impl Drop for Vst3Processor {
    fn drop(&mut self) {
        let current = self.inst.activation.lock().unwrap_or_else(|p| p.into_inner());
        if *current == self.generation {
            unsafe {
                self.processor.setProcessing(0);
                self.inst.component.setActive(0);
            }
        }
    }
}

unsafe fn create<I: vst3::Interface>(factory: &ComPtr<IPluginFactory>, cid: &TUID, iid: &TUID) -> Option<ComPtr<I>> {
    let mut obj: *mut c_void = std::ptr::null_mut();
    unsafe {
        if factory.createInstance(cid.as_ptr(), iid.as_ptr(), &mut obj) != kResultOk {
            return None;
        }
        ComPtr::from_raw(obj as *mut I)
    }
}

/// Index of the plug-in's side-chain input: the first aux audio input bus after the main one.
unsafe fn side_chain_bus(component: &ComPtr<IComponent>) -> Option<usize> {
    let (audio, input) = (MediaTypes_::kAudio as MediaType, BusDirections_::kInput as BusDirection);
    let n = unsafe { component.getBusCount(audio, input) }.max(0) as usize;
    (1..n).find(|&i| {
        let mut info: BusInfo = unsafe { std::mem::zeroed() };
        unsafe { component.getBusInfo(audio, input, i as i32, &mut info) == kResultOk && info.busType == BusTypes_::kAux as BusType }
    })
}

unsafe fn bus_arrangement(processor: &ComPtr<IAudioProcessor>, dir: BusDirection, index: usize) -> SpeakerArrangement {
    let mut arr: SpeakerArrangement = SpeakerArr::kStereo;
    unsafe { processor.getBusArrangement(dir, index as i32, &mut arr) };
    arr
}

unsafe fn get_component_state(component: &ComPtr<IComponent>) -> Result<Vec<u8>> {
    let stream = ComWrapper::new(MemoryStream::default());
    let s = stream.to_com_ptr::<IBStream>().ok_or(Error::Com("IBStream"))?;
    check(unsafe { component.getState(s.as_ptr()) }, "IComponent::getState")?;
    Ok(stream.bytes())
}

fn read_params(controller: Option<&ComPtr<IEditController>>) -> Vec<ParamInfo> {
    let Some(ctrl) = controller else { return Vec::new() };
    use ParameterInfo_::ParameterFlags_::*;
    let mut params = Vec::new();
    unsafe {
        for i in 0..ctrl.getParameterCount() {
            let mut info: ParameterInfo = std::mem::zeroed();
            if ctrl.getParameterInfo(i, &mut info) != kResultOk {
                continue;
            }
            let flags = info.flags as u32;
            if flags & kIsHidden as u32 != 0 {
                continue;
            }
            params.push(ParamInfo {
                id: info.id,
                title: from_wstring(&info.title),
                units: from_wstring(&info.units),
                default_normalized: info.defaultNormalizedValue,
                step_count: info.stepCount,
                can_automate: flags & kCanAutomate as u32 != 0,
                read_only: flags & kIsReadOnly as u32 != 0,
                is_bypass: flags & kIsBypass as u32 != 0,
            });
        }
    }
    params
}

fn check(result: tresult, what: &'static str) -> Result<()> {
    if result == kResultOk { Ok(()) } else { Err(Error::Call(what, result)) }
}
