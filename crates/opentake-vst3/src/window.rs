//! Native top-level windows that host plug-in editors (VST3 views, and Audio Unit views
//! from `opentake-au`), separate from the egui window: an `NSWindow` on macOS, an `HWND`
//! on Windows. Elsewhere [`NativeWindow::new`] fails and only the generic editor exists.
//!
//! Windows must be created, used and dropped on the main (GUI) thread; the macOS version
//! checks that. Sizes are in the units the plug-in APIs use for views: points on macOS,
//! pixels on Windows.

pub use imp::{NativeWindow, pump_events};

/// The VST3 platform type a [`NativeWindow::parent`] handle is, if editors are supported.
pub fn platform_type() -> Option<&'static std::ffi::CStr> {
    if cfg!(target_os = "macos") {
        Some(c"NSView")
    } else if cfg!(windows) {
        Some(c"HWND")
    } else {
        None
    }
}

/// Whether this thread may create editor windows (on Windows, any thread with a message
/// loop could; OpenTake uses its GUI thread).
pub fn is_main_thread() -> bool {
    #[cfg(target_os = "macos")]
    return objc2::MainThreadMarker::new().is_some();
    #[cfg(not(target_os = "macos"))]
    cfg!(windows)
}

#[cfg(target_os = "macos")]
mod imp {
    use std::cell::Cell;
    use std::ffi::c_void;

    use objc2::rc::Retained;
    use objc2::runtime::ProtocolObject;
    use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
    use objc2_app_kit::{NSBackingStoreType, NSFloatingWindowLevel, NSView, NSWindow, NSWindowDelegate, NSWindowStyleMask};
    use objc2_foundation::{NSNotification, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString};

    use crate::{Error, Result};

    define_class!(
        /// Notes when the user closes the window (the editor is then torn down by its owner).
        #[unsafe(super(NSObject))]
        #[thread_kind = MainThreadOnly]
        #[name = "OpenTakeEditorWindowDelegate"]
        #[ivars = Cell<bool>]
        struct Delegate;

        unsafe impl NSObjectProtocol for Delegate {}

        unsafe impl NSWindowDelegate for Delegate {
            #[unsafe(method(windowWillClose:))]
            fn window_will_close(&self, _notification: &NSNotification) {
                self.ivars().set(true);
            }
        }
    );

    impl Delegate {
        fn new(mtm: MainThreadMarker) -> Retained<Self> {
            let this = Self::alloc(mtm).set_ivars(Cell::new(false));
            unsafe { msg_send![super(this), init] }
        }
    }

    pub struct NativeWindow {
        window: Retained<NSWindow>,
        delegate: Retained<Delegate>,
    }

    impl NativeWindow {
        /// A titled, closable window floating above the app's windows (and hidden while
        /// another app is active, like Logic's plug-in windows), shown centred.
        pub fn new(title: &str, size: (f64, f64), resizable: bool) -> Result<Self> {
            let mtm = MainThreadMarker::new().ok_or(Error::Unsupported("plug-in editors open on the main thread only"))?;
            let mut style = NSWindowStyleMask::Titled | NSWindowStyleMask::Closable | NSWindowStyleMask::Miniaturizable;
            if resizable {
                style |= NSWindowStyleMask::Resizable;
            }
            let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(size.0.max(1.0), size.1.max(1.0)));
            // SAFETY: a fresh allocation on the main thread; not released on close, since we
            // own it (and drop it after closing).
            let window = unsafe { NSWindow::initWithContentRect_styleMask_backing_defer(NSWindow::alloc(mtm), rect, style, NSBackingStoreType::Buffered, false) };
            unsafe { window.setReleasedWhenClosed(false) };
            let delegate = Delegate::new(mtm);
            window.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
            window.setTitle(&NSString::from_str(title));
            window.setLevel(NSFloatingWindowLevel);
            window.setHidesOnDeactivate(true);
            window.center();
            window.makeKeyAndOrderFront(None);
            Ok(Self { window, delegate })
        }

        /// The content `NSView*` to attach a plug-in view to.
        pub fn parent(&self) -> *mut c_void {
            self.window.contentView().map_or(std::ptr::null_mut(), |v| Retained::as_ptr(&v) as *mut c_void)
        }

        /// The content view, for embedding views directly.
        pub fn content_view(&self) -> Option<Retained<NSView>> {
            self.window.contentView()
        }

        pub fn content_size(&self) -> (f64, f64) {
            let size = self.window.contentView().map_or(NSSize::new(0.0, 0.0), |v| v.frame().size);
            (size.width, size.height)
        }

        /// Resize the content area, keeping the top-left corner where it is.
        pub fn set_content_size(&self, width: f64, height: f64) {
            let frame = self.window.frame();
            let top_left = NSPoint::new(frame.origin.x, frame.origin.y + frame.size.height);
            self.window.setContentSize(NSSize::new(width.max(1.0), height.max(1.0)));
            self.window.setFrameTopLeftPoint(top_left);
        }

        /// Views on macOS work in points: plug-ins scale themselves.
        pub fn scale(&self) -> f64 {
            1.0
        }

        /// False once the user closed the window.
        pub fn is_open(&self) -> bool {
            !self.delegate.ivars().get()
        }

        pub fn set_title(&self, title: &str) {
            self.window.setTitle(&NSString::from_str(title));
        }

        pub fn bring_to_front(&self) {
            self.window.makeKeyAndOrderFront(None);
        }
    }

    impl Drop for NativeWindow {
        fn drop(&mut self) {
            self.window.setDelegate(None);
            if self.is_open() {
                self.window.close();
            }
        }
    }

    impl NativeWindow {
        /// The window server's number for the window (`screencapture -l`).
        pub fn window_id(&self) -> u64 {
            self.window.windowNumber() as u64
        }
    }

    /// Handle window events for `timeout`, for programs without an event loop of their own
    /// (the `open_editor` example); the first call makes the process a regular app.
    pub fn pump_events(timeout: std::time::Duration) {
        use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSEventMask};
        use objc2_foundation::{NSDate, NSDefaultRunLoopMode};
        let Some(mtm) = MainThreadMarker::new() else { return std::thread::sleep(timeout) };
        let app = NSApplication::sharedApplication(mtm);
        thread_local!(static LAUNCHED: Cell<bool> = const { Cell::new(false) });
        if !app.isRunning() && !LAUNCHED.replace(true) {
            app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
            app.finishLaunching();
            #[allow(deprecated)]
            app.activateIgnoringOtherApps(true);
        }
        let start = std::time::Instant::now();
        while start.elapsed() < timeout {
            let until = NSDate::dateWithTimeIntervalSinceNow(0.01);
            // SAFETY: the default mode is a valid run loop mode.
            while let Some(event) = unsafe { app.nextEventMatchingMask_untilDate_inMode_dequeue(NSEventMask::Any, Some(&until), NSDefaultRunLoopMode, true) } {
                app.sendEvent(&event);
            }
            app.updateWindows();
        }
    }
}

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use std::sync::Once;

    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetActiveWindow, SetActiveWindow};
    use windows_sys::Win32::UI::WindowsAndMessaging::*;

    use crate::{Error, Result};

    const CLASS: &str = "OpenTakePluginEditor\0";

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    /// Closing only hides the window: its owner notices (`is_open`) and tears the editor
    /// down, detaching the plug-in's view before the window is destroyed.
    unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        unsafe {
            if msg == WM_CLOSE {
                ShowWindow(hwnd, SW_HIDE);
                return 0;
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
    }

    pub struct NativeWindow {
        hwnd: HWND,
        style: WINDOW_STYLE,
    }

    impl NativeWindow {
        /// A window owned by the active (main) window, so it stays above it, shown.
        pub fn new(title: &str, size: (f64, f64), resizable: bool) -> Result<Self> {
            static REGISTER: Once = Once::new();
            let class = wide(CLASS);
            // SAFETY: plain Win32 calls on the GUI thread with valid, NUL-terminated strings.
            unsafe {
                let instance = GetModuleHandleW(std::ptr::null());
                REGISTER.call_once(|| {
                    let wc = WNDCLASSW {
                        style: CS_HREDRAW | CS_VREDRAW,
                        lpfnWndProc: Some(wnd_proc),
                        cbClsExtra: 0,
                        cbWndExtra: 0,
                        hInstance: instance,
                        hIcon: std::ptr::null_mut(),
                        hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
                        hbrBackground: std::ptr::null_mut(),
                        lpszMenuName: std::ptr::null(),
                        lpszClassName: class.as_ptr(),
                    };
                    RegisterClassW(&wc);
                });
                let mut style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_CLIPCHILDREN;
                if resizable {
                    style |= WS_THICKFRAME | WS_MAXIMIZEBOX;
                }
                let (w, h) = outer_size(style, size.0 as i32, size.1 as i32);
                let title = wide(title);
                let hwnd = CreateWindowExW(0, class.as_ptr(), title.as_ptr(), style, CW_USEDEFAULT, CW_USEDEFAULT, w, h, GetActiveWindow(), std::ptr::null_mut(), instance, std::ptr::null());
                if hwnd.is_null() {
                    return Err(Error::Unsupported("could not create the editor window"));
                }
                ShowWindow(hwnd, SW_SHOW);
                Ok(Self { hwnd, style })
            }
        }

        pub fn parent(&self) -> *mut c_void {
            self.hwnd
        }

        pub fn content_size(&self) -> (f64, f64) {
            let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
            unsafe { GetClientRect(self.hwnd, &mut r) };
            ((r.right - r.left) as f64, (r.bottom - r.top) as f64)
        }

        pub fn set_content_size(&self, width: f64, height: f64) {
            let (w, h) = outer_size(self.style, width as i32, height as i32);
            unsafe { SetWindowPos(self.hwnd, std::ptr::null_mut(), 0, 0, w, h, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE) };
        }

        /// The window's DPI scale (views on Windows work in pixels).
        pub fn scale(&self) -> f64 {
            match unsafe { GetDpiForWindow(self.hwnd) } {
                0 => 1.0,
                dpi => dpi as f64 / 96.0,
            }
        }

        pub fn is_open(&self) -> bool {
            unsafe { IsWindowVisible(self.hwnd) != 0 }
        }

        pub fn set_title(&self, title: &str) {
            let title = wide(title);
            unsafe { SetWindowTextW(self.hwnd, title.as_ptr()) };
        }

        pub fn bring_to_front(&self) {
            unsafe {
                ShowWindow(self.hwnd, SW_SHOWNORMAL);
                SetActiveWindow(self.hwnd);
            }
        }
    }

    /// The window size for a client area of `w` x `h`.
    fn outer_size(style: WINDOW_STYLE, w: i32, h: i32) -> (i32, i32) {
        let mut r = RECT { left: 0, top: 0, right: w.max(1), bottom: h.max(1) };
        unsafe { AdjustWindowRectEx(&mut r, style, 0, 0) };
        (r.right - r.left, r.bottom - r.top)
    }

    impl Drop for NativeWindow {
        fn drop(&mut self) {
            unsafe { DestroyWindow(self.hwnd) };
        }
    }

    impl NativeWindow {
        pub fn window_id(&self) -> u64 {
            self.hwnd as u64
        }
    }

    /// Handle window messages for `timeout`, for programs without a message loop of their own.
    pub fn pump_events(timeout: std::time::Duration) {
        let start = std::time::Instant::now();
        while start.elapsed() < timeout {
            // SAFETY: a plain message loop on this thread.
            unsafe {
                let mut msg: MSG = std::mem::zeroed();
                while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}

/// No native editor windows here yet (X11 embedding needs the host's `IRunLoop`).
#[cfg(not(any(target_os = "macos", windows)))]
mod imp {
    use std::ffi::c_void;

    use crate::{Error, Result};

    pub struct NativeWindow(());

    impl NativeWindow {
        pub fn new(_title: &str, _size: (f64, f64), _resizable: bool) -> Result<Self> {
            Err(Error::Unsupported("plug-in editor windows on this platform"))
        }

        pub fn parent(&self) -> *mut c_void {
            std::ptr::null_mut()
        }

        pub fn content_size(&self) -> (f64, f64) {
            (0.0, 0.0)
        }

        pub fn set_content_size(&self, _width: f64, _height: f64) {}

        pub fn scale(&self) -> f64 {
            1.0
        }

        pub fn is_open(&self) -> bool {
            false
        }

        pub fn set_title(&self, _title: &str) {}

        pub fn bring_to_front(&self) {}

        pub fn window_id(&self) -> u64 {
            0
        }
    }

    pub fn pump_events(timeout: std::time::Duration) {
        std::thread::sleep(timeout);
    }
}
