//! Open a plug-in's own editor window for a manual check, without the app:
//!
//! ```text
//! cargo run -p opentake-engine --example open_editor -- <plugin> [seconds]
//! ```
//!
//! `<plugin>` is anything `insert_plugin` takes: "au:aufx:dely:appl", a VST3 name or bundle
//! path. The window's id is printed (for `screencapture -l <id>` on macOS); edits made in
//! the editor are applied as in the app and listed when it closes, after `seconds` (default
//! 10) or when the window is closed. No audio device is started.

use std::io::Write;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use opentake_engine::{Engine, SlotRef};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let query = args.first().ok_or_else(|| anyhow!("usage: open_editor <plugin> [seconds]"))?;
    let secs: f64 = args.get(1).map(|s| s.parse()).transpose().context("seconds")?.unwrap_or(10.0);

    let mut e = Engine::new();
    let t = e.add_track("Track")?;
    let which = match e.insert_plugin(t, query, None) {
        Ok(slot) => SlotRef::Effect(slot),
        Err(_) => {
            e.set_instrument(t, Some(query))?;
            SlotRef::Instrument
        }
    };
    let name = e.slot_params(t, which).map(|p| p.len()).unwrap_or(0);
    let (instance, mut editor) = e.open_editor(t, which, &format!("Track — {query}"))?;
    println!("opened {query} ({name} parameters)");

    let start = Instant::now();
    let mut shown = false;
    while start.elapsed().as_secs_f64() < secs && editor.is_open() {
        opentake_vst3::window::pump_events(Duration::from_millis(30));
        editor.idle();
        if !shown && editor.window_id() != 0 {
            println!("window {}", editor.window_id());
            std::io::stdout().flush()?;
            shown = true;
        }
        e.apply_editor_edits()?;
        e.sync_editors(&[instance]);
    }
    let closed_by_user = !editor.is_open();
    drop(editor);
    opentake_vst3::window::pump_events(Duration::from_millis(100));
    println!("closed{}; edits: {:?}", if closed_by_user { " by the user" } else { "" }, e.history_info().undo_steps);
    Ok(())
}
