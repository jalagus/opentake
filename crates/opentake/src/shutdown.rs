//! Releasing audio and plug-ins before the process exits.
//!
//! Plug-ins must be shut down by the host (instances released, then the module's
//! `bundleExit`) before `exit()` runs their static destructors, or plug-ins with their own
//! threads (Kontakt) crash on quit. The GUI quits two ways: closing the window returns from
//! the event loop, and Quit (⌘Q, the Dock, logging out) goes through
//! `-[NSApplication terminate:]`, which calls `exit()` without returning. Both end here.

use std::sync::{Mutex, OnceLock};
use std::thread::JoinHandle;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::server::SharedEngine;

static ENGINE: OnceLock<SharedEngine> = OnceLock::new();
static DONE: AtomicBool = AtomicBool::new(false);
/// Files being written in the background (saves): quitting waits for them.
static WRITING: Mutex<Vec<JoinHandle<()>>> = Mutex::new(Vec::new());

/// Keep the process alive for `writer` when quitting.
pub fn wait_for(writer: JoinHandle<()>) {
    let mut writing = WRITING.lock().unwrap_or_else(|p| p.into_inner());
    writing.retain(|w| !w.is_finished());
    writing.push(writer);
}

/// Wait until background writes are done.
pub fn finish_writing() {
    let writing = std::mem::take(&mut *WRITING.lock().unwrap_or_else(|p| p.into_inner()));
    for w in writing {
        let _ = w.join();
    }
}

/// Shut the engine down when the app terminates (call on the main thread, once the GUI runs).
pub fn install(engine: &SharedEngine) {
    let _ = ENGINE.set(engine.clone());
    #[cfg(target_os = "macos")]
    macos::observe_will_terminate();
}

/// Release audio and plug-ins (once; later calls do nothing). Main thread.
pub fn run() {
    finish_writing();
    if DONE.swap(true, Ordering::SeqCst) {
        return;
    }
    let Some(engine) = ENGINE.get() else { return };
    let mut e = engine.lock().unwrap_or_else(|p| p.into_inner());
    e.shutdown();
    tracing::info!("audio and plug-ins shut down");
}

#[cfg(target_os = "macos")]
mod macos {
    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject, NSObject};
    use objc2::{AllocAnyThread, define_class, msg_send, sel};

    define_class!(
        #[unsafe(super(NSObject))]
        #[name = "OpenTakeTerminationObserver"]
        struct Observer;

        impl Observer {
            #[unsafe(method(willTerminate:))]
            fn will_terminate(&self, _note: &AnyObject) {
                super::run();
            }
        }
    );

    impl Observer {
        fn new() -> Retained<Self> {
            let this = Self::alloc().set_ivars(());
            unsafe { msg_send![super(this), init] }
        }
    }

    /// Observe NSApplicationWillTerminateNotification, posted by `terminate:` just before
    /// it calls `exit()`.
    pub fn observe_will_terminate() {
        let (Some(center_class), Some(string_class)) = (AnyClass::get(c"NSNotificationCenter"), AnyClass::get(c"NSString")) else { return };
        unsafe {
            let center: Retained<AnyObject> = msg_send![center_class, defaultCenter];
            let name: Retained<AnyObject> = msg_send![string_class, stringWithUTF8String: c"NSApplicationWillTerminateNotification".as_ptr()];
            // The center doesn't retain observers; this one lives as long as the process.
            let observer = Observer::new();
            let _: () = msg_send![
                &*center,
                addObserver: &*observer,
                selector: sel!(willTerminate:),
                name: &*name,
                object: std::ptr::null::<AnyObject>()
            ];
            std::mem::forget(observer);
        }
    }
}
