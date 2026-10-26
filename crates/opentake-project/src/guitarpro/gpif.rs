//! GPIF (Guitar Pro 7/8 XML) parser.
//!
//! GPIF is a normalised document: MasterBars reference Bars (one per track), Bars reference
//! Voices, Voices reference Beats, Beats reference Notes and Rhythms, all by id.

use std::collections::HashMap;

use roxmltree::{Document, Node};

use super::{Bar, Beat, GpError, MasterBar, Note, Score, TICKS_PER_QUARTER, TempoChange, Track, TrackKind, Voice};

type El<'a> = Node<'a, 'a>;

fn child<'a>(el: Option<El<'a>>, tag: &str) -> Option<El<'a>> {
    el?.children().find(|c| c.has_tag_name(tag))
}

fn children<'a>(el: Option<El<'a>>, tag: &'a str) -> impl Iterator<Item = El<'a>> + 'a {
    el.into_iter().flat_map(move |e| e.children().filter(move |c| c.has_tag_name(tag)))
}

fn text(el: Option<El>) -> String {
    // Only text/CDATA nodes: roxmltree's `text()` on an element repeats its first child's text.
    el.map(|e| e.descendants().filter(|n| n.is_text()).filter_map(|n| n.text()).collect::<String>().trim().to_string()).unwrap_or_default()
}

fn ids(s: &str) -> Vec<&str> {
    s.split_whitespace().collect()
}

fn property<'a>(el: El<'a>, name: &str) -> Option<El<'a>> {
    child(Some(el), "Properties")?.children().find(|p| p.has_tag_name("Property") && p.attribute("name") == Some(name))
}

fn num<T: std::str::FromStr>(s: &str, fallback: T) -> T {
    s.parse().unwrap_or(fallback)
}

const DYNAMICS: [(&str, u8); 8] = [("PPP", 20), ("PP", 35), ("P", 50), ("MP", 65), ("MF", 80), ("F", 95), ("FF", 110), ("FFF", 127)];

fn note_value(name: &str) -> u32 {
    match name {
        "Whole" => 1,
        "Half" => 2,
        "Quarter" => 4,
        "Eighth" => 8,
        "16th" => 16,
        "32nd" => 32,
        "64th" => 64,
        "128th" => 128,
        _ => 4,
    }
}

/// GP tempo reference units, in quarter notes.
fn tempo_unit(unit: &str) -> f64 {
    match unit {
        "1" => 0.5,
        "3" => 1.5,
        "4" => 2.0,
        "5" => 3.0,
        _ => 1.0,
    }
}

#[derive(Clone, Copy)]
struct Rhythm {
    ticks: u32,
    tuplet: Option<(u32, u32)>,
}

const DEFAULT_RHYTHM: Rhythm = Rhythm { ticks: TICKS_PER_QUARTER, tuplet: None };

fn parse_rhythm(el: El) -> Rhythm {
    let value = note_value(&text(child(Some(el), "NoteValue")));
    let dots: u32 = child(Some(el), "AugmentationDot").and_then(|d| d.attribute("count")).map_or(0, |c| num(c, 0));
    let tuplet = child(Some(el), "PrimaryTuplet").map(|t| (num(t.attribute("num").unwrap_or("0"), 0), num(t.attribute("den").unwrap_or("0"), 0)));
    let mut ticks = 4.0 * TICKS_PER_QUARTER as f64 / value as f64;
    let mut add = ticks;
    for _ in 0..dots {
        add /= 2.0;
        ticks += add;
    }
    if let Some((n, d)) = tuplet.filter(|t| t.0 > 0) {
        ticks = ticks * d as f64 / n as f64;
    }
    Rhythm { ticks: ticks.round() as u32, tuplet }
}

fn parse_master_bar(el: El) -> MasterBar {
    let time = text(child(Some(el), "Time"));
    let mut parts = time.split('/').map(|p| p.trim().parse::<u32>().unwrap_or(0));
    let numerator = parts.next().filter(|&n| n > 0).unwrap_or(4);
    let denominator = parts.next().filter(|&d| d > 0).unwrap_or(4);
    let repeat = child(Some(el), "Repeat");
    let section = child(Some(el), "Section").map(|s| {
        [text(child(Some(s), "Letter")), text(child(Some(s), "Text"))].into_iter().filter(|t| !t.is_empty()).collect::<Vec<_>>().join(" ")
    });
    MasterBar {
        numerator,
        denominator,
        duration: (numerator as f64 * 4.0 / denominator as f64 * TICKS_PER_QUARTER as f64).round() as u32,
        section: section.filter(|s| !s.is_empty()),
        repeat_start: repeat.and_then(|r| r.attribute("start")) == Some("true"),
        repeat_count: match repeat {
            Some(r) if r.attribute("end") == Some("true") => num(r.attribute("count").unwrap_or("2"), 2),
            _ => 0,
        },
        alternate_endings: ids(&text(child(Some(el), "AlternateEndings"))).iter().filter_map(|n| n.parse().ok()).filter(|&n| n > 0).collect(),
        triplet_feel: Some(text(child(Some(el), "TripletFeel"))).filter(|t| !t.is_empty()),
    }
}

fn parse_track_header(el: El, index: usize) -> Track {
    let kind_text = text(child(child(Some(el), "InstrumentSet"), "Type")).to_lowercase();
    let kind = if kind_text.contains("drum") {
        TrackKind::Drums
    } else if kind_text.contains("bass") {
        TrackKind::Bass
    } else if kind_text.contains("guitar") {
        TrackKind::Guitar
    } else {
        TrackKind::Other
    };
    let staff = child(child(Some(el), "Staves"), "Staff");
    let tuning: Vec<u8> = match (kind, staff.and_then(|s| property(s, "Tuning"))) {
        (TrackKind::Drums, _) => Vec::new(),
        (_, prop) => ids(&text(child(prop, "Pitches"))).iter().filter_map(|p| p.parse().ok()).collect(),
    };
    let capo = num(&text(child(staff.and_then(|s| property(s, "CapoFret")), "Fret")), 0);
    let program_text = {
        let sound = text(child(child(child(child(Some(el), "Sounds"), "Sound"), "MIDI"), "Program"));
        if sound.is_empty() { text(child(child(Some(el), "GeneralMidi"), "Program")) } else { sound }
    };
    let default_program = match kind {
        TrackKind::Bass => 33,
        TrackKind::Guitar => 29,
        _ => 0,
    };
    let name = text(child(Some(el), "Name"));
    Track {
        name: if name.is_empty() { format!("Track {}", index + 1) } else { name },
        kind,
        tuning: match (tuning.is_empty(), kind) {
            (false, _) | (true, TrackKind::Drums) => tuning,
            (true, TrackKind::Bass) => vec![28, 33, 38, 43],
            (true, _) => vec![40, 45, 50, 55, 59, 64],
        },
        tuning_name: text(child(child(staff, "Properties"), "Name")),
        capo,
        midi_program: num(&program_text, default_program),
        bars: Vec::new(),
    }
}

fn parse_note(el: El, track: &Track) -> Note {
    let prop = |name: &str| property(el, name);
    let prop_value = |name: &str, tag: &str| text(child(prop(name), tag));
    let string: u8 = num(&prop_value("String", "String"), 0);
    let fret: u8 = num(&prop_value("Fret", "Fret"), 0);
    let midi = match prop_value("Midi", "Number").parse::<u8>() {
        Ok(m) => m,
        Err(_) if !track.tuning.is_empty() => {
            let open = track.tuning.get(string as usize).copied().unwrap_or(40);
            (open as u32 + fret as u32 + track.capo as u32).min(127) as u8
        }
        Err(_) => 60,
    };
    let tie = child(Some(el), "Tie");
    let mut note = Note {
        string,
        fret,
        midi,
        tie_origin: tie.and_then(|t| t.attribute("origin")) == Some("true"),
        tie_destination: tie.and_then(|t| t.attribute("destination")) == Some("true"),
        palm_mute: prop("PalmMuted").is_some(),
        dead: prop("Muted").is_some(),
        let_ring: child(Some(el), "LetRing").is_some(),
        accent: num::<u32>(&text(child(Some(el), "Accent")), 0) > 0,
        ghost: child(Some(el), "AntiAccent").is_some(),
        bend: Vec::new(),
        slide: num(&prop_value("Slide", "Flags"), 0u32).min(255) as u8,
        vibrato: match text(child(Some(el), "Vibrato")).as_str() {
            "Slight" => 1,
            "Wide" => 2,
            _ => 0,
        },
    };
    if prop("Bended").is_some() {
        let f = |name: &str, fallback: f64| prop_value(name, "Float").parse::<f64>().unwrap_or(fallback);
        let origin = f("BendOriginValue", 0.0);
        let dest = f("BendDestinationValue", origin);
        let middle = f("BendMiddleValue", (origin + dest) / 2.0);
        let mut points = vec![
            (f("BendOriginOffset", 0.0), origin),
            (f("BendMiddleOffset1", 50.0), middle),
            (f("BendMiddleOffset2", 50.0), middle),
            (f("BendDestinationOffset", 100.0), dest),
        ];
        // GPIF bend values: 100 = one whole tone; offsets are percentages of the note.
        for p in &mut points {
            *p = ((p.0 / 100.0).clamp(0.0, 1.0), p.1 / 50.0);
        }
        points.sort_by(|a, b| a.0.total_cmp(&b.0));
        note.bend = points;
    }
    note
}

fn parse_tempos(automations: Option<El>) -> Vec<TempoChange> {
    let mut out: Vec<TempoChange> = children(automations, "Automation")
        .filter(|a| text(child(Some(*a), "Type")) == "Tempo")
        .map(|a| {
            let value = text(child(Some(a), "Value"));
            let mut parts = value.split_whitespace();
            let bpm: f64 = parts.next().and_then(|v| v.parse().ok()).unwrap_or(120.0);
            TempoChange {
                bar: num(&text(child(Some(a), "Bar")), 0),
                position: num(&text(child(Some(a), "Position")), 0.0),
                bpm: bpm * tempo_unit(parts.next().unwrap_or("2")),
            }
        })
        .collect();
    out.sort_by(|a, b| a.bar.cmp(&b.bar).then(a.position.total_cmp(&b.position)));
    out
}

pub fn parse(xml: &str) -> Result<Score, GpError> {
    let doc = Document::parse(xml)?;
    let root = doc.root_element();
    if !root.has_tag_name("GPIF") {
        return Err(GpError::Invalid("not a GPIF document".into()));
    }
    let by_id = |section: &str, tag: &str| -> HashMap<String, El> {
        child(Some(root), section)
            .into_iter()
            .flat_map(|c| c.children().filter(|e| e.has_tag_name(tag)))
            .map(|e| (e.attribute("id").unwrap_or_default().to_string(), e))
            .collect()
    };
    let bar_els = by_id("Bars", "Bar");
    let voice_els = by_id("Voices", "Voice");
    let beat_els = by_id("Beats", "Beat");
    let note_els = by_id("Notes", "Note");
    let rhythms: HashMap<String, Rhythm> = by_id("Rhythms", "Rhythm").into_iter().map(|(id, el)| (id, parse_rhythm(el))).collect();

    let master_bar_els: Vec<El> = children(child(Some(root), "MasterBars"), "MasterBar").collect();
    let master_bars: Vec<MasterBar> = master_bar_els.iter().map(|el| parse_master_bar(*el)).collect();
    let mut tracks: Vec<Track> = children(child(Some(root), "Tracks"), "Track").enumerate().map(|(i, el)| parse_track_header(el, i)).collect();

    let parse_voice = |el: El, track: &Track| -> Voice {
        let mut beats = Vec::new();
        let mut pos = 0u32;
        for beat_id in ids(&text(child(Some(el), "Beats"))) {
            let Some(beat_el) = beat_els.get(beat_id) else { continue };
            let rhythm = child(Some(*beat_el), "Rhythm")
                .and_then(|r| r.attribute("ref"))
                .and_then(|r| rhythms.get(r))
                .copied()
                .unwrap_or(DEFAULT_RHYTHM);
            let grace = child(Some(*beat_el), "GraceNotes").is_some();
            let notes = ids(&text(child(Some(*beat_el), "Notes"))).iter().filter_map(|id| note_els.get(*id)).map(|n| parse_note(*n, track)).collect();
            let dynamic_name = text(child(Some(*beat_el), "Dynamic"));
            let tremolo = text(child(Some(*beat_el), "Tremolo"));
            let tremolo = tremolo.split_once('/').and_then(|(n, d)| {
                let (n, d): (u32, u32) = (n.trim().parse().ok()?, d.trim().parse().ok()?);
                (n > 0 && d > 0).then(|| 4 * d / n)
            });
            let brush_up = property(*beat_el, "Brush").map(|b| text(child(Some(b), "Direction")) == "Up");
            let beat = Beat {
                start: pos,
                duration: if grace { 0 } else { rhythm.ticks },
                tuplet: rhythm.tuplet,
                notes,
                tremolo,
                dynamic: DYNAMICS.iter().find(|(n, _)| *n == dynamic_name).map_or(80, |d| d.1),
                brush_up,
                grace,
            };
            pos += beat.duration;
            beats.push(beat);
        }
        Voice { beats }
    };

    for mb_el in &master_bar_els {
        let bar_ids: Vec<String> = ids(&text(child(Some(*mb_el), "Bars"))).iter().map(|s| s.to_string()).collect();
        for (ti, track) in tracks.iter_mut().enumerate() {
            let mut bar = Bar::default();
            if let Some(bar_el) = bar_ids.get(ti).and_then(|id| bar_els.get(id.as_str())) {
                for voice_id in ids(&text(child(Some(*bar_el), "Voices"))) {
                    if voice_id == "-1" {
                        continue;
                    }
                    if let Some(voice_el) = voice_els.get(voice_id) {
                        bar.voices.push(parse_voice(*voice_el, track));
                    }
                }
            }
            track.bars.push(bar);
        }
    }

    let score_el = child(Some(root), "Score");
    let tempos = parse_tempos(child(child(Some(root), "MasterTrack"), "Automations"));
    Ok(Score {
        title: text(child(score_el, "Title")),
        artist: text(child(score_el, "Artist")),
        album: text(child(score_el, "Album")),
        master_bars,
        tracks,
        tempos: if tempos.is_empty() { vec![TempoChange { bar: 0, position: 0.0, bpm: 120.0 }] } else { tempos },
    })
}
