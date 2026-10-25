//! Loading a VST3 binary and reading its plugin factory.

use std::ffi::c_char;
#[cfg(not(target_os = "windows"))]
use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use libloading::Library;
use serde::Serialize;
use vst3::ComPtr;
use vst3::Steinberg::*;

use crate::{Error, Result, scan};

pub const AUDIO_MODULE_CLASS: &str = "Audio Module Class";

type GetPluginFactoryFn = unsafe extern "system" fn() -> *mut IPluginFactory;
type ExitFn = unsafe extern "system" fn() -> bool;

/// A class exported by a plugin factory.
#[derive(Debug, Clone, Serialize)]
pub struct ClassInfo {
    pub name: String,
    pub vendor: String,
    pub category: String,
    pub sub_categories: String,
    pub version: String,
    /// 32 hex digits.
    pub cid: String,
    #[serde(skip)]
    pub(crate) tuid: TUID,
}

impl ClassInfo {
    /// VST3 instruments advertise "Instrument" in their sub-categories.
    pub fn is_instrument(&self) -> bool {
        self.sub_categories.split('|').any(|c| c.eq_ignore_ascii_case("Instrument"))
    }
}

/// A loaded plugin binary. Kept alive (via `Arc`) by every instance created from it.
pub struct Module {
    pub path: PathBuf,
    pub vendor: String,
    factory: ManuallyDrop<ComPtr<IPluginFactory>>,
    exit: Option<ExitFn>,
    #[cfg(target_os = "macos")]
    bundle_ref: *mut c_void,
    _lib: Library,
}

// The factory is documented as thread-safe for class enumeration and instantiation.
unsafe impl Send for Module {}
unsafe impl Sync for Module {}

impl Module {
    pub fn load(bundle: &Path) -> Result<Arc<Self>> {
        let binary = scan::bundle_binary(bundle).ok_or_else(|| Error::BadBundle(bundle.to_path_buf()))?;
        unsafe {
            let lib = Library::new(&binary)?;

            #[cfg(target_os = "macos")]
            let bundle_ref = macos::create_bundle(bundle);
            let exit = Self::call_entry(&lib, #[cfg(target_os = "macos")] bundle_ref)?;

            let get_factory: libloading::Symbol<GetPluginFactoryFn> = lib.get(b"GetPluginFactory\0")?;
            let factory = ComPtr::from_raw(get_factory()).ok_or(Error::NoFactory)?;

            let mut info: PFactoryInfo = std::mem::zeroed();
            let vendor = if factory.getFactoryInfo(&mut info) == kResultOk {
                cstr(&info.vendor)
            } else {
                String::new()
            };

            Ok(Arc::new(Self {
                path: bundle.to_path_buf(),
                vendor,
                factory: ManuallyDrop::new(factory),
                exit,
                #[cfg(target_os = "macos")]
                bundle_ref,
                _lib: lib,
            }))
        }
    }

    /// Calls the platform module-entry function and returns the matching exit function.
    unsafe fn call_entry(lib: &Library, #[cfg(target_os = "macos")] bundle_ref: *mut c_void) -> Result<Option<ExitFn>> {
        unsafe {
            #[cfg(target_os = "macos")]
            let (entries, exits, arg): (&[&[u8]], &[&[u8]], *mut c_void) =
                (&[b"bundleEntry\0", b"BundleEntry\0"], &[b"bundleExit\0", b"BundleExit\0"], bundle_ref);
            #[cfg(target_os = "linux")]
            let (entries, exits, arg): (&[&[u8]], &[&[u8]], *mut c_void) =
                (&[b"ModuleEntry\0"], &[b"ModuleExit\0"], std::ptr::null_mut());

            #[cfg(any(target_os = "macos", target_os = "linux"))]
            {
                type EntryFn = unsafe extern "system" fn(*mut c_void) -> bool;
                if let Some(entry) = entries.iter().find_map(|n| lib.get::<EntryFn>(*n).ok()) {
                    if !entry(arg) {
                        return Err(Error::EntryFailed);
                    }
                }
                Ok(exits.iter().find_map(|n| lib.get::<ExitFn>(*n).ok().map(|f| *f)))
            }

            #[cfg(target_os = "windows")]
            {
                type InitFn = unsafe extern "system" fn() -> bool;
                if let Ok(init) = lib.get::<InitFn>(b"InitDll\0") {
                    if !init() {
                        return Err(Error::EntryFailed);
                    }
                }
                Ok(lib.get::<ExitFn>(b"ExitDll\0").ok().map(|f| *f))
            }
        }
    }

    pub fn factory(&self) -> &ComPtr<IPluginFactory> {
        &self.factory
    }

    pub fn classes(&self) -> Vec<ClassInfo> {
        let mut out = Vec::new();
        unsafe {
            let f2: Option<ComPtr<IPluginFactory2>> = self.factory.cast();
            for i in 0..self.factory.countClasses() {
                if let Some(f2) = &f2 {
                    let mut info: PClassInfo2 = std::mem::zeroed();
                    if f2.getClassInfo2(i, &mut info) == kResultOk {
                        let vendor = cstr(&info.vendor);
                        out.push(ClassInfo {
                            name: cstr(&info.name),
                            vendor: if vendor.is_empty() { self.vendor.clone() } else { vendor },
                            category: cstr(&info.category),
                            sub_categories: cstr(&info.subCategories),
                            version: cstr(&info.version),
                            cid: hex(&info.cid),
                            tuid: info.cid,
                        });
                        continue;
                    }
                }
                let mut info: PClassInfo = std::mem::zeroed();
                if self.factory.getClassInfo(i, &mut info) == kResultOk {
                    out.push(ClassInfo {
                        name: cstr(&info.name),
                        vendor: self.vendor.clone(),
                        category: cstr(&info.category),
                        sub_categories: String::new(),
                        version: String::new(),
                        cid: hex(&info.cid),
                        tuid: info.cid,
                    });
                }
            }
        }
        out
    }

    /// Audio effect / instrument classes (the ones a host instantiates).
    pub fn audio_classes(&self) -> Vec<ClassInfo> {
        self.classes().into_iter().filter(|c| c.category == AUDIO_MODULE_CLASS).collect()
    }
}

impl Drop for Module {
    fn drop(&mut self) {
        unsafe {
            ManuallyDrop::drop(&mut self.factory);
            if let Some(exit) = self.exit {
                exit();
            }
            #[cfg(target_os = "macos")]
            if !self.bundle_ref.is_null() {
                macos::CFRelease(self.bundle_ref);
            }
        }
    }
}

pub(crate) fn cstr(chars: &[c_char]) -> String {
    let bytes: Vec<u8> = chars.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

pub(crate) fn hex(tuid: &TUID) -> String {
    tuid.iter().map(|&b| format!("{:02X}", b as u8)).collect()
}

#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::c_void;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFURLCreateFromFileSystemRepresentation(
            allocator: *const c_void,
            buffer: *const u8,
            len: isize,
            is_directory: u8,
        ) -> *mut c_void;
        fn CFBundleCreate(allocator: *const c_void, url: *const c_void) -> *mut c_void;
        pub fn CFRelease(cf: *const c_void);
    }

    /// `bundleEntry` expects a `CFBundleRef` for the plugin bundle (used to find resources).
    pub unsafe fn create_bundle(path: &Path) -> *mut c_void {
        unsafe {
            let bytes = path.as_os_str().as_bytes();
            let url = CFURLCreateFromFileSystemRepresentation(std::ptr::null(), bytes.as_ptr(), bytes.len() as isize, 1);
            if url.is_null() {
                return std::ptr::null_mut();
            }
            let bundle = CFBundleCreate(std::ptr::null(), url);
            CFRelease(url);
            bundle
        }
    }
}
