//! An audio unit's own view in a native window (`opentake_vst3::window`), and the
//! parameter edits made in it.
//!
//! The view works on the very unit that renders audio, so its changes are heard at once;
//! the host learns of them through an AU event listener (value changes and begin/end
//! gestures), which queues them as [`Edit`]s for the engine (project, undo, automation),
//! the same as VST3 editors' edits. AUv2 units provide a Cocoa view factory
//! (`kAudioUnitProperty_CocoaUI`); AUv3 units hand over a view controller, asynchronously
//! (`kAudioUnitProperty_RequestViewController`). Main thread only.

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Sel};
use objc2::{MainThreadMarker, msg_send, sel};
use objc2_app_kit::{NSAutoresizingMaskOptions, NSView, NSViewController};
use objc2_audio_toolbox::*;
use objc2_core_foundation::{CFRetained, CFRunLoop, CFString, CFURL, kCFRunLoopCommonModes, kCFRunLoopDefaultMode};
use objc2_foundation::{NSBundle, NSPoint, NSRect, NSSize, NSString, NSURL};
use opentake_vst3::Edit;
use opentake_vst3::window::NativeWindow;

use crate::plugin::{Instance, get_prop_size, set_prop};
use crate::{Error, Result};

/// How long an AUv3 unit may take to deliver its view controller.
const VIEW_TIMEOUT: Duration = Duration::from_secs(10);

/// `AudioUnitEvent`: the C struct's union holds an `AudioUnitParameter` or an
/// `AudioUnitProperty`, which share one layout (unit, id, scope, element).
#[repr(C)]
struct AudioUnitEvent {
    kind: u32,
    target: AudioUnitParameter,
}

const VALUE_CHANGE: u32 = 0;
const BEGIN_GESTURE: u32 = 1;
const END_GESTURE: u32 = 2;
const PROPERTY_CHANGE: u32 = 3;

type EventProc = unsafe extern "C-unwind" fn(*mut c_void, *mut c_void, *const AudioUnitEvent, u64, f32);

// The event-listener half of AudioUnitUtilities.h, which objc2-audio-toolbox leaves out
// (the union in `AudioUnitEvent`).
#[link(name = "AudioToolbox", kind = "framework")]
unsafe extern "C-unwind" {
    fn AUEventListenerCreate(
        proc_: EventProc,
        user: *mut c_void,
        run_loop: *const CFRunLoop,
        mode: *const CFString,
        interval: f32,
        granularity: f32,
        out: *mut AUEventListenerRef,
    ) -> i32;
    fn AUEventListenerAddEventType(listener: AUEventListenerRef, object: *mut c_void, event: *const AudioUnitEvent) -> i32;
    fn AUEventListenerNotify(sender: AUEventListenerRef, object: *mut c_void, event: *const AudioUnitEvent) -> i32;
}

/// Runs on the run loop the listener was created on, so never on the audio thread.
unsafe extern "C-unwind" fn on_event(user: *mut c_void, _object: *mut c_void, event: *const AudioUnitEvent, _time: u64, value: f32) {
    // SAFETY: `user` is the instance the listener belongs to, which outlives it (the
    // listener holds an Arc and is disposed first); `event` is valid for the call.
    let (inst, event) = unsafe { (&*(user as *const Instance), &*event) };
    let id = event.target.mParameterID;
    let edit = match event.kind {
        VALUE_CHANGE => match inst.param(id) {
            Some(p) => Edit::Perform(id, p.normalize(value)),
            None => return,
        },
        BEGIN_GESTURE => Edit::Begin(id),
        END_GESTURE => Edit::End(id),
        PROPERTY_CHANGE => Edit::Reload,
        _ => return,
    };
    inst.edits.push(edit);
}

fn event(unit: AudioUnit, kind: u32, id: u32) -> AudioUnitEvent {
    AudioUnitEvent { kind, target: AudioUnitParameter { mAudioUnit: unit, mParameterID: id, mScope: kAudioUnitScope_Global, mElement: 0 } }
}

/// Listens for the unit's parameter changes and gestures (and preset changes), queueing
/// them as edits. Callbacks come on the run loop of the thread that created it (the main
/// one in the app). While it lives, the host's own changes (`AuPlugin::set_param`) are sent
/// as from it, so they reach the view but don't come back as edits.
pub struct ParamListener {
    listener: AUEventListenerRef,
    inst: Arc<Instance>,
}

impl ParamListener {
    pub(crate) fn new(inst: Arc<Instance>) -> Result<Self> {
        let run_loop = CFRunLoop::current().ok_or(Error::Other("no run loop for parameter events"))?;
        let mode = unsafe { kCFRunLoopCommonModes }.ok_or(Error::Other("no run loop mode"))?;
        let mut listener: AUEventListenerRef = std::ptr::null_mut();
        let user = Arc::as_ptr(&inst) as *mut c_void;
        // SAFETY: valid run loop and mode; `user` stays valid while the listener lives (see
        // `on_event`).
        let status = unsafe { AUEventListenerCreate(on_event, user, &*run_loop, (mode as *const objc2_core_foundation::CFRunLoopMode).cast(), 0.02, 0.0, &mut listener) };
        if status != 0 || listener.is_null() {
            return Err(Error::Call("AUEventListenerCreate", status));
        }
        let this = Self { listener, inst };
        let unit = this.inst.unit;
        for p in &this.inst.params {
            for kind in [BEGIN_GESTURE, VALUE_CHANGE, END_GESTURE] {
                unsafe { AUEventListenerAddEventType(listener, std::ptr::null_mut(), &event(unit, kind, p.id)) };
            }
        }
        unsafe { AUEventListenerAddEventType(listener, std::ptr::null_mut(), &event(unit, PROPERTY_CHANGE, kAudioUnitProperty_PresentPreset)) };
        this.inst.listener.store(listener, Ordering::Release);
        Ok(this)
    }
}

impl Drop for ParamListener {
    fn drop(&mut self) {
        // Stop sending as this listener before it goes.
        let _ = self.inst.listener.compare_exchange(self.listener, std::ptr::null_mut(), Ordering::AcqRel, Ordering::Acquire);
        unsafe { AUListenerDispose(self.listener) };
    }
}

/// Do what a unit's view does when one of its controls is moved: begin a gesture, set the
/// value (notifying listeners), end the gesture. For tests and scripting without a window.
pub(crate) fn emulate_view_edit(inst: &Instance, id: u32, normalized: f64) {
    let Some(p) = inst.param(id) else { return };
    let unit = inst.unit;
    // SAFETY: valid events and parameter for a live unit; no sending listener, like a view
    // that doesn't listen to itself.
    unsafe {
        AUEventListenerNotify(std::ptr::null_mut(), std::ptr::null_mut(), &event(unit, BEGIN_GESTURE, id));
        AUParameterSet(std::ptr::null_mut(), std::ptr::null_mut(), NonNull::from(&event(unit, VALUE_CHANGE, id).target), p.denormalize(normalized), 0);
        AUEventListenerNotify(std::ptr::null_mut(), std::ptr::null_mut(), &event(unit, END_GESTURE, id));
    }
}

/// Run the current thread's run loop for up to `timeout`, delivering parameter events (the
/// app's main loop does this anyway).
pub fn pump_events(timeout: Duration) {
    // Running returns at once while the loop has nothing scheduled yet, so keep at it.
    let start = Instant::now();
    while start.elapsed() < timeout {
        CFRunLoop::run_in_mode(unsafe { kCFRunLoopDefaultMode }, 0.01, false);
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// The view controller an AUv3 unit hands over, from whatever thread it calls back on.
struct Delivered(Option<Retained<NSViewController>>);

// SAFETY: only moved to the main thread, where it is used.
unsafe impl Send for Delivered {}

/// An open audio unit view. Dropping it removes the view from its window before the
/// window closes and before the unit (kept alive until then) can be disposed.
pub struct AuEditor {
    title: String,
    window: Option<NativeWindow>,
    view: Option<Retained<NSView>>,
    /// AUv3: the controller owning `view`, or the request still waiting for it.
    controller: Option<Retained<NSViewController>>,
    pending: Option<(Arc<Mutex<Option<Delivered>>>, Instant)>,
    failed: bool,
    /// Views that size themselves (Cocoa); the window follows them. Otherwise the view
    /// follows the window.
    fixed: bool,
    _listener: ParamListener,
    _inst: Arc<Instance>,
}

/// Whether the unit has a view of its own: a Cocoa view factory, or (AUv3) a view controller.
pub(crate) fn has_view(inst: &Instance, v3: bool) -> bool {
    v3 || unsafe { get_prop_size(inst.unit, kAudioUnitProperty_CocoaUI) }.is_some_and(|n| n >= size_of::<AudioUnitCocoaViewInfo>())
}

pub(crate) fn open(inst: Arc<Instance>, v3: bool, title: &str) -> Result<AuEditor> {
    MainThreadMarker::new().ok_or(Error::Other("audio unit views open on the main thread only"))?;
    let listener = ParamListener::new(inst.clone())?;
    let mut editor = AuEditor { title: title.into(), window: None, view: None, controller: None, pending: None, failed: false, fixed: true, _listener: listener, _inst: inst.clone() };
    // SAFETY: main thread; the unit is alive.
    if let Some(view) = unsafe { cocoa_view(inst.unit) } {
        editor.show(view)?;
        return Ok(editor);
    }
    if !v3 {
        return Err(Error::Other("the audio unit has no view of its own"));
    }
    let slot = Arc::new(Mutex::new(None));
    let delivered = slot.clone();
    let done = block2::RcBlock::new(move |vc: *mut NSViewController| {
        // SAFETY: the unit passes a valid controller (or null); retaining is thread-safe.
        let vc = unsafe { Retained::retain(vc) };
        *delivered.lock().unwrap_or_else(|p| p.into_inner()) = Some(Delivered(vc));
    });
    let block: *const block2::Block<dyn Fn(*mut NSViewController)> = &*done;
    // SAFETY: the property takes a block pointer, which the unit copies.
    unsafe { set_prop(inst.unit, kAudioUnitProperty_RequestViewController, kAudioUnitScope_Global, 0, &block, "RequestViewController")? };
    editor.pending = Some((slot, Instant::now()));
    Ok(editor)
}

/// An AUv2 unit's Cocoa view: load the view factory class from the bundle the unit names
/// and ask it for a view of this unit.
unsafe fn cocoa_view(unit: AudioUnit) -> Option<Retained<NSView>> {
    let size = unsafe { get_prop_size(unit, kAudioUnitProperty_CocoaUI) }?;
    // A bundle URL followed by one or more class names, all handed over owned (+1).
    let mut info = vec![std::ptr::null_mut::<c_void>(); size / size_of::<*mut c_void>()];
    if info.len() < 2 {
        return None;
    }
    let mut n = (info.len() * size_of::<*mut c_void>()) as u32;
    let status = unsafe { AudioUnitGetProperty(unit, kAudioUnitProperty_CocoaUI, kAudioUnitScope_Global, 0, NonNull::from(&mut info[..]).cast(), NonNull::from(&mut n)) };
    if status != 0 {
        return None;
    }
    // SAFETY: owned CF objects (released when these drop); CFURL and NSURL, CFString and
    // NSString are toll-free bridged.
    let owned: Vec<CFRetained<objc2_core_foundation::CFType>> = info.iter().filter_map(|&p| NonNull::new(p as *mut objc2_core_foundation::CFType)).map(|p| unsafe { CFRetained::from_raw(p) }).collect();
    let url = unsafe { &*(info[0] as *const CFURL as *const NSURL) };
    let class_name = unsafe { &*(info[1] as *const NSString) };
    let bundle = NSBundle::bundleWithURL(url)?;
    let class = bundle.classNamed(class_name)?;
    // SAFETY: the class implements AUCocoaUIBase: `new` gives a factory, and
    // `-uiViewForAudioUnit:withSize:` takes (AudioUnit, NSSize) and returns an autoreleased
    // view. Sent through `objc_msgSend` directly: factories built with older SDKs encode the
    // AudioUnit type differently, which `msg_send!`'s debug checks would reject.
    let view: Option<Retained<NSView>> = unsafe {
        let factory: Retained<AnyObject> = msg_send![class, new];
        type ViewForUnit = unsafe extern "C-unwind" fn(*const AnyObject, Sel, AudioUnit, NSSize) -> *mut NSView;
        let send: ViewForUnit = std::mem::transmute(objc2::ffi::objc_msgSend as unsafe extern "C-unwind" fn());
        Retained::retain_autoreleased(send(&*factory, sel!(uiViewForAudioUnit:withSize:), unit, NSSize::new(0.0, 0.0)))
    };
    drop(owned);
    view
}

impl AuEditor {
    /// Put the view into a new window sized to it.
    fn show(&mut self, view: Retained<NSView>) -> Result<()> {
        let frame = view.frame().size;
        let size = if frame.width >= 1.0 && frame.height >= 1.0 { (frame.width, frame.height) } else { (480.0, 320.0) };
        let window = NativeWindow::new(&self.title, size, !self.fixed).map_err(|e| Error::Window(e.to_string()))?;
        let content = window.content_view().ok_or(Error::Other("the editor window has no content view"))?;
        content.addSubview(&view);
        if self.fixed {
            view.setFrameOrigin(NSPoint::new(0.0, 0.0));
        } else {
            view.setFrame(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(size.0, size.1)));
            view.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable);
        }
        self.window = Some(window);
        self.view = Some(view);
        Ok(())
    }

    /// False once the user closed the window (or an AUv3 view never came).
    pub fn is_open(&self) -> bool {
        !self.failed && (self.pending.is_some() || self.window.as_ref().is_some_and(NativeWindow::is_open))
    }

    /// Call regularly: shows an AUv3 view once it's delivered, and keeps the window the
    /// size of a view that resized itself.
    pub fn idle(&mut self) {
        if let Some((slot, since)) = &self.pending {
            let delivered = slot.lock().unwrap_or_else(|p| p.into_inner()).take();
            match delivered {
                Some(Delivered(Some(vc))) => {
                    self.pending = None;
                    let view = vc.view();
                    let preferred = vc.preferredContentSize();
                    if preferred.width >= 1.0 && preferred.height >= 1.0 {
                        view.setFrame(NSRect::new(NSPoint::new(0.0, 0.0), preferred));
                    }
                    self.controller = Some(vc);
                    self.fixed = false;
                    self.failed = self.show(view).is_err();
                }
                Some(Delivered(None)) => self.failed = true,
                None if since.elapsed() > VIEW_TIMEOUT => self.failed = true,
                None => {}
            }
            return;
        }
        if let (true, Some(window), Some(view)) = (self.fixed, &self.window, &self.view) {
            let size = view.frame().size;
            let (w, h) = window.content_size();
            if size.width >= 1.0 && size.height >= 1.0 && ((size.width - w).abs() > 0.5 || (size.height - h).abs() > 0.5) {
                window.set_content_size(size.width, size.height);
                view.setFrameOrigin(NSPoint::new(0.0, 0.0));
            }
        }
    }

    pub fn set_title(&mut self, title: &str) {
        self.title = title.into();
        if let Some(w) = &self.window {
            w.set_title(title);
        }
    }

    pub fn bring_to_front(&self) {
        if let Some(w) = &self.window {
            w.bring_to_front();
        }
    }

    /// The window's id, once it's shown (0 before).
    pub fn window_id(&self) -> u64 {
        self.window.as_ref().map_or(0, NativeWindow::window_id)
    }

    /// The view's size in points (0 x 0 until it's shown).
    pub fn size(&self) -> (f64, f64) {
        self.window.as_ref().map_or((0.0, 0.0), NativeWindow::content_size)
    }
}

impl Drop for AuEditor {
    fn drop(&mut self) {
        if let Some(view) = self.view.take() {
            view.removeFromSuperview();
        }
        self.controller.take();
        self.window.take();
        // Then the listener goes, then (unless the engine still has it) the unit.
    }
}
