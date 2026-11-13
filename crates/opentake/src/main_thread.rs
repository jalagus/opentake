//! Running engine work on the main thread while the GUI owns it.
//!
//! Some plug-ins must be created (and have their editors and many host calls made) on the
//! process's main thread: Qt-based ones such as Kontakt create a `QApplication` when they
//! are instantiated and abort when that happens on another thread. With the GUI running,
//! MCP tool calls therefore hop to the main thread (through its Grand Central Dispatch
//! queue, which the GUI's run loop drains) for anything that may instantiate a plug-in.
//! Headless, the main thread is busy running the tokio runtime, so work runs in place.
//!
//! Never call [`run`] while holding the engine lock: the main thread may be waiting for it.

use std::sync::atomic::{AtomicBool, Ordering};

static GUI_OWNS_MAIN: AtomicBool = AtomicBool::new(false);

/// Called by the GUI once its event loop owns the main thread.
pub fn enable() {
    GUI_OWNS_MAIN.store(true, Ordering::SeqCst);
}

/// Run `f` on the main thread when the GUI is running (blocking until it's done), else here.
pub fn run<R: Send>(f: impl FnOnce() -> R + Send) -> R {
    #[cfg(target_os = "macos")]
    if GUI_OWNS_MAIN.load(Ordering::SeqCst) {
        return dispatch2::run_on_main(|_| f());
    }
    f()
}
