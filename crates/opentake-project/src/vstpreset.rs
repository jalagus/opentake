//! VST3 plug-in state as Steinberg's `.vstpreset` file, the format a VST3 slot's
//! [`PluginSlot::state`](crate::PluginSlot::state) holds (base64, in lines) and DAWproject
//! embeds as is. Version 1 of the format, little-endian throughout:
//!
//! ```text
//!   0  "VST3"                     magic
//!   4  i32 version (1)
//!   8  32 ASCII hex chars         the plug-in's class id
//!  40  i64 offset of the chunk list
//!  48  chunk data ...
//!      "List" i32 count, then per chunk: [u8; 4] id, i64 offset, i64 size
//! ```
//!
//! "Comp" holds the component's state (`IComponent::getState`), "Cont" the edit
//! controller's; other chunks ("Info" metadata, ...) are kept as they are.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

const MAGIC: &[u8; 4] = b"VST3";
const VERSION: i32 = 1;
const HEADER: usize = 48;
pub const COMPONENT: [u8; 4] = *b"Comp";
pub const CONTROLLER: [u8; 4] = *b"Cont";

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum PresetError {
    #[error("not a VST3 preset")]
    NotPreset,
    #[error("damaged VST3 preset: {0}")]
    Damaged(&'static str),
    #[error("VST3 preset without the plug-in's state")]
    NoComponent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VstPreset {
    /// The class id as 32 hex characters (as `PluginSlot::uid` has it for VST3).
    pub class_id: String,
    /// (id, data) in file order.
    pub chunks: Vec<([u8; 4], Vec<u8>)>,
}

impl VstPreset {
    pub fn new(class_id: &str, component: Vec<u8>, controller: Option<Vec<u8>>) -> Self {
        let mut chunks = vec![(COMPONENT, component)];
        chunks.extend(controller.map(|c| (CONTROLLER, c)));
        Self { class_id: class_id.to_string(), chunks }
    }

    pub fn chunk(&self, id: [u8; 4]) -> Option<&[u8]> {
        self.chunks.iter().find(|c| c.0 == id).map(|c| c.1.as_slice())
    }

    /// The component's state; every preset this module reads has one.
    pub fn component(&self) -> &[u8] {
        self.chunk(COMPONENT).unwrap_or_default()
    }

    pub fn controller(&self) -> Option<&[u8]> {
        self.chunk(CONTROLLER)
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER + self.chunks.iter().map(|c| c.1.len() + 20).sum::<usize>() + 8);
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&VERSION.to_le_bytes());
        // Exactly 32 characters: padded or cut if the id isn't a proper one.
        let mut id = [b'0'; 32];
        for (d, s) in id.iter_mut().zip(self.class_id.bytes()) {
            *d = s;
        }
        out.extend_from_slice(&id);
        out.extend_from_slice(&[0; 8]); // the list offset, patched below
        let mut list = Vec::new();
        for (cid, data) in &self.chunks {
            list.push((*cid, out.len() as i64, data.len() as i64));
            out.extend_from_slice(data);
        }
        let at = out.len() as i64;
        out[40..48].copy_from_slice(&at.to_le_bytes());
        out.extend_from_slice(b"List");
        out.extend_from_slice(&(list.len() as i32).to_le_bytes());
        for (cid, offset, size) in list {
            out.extend_from_slice(&cid);
            out.extend_from_slice(&offset.to_le_bytes());
            out.extend_from_slice(&size.to_le_bytes());
        }
        out
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, PresetError> {
        if bytes.len() < HEADER || &bytes[..4] != MAGIC {
            return Err(PresetError::NotPreset);
        }
        let i64_at = |at: usize| bytes.get(at..at + 8).map(|b| i64::from_le_bytes(b.try_into().expect("8 bytes")));
        let class_id = String::from_utf8_lossy(&bytes[8..40]).trim_end_matches('\0').to_string();
        let list = usize::try_from(i64_at(40).unwrap_or(-1)).map_err(|_| PresetError::Damaged("chunk list offset"))?;
        if bytes.get(list..list + 4) != Some(b"List") {
            return Err(PresetError::Damaged("no chunk list"));
        }
        let count = bytes.get(list + 4..list + 8).map(|b| i32::from_le_bytes(b.try_into().expect("4 bytes"))).ok_or(PresetError::Damaged("chunk list"))?;
        let mut chunks = Vec::new();
        for i in 0..count.max(0) as usize {
            let at = list + 8 + i * 20;
            let id: [u8; 4] = bytes.get(at..at + 4).ok_or(PresetError::Damaged("chunk list"))?.try_into().expect("4 bytes");
            let (offset, size) = (i64_at(at + 4), i64_at(at + 12));
            let range = match (offset.and_then(|o| usize::try_from(o).ok()), size.and_then(|s| usize::try_from(s).ok())) {
                (Some(o), Some(s)) if o.checked_add(s).is_some_and(|end| end <= bytes.len()) => o..o + s,
                _ => return Err(PresetError::Damaged("chunk outside the file")),
            };
            chunks.push((id, bytes[range].to_vec()));
        }
        if !chunks.iter().any(|c| c.0 == COMPONENT) {
            return Err(PresetError::NoComponent);
        }
        Ok(Self { class_id, chunks })
    }

    /// As `PluginSlot::state` stores it.
    pub fn to_lines(&self) -> Vec<String> {
        encode_lines(&self.to_bytes())
    }

    /// From `PluginSlot::state`; `NotPreset` for other content (e.g. a Reaper chunk this
    /// host couldn't read).
    pub fn from_lines(lines: &[String]) -> Result<Self, PresetError> {
        Self::from_bytes(&decode_lines(lines).ok_or(PresetError::NotPreset)?)
    }
}

/// Base64 in lines of 128 characters (as Reaper stores plug-in state).
pub fn encode_lines(bytes: &[u8]) -> Vec<String> {
    STANDARD.encode(bytes).as_bytes().chunks(128).map(|c| String::from_utf8_lossy(c).into_owned()).collect()
}

pub fn decode_lines(lines: &[String]) -> Option<Vec<u8>> {
    STANDARD.decode(lines.concat().trim()).ok()
}

/// The size of the state stored in base64 lines, without decoding it.
pub fn state_size(lines: &[String]) -> usize {
    let chars: usize = lines.iter().map(|l| l.trim().len()).sum();
    let pad = lines.last().map_or(0, |l| l.trim_end().bytes().rev().take_while(|&b| b == b'=').count());
    (chars / 4 * 3).saturating_sub(pad)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CID: &str = "ABCDEF0123456789ABCDEF0123456789";

    #[test]
    fn round_trips_with_steinbergs_layout() {
        let comp: Vec<u8> = (0..=255).collect();
        let preset = VstPreset::new(CID, comp.clone(), Some(b"<editor zoom=2/>".to_vec()));
        let bytes = preset.to_bytes();
        // Header, then the chunks, then the list pointing back at them.
        assert_eq!(&bytes[..4], b"VST3");
        assert_eq!(i32::from_le_bytes(bytes[4..8].try_into().unwrap()), 1);
        assert_eq!(&bytes[8..40], CID.as_bytes());
        let list = i64::from_le_bytes(bytes[40..48].try_into().unwrap()) as usize;
        assert_eq!(list, 48 + 256 + 16);
        assert_eq!(&bytes[list..list + 4], b"List");
        assert_eq!(i32::from_le_bytes(bytes[list + 4..list + 8].try_into().unwrap()), 2);
        assert_eq!(&bytes[list + 8..list + 12], b"Comp");
        assert_eq!(i64::from_le_bytes(bytes[list + 12..list + 20].try_into().unwrap()), 48);
        assert_eq!(i64::from_le_bytes(bytes[list + 20..list + 28].try_into().unwrap()), 256);
        assert_eq!(&bytes[list + 28..list + 32], b"Cont");
        assert_eq!(bytes.len(), list + 8 + 2 * 20);

        let back = VstPreset::from_lines(&preset.to_lines()).unwrap();
        assert_eq!(back, preset);
        assert_eq!((back.component(), back.controller()), (&comp[..], Some(&b"<editor zoom=2/>"[..])));
        assert_eq!(state_size(&preset.to_lines()), bytes.len());
        // Without a controller chunk.
        let bare = VstPreset::new(CID, vec![1, 2, 3], None);
        assert_eq!(VstPreset::from_bytes(&bare.to_bytes()).unwrap().controller(), None);
    }

    #[test]
    fn keeps_other_chunks_and_rejects_damage() {
        let mut preset = VstPreset::new(CID, vec![7; 10], None);
        preset.chunks.push((*b"Info", b"<MetaInfo/>".to_vec()));
        assert_eq!(VstPreset::from_bytes(&preset.to_bytes()).unwrap().chunk(*b"Info"), Some(&b"<MetaInfo/>"[..]));

        let bytes = preset.to_bytes();
        assert_eq!(VstPreset::from_bytes(b"VST2"), Err(PresetError::NotPreset));
        assert_eq!(VstPreset::from_lines(&["not base64!".into()]), Err(PresetError::NotPreset));
        assert!(matches!(VstPreset::from_bytes(&bytes[..bytes.len() - 1]), Err(PresetError::Damaged(_))), "list cut short");
        let mut far = bytes.clone();
        far[40..48].copy_from_slice(&1_000_000i64.to_le_bytes());
        assert!(matches!(VstPreset::from_bytes(&far), Err(PresetError::Damaged(_))));
        let only_info = VstPreset { class_id: CID.into(), chunks: vec![(*b"Info", vec![])] };
        assert_eq!(VstPreset::from_bytes(&only_info.to_bytes()), Err(PresetError::NoComponent));
    }
}
