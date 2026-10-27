//! Host-side COM objects handed to plugins.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::{CStr, CString, c_void};
use std::sync::Arc;

use vst3::Steinberg::Vst::IAttributeList_::AttrID;
use vst3::Steinberg::Vst::*;
use vst3::Steinberg::*;
use vst3::{Class, ComWrapper};

use crate::editor::{Edit, EditQueue};

pub const HOST_NAME: &str = "OpenTake";

pub struct HostApp;

impl Class for HostApp {
    type Interfaces = (IHostApplication,);
}

impl IHostApplicationTrait for HostApp {
    unsafe fn getName(&self, name: *mut String128) -> tresult {
        if name.is_null() {
            return kInvalidArgument;
        }
        unsafe { copy_wstring(HOST_NAME, &mut *name) };
        kResultOk
    }

    /// Component and controller talk through `IConnectionPoint::notify` with messages the
    /// host creates here (many editors only show the right state once that works).
    unsafe fn createInstance(&self, cid: *mut TUID, iid: *mut TUID, obj: *mut *mut c_void) -> tresult {
        if obj.is_null() {
            return kInvalidArgument;
        }
        // SAFETY: the ids are valid when non-null (sloppy plug-ins pass only one).
        let asks = |id: &TUID| unsafe { (!cid.is_null() && *cid == *id) || (!iid.is_null() && *iid == *id) };
        let created = if asks(&IMessage_iid) {
            ComWrapper::new(HostMessage::default()).to_com_ptr::<IMessage>().map(|p| p.into_raw() as *mut c_void)
        } else if asks(&IAttributeList_iid) {
            ComWrapper::new(AttributeList::default()).to_com_ptr::<IAttributeList>().map(|p| p.into_raw() as *mut c_void)
        } else {
            None
        };
        // SAFETY: checked non-null; the plug-in owns the reference handed out.
        unsafe { *obj = created.unwrap_or(std::ptr::null_mut()) };
        if created.is_some() { kResultOk } else { kNotImplemented }
    }
}

/// `IMessage`: an id plus an attribute list, passed between component and controller.
pub struct HostMessage {
    id: RefCell<Option<CString>>,
    attributes: ComWrapper<AttributeList>,
}

impl Default for HostMessage {
    fn default() -> Self {
        Self { id: RefCell::new(None), attributes: ComWrapper::new(AttributeList::default()) }
    }
}

impl Class for HostMessage {
    type Interfaces = (IMessage,);
}

impl IMessageTrait for HostMessage {
    unsafe fn getMessageID(&self) -> FIDString {
        self.id.borrow().as_ref().map_or(std::ptr::null(), |id| id.as_ptr())
    }

    unsafe fn setMessageID(&self, id: FIDString) {
        // SAFETY: a non-null id is a NUL-terminated string.
        *self.id.borrow_mut() = (!id.is_null()).then(|| unsafe { CStr::from_ptr(id) }.to_owned());
    }

    /// Not add-ref'ed, as in the SDK's host classes: the message owns its list.
    unsafe fn getAttributes(&self) -> *mut IAttributeList {
        self.attributes.as_com_ref::<IAttributeList>().map_or(std::ptr::null_mut(), |r| r.as_ptr())
    }
}

enum Attr {
    Int(i64),
    Float(f64),
    /// UTF-16 without the terminator.
    String(Vec<TChar>),
    Binary(Vec<u8>),
}

/// `IAttributeList`: typed values by key. The list owns string and binary data, so the
/// pointer `getBinary` hands out lives as long as the message (or until the key is set again).
#[derive(Default)]
pub struct AttributeList {
    values: RefCell<HashMap<CString, Attr>>,
}

impl AttributeList {
    fn set(&self, id: AttrID, value: Attr) -> tresult {
        if id.is_null() {
            return kInvalidArgument;
        }
        // SAFETY: a non-null id is a NUL-terminated string.
        self.values.borrow_mut().insert(unsafe { CStr::from_ptr(id) }.to_owned(), value);
        kResultOk
    }

    fn get<T>(&self, id: AttrID, f: impl FnOnce(&Attr) -> Option<T>) -> Option<T> {
        if id.is_null() {
            return None;
        }
        // SAFETY: as in `set`.
        self.values.borrow().get(unsafe { CStr::from_ptr(id) }).and_then(f)
    }
}

impl Class for AttributeList {
    type Interfaces = (IAttributeList,);
}

/// Write `value` through `out` (if both exist).
unsafe fn put<T>(value: Option<T>, out: *mut T) -> tresult {
    match value {
        Some(v) if !out.is_null() => {
            unsafe { *out = v };
            kResultOk
        }
        _ => kResultFalse,
    }
}

impl IAttributeListTrait for AttributeList {
    unsafe fn setInt(&self, id: AttrID, value: int64) -> tresult {
        self.set(id, Attr::Int(value))
    }

    unsafe fn getInt(&self, id: AttrID, value: *mut int64) -> tresult {
        unsafe { put(self.get(id, |a| if let Attr::Int(v) = a { Some(*v) } else { None }), value) }
    }

    unsafe fn setFloat(&self, id: AttrID, value: f64) -> tresult {
        self.set(id, Attr::Float(value))
    }

    unsafe fn getFloat(&self, id: AttrID, value: *mut f64) -> tresult {
        unsafe { put(self.get(id, |a| if let Attr::Float(v) = a { Some(*v) } else { None }), value) }
    }

    unsafe fn setString(&self, id: AttrID, string: *const TChar) -> tresult {
        if string.is_null() {
            return kInvalidArgument;
        }
        // SAFETY: a NUL-terminated UTF-16 string.
        let len = (0..).take_while(|&i| unsafe { *string.add(i) } != 0).count();
        self.set(id, Attr::String(unsafe { std::slice::from_raw_parts(string, len) }.to_vec()))
    }

    /// `size_in_bytes` is the buffer's size; the copy is cut to fit and always terminated.
    unsafe fn getString(&self, id: AttrID, string: *mut TChar, size_in_bytes: uint32) -> tresult {
        let capacity = size_in_bytes as usize / std::mem::size_of::<TChar>();
        if string.is_null() || capacity == 0 {
            return kInvalidArgument;
        }
        let found = self.get(id, |a| {
            let Attr::String(s) = a else { return None };
            let n = s.len().min(capacity - 1);
            // SAFETY: the plug-in's buffer holds `capacity` characters.
            unsafe {
                std::ptr::copy_nonoverlapping(s.as_ptr(), string, n);
                *string.add(n) = 0;
            }
            Some(())
        });
        if found.is_some() { kResultOk } else { kResultFalse }
    }

    unsafe fn setBinary(&self, id: AttrID, data: *const c_void, size_in_bytes: uint32) -> tresult {
        // SAFETY: `data` holds `size_in_bytes` bytes.
        let bytes = if data.is_null() { Vec::new() } else { unsafe { std::slice::from_raw_parts(data as *const u8, size_in_bytes as usize) }.to_vec() };
        self.set(id, Attr::Binary(bytes))
    }

    unsafe fn getBinary(&self, id: AttrID, data: *mut *const c_void, size_in_bytes: *mut uint32) -> tresult {
        if data.is_null() || size_in_bytes.is_null() {
            return kInvalidArgument;
        }
        match self.get(id, |a| if let Attr::Binary(b) = a { Some((b.as_ptr(), b.len())) } else { None }) {
            Some((ptr, len)) => {
                unsafe {
                    *data = ptr as *const c_void;
                    *size_in_bytes = len as uint32;
                }
                kResultOk
            }
            None => kResultFalse,
        }
    }
}

/// `IComponentHandler`: how the edit controller (in practice its editor) reports parameter
/// edits. They're queued for the engine, which applies them to the processor, the project,
/// undo and automation (see `Vst3Plugin::take_edits`).
pub struct ComponentHandler {
    pub(crate) edits: Arc<EditQueue>,
}

impl Class for ComponentHandler {
    type Interfaces = (IComponentHandler,);
}

impl IComponentHandlerTrait for ComponentHandler {
    unsafe fn beginEdit(&self, id: ParamID) -> tresult {
        self.edits.push(Edit::Begin(id));
        kResultOk
    }

    unsafe fn performEdit(&self, id: ParamID, value: ParamValue) -> tresult {
        self.edits.push(Edit::Perform(id, value.clamp(0.0, 1.0)));
        kResultOk
    }

    unsafe fn endEdit(&self, id: ParamID) -> tresult {
        self.edits.push(Edit::End(id));
        kResultOk
    }

    /// New parameter values (a preset loaded in the editor) are picked up; other restarts
    /// (latency, buses) take effect when the processor is next set up.
    unsafe fn restartComponent(&self, flags: int32) -> tresult {
        if flags & RestartFlags_::kParamValuesChanged != 0 {
            self.edits.push(Edit::Reload);
        }
        kResultOk
    }
}

/// In-memory `IBStream` (and `ISizeableStream`, which some plug-in frameworks ask for to
/// size their reads), used for saving and restoring plugin state. Semantics follow the
/// SDK's `MemoryStream`: reads stop at the end, the position may be sought past the end
/// (never before the start), and writing there fills the gap with zeros.
#[derive(Default)]
pub struct MemoryStream {
    data: RefCell<Vec<u8>>,
    pos: Cell<usize>,
}

impl MemoryStream {
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        Self { data: RefCell::new(bytes), pos: Cell::new(0) }
    }

    pub fn bytes(&self) -> Vec<u8> {
        self.data.borrow().clone()
    }
}

impl Class for MemoryStream {
    type Interfaces = (IBStream, ISizeableStream);
}

impl IBStreamTrait for MemoryStream {
    unsafe fn read(&self, buffer: *mut c_void, num_bytes: int32, num_read: *mut int32) -> tresult {
        let data = self.data.borrow();
        let pos = self.pos.get().min(data.len());
        let n = (num_bytes.max(0) as usize).min(data.len() - pos);
        if buffer.is_null() && n > 0 {
            return kInvalidArgument;
        }
        // SAFETY: the plug-in's buffer holds `num_bytes` >= `n` bytes; `num_read` is null or valid.
        unsafe {
            std::ptr::copy_nonoverlapping(data[pos..].as_ptr(), buffer as *mut u8, n);
            if !num_read.is_null() {
                *num_read = n as int32;
            }
        }
        self.pos.set(pos + n);
        kResultOk
    }

    unsafe fn write(&self, buffer: *mut c_void, num_bytes: int32, num_written: *mut int32) -> tresult {
        let n = num_bytes.max(0) as usize;
        if buffer.is_null() && n > 0 {
            return kInvalidArgument;
        }
        let mut data = self.data.borrow_mut();
        let pos = self.pos.get();
        if data.len() < pos + n {
            data.resize(pos + n, 0);
        }
        // SAFETY: `buffer` holds `n` bytes and `data` was grown to fit them.
        unsafe {
            std::ptr::copy_nonoverlapping(buffer as *const u8, data[pos..].as_mut_ptr(), n);
            if !num_written.is_null() {
                *num_written = n as int32;
            }
        }
        self.pos.set(pos + n);
        kResultOk
    }

    unsafe fn seek(&self, pos: int64, mode: int32, result: *mut int64) -> tresult {
        use IBStream_::IStreamSeekMode_::*;
        let base = match mode as IBStream_::IStreamSeekMode {
            m if m == kIBSeekSet => 0,
            m if m == kIBSeekCur => self.pos.get() as i64,
            m if m == kIBSeekEnd => self.data.borrow().len() as i64,
            _ => return kInvalidArgument,
        };
        let new = base.saturating_add(pos).max(0);
        self.pos.set(new as usize);
        if !result.is_null() {
            // SAFETY: checked non-null.
            unsafe { *result = new };
        }
        kResultOk
    }

    unsafe fn tell(&self, pos: *mut int64) -> tresult {
        if pos.is_null() {
            return kInvalidArgument;
        }
        // SAFETY: checked non-null.
        unsafe { *pos = self.pos.get() as int64 };
        kResultOk
    }
}

impl ISizeableStreamTrait for MemoryStream {
    unsafe fn getStreamSize(&self, size: *mut int64) -> tresult {
        if size.is_null() {
            return kInvalidArgument;
        }
        // SAFETY: checked non-null.
        unsafe { *size = self.data.borrow().len() as int64 };
        kResultOk
    }

    unsafe fn setStreamSize(&self, size: int64) -> tresult {
        if size < 0 {
            return kInvalidArgument;
        }
        self.data.borrow_mut().resize(size as usize, 0);
        kResultOk
    }
}

const MAX_POINTS: usize = 16;
pub const MAX_PARAM_QUEUES: usize = 64;

/// One parameter's automation points within a process block. Preallocated; no
/// allocation happens on the audio thread.
pub struct ParamQueue {
    id: Cell<ParamID>,
    points: [Cell<(int32, ParamValue)>; MAX_POINTS],
    len: Cell<usize>,
}

impl Class for ParamQueue {
    type Interfaces = (IParamValueQueue,);
}

impl IParamValueQueueTrait for ParamQueue {
    unsafe fn getParameterId(&self) -> ParamID {
        self.id.get()
    }

    unsafe fn getPointCount(&self) -> int32 {
        self.len.get() as int32
    }

    unsafe fn getPoint(&self, index: int32, offset: *mut int32, value: *mut ParamValue) -> tresult {
        let Some(point) = self.points.get(index as usize).filter(|_| (index as usize) < self.len.get()) else {
            return kInvalidArgument;
        };
        let (o, v) = point.get();
        unsafe {
            if !offset.is_null() {
                *offset = o;
            }
            if !value.is_null() {
                *value = v;
            }
        }
        kResultOk
    }

    unsafe fn addPoint(&self, offset: int32, value: ParamValue, index: *mut int32) -> tresult {
        let len = self.len.get();
        if len >= MAX_POINTS {
            return kResultFalse;
        }
        self.points[len].set((offset, value));
        self.len.set(len + 1);
        if !index.is_null() {
            unsafe { *index = len as int32 };
        }
        kResultOk
    }
}

/// `IParameterChanges` backed by a fixed pool of queues.
pub struct ParamChanges {
    queues: Vec<ComWrapper<ParamQueue>>,
    used: Cell<usize>,
}

impl ParamChanges {
    pub fn new() -> Self {
        let queues = (0..MAX_PARAM_QUEUES)
            .map(|_| {
                ComWrapper::new(ParamQueue {
                    id: Cell::new(0),
                    points: std::array::from_fn(|_| Cell::new((0, 0.0))),
                    len: Cell::new(0),
                })
            })
            .collect();
        Self { queues, used: Cell::new(0) }
    }

    pub fn clear(&self) {
        for q in &self.queues[..self.used.get()] {
            q.len.set(0);
        }
        self.used.set(0);
    }

    /// Host-side: set a parameter to `value` at the start of the next block.
    pub fn push(&self, id: ParamID, value: ParamValue) -> bool {
        self.push_at(id, 0, value)
    }

    /// Host-side: set a parameter to `value` at sample `offset` of the next block (offsets
    /// per parameter in increasing order). A point at the same offset as the last one
    /// replaces it, and a full queue moves its last point to the newest value, so the
    /// parameter always ends the block where it should.
    pub fn push_at(&self, id: ParamID, offset: i32, value: ParamValue) -> bool {
        let Some(queue) = self.find(id).or_else(|| self.alloc(id)) else { return false };
        let len = queue.len.get();
        let last = len.checked_sub(1).map(|i| queue.points[i].get());
        match last {
            Some((o, _)) if o >= offset || len >= MAX_POINTS => {
                queue.points[len - 1].set((o.max(offset), value));
                true
            }
            _ => unsafe { queue.addPoint(offset, value, std::ptr::null_mut()) == kResultOk },
        }
    }

    /// The queued points of parameter `id`.
    pub fn points(&self, id: ParamID) -> Vec<(i32, ParamValue)> {
        self.find(id).map(|q| q.points[..q.len.get()].iter().map(Cell::get).collect()).unwrap_or_default()
    }

    /// Iterate the last value of every changed parameter (used for plugin -> host changes).
    pub fn last_values(&self) -> impl Iterator<Item = (ParamID, ParamValue)> + '_ {
        self.queues[..self.used.get()].iter().filter_map(|q| {
            let len = q.len.get();
            (len > 0).then(|| (q.id.get(), q.points[len - 1].get().1))
        })
    }

    fn find(&self, id: ParamID) -> Option<&ComWrapper<ParamQueue>> {
        self.queues[..self.used.get()].iter().find(|q| q.id.get() == id)
    }

    fn alloc(&self, id: ParamID) -> Option<&ComWrapper<ParamQueue>> {
        let used = self.used.get();
        let q = self.queues.get(used)?;
        q.id.set(id);
        q.len.set(0);
        self.used.set(used + 1);
        Some(q)
    }
}

impl Default for ParamChanges {
    fn default() -> Self {
        Self::new()
    }
}

impl Class for ParamChanges {
    type Interfaces = (IParameterChanges,);
}

impl IParameterChangesTrait for ParamChanges {
    unsafe fn getParameterCount(&self) -> int32 {
        self.used.get() as int32
    }

    unsafe fn getParameterData(&self, index: int32) -> *mut IParamValueQueue {
        match self.queues[..self.used.get()].get(index as usize) {
            Some(q) => q.as_com_ref::<IParamValueQueue>().map_or(std::ptr::null_mut(), |r| r.as_ptr()),
            None => std::ptr::null_mut(),
        }
    }

    unsafe fn addParameterData(&self, id: *const ParamID, index: *mut int32) -> *mut IParamValueQueue {
        if id.is_null() {
            return std::ptr::null_mut();
        }
        let id = unsafe { *id };
        let pos = self.queues[..self.used.get()].iter().position(|q| q.id.get() == id);
        let pos = match pos {
            Some(p) => p,
            None => {
                if self.alloc(id).is_none() {
                    return std::ptr::null_mut();
                }
                self.used.get() - 1
            }
        };
        if !index.is_null() {
            unsafe { *index = pos as int32 };
        }
        unsafe { self.getParameterData(pos as int32) }
    }
}

/// VST3 numbers MIDI controllers like `ControllerNumbers`: CCs 0..=127, then channel
/// pressure and pitch bend.
pub const MIDI_CONTROLLERS: usize = 130;
pub const CTRL_AFTERTOUCH: u8 = 128;
pub const CTRL_PITCH_BEND: u8 = 129;

/// VST3 has no MIDI controller events: a plug-in maps controllers to parameters
/// (`IMidiMapping`) and the host sends them as parameter changes. Queue controller
/// `controller` at `offset` as its parameter's change; a controller the plug-in doesn't map
/// is dropped (false).
pub fn queue_midi_control(changes: &ParamChanges, map: &[Option<ParamID>; MIDI_CONTROLLERS], offset: i32, controller: u8, normalized: ParamValue) -> bool {
    match map.get(controller as usize).copied().flatten() {
        Some(id) => changes.push_at(id, offset, normalized.clamp(0.0, 1.0)),
        None => false,
    }
}

pub fn copy_wstring(src: &str, dst: &mut [TChar]) {
    let mut len = 0;
    for (s, d) in src.encode_utf16().zip(dst.iter_mut()) {
        *d = s as TChar;
        len += 1;
    }
    if len < dst.len() {
        dst[len] = 0;
    } else if let Some(last) = dst.last_mut() {
        *last = 0;
    }
}

pub fn from_wstring(src: &[TChar]) -> String {
    let len = src.iter().position(|&c| c == 0).unwrap_or(src.len());
    String::from_utf16_lossy(&src[..len])
}

pub const MAX_EVENTS: usize = 512;

/// `IEventList` for note events, preallocated so filling it never allocates.
pub struct EventList {
    events: std::cell::UnsafeCell<Vec<Event>>,
}

impl EventList {
    pub fn new() -> Self {
        Self { events: std::cell::UnsafeCell::new(Vec::with_capacity(MAX_EVENTS)) }
    }

    pub fn clear(&self) {
        unsafe { (*self.events.get()).clear() }
    }

    pub fn push(&self, e: Event) -> bool {
        let events = unsafe { &mut *self.events.get() };
        if events.len() < events.capacity() {
            events.push(e);
            true
        } else {
            false
        }
    }
}

impl Default for EventList {
    fn default() -> Self {
        Self::new()
    }
}

impl Class for EventList {
    type Interfaces = (IEventList,);
}

impl IEventListTrait for EventList {
    unsafe fn getEventCount(&self) -> int32 {
        unsafe { (*self.events.get()).len() as int32 }
    }

    unsafe fn getEvent(&self, index: int32, e: *mut Event) -> tresult {
        let events = unsafe { &*self.events.get() };
        match events.get(index as usize) {
            Some(ev) if !e.is_null() => {
                unsafe { *e = *ev };
                kResultOk
            }
            _ => kInvalidArgument,
        }
    }

    unsafe fn addEvent(&self, e: *mut Event) -> tresult {
        if e.is_null() {
            return kInvalidArgument;
        }
        if self.push(unsafe { *e }) { kResultOk } else { kResultFalse }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What plug-ins do with a state stream: write, tell, seek back to patch a size, seek
    /// to the end to size it, read in pieces past the end.
    #[test]
    fn memory_stream_reads_writes_and_seeks_like_the_sdk() {
        use IBStream_::IStreamSeekMode_::*;
        let stream = ComWrapper::new(MemoryStream::default());
        let s = stream.to_com_ptr::<IBStream>().unwrap();
        let mut n = 0;
        let mut at = 0i64;
        unsafe {
            let mut header = *b"SIZE";
            assert_eq!(s.write(header.as_mut_ptr() as *mut c_void, 4, &mut n), kResultOk);
            let mut body = *b"payload";
            s.write(body.as_mut_ptr() as *mut c_void, 7, &mut n);
            assert_eq!(n, 7);
            s.tell(&mut at);
            assert_eq!(at, 11);
            // Patch the header in place, then back to the end.
            s.seek(0, kIBSeekSet as i32, &mut at);
            let mut size = 7u32.to_le_bytes();
            s.write(size.as_mut_ptr() as *mut c_void, 4, std::ptr::null_mut());
            s.seek(0, kIBSeekEnd as i32, &mut at);
            assert_eq!(at, 11);
            // Past the end: the gap is zero-filled on write; never before the start.
            s.seek(2, kIBSeekCur as i32, &mut at);
            assert_eq!(at, 13);
            let mut tail = [9u8];
            s.write(tail.as_mut_ptr() as *mut c_void, 1, std::ptr::null_mut());
            s.seek(-100, kIBSeekCur as i32, &mut at);
            assert_eq!(at, 0);
            assert_eq!(s.seek(0, 7, &mut at), kInvalidArgument);
        }
        assert_eq!(stream.bytes(), [&7u32.to_le_bytes()[..], b"payload", &[0, 0, 9]].concat());

        let sized = stream.to_com_ptr::<ISizeableStream>().expect("sizeable");
        let mut len = 0i64;
        unsafe { sized.getStreamSize(&mut len) };
        assert_eq!(len, 14);
        // Reads stop at the end and report what they got.
        let mut buf = [0u8; 10];
        unsafe {
            s.seek(4, kIBSeekSet as i32, std::ptr::null_mut());
            assert_eq!(s.read(buf.as_mut_ptr() as *mut c_void, 10, &mut n), kResultOk);
            assert_eq!((n, &buf[..7]), (10, &b"payload"[..]));
            s.read(buf.as_mut_ptr() as *mut c_void, 10, &mut n);
            assert_eq!(n, 0, "at the end");
            s.tell(&mut at);
            assert_eq!(at, 14);
            s.seek(20, kIBSeekSet as i32, std::ptr::null_mut());
            s.read(buf.as_mut_ptr() as *mut c_void, 4, &mut n);
            assert_eq!(n, 0, "past the end");
            sized.setStreamSize(4);
        }
        assert_eq!(stream.bytes(), 7u32.to_le_bytes());
    }

    #[test]
    fn midi_controllers_become_parameter_changes_at_their_offsets() {
        let changes = ParamChanges::new();
        let mut map = [None; MIDI_CONTROLLERS];
        map[CTRL_PITCH_BEND as usize] = Some(1000);
        map[1] = Some(1001);
        // Automation at the block start, then a bend moving through the block.
        changes.push(1000, 0.5);
        for (offset, v) in [(10, 0.6), (10, 0.65), (200, 0.7)] {
            assert!(queue_midi_control(&changes, &map, offset, CTRL_PITCH_BEND, v));
        }
        assert!(queue_midi_control(&changes, &map, 5, 1, 2.0));
        assert!(!queue_midi_control(&changes, &map, 5, 7, 0.3), "CC7 isn't mapped: dropped");
        assert_eq!(changes.points(1000), vec![(0, 0.5), (10, 0.65), (200, 0.7)], "same offset replaces");
        assert_eq!(changes.points(1001), vec![(5, 1.0)], "clamped to 0..1");
        assert_eq!(unsafe { changes.getParameterCount() }, 2);
        // A dense stream fills the queue; the last point follows the newest value.
        for i in 0..40 {
            queue_midi_control(&changes, &map, 300 + i, CTRL_PITCH_BEND, i as f64 / 40.0);
        }
        let points = changes.points(1000);
        assert_eq!(points.len(), MAX_POINTS);
        assert_eq!(*points.last().unwrap(), (339, 39.0 / 40.0));
        changes.clear();
        assert!(changes.points(1000).is_empty());
    }
}
