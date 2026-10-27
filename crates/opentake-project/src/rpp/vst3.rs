//! Reaper's VST3 state chunk: the base64 lines inside `<VST "VST3: …" …>`. Three parts, each
//! base64-encoded on its own (so each ends with its own padding):
//!
//! ```text
//!   header  u32 plug-in id (the number before '{' in the uid), u32 0xFEED5EEE,
//!           u32 inputs, u64 channel mask per input, u32 outputs, u64 mask per output,
//!           u32 size of the data, u32 1, u32 0x0010FFFF
//!   data    u32 n, the component state (n bytes), u32 m, the controller state (m bytes)
//!   footer  0, the program name, 0, u32 0x10
//! ```
//!
//! The header is the same as for VST2 chunks; the data layout is reverse-engineered, so
//! reading accepts it only when the sizes account for every byte (a data part that is
//! just the component state after its size, or the controller state without a size, is
//! accepted too). Anything else is left alone: the lines are kept and written back as they
//! were, and the plug-in starts from its saved parameters.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

use crate::vstpreset::{VstPreset, encode_lines};

const FEED: u32 = 0xFEED_5EEE;
/// Empty program name.
const FOOTER: [u8; 6] = [0, 0, 0x10, 0, 0, 0];

/// The class id in a Reaper uid ("<id>{<cid>}"), or the uid if it is a bare one.
pub(super) fn class_id(uid: &str) -> &str {
    uid.rsplit('{').next().unwrap_or_default().trim_end_matches('}')
}

/// The state in a Reaper VST3 chunk, as a `.vstpreset` for plug-in `uid`.
pub(super) fn decode(uid: &str, lines: &[String]) -> Option<VstPreset> {
    let bytes = decode_groups(lines)?;
    let mut r = Reader(&bytes);
    r.u32()?;
    if r.u32()? != FEED {
        return None;
    }
    for _ in 0..2 {
        let buses = r.u32()? as usize;
        r.take(buses.checked_mul(8)?)?;
    }
    let size = r.u32()? as usize;
    r.take(8)?;
    let (component, controller) = split_data(r.take(size)?)?;
    Some(VstPreset::new(class_id(uid), component.to_vec(), controller.map(<[u8]>::to_vec)))
}

/// A Reaper VST3 chunk holding `preset`'s state, for plug-in `uid`.
pub(super) fn encode(uid: &str, preset: &VstPreset) -> Vec<String> {
    let mut data = Vec::new();
    let component = preset.component();
    data.extend_from_slice(&(component.len() as u32).to_le_bytes());
    data.extend_from_slice(component);
    let controller = preset.controller().unwrap_or_default();
    data.extend_from_slice(&(controller.len() as u32).to_le_bytes());
    data.extend_from_slice(controller);

    let mut header = Vec::new();
    header.extend_from_slice(&plugin_id(uid).to_le_bytes());
    header.extend_from_slice(&FEED.to_le_bytes());
    // Stereo in and out: one bus each, channels 1 and 2.
    for _ in 0..2 {
        header.extend_from_slice(&2u32.to_le_bytes());
        header.extend_from_slice(&1u64.to_le_bytes());
        header.extend_from_slice(&2u64.to_le_bytes());
    }
    header.extend_from_slice(&(data.len() as u32).to_le_bytes());
    header.extend_from_slice(&1u32.to_le_bytes());
    header.extend_from_slice(&0x0010_FFFFu32.to_le_bytes());

    let mut lines = encode_lines(&header);
    lines.extend(encode_lines(&data));
    lines.extend(encode_lines(&FOOTER));
    lines
}

/// Reaper's number for a plug-in: the one in its uid, else one made from the class id.
fn plugin_id(uid: &str) -> u32 {
    match uid.split_once('{') {
        Some((n, _)) => n.trim().parse::<i64>().map_or(0, |n| n as u32),
        None => u32::from_str_radix(class_id(uid).get(..8).unwrap_or_default(), 16).unwrap_or(0),
    }
}

/// The data part: (component, controller).
fn split_data(data: &[u8]) -> Option<(&[u8], Option<&[u8]>)> {
    let mut r = Reader(data);
    let n = r.u32()? as usize;
    let component = r.take(n)?;
    let rest = r.0;
    if rest.is_empty() {
        return Some((component, None));
    }
    let mut c = Reader(rest);
    match c.u32() {
        Some(n) if n as usize == rest.len() - 4 => Some((component, Some(c.0).filter(|c| !c.is_empty()))),
        _ => Some((component, Some(rest))),
    }
}

/// The lines decoded: a group ends where the base64 length reaches a multiple of 4 (lines
/// end on whole groups of four characters, padded or not).
fn decode_groups(lines: &[String]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut group = String::new();
    for line in lines {
        group.push_str(line.trim());
        if group.len().is_multiple_of(4) {
            out.extend(STANDARD.decode(&group).ok()?);
            group.clear();
        }
    }
    group.is_empty().then_some(out)
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let (head, rest) = (self.0.get(..n)?, self.0.get(n..)?);
        self.0 = rest;
        Some(head)
    }

    fn u32(&mut self) -> Option<u32> {
        self.take(4).map(|b| u32::from_le_bytes(b.try_into().expect("4 bytes")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UID: &str = "1919247729{5653545265716572656165712E000000}";

    #[test]
    fn chunk_round_trips_and_reads_reapers_layout() {
        let preset = VstPreset::new(class_id(UID), (0..200).map(|i| i as u8).collect(), Some(b"ctrl".to_vec()));
        let lines = encode(UID, &preset);
        // Three groups; the header says who and how big.
        let header = STANDARD.decode(&lines[0]).unwrap();
        assert_eq!(header.len(), 60, "{lines:?}");
        assert_eq!(u32::from_le_bytes(header[..4].try_into().unwrap()), 1919247729);
        assert_eq!(&header[4..8], &[0xEE, 0x5E, 0xED, 0xFE]);
        assert_eq!(u32::from_le_bytes(header[48..52].try_into().unwrap()), 4 + 200 + 4 + 4);
        assert_eq!(lines.last().unwrap(), "AAAQAAAA");
        assert_eq!(decode(UID, &lines), Some(preset));

        // Without a controller state; a controller state without its size.
        let bare = VstPreset::new(class_id(UID), vec![1, 2, 3], None);
        assert_eq!(decode(UID, &encode(UID, &bare)), Some(bare.clone()));
        let mut data = 3u32.to_le_bytes().to_vec();
        data.extend_from_slice(&[1, 2, 3, 9, 9]);
        assert_eq!(split_data(&data), Some((&[1u8, 2, 3][..], Some(&[9u8, 9][..]))));
    }

    #[test]
    fn unknown_chunks_are_not_decoded() {
        // Sizes that don't add up, a VST2-style chunk, damaged base64.
        let bad_size = vec!["cWVyUu5e7f4CAAAAAQAAAAAAAAACAAAAAAAAAAIAAAABAAAAAAAAAAIAAAAAAAAAWwAAAAAAAAA=".to_string(), "AAAQAAAA".into()];
        assert_eq!(decode(UID, &bad_size), None);
        let mut lines = encode(UID, &VstPreset::new("x", vec![5; 8], None));
        lines[1] = STANDARD.encode([200u8, 0, 0, 0, 1, 2]);
        assert_eq!(decode(UID, &lines), None, "component size past the data");
        assert_eq!(decode(UID, &["@@@@".into()]), None);
        assert_eq!(decode(UID, &["QUJD".into(), "RA".into()]), None, "a group left open");
        assert_eq!(plugin_id("ABCDEF01ABCDEF01ABCDEF01ABCDEF01"), 0xABCDEF01);
    }
}
