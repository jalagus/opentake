//! Files the operating system asks OpenTake to open (a double-clicked `.otp`, `.rpp`,
//! `.gp`, `.mid`, `.dawproject`, …), queued for the GUI. On macOS documents arrive as an "open documents" Apple Event
//! rather than as command-line arguments, so a small handler catches those.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use eframe::egui;

static QUEUE: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
static WAKE: OnceLock<egui::Context> = OnceLock::new();

/// Queue a file to open and wake the GUI.
pub fn push(path: PathBuf) {
    if let Ok(mut q) = QUEUE.lock() {
        q.push(path);
    }
    if let Some(ctx) = WAKE.get() {
        ctx.request_repaint();
    }
}

/// Files waiting to be opened.
pub fn take() -> Vec<PathBuf> {
    QUEUE.lock().map(|mut q| std::mem::take(&mut *q)).unwrap_or_default()
}

/// Let `push` wake the GUI.
pub fn set_context(ctx: &egui::Context) {
    let _ = WAKE.set(ctx.clone());
}

/// Start listening for the OS's open-document requests (macOS; elsewhere files come in
/// as command-line arguments).
pub fn install() {
    #[cfg(target_os = "macos")]
    macos::install();
}

#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::{CStr, c_char};
    use std::path::PathBuf;

    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject, NSObject};
    use objc2::{AllocAnyThread, define_class, msg_send, sel};

    const CORE_EVENT_CLASS: u32 = u32::from_be_bytes(*b"aevt");
    const OPEN_DOCUMENTS: u32 = u32::from_be_bytes(*b"odoc");
    const DIRECT_OBJECT: u32 = u32::from_be_bytes(*b"----");

    define_class!(
        #[unsafe(super(NSObject))]
        #[name = "OpenTakeOpenDocumentsHandler"]
        struct OpenHandler;

        impl OpenHandler {
            #[unsafe(method(handleOpen:withReply:))]
            fn handle_open(&self, event: &AnyObject, _reply: &AnyObject) {
                for path in file_paths(event) {
                    super::push(path);
                }
            }
        }
    );

    impl OpenHandler {
        fn new() -> Retained<Self> {
            let this = Self::alloc().set_ivars(());
            unsafe { msg_send![super(this), init] }
        }
    }

    /// The file paths in an "open documents" event's direct-object list.
    fn file_paths(event: &AnyObject) -> Vec<PathBuf> {
        let mut out = Vec::new();
        unsafe {
            let list: Option<Retained<AnyObject>> = msg_send![event, paramDescriptorForKeyword: DIRECT_OBJECT];
            let Some(list) = list else { return out };
            let count: isize = msg_send![&*list, numberOfItems];
            for i in 1..=count {
                let item: Option<Retained<AnyObject>> = msg_send![&*list, descriptorAtIndex: i];
                let url: Option<Retained<AnyObject>> = match &item {
                    Some(item) => msg_send![&**item, fileURLValue],
                    None => None,
                };
                let path: Option<Retained<AnyObject>> = match &url {
                    Some(url) => msg_send![&**url, path],
                    None => None,
                };
                if let Some(path) = path {
                    let utf8: *const c_char = msg_send![&*path, UTF8String];
                    if !utf8.is_null() {
                        out.push(PathBuf::from(CStr::from_ptr(utf8).to_string_lossy().into_owned()));
                    }
                }
            }
        }
        out
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Builds the Apple Event macOS sends for a double-clicked file and checks the path
        /// comes out (registration itself is the standard NSAppleEventManager call).
        #[test]
        fn reads_paths_from_an_open_documents_event() {
            unsafe {
                let desc = AnyClass::get(c"NSAppleEventDescriptor").unwrap();
                let url_class = AnyClass::get(c"NSURL").unwrap();
                let string_class = AnyClass::get(c"NSString").unwrap();
                let list: Retained<AnyObject> = msg_send![desc, listDescriptor];
                for path in [c"/tmp/My Song.otp", c"/tmp/riff.gp"] {
                    let s: Retained<AnyObject> = msg_send![string_class, stringWithUTF8String: path.as_ptr()];
                    let url: Retained<AnyObject> = msg_send![url_class, fileURLWithPath: &*s];
                    let item: Retained<AnyObject> = msg_send![desc, descriptorWithFileURL: &*url];
                    let _: () = msg_send![&*list, insertDescriptor: &*item, atIndex: 0isize];
                }
                let event: Retained<AnyObject> = msg_send![
                    desc,
                    appleEventWithEventClass: CORE_EVENT_CLASS,
                    eventID: OPEN_DOCUMENTS,
                    targetDescriptor: std::ptr::null::<AnyObject>(),
                    returnID: -1i16,
                    transactionID: 0i32
                ];
                let _: () = msg_send![&*event, setParamDescriptor: &*list, forKeyword: DIRECT_OBJECT];
                let paths = file_paths(&event);
                assert_eq!(paths, vec![PathBuf::from("/tmp/My Song.otp"), PathBuf::from("/tmp/riff.gp")]);
            }
        }
    }

    /// Register for "open documents". Called before the app finishes launching and again
    /// once the GUI exists (AppKit may install its own handler in between).
    pub fn install() {
        let Some(class) = AnyClass::get(c"NSAppleEventManager") else { return };
        let handler = OpenHandler::new();
        unsafe {
            let manager: Option<Retained<AnyObject>> = msg_send![class, sharedAppleEventManager];
            if let Some(manager) = manager {
                let _: () = msg_send![
                    &*manager,
                    setEventHandler: &*handler,
                    andSelector: sel!(handleOpen:withReply:),
                    forEventClass: CORE_EVENT_CLASS,
                    andEventID: OPEN_DOCUMENTS
                ];
            }
        }
        // The event manager doesn't retain its handlers; this one lives as long as the app.
        std::mem::forget(handler);
    }
}
