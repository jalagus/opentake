//! A plug-in's own editor (`IPlugView`) in a native window, and the parameter edits it
//! makes, which arrive through the host's `IComponentHandler` (see `host.rs`) and wait in
//! an [`EditQueue`] for the engine.
//!
//! Everything here runs on the main thread, as VST3 requires for views.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use vst3::Steinberg::Vst::IEditControllerTrait;
use vst3::Steinberg::*;
use vst3::{ComPtr, ComRef, ComWrapper};

use crate::plugin::Instance;
use crate::window::{NativeWindow, platform_type};
use crate::{Error, Result};

/// A parameter edit made in a plug-in's own editor. The same events come from Audio Unit
/// views (`opentake-au`), so the engine handles both alike.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Edit {
    /// A gesture starts (mouse down on a control): the edits until `End` are one step.
    Begin(u32),
    /// The parameter moved to this normalized value.
    Perform(u32, f64),
    End(u32),
    /// The plug-in changed many values itself (e.g. loaded a preset): re-read them.
    Reload,
}

/// Edits waiting for the engine. Filled on the main thread (by the editor), drained there
/// too, so the lock is never contended by the audio thread.
#[derive(Default)]
pub struct EditQueue(Mutex<Vec<Edit>>);

impl EditQueue {
    pub fn push(&self, edit: Edit) {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).push(edit);
    }

    pub fn take(&self) -> Vec<Edit> {
        std::mem::take(&mut *self.0.lock().unwrap_or_else(|p| p.into_inner()))
    }
}

/// Width and height of a view rectangle.
fn rect_size(r: &ViewRect) -> (i32, i32) {
    (r.right - r.left, r.bottom - r.top)
}

fn rect(size: (i32, i32)) -> ViewRect {
    ViewRect { left: 0, top: 0, right: size.0, bottom: size.1 }
}

/// `IPlugFrame`: the plug-in asks the host to resize its view. The window follows, then
/// the view is told its new size (`onSize`), as the SDK's hosts do.
pub struct PlugFrame {
    window: RefCell<Option<Rc<NativeWindow>>>,
    /// The view's size as last agreed, by the plug-in (`resizeView`) or the user (window).
    size: Cell<(i32, i32)>,
    /// Inside `resizeView`: a plug-in resizing again from `onSize` is refused.
    resizing: Cell<bool>,
}

impl PlugFrame {
    fn new(window: Option<Rc<NativeWindow>>, size: (i32, i32)) -> Self {
        Self { window: RefCell::new(window), size: Cell::new(size), resizing: Cell::new(false) }
    }

    pub fn size(&self) -> (i32, i32) {
        self.size.get()
    }
}

impl vst3::Class for PlugFrame {
    type Interfaces = (IPlugFrame,);
}

impl IPlugFrameTrait for PlugFrame {
    unsafe fn resizeView(&self, view: *mut IPlugView, new_size: *mut ViewRect) -> tresult {
        // SAFETY: the plug-in passes its live view and a valid rectangle (or nulls, refused).
        let (Some(view), false) = (unsafe { ComRef::from_raw(view) }, new_size.is_null()) else { return kInvalidArgument };
        let size = rect_size(unsafe { &*new_size });
        if size.0 <= 0 || size.1 <= 0 {
            return kInvalidArgument;
        }
        if self.resizing.replace(true) {
            return kResultFalse;
        }
        self.size.set(size);
        if let Some(window) = &*self.window.borrow() {
            window.set_content_size(size.0 as f64, size.1 as f64);
        }
        unsafe { view.onSize(new_size) };
        self.resizing.set(false);
        kResultOk
    }
}

/// An open plug-in editor. Dropping it detaches the view (`removed`), releases it, then
/// closes the window; it keeps the plug-in instance alive until then.
pub struct Vst3Editor {
    view: ComPtr<IPlugView>,
    frame: ComWrapper<PlugFrame>,
    window: Rc<NativeWindow>,
    resizable: bool,
    _inst: Arc<Instance>,
}

/// A plausible editor size: what the view says, or a default.
fn usable(size: (i32, i32)) -> (i32, i32) {
    if size.0 > 0 && size.1 > 0 { size } else { (480, 320) }
}

pub(crate) fn open(inst: Arc<Instance>, title: &str) -> Result<Vst3Editor> {
    let platform = platform_type().ok_or(Error::Unsupported("plug-in editor windows on this platform"))?;
    if !crate::window::is_main_thread() {
        return Err(Error::Unsupported("plug-in editors open on the main thread only"));
    }
    let ctrl = inst.controller.as_ref().ok_or(Error::Unsupported("the plug-in has no edit controller"))?;
    // SAFETY: VST3 view calls on the main thread (the window checks that on macOS), with the
    // view owned by the returned editor, which detaches it before releasing it.
    unsafe {
        let view: ComPtr<IPlugView> = ComPtr::from_raw(ctrl.createView(c"editor".as_ptr())).ok_or(Error::Unsupported("the plug-in has no editor of its own"))?;
        if view.isPlatformTypeSupported(platform.as_ptr()) != kResultOk {
            return Err(Error::Unsupported("the plug-in's editor doesn't support this platform's windows"));
        }
        let mut r = rect((0, 0));
        view.getSize(&mut r);
        let size = usable(rect_size(&r));
        let resizable = view.canResize() == kResultOk;
        let window = Rc::new(NativeWindow::new(title, (size.0 as f64, size.1 as f64), resizable)?);
        let frame = ComWrapper::new(PlugFrame::new(Some(window.clone()), size));
        let frame_ptr = frame.as_com_ref::<IPlugFrame>().map_or(std::ptr::null_mut(), |f| f.as_ptr());
        view.setFrame(frame_ptr);
        // Windows views work in pixels and want the monitor's scale; macOS handles it itself.
        if cfg!(windows)
            && let Some(scaling) = view.cast::<IPlugViewContentScaleSupport>()
        {
            scaling.setContentScaleFactor(window.scale() as f32);
        }
        let attached = view.attached(window.parent(), platform.as_ptr());
        if attached != kResultOk {
            view.setFrame(std::ptr::null_mut());
            return Err(Error::Call("IPlugView::attached", attached));
        }
        // Some plug-ins settle on their size only once attached.
        if view.getSize(&mut r) == kResultOk && rect_size(&r) != frame.size() && rect_size(&r).0 > 0 && rect_size(&r).1 > 0 {
            frame.size.set(rect_size(&r));
            window.set_content_size(r.right as f64 - r.left as f64, r.bottom as f64 - r.top as f64);
        }
        Ok(Vst3Editor { view, frame, window, resizable, _inst: inst })
    }
}

impl Vst3Editor {
    /// False once the user closed the window (drop the editor then).
    pub fn is_open(&self) -> bool {
        self.window.is_open()
    }

    /// Call regularly: a window the user resized gets the size the view accepts
    /// (`checkSizeConstraint`), and the view is told (`onSize`).
    pub fn idle(&self) {
        let (w, h) = self.window.content_size();
        let now = (w.round() as i32, h.round() as i32);
        if !self.resizable || !self.is_open() || now == self.frame.size() || now.0 <= 0 || now.1 <= 0 {
            return;
        }
        let mut r = rect(now);
        // SAFETY: the view is attached and alive (owned by self).
        unsafe {
            self.view.checkSizeConstraint(&mut r);
            let fitted = usable(rect_size(&r));
            if fitted != now {
                self.window.set_content_size(fitted.0 as f64, fitted.1 as f64);
            }
            self.frame.size.set(fitted);
            let mut r = rect(fitted);
            self.view.onSize(&mut r);
        }
    }

    pub fn set_title(&self, title: &str) {
        self.window.set_title(title);
    }

    pub fn bring_to_front(&self) {
        self.window.bring_to_front();
    }

    /// The view's current size (as last agreed with the plug-in).
    pub fn size(&self) -> (i32, i32) {
        self.frame.size()
    }

    /// The native window's id (see [`NativeWindow::window_id`]).
    pub fn window_id(&self) -> u64 {
        self.window.window_id()
    }
}

impl Drop for Vst3Editor {
    fn drop(&mut self) {
        // SAFETY: detach before the view is released (fields drop after this) and before the
        // window it lives in goes.
        unsafe {
            self.view.removed();
            self.view.setFrame(std::ptr::null_mut());
        }
        self.frame.window.borrow_mut().take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vst3::Class;

    /// A stand-in plug-in view that records what the host tells it.
    #[derive(Default)]
    struct FakeView {
        sizes: RefCell<Vec<(i32, i32)>>,
        frame: Cell<Option<*mut IPlugFrame>>,
        me: Cell<Option<*mut IPlugView>>,
    }

    impl Class for FakeView {
        type Interfaces = (IPlugView,);
    }

    #[allow(non_snake_case)]
    impl IPlugViewTrait for FakeView {
        unsafe fn isPlatformTypeSupported(&self, _t: FIDString) -> tresult {
            kResultOk
        }
        unsafe fn attached(&self, _parent: *mut std::ffi::c_void, _t: FIDString) -> tresult {
            kResultOk
        }
        unsafe fn removed(&self) -> tresult {
            kResultOk
        }
        unsafe fn onWheel(&self, _d: f32) -> tresult {
            kResultFalse
        }
        unsafe fn onKeyDown(&self, _k: char16, _c: int16, _m: int16) -> tresult {
            kResultFalse
        }
        unsafe fn onKeyUp(&self, _k: char16, _c: int16, _m: int16) -> tresult {
            kResultFalse
        }
        unsafe fn getSize(&self, size: *mut ViewRect) -> tresult {
            unsafe { *size = rect((300, 200)) };
            kResultOk
        }
        unsafe fn onSize(&self, size: *mut ViewRect) -> tresult {
            let s = rect_size(unsafe { &*size });
            self.sizes.borrow_mut().push(s);
            // A plug-in that resizes again from onSize is refused (no recursion).
            if let (Some(frame), Some(me)) = (self.frame.get(), self.me.get()) {
                let frame = unsafe { ComRef::from_raw(frame) }.unwrap();
                let mut again = rect((s.0 + 1, s.1));
                assert_eq!(unsafe { frame.resizeView(me, &mut again) }, kResultFalse);
            }
            kResultOk
        }
        unsafe fn onFocus(&self, _s: TBool) -> tresult {
            kResultOk
        }
        unsafe fn setFrame(&self, frame: *mut IPlugFrame) -> tresult {
            self.frame.set((!frame.is_null()).then_some(frame));
            kResultOk
        }
        unsafe fn canResize(&self) -> tresult {
            kResultOk
        }
        unsafe fn checkSizeConstraint(&self, _r: *mut ViewRect) -> tresult {
            kResultOk
        }
    }

    #[test]
    fn plug_frame_resizes_and_tells_the_view() {
        let frame = ComWrapper::new(PlugFrame::new(None, (300, 200)));
        let view = ComWrapper::new(FakeView::default());
        let view_ptr = view.as_com_ref::<IPlugView>().unwrap().as_ptr();
        let frame_ref = frame.as_com_ref::<IPlugFrame>().unwrap();
        unsafe { view.setFrame(frame_ref.as_ptr()) };
        view.me.set(Some(view_ptr));

        let mut r = ViewRect { left: 10, top: 20, right: 650, bottom: 500 };
        assert_eq!(unsafe { frame_ref.resizeView(view_ptr, &mut r) }, kResultOk);
        assert_eq!(frame.size(), (640, 480), "the frame keeps the agreed size");
        assert_eq!(*view.sizes.borrow(), vec![(640, 480)], "the view is told once");

        let mut empty = rect((0, 0));
        assert_eq!(unsafe { frame_ref.resizeView(view_ptr, &mut empty) }, kInvalidArgument);
        assert_eq!(frame.size(), (640, 480));
        assert!(!frame.resizing.get());
    }

    #[test]
    fn component_handler_queues_edits() {
        use crate::host::ComponentHandler;
        use vst3::Steinberg::Vst::{IComponentHandler, IComponentHandlerTrait};
        let edits = Arc::new(EditQueue::default());
        let handler = ComWrapper::new(ComponentHandler { edits: edits.clone() });
        let h = handler.as_com_ref::<IComponentHandler>().unwrap();
        unsafe {
            h.beginEdit(3);
            h.performEdit(3, 0.25);
            h.performEdit(3, 1.5);
            h.endEdit(3);
            h.restartComponent(Vst::RestartFlags_::kParamValuesChanged);
            h.restartComponent(Vst::RestartFlags_::kLatencyChanged);
        }
        assert_eq!(edits.take(), vec![Edit::Begin(3), Edit::Perform(3, 0.25), Edit::Perform(3, 1.0), Edit::End(3), Edit::Reload]);
        assert!(edits.take().is_empty());
    }

    #[test]
    fn messages_carry_attributes() {
        use crate::host::HostApp;
        use vst3::Steinberg::Vst::*;
        let host = ComWrapper::new(HostApp);
        let mut obj = std::ptr::null_mut();
        let (mut cid, mut iid) = (IMessage_iid, IMessage_iid);
        assert_eq!(unsafe { host.createInstance(&mut cid, &mut iid, &mut obj) }, kResultOk);
        let msg = unsafe { ComPtr::from_raw(obj as *mut IMessage) }.unwrap();
        unsafe {
            msg.setMessageID(c"Spectrum".as_ptr());
            assert_eq!(std::ffi::CStr::from_ptr(msg.getMessageID()), c"Spectrum");
            let attrs = ComRef::from_raw(msg.getAttributes()).unwrap();
            attrs.setInt(c"n".as_ptr(), 42);
            attrs.setBinary(c"data".as_ptr(), [1u8, 2, 3].as_ptr().cast(), 3);
            let name: Vec<u16> = "Kick\0".encode_utf16().collect();
            attrs.setString(c"name".as_ptr(), name.as_ptr());
            let mut n = 0;
            assert_eq!(attrs.getInt(c"n".as_ptr(), &mut n), kResultOk);
            assert_eq!(n, 42);
            assert_eq!(attrs.getFloat(c"n".as_ptr(), &mut 0.0), kResultFalse, "typed");
            let (mut data, mut len) = (std::ptr::null(), 0);
            assert_eq!(attrs.getBinary(c"data".as_ptr(), &mut data, &mut len), kResultOk);
            assert_eq!(std::slice::from_raw_parts(data as *const u8, len as usize), [1, 2, 3]);
            let mut buf = [0u16; 3];
            assert_eq!(attrs.getString(c"name".as_ptr(), buf.as_mut_ptr(), 6), kResultOk);
            assert_eq!(String::from_utf16_lossy(&buf), "Ki\0", "cut to fit, terminated");
        }
        let (mut cid, mut iid) = (IAttributeList_iid, IAttributeList_iid);
        assert_eq!(unsafe { host.createInstance(&mut cid, &mut iid, &mut obj) }, kResultOk);
        drop(unsafe { ComPtr::from_raw(obj as *mut IAttributeList) }.unwrap());
        let (mut cid, mut iid) = (IPlugView_iid, IPlugView_iid);
        assert_eq!(unsafe { host.createInstance(&mut cid, &mut iid, &mut obj) }, kNotImplemented);
        assert!(obj.is_null());
    }
}
