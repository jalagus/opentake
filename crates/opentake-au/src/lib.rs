//! Audio Unit hosting on macOS: listing installed effects and instruments, loading them
//! through the AudioComponent API (AUv2, and AUv3 extensions via the same API),
//! parameters, state and realtime processing. On other platforms this crate is empty.
#![cfg(target_os = "macos")]

mod editor;
mod plugin;

use std::ptr::NonNull;

use objc2_audio_toolbox::*;
use objc2_core_foundation::{CFRetained, CFString};
use serde::Serialize;

pub use editor::{AuEditor, ParamListener, pump_events};
pub use plugin::{AuPlugin, AuProcessor, NoteEvent, ParamInfo, Transport};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("no audio unit '{0}' is installed")]
    NotFound(String),
    #[error("{0} failed (OSStatus {1})")]
    Call(&'static str, i32),
    #[error("{0}")]
    Other(&'static str),
    #[error("editor window: {0}")]
    Window(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// The component types we host.
const TYPES: [u32; 3] = [kAudioUnitType_Effect, kAudioUnitType_MusicEffect, kAudioUnitType_MusicDevice];

/// An installed audio unit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ComponentInfo {
    pub name: String,
    pub manufacturer: String,
    /// Type, subtype and manufacturer codes, e.g. "aufx:dely:appl".
    pub id: String,
    /// A music device (takes notes, has no audio input).
    pub instrument: bool,
    /// An AUv3 app extension.
    pub v3: bool,
}

impl ComponentInfo {
    /// "AU: Name (Manufacturer)", or "AUi: ..." for instruments (as Reaper names them).
    pub fn display_name(&self) -> String {
        format!("{}: {} ({})", if self.instrument { "AUi" } else { "AU" }, self.name, self.manufacturer)
    }

    /// The name the system reports, "Manufacturer: Name".
    pub fn component_name(&self) -> String {
        format!("{}: {}", self.manufacturer, self.name)
    }

    fn description(&self) -> Option<AudioComponentDescription> {
        let codes = parse_id(&self.id)?;
        Some(AudioComponentDescription {
            componentType: codes[0],
            componentSubType: codes[1],
            componentManufacturer: codes[2],
            componentFlags: 0,
            componentFlagsMask: 0,
        })
    }
}

/// Held around AudioToolbox calls that create, initialize, tear down or look up units.
/// Doing those on several threads at once can crash inside CoreAudio (seen disposing one
/// unit while another thread's first lookup ran the component registry's set-up), so they
/// take turns; none of them happen on the audio thread.
pub(crate) fn lifecycle() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

/// Every installed effect and instrument, sorted by name. Nothing is loaded.
pub fn scan() -> Vec<ComponentInfo> {
    let _turn = lifecycle();
    let mut list = Vec::new();
    for ty in TYPES {
        let mut desc = AudioComponentDescription { componentType: ty, componentSubType: 0, componentManufacturer: 0, componentFlags: 0, componentFlagsMask: 0 };
        let mut comp: AudioComponent = std::ptr::null_mut();
        loop {
            // SAFETY: `desc` is valid; `comp` is null or the previous result of this search.
            comp = unsafe { AudioComponentFindNext(comp, NonNull::from(&mut desc)) };
            if comp.is_null() {
                break;
            }
            if let Some(info) = unsafe { component_info(comp) } {
                list.push(info);
            }
        }
    }
    list.sort_by_key(|c| (c.name.to_lowercase(), c.manufacturer.to_lowercase()));
    list
}

/// Look up an installed audio unit by its id ("aufx:dely:appl").
pub fn find(id: &str) -> Option<ComponentInfo> {
    let comp = find_component(id)?;
    unsafe { component_info(comp) }
}

fn find_component(id: &str) -> Option<AudioComponent> {
    let _turn = lifecycle();
    let probe = ComponentInfo { name: String::new(), manufacturer: String::new(), id: id.into(), instrument: false, v3: false };
    let mut desc = probe.description()?;
    let comp = unsafe { AudioComponentFindNext(std::ptr::null_mut(), NonNull::from(&mut desc)) };
    (!comp.is_null()).then_some(comp)
}

/// # Safety
/// `comp` must be a component returned by `AudioComponentFindNext`.
unsafe fn component_info(comp: AudioComponent) -> Option<ComponentInfo> {
    let mut desc: AudioComponentDescription = unsafe { std::mem::zeroed() };
    if unsafe { AudioComponentGetDescription(comp, NonNull::from(&mut desc)) } != 0 {
        return None;
    }
    let mut name: *const CFString = std::ptr::null();
    let full = if unsafe { AudioComponentCopyName(comp, NonNull::from(&mut name)) } == 0 {
        // SAFETY: a "Copy" function returns a +1 reference we now own.
        NonNull::new(name as *mut CFString).map(|n| unsafe { CFRetained::from_raw(n) }.to_string()).unwrap_or_default()
    } else {
        String::new()
    };
    let (manufacturer, name) = match full.split_once(": ") {
        Some((m, n)) => (m.trim().to_string(), n.trim().to_string()),
        None => (fourcc(desc.componentManufacturer), full.clone()),
    };
    Some(ComponentInfo {
        name: if name.is_empty() { fourcc(desc.componentSubType) } else { name },
        manufacturer,
        id: format!("{}:{}:{}", fourcc(desc.componentType), fourcc(desc.componentSubType), fourcc(desc.componentManufacturer)),
        instrument: desc.componentType == kAudioUnitType_MusicDevice,
        v3: desc.componentFlags & AudioComponentFlags::IsV3AudioUnit.bits() != 0,
    })
}

/// A four-character code as text. Bytes map 1:1 to chars so codes round-trip.
pub fn fourcc(code: u32) -> String {
    code.to_be_bytes().iter().map(|&b| b as char).collect()
}

/// "aufx:dely:appl" -> the three codes. Codes are exactly four chars, so one may even
/// contain ':' or spaces.
pub fn parse_id(id: &str) -> Option<[u32; 3]> {
    let chars: Vec<char> = id.chars().collect();
    if chars.len() != 14 || chars[4] != ':' || chars[9] != ':' {
        return None;
    }
    let code = |at: usize| -> Option<u32> {
        let mut v = 0u32;
        for &c in &chars[at..at + 4] {
            v = (v << 8) | u8::try_from(c as u32).ok()? as u32;
        }
        Some(v)
    };
    Some([code(0)?, code(5)?, code(10)?])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip() {
        let id = format!("{}:{}:{}", fourcc(kAudioUnitType_MusicDevice), fourcc(0x646c7320), fourcc(0x6170706c));
        assert_eq!(id, "aumu:dls :appl");
        assert_eq!(parse_id(&id), Some([kAudioUnitType_MusicDevice, 0x646c7320, 0x6170706c]));
        assert_eq!(parse_id("aufx:dely"), None);
    }

    #[test]
    fn scan_lists_apple_units() {
        let list = scan();
        let delay = list.iter().find(|c| c.id == "aufx:dely:appl").expect("AUDelay is built in");
        assert_eq!((delay.name.as_str(), delay.manufacturer.as_str(), delay.instrument), ("AUDelay", "Apple", false));
        assert_eq!(delay.display_name(), "AU: AUDelay (Apple)");
        assert!(list.iter().any(|c| c.instrument && c.name == "DLSMusicDevice"));
        assert_eq!(find("aufx:dely:appl").as_ref(), Some(delay));
    }
}
