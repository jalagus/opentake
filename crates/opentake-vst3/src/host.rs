//! Host-side COM objects handed to plugins.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;

use vst3::Steinberg::Vst::*;
use vst3::Steinberg::*;
use vst3::{Class, ComWrapper};

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

    unsafe fn createInstance(&self, _cid: *mut TUID, _iid: *mut TUID, obj: *mut *mut c_void) -> tresult {
        // TODO: provide IMessage / IAttributeList so component <-> controller messaging works.
        if !obj.is_null() {
            unsafe { *obj = std::ptr::null_mut() };
        }
        kNotImplemented
    }
}

/// In-memory `IBStream`, used for saving and restoring plugin state.
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
    type Interfaces = (IBStream,);
}

impl IBStreamTrait for MemoryStream {
    unsafe fn read(&self, buffer: *mut c_void, num_bytes: int32, num_read: *mut int32) -> tresult {
        let data = self.data.borrow();
        let pos = self.pos.get().min(data.len());
        let n = (num_bytes.max(0) as usize).min(data.len() - pos);
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
        let mut data = self.data.borrow_mut();
        let pos = self.pos.get();
        if data.len() < pos + n {
            data.resize(pos + n, 0);
        }
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
        let len = self.data.borrow().len() as i64;
        let base = match mode {
            0 => 0,                     // kIBSeekSet
            1 => self.pos.get() as i64, // kIBSeekCur
            2 => len,                   // kIBSeekEnd
            _ => return kInvalidArgument,
        };
        let new = (base + pos).clamp(0, len);
        self.pos.set(new as usize);
        if !result.is_null() {
            unsafe { *result = new };
        }
        kResultOk
    }

    unsafe fn tell(&self, pos: *mut int64) -> tresult {
        if pos.is_null() {
            return kInvalidArgument;
        }
        unsafe { *pos = self.pos.get() as int64 };
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
        let queue = match self.find(id) {
            Some(q) => q,
            None => match self.alloc(id) {
                Some(q) => q,
                None => return false,
            },
        };
        unsafe { queue.addPoint(0, value, std::ptr::null_mut()) == kResultOk }
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
