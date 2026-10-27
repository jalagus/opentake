<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/logo/opentake-wordmark-dark.png">
    <img src="assets/logo/opentake-wordmark-light.png" alt="OpenTake" width="560">
  </picture>
</p>

# OpenTake

**The open-source DAW you can make music with — together with your AI agent.**

OpenTake is a fast, native digital audio workstation written in Rust. Record from your
audio interface, arrange audio and MIDI, play VST3 instruments and effects, and mix —
in a clean, Logic-inspired interface. Every part of a session is also exposed over the
[Model Context Protocol](https://modelcontextprotocol.io), so an AI agent can work in the
same project you have open: create tracks, write MIDI, set up plug-ins, edit and mix while
you watch the changes appear live — and undo anything you don't like.

- 🎙️ Multitrack recording from any audio interface (CoreAudio, WASAPI, ALSA/JACK via cpal), latency-compensated with a built-in loopback measurement
- 🎹 Instrument tracks, piano roll, a built-in synth and GM drum kit; VST3 instruments and effects, plus Audio Units on macOS
- 🎼 MIDI in musical time: regions and notes are stored in beats and follow the tempo map, with quantize (straight and triplet grids, strength, swing, note ends, quantize on record)
- 🎚️ Pitch bend, mod wheel and any CC or channel pressure: drawn in the piano roll's MIDI Draw lane, recorded from a keyboard, chased when playback starts mid-region
- 🎛️ Built-in EQ, compressor, multiband compressor, limiter, delay, reverb, gate, de-esser, chorus and phaser on any track or the master bus; plug-in delay compensation
- 🪟 Plug-ins' own editor windows (VST3 on macOS and Windows, Audio Units): what you turn there is undoable and writes automation
- 🔀 Buses and sends: route tracks into a bus, or send them pre/post-fader to a shared reverb or delay bus
- 🔑 Side-chain inputs for dynamics: key a compressor, gate, limiter, multiband or de-esser (or a side-chain-capable VST3 / AU) from another track, e.g. a kick pumping a pad
- 📈 Automation of volume, pan, mute and any plug-in parameter, in lanes under each track, with Read, Touch and Latch modes
- ⏱️ Project tempo map (tempo and time-signature changes), metronome with count-in
- 🚩 Marker track: named, coloured markers to jump between and cycle; imported from Guitar Pro sections, MIDI files, Reaper, DAWproject and Ableton
- 🔁 Cycle recording: loop a section and record take after take, then pick the one that plays
  or comp the best parts of several takes together (Logic-style quick swipe comping in take lanes)
- 🎹 MIDI input and MIDI recording from any keyboard or controller (CoreMIDI, Windows MIDI, ALSA),
  with sustain pedal, latency-compensated note placement and cycle overdub
- ✂️ Region editing: select, move, copy, split, trim, snap, cycle/loop; region gain and normalize;
  fades, crossfades and click-free merging of regions
- 🔂 Region looping (Logic's loop handle): drag a region's upper-right corner and its audio or
  MIDI repeats, sample-exact, up to there; L loops up to the next region
- 🎚️ Full mixer, metronome, undo/redo for everything
- 💾 Export the mix and stems to WAV or FLAC, faster than real time
- 🤖 MCP server (stdio or HTTP) with 103 tools — the agent and you share one session
- 📂 Its own `.otp` project files (double-click to open), Reaper `.rpp` and DAWproject (Bitwig, Studio One, Cubase) projects
- 🔀 Imports Ableton Live sets (`.als`), Ardour sessions, and Logic Pro projects exported as Final Cut Pro XML
- 🎸 Imports Guitar Pro 7/8 songs (`.gp`) as MIDI tracks for VST instruments or the built-in synth, with the tablature shown in sync with the timeline
- 🎼 Imports and exports Standard MIDI Files (`.mid`): one instrument track per part, with the file's tempo map
- 🦀 Real-time safe audio engine: no locks or allocation on the audio thread

## Download

Latest build (rebuilt from `main` on every push):

| Platform | Download |
|---|---|
| macOS — Apple Silicon | [opentake-macos-arm64.zip](https://github.com/jalagus/opentake/releases/download/nightly/opentake-macos-arm64.zip) |
| macOS — Intel | [opentake-macos-x86_64.zip](https://github.com/jalagus/opentake/releases/download/nightly/opentake-macos-x86_64.zip) |
| Windows (x86_64) | [opentake-windows-x86_64.zip](https://github.com/jalagus/opentake/releases/download/nightly/opentake-windows-x86_64.zip) |
| Linux (x86_64) | [opentake-linux-x86_64.tar.gz](https://github.com/jalagus/opentake/releases/download/nightly/opentake-linux-x86_64.tar.gz) |

Tagged versions (`v*`) get their own [release](https://github.com/jalagus/opentake/releases).

The builds aren't signed yet:

- **macOS**: unzip and move `OpenTake.app` to Applications. The first time, right-click it →
  Open (or run `xattr -dr com.apple.quarantine /Applications/OpenTake.app`).
- **Windows**: SmartScreen may warn — choose More info → Run anyway.
- **Linux**: needs ALSA and GTK 3 (`libasound2`, `libgtk-3-0`), present on most desktops.

## Layout

| Crate | What it does |
|---|---|
| `crates/opentake-project` | Project model (tracks, clips, plugin slots), Reaper `.rpp` import/export, Guitar Pro 7/8 reader |
| `crates/opentake-vst3` | VST3 hosting: bundle scanning, loading, parameters, state, realtime processing, editor views and the native windows plug-in editors live in |
| `crates/opentake-au` | Audio Unit hosting on macOS (AudioToolbox): listing, loading, parameters, state, host transport, realtime processing, Cocoa / AUv3 views |
| `crates/opentake-engine` | cpal audio I/O, lock-free mixer, WAV recorder, control-side `Engine` API |
| `crates/opentake` | `opentake` binary: egui GUI, MCP server (stdio or streamable HTTP), small CLI |

### Realtime design

```
 MCP tools ──▶ Engine (control thread) ──Command ring──▶ Mixer (audio callback)
                    ▲                                      │  │
                    └──────── Garbage ring (freed here) ◀──┘  └─▶ record ring ─▶ Recorder thread ─▶ WAV
```

The audio callback never locks or allocates. Structural changes (new track, inserted
plugin, ...) are built on the control side and sent as boxes over an `rtrb` ring;
anything removed is returned and dropped on a GC thread. Input is passed from the
input stream to the output stream through another ring, and recording copies raw
input to a disk-writer thread.

## Usage

```sh
cargo build --release
./target/release/opentake                 # GUI + MCP server at http://127.0.0.1:8808/mcp
./target/release/opentake serve           # headless, MCP over stdio (for agents that launch it)
./target/release/opentake serve --http 127.0.0.1:8808   # headless, MCP over HTTP
./target/release/opentake devices         # list audio interfaces
./target/release/opentake scan            # list VST3 plugins (and Audio Units on macOS)
```

Options: `--project song.rpp` (open on start), `--no-input` (output only), `--no-mcp` (GUI without
MCP server), `--http ADDR`, `--start-audio` (headless), `--plugin-path DIR` (repeatable).
Logs go to stderr; set `RUST_LOG=debug` for more. Settings (VST3 scan folders, recent projects)
live in `~/Library/Application Support/OpenTake/settings.json` (the platform config dir
elsewhere); set `OPENTAKE_CONFIG_DIR` to keep them somewhere else.

### GUI

![OpenTake: arrangement with tracks, fades and a crossfade, a take folder, buses and reverb sends, a volume automation lane and the mixer](assets/screenshots/arrange.png)

<p>
  <img src="assets/screenshots/tablature.png" alt="Guitar Pro import shown as notation and tablature under the arrangement" width="32.5%">
  <img src="assets/screenshots/eq.png" alt="The built-in EQ with its live analyzer on the master bus" width="32.5%">
  <img src="assets/screenshots/multiband.png" alt="The built-in multiband compressor with its bands, crossovers and gain reduction" width="32.5%">
</p>

A dark, Logic Pro–inspired interface. The GUI and the MCP server share one engine, so
whatever an agent does appears live in the window, and everything you do is visible to the
agent.

- **Record and arrange**: audio and instrument tracks; move, trim, split, copy and loop regions
  with snapping; tempo and time-signature changes; a metronome with count-in.
- **Markers**: the Marker track under the ruler shows named, coloured flags (and faint lines
  through the arrangement). **M** (Transport → Create Marker, or the track's **+**) adds one at
  the playhead — at the nearest bar when snapping — and double-clicking empty marker space adds
  one there. Click a marker to move the playhead to it, drag it to move it (snapped),
  double-click to rename it, ⌫ to delete the selected one; right-click for Rename, Color, Delete
  and **Set Cycle to Marker** (from it to the next marker). **⌥←** / **⌥→** go to the previous /
  next marker. Guitar Pro sections and MIDI file markers become markers on import; Reaper
  (`MARKER`; regions as a marker at their start), DAWproject markers and Ableton locators too.
- **MIDI**: a built-in synth and drum kit or any VST3 / Audio Unit instrument, edited in the piano roll.
  MIDI regions live in beats (quarter notes, like Logic's ticks): change the tempo or add a tempo
  change and they stay on their bars, notes and controllers re-timed with them (undo too).
  Projects saved before this are converted on load, so they play exactly as they did.
- **Quantize**: the piano roll toolbar's grid menu (Smart = the snap value, 1/4, 1/8, 1/16, 1/32,
  1/4T, 1/8T, 1/16T), the **Quantize** button and **Q** quantize the selected notes, or all of
  them; **Options** sets the strength (50 % moves notes halfway), swing (every second line of
  a straight grid later, 100 % = triplet feel), quantizing note ends, and **Quantize on record**
  (recorded MIDI snaps to the grid when you stop). With MIDI regions selected in the
  arrangement, **Q**, the region menu's **Quantize** and the inspector's **Quantize · Apply**
  quantize all their notes. One undo step each.
- **Controllers (MIDI Draw)**: the piano roll's **MIDI Draw** button opens a lane under the notes
  for pitch bend, mod wheel (CC1), volume (CC7), expression (CC11), sustain (CC64), pan (CC10),
  channel pressure or any CC number. Drag on empty space to draw a curve with the pencil, click
  to add a step, drag a handle to move it, right-click or ⌫ to delete; each gesture is one undo
  step. Pitch bend is centred at 0. Controllers play sample-accurately, and starting mid-region
  (or a cycle wrap) sends each controller's value at that point, like Logic's chase; stop
  centres the bend and lifts the pedal. The built-in synth bends ±2 semitones (its Bend Range
  parameter goes to ±24), turns the mod wheel into vibrato and follows volume, expression and
  the sustain pedal; VST3 instruments get controllers as the parameters they map them to,
  Audio Units as MIDI.
- **Play and record MIDI**: plug in a keyboard (Audio Settings → MIDI Inputs: all inputs by
  default, or tick the ones to use; the MIDI light in the control bar flashes as notes come in).
  What you play goes to every record-armed (**R**) instrument track, or with none armed to the
  selected one, like Logic. Press record (**R**): armed instrument tracks record MIDI — together
  with armed audio tracks, or on their own without an audio input — and the notes appear in the
  red region as you play. Stop makes one MIDI region per track, with notes placed where you heard
  them (output latency compensated); pitch bend, mod wheel, the sustain pedal and other CCs are
  recorded into its controller lanes the same way (notes keep the length they were held). With the
  cycle on, every pass merges into one region over the cycle (overdub). One undo removes the
  take and puts the playhead back where recording began.
- **Guitar Pro import**: drop a `.gp` file to get one instrument track per part, the song's
  tempo map, and tablature or notation that scrolls along with the arrangement (**T**). Bends,
  slides and vibrato are played as a pitch-bend lane (the built-in synth's bend range is set to
  ±2 semitones, or ±12 when a slide or bend goes further).
- **MIDI files**: drop a `.mid` file (or File → Import MIDI File…) to pick its parts; each
  becomes an instrument track (channel 10 on the drum kit), with its CCs, pitch bend and
  channel pressure. File → Export MIDI File… writes the MIDI regions (notes and controllers) back out.
- **Mix**: an inspector and a mixer (**I**, **X**), plus built-in EQ, compressor, multiband compressor, limiter,
  delay, reverb, gate, de-esser, chorus and phaser alongside VST3 and Audio Unit effects. The reverb (pre-delay,
  size, decay, damping, low cut, width, diffusion, modulation) shows its decay tail as you turn
  the knobs; it starts 100% wet for a reverb bus, so turn Mix down when using it as an insert.
  The gate (threshold, hysteresis, hold, range, key filters) and de-esser (frequency, threshold,
  range; split or wideband, Listen to hear the detected band) show their gain reduction over time.
  The chorus (1–4 voices) shows each voice's sweeping delay, and the phaser (2–12 stages) its
  notches across the swept range.
  Plug-in latency is compensated automatically.
- **Plug-in editors**: double-click a VST3 or Audio Unit slot in the mixer (or click **Open
  Editor** in its window's header) to open the plug-in's own editor, in a window of its own
  floating above OpenTake. Each control you move there is one undo step, and with an
  automation lane in Touch or Latch it's written while playing; automation playback, undo and
  OpenTake's sliders move the plug-in's controls in turn. Closing it brings back OpenTake's
  generic controls if they were open. Built-in effects keep their own editors. An agent can
  show one with `open_plugin_editor`.
- **Buses**: Track → New Bus (or Create Bus from Selection), then pick a track's Output in
  the inspector or mixer; effects on the bus process the whole group.
- **Sends**: the Sends slots under a strip's Audio FX feed buses besides the output. Click the
  empty slot for a bus, a new bus or **New Reverb Send**; each send has its bus (click to change
  it, or No Send), a level knob (double-click: 0 dB), Pre (pre-fader) and M (mute). A bus strip
  counts the tracks and sends feeding it. Send levels can be automated like the fader.
- **Side chain (ducking / pumping)**: a dynamics plug-in's window has a **Side Chain** menu in its
  header (built-in compressor, multiband, limiter, gate and de-esser, and VST3 / AU effects with a
  side-chain input). To make a pad or bass pump with the kick: put a compressor on the pad, set
  Side Chain to the kick track, then a low threshold (−30 to −40 dB), a high ratio (8:1–20:1), a
  fast attack (~1 ms) and a 100–200 ms release; raise Key Low Cut so the kick's sub doesn't
  dominate. **PRE** taps the kick before its fader and mute, so a muted "ghost" kick still keys
  (post-fader, the default, follows them). A gate keyed by a drum opens on its hits. The editor
  shows **KEY ‹track›** and the mixer slot a small key; the key is lined up through plug-in delay.
- **Automation**: the **A** button in a track header (or **A** for all tracks, View → Show
  Automation, or the track's context menu) shows its automation lanes right under it; the **A**
  on the Stereo Out strip (or View → Show Master Automation) shows the master's below the tracks. Pick what a
  lane moves (volume, pan, mute, a send level or any plug-in parameter) in its header, click to add points
  (snapped), drag them (Shift: fine), double-click or ⌫ to delete; right-click a point for its
  curve (linear, hold, smooth, fast start/end). In Read, faders and knobs follow the lane; moved
  while stopped they set the point at the playhead. Touch and Latch record control moves while
  playing. The master's volume and plug-ins can be automated too (over MCP).
- **Region gain**: select an audio region and set its Gain in the inspector, or Normalize it so
  its peak reaches 0 dBFS.
- **Fades and crossfades**: hover an audio region and drag the small square in its top-left
  (fade in) or top-right (fade out) corner; the inspector sets the lengths and curves (linear,
  equal power, exponential, logarithmic, S-curve), as does the region menu's Fade In / Fade Out.
  Overlapping regions on a track crossfade automatically (equal power, drawn as an X); select two
  neighbours and choose Edit → Crossfade Regions (or a length from the region menu's Crossfade)
  to overlap them around their boundary.
- **Merge regions**: select two or more audio regions on a track and press ⌘J (Edit → Merge
  Regions). They're rendered into one new file in the project's Audio folder — gain, fades,
  pitch/stretch and the active take included, gaps as silence — with a 5 ms crossfade where
  regions butt against each other, so the join doesn't click. Undo restores the originals.
- **Arrange tracks**: drag a track's header (or its mixer strip) to move it; click a strip to
  select its track. The buttons left of the transport show the inspector, mixer, piano roll and
  tablature.
- **Recording**: armed tracks monitor their input while stopped and while recording (Transport →
  Auto Input Monitoring; turn it off if your interface monitors directly), and show a red region
  growing with the playhead while they record.
- **Dynamics**: the compressor and limiter show a scrolling gain-reduction history.
- **Takes**: record with the cycle on and each pass becomes a take in one region; its badge
  (e.g. 3/5) picks the take that plays, and recording the same cycle again adds more (a new
  pass plays whole, replacing the comp).
- **Comping**: click the triangle in a take folder's name bar (or right-click → Expand Takes)
  to open one lane per take under the track. Swipe (drag) across a take lane to play that take
  over the range — snapped to the grid, ⌥ for free — and the folder plays the best parts of each
  take, crossfaded over 10 ms at every join. The lit parts of each lane are what plays; click a
  lane's header to play that take all through. Right-click the folder for Flatten Comp (render
  the comp into one region) and Clear Comp. Moves, trims, splits and stretches keep the comp.
- **Looping**: hover a region's upper-right corner (the end of its name bar) for the loop
  handle and drag it right: the region's content repeats up to there (snapped), drawn as
  fainter copies with notches at each loop boundary. Right-click → Loop (or L, or the
  inspector's Loop box) loops it up to the next region on the track, and off again. Dragging
  the end edge changes how many repeats play; trimming the start shortens the loop; splitting
  keeps both parts looping as they sounded. Convert Loops to Regions turns the repeats into
  real regions. The piano roll edits the first pass and shows the repeats as ghost notes.
- **Latency compensation**: recordings land where you played them. Audio Settings → Measure
  Latency checks the device's figure with a loopback (a cable, or speakers into a microphone).
- **Pitch and stretch**: transpose an audio region (semitones and cents) and time-stretch it,
  each without changing the other: use the inspector, or ⌥-drag the region's end to stretch it
  to the grid. Uses Signalsmith Stretch (MIT).
- **Export**: the mix and stems as WAV or FLAC (File → Export Audio…, ⇧⌘E).
- **Undo everything**: ⌘Z / ⇧⌘Z, including edits made by an agent.

Projects save as `.otp` (JSON), and the last one reopens on start. Space plays, R records and
C toggles the cycle; the menus list every other shortcut.

### Connect an agent

With the GUI running, connect over HTTP:

```sh
claude mcp add --transport http opentake http://127.0.0.1:8808/mcp
```

Or let the agent launch a headless instance over stdio:

```sh
claude mcp add opentake -- /absolute/path/to/target/release/opentake serve
```

### What to ask

A few ideas (more in **[docs/mcp-examples.md](docs/mcp-examples.md)**, with worked examples
of the tool calls behind them):

- "Import `~/Songs/riff.gp` with the drum kit for drums and use the song's tempo map."
- "Loop bars 17–24 with a two-bar count-in so I can practise the solo."
- "Arm the Guitar track on input 1 and record from bar 9."
- "Cycle bars 5–8 so I can record a few takes of the chorus vocal." Then: "Play take 3."
- "Write a root-note bass line for an A–F–C–G progression over bars 1–8."
- "Do a balanced mix that works on phones and studio monitors." The agent measures with
  `analyze_mix`, adjusts levels, pans and EQs, and re-checks until it's right.
- "What did you change in the last ten steps? Undo the EQ changes but keep the levels."

### Tools

`get_status`, `list_audio_devices`, `start_audio`, `stop_audio`, `undo`, `redo`, `get_history`,
`get_project`, `get_midi_notes`, `analyze_mix`, `export_audio`, `new_project`, `load_project`, `save_project`, `set_tempo`, `set_tempo_map`, `get_tempo_map`, `convert_time`,
`set_master_volume`, `set_metronome`, `set_auto_input_monitoring`, `set_count_in`, `set_loop`,
`list_markers`, `add_marker`, `update_marker`, `remove_marker`, `goto_marker`, `cycle_marker`,
`add_track`, `add_bus`, `add_send`, `update_send`, `remove_send`, `move_track`, `remove_track`, `update_track`, `add_clip`, `move_clip`, `copy_clip`, `split_clip`, `trim_clip`, `set_clip_gain`, `normalize_clip`, `set_clip_pitch_stretch`, `set_clip_fades`, `crossfade_clips`, `merge_clips`, `set_clip_loop`, `remove_clip`,
`select_take`, `delete_take`, `comp_take`, `clear_comp`, `flatten_comp`,
`play`, `stop`, `seek`, `record`, `list_midi_inputs`, `set_midi_inputs`,
`scan_plugins`, `insert_plugin`, `remove_plugin`, `insert_master_plugin`, `remove_master_plugin`, `set_plugin_bypass`, `open_plugin_editor`,
`list_plugin_params`, `set_plugin_param`, `set_sidechain`, `save_plugin_preset`, `load_plugin_preset` (`"eq"` inserts the built-in EQ; `master: true` addresses the master bus; presets are VST3 `.vstpreset` files),
`list_instruments`, `add_instrument_track`, `set_instrument`, `list_instrument_params`, `set_instrument_param`,
`add_midi_clip`, `set_midi_notes`, `add_midi_notes`, `quantize_midi`, `set_record_quantize`, `get_midi_controllers`, `set_midi_controllers`, `update_midi_clip`, `copy_midi_clip`, `remove_midi_clip`,
`inspect_guitar_pro`, `import_guitar_pro`, `inspect_midi_file`, `import_midi_file`, `export_midi_file`,
`list_automation_params`, `get_automation`, `add_automation_lane`, `set_automation_points`, `clear_automation`,
`set_automation_lane`, `remove_automation_lane`.

MIDI times: notes, controllers and regions take beats (`start_beats`, `length_beats`,
`position_beats`, `time_beats`: quarter notes) or seconds (`start_secs`... converted through the
tempo map where they land), and `add_midi_clip` also a `bar` and `length_bars`; `get_project`,
`get_midi_notes` and `get_midi_controllers` show both, plus each note's bar and beat.
`quantize_midi` takes a grid (`1/16`, `1/8T`...), strength and swing (percent), `ends`, a `window`
and optional note indices.

Controllers: `set_midi_controllers` sets a MIDI region's pitch bend, CC or channel-pressure
events (beats or seconds from the region start; CC 0..127, pitch bend -8192..8191, or semitones with
`bend_range`), replacing the lane or adding to it; `get_midi_controllers` reads them back and
`get_project` lists each region's lanes with their event counts.

Automation: `list_automation_params` lists what a track (or `master: true`) can automate;
`add_automation_lane` creates a lane and `set_automation_points` draws it, with times in seconds
and values normalized 0..1, or `db` for volume and `pan` (-1..1) for pan. Lanes play during
playback and in `analyze_mix` / `export_audio`.

Mixing: `analyze_mix` renders the mix offline (built-in instruments and EQs, audio regions,
faders, pans, master) and reports, per track and for the master, the peak, integrated loudness
(LUFS, ITU BS.1770), energy per frequency band, and how much loudness survives a phone speaker.
An agent can balance a mix by numbers — adjust `update_track` / EQs, re-analyze, repeat —
keeping the master peak under -1 dBFS and the phone drop similar across tracks.

Markers: `add_marker` takes seconds or a bar/beat, a name and a colour (`#rrggbb` or red,
orange, yellow, green, teal, blue, purple, pink); the others find a marker by `index` (in time
order, as `list_markers` shows) or `marker_name`. `cycle_marker` loops from a marker to the next.

Recording: arm a track (`update_track` with `armed: true` and `input_channel`), call
`record`, then `stop`. Each armed track gets a mono 32-bit float WAV in
`<project dir>/Audio/`, and it's added to the track as a clip. Armed instrument tracks record
the MIDI played on the inputs `list_midi_inputs` shows (`set_midi_inputs` chooses them); `stop`
returns the new MIDI regions too.

## Reaper projects

`load_project`/`save_project` pick the format from the extension (`.rpp` vs `.otp`;
`.json` files from earlier versions still open).
Supported: tempo/time signature and the tempo map (tempo envelope), sample rate, master volume, loop range (`LOOP`/`SELECTION`), track name/volume/pan/mute/solo/
record-arm/input/monitoring, audio items (position, length, source offset, gain, mute,
play rate / pitch, fades (`FADEIN`/`FADEOUT`, shapes mapped to the nearest), media file,
item looping (`LOOP 1`: the whole source loops; exported as one item per pass)), FX chain references, buses (written as receives; folders and receives
from tracks with the master send off import as buses), sends (receives with their volume, mute
and mode: post-fader, or pre-fader; Reaper's pre-FX loads as pre-fader), volume/pan/mute envelopes
(`VOLENV2`/`PANENV2`/`MUTEENV`) and markers (`MARKER`; a region comes in as a marker at its start). VST3 plugins in a Reaper project are matched by
bundle file name and class id/name and loaded if installed, with their state chunk
(written back in Reaper's form on export); Audio Units by their type/subtype/manufacturer
codes or name.

Not (yet) supported: plug-in parameter envelopes (`PARMENV`, which number parameters by
position), fader-scaled volume envelopes, send envelopes, MIDI items, takes beyond the
first, and the state blobs of VST2/CLAP plug-ins. Those are kept in the model and written
back on export, but they aren't applied to the loaded plugin. Reaper's VST3 chunk layout
isn't documented; a chunk that doesn't read as expected is kept and written back the same
way, and the plug-in starts from its defaults.

## Projects from other DAWs

Open them like any project (File → Open, drag and drop, or `load_project`); what couldn't be
carried over is listed as warnings. An imported project has no path until you save it, so the
original file is never overwritten.

- **DAWproject** (`.dawproject`; Bitwig Studio, Studio One, Cubase 14+): tracks, volume/pan/
  mute/solo, audio clips (embedded audio is extracted to `<name> Media/` next to the file),
  MIDI clips, clip loops, tempo and time-signature automation, arrangement markers, plug-in references
  (VST3 ones with their state, from the embedded `.vstpreset` files). Also **exported** (Save As →
  DAWproject) with the audio and VST3 states embedded, to take a project into those DAWs.
- **Ableton Live** (`.als`, Live 9–12): audio and MIDI tracks, arrangement clips with
  start/loop markers, mixer, tempo automation, locators (as markers), plug-in references. Live's own devices become
  the built-in synth/drums or are skipped; session-view clips aren't imported.
- **Logic Pro**: `.logicx` is a closed format and can't be opened. In Logic choose
  File → Export → Project to Final Cut Pro XML and open the `.fcpxml`: audio regions arrive on
  tracks per role/lane with their positions, offsets and volume. FCPXML has no MIDI, tempo or
  mixer, so set the tempo afterwards, and bring MIDI over as audio (bounce) or MIDI files.
- **Ardour** (`.ardour`, Ardour 5–8): audio tracks and their regions, gain/pan/mute/solo,
  tempo map. MIDI regions (kept in separate `.mid` files), buses and sends aren't imported.

Common limits: warped/time-stretched clips play at their original speed, plug-in states
(presets) aren't transferred apart from DAWproject's VST3 states (VST3 plug-ins are found
by name and start at their defaults),
automation other than tempo, sends/returns and fades aren't imported.

## Known limitations / next steps

- f32 devices only (fine for CoreAudio; some ASIO/ALSA setups need sample conversion).
- Audio import is WAV only (export also writes FLAC); add `symphonia` for MP3/FLAC/OGG.
  Resampling is linear.
- Recording latency comes from what the device reports; some devices misreport it (the
  built-in Mac audio by 20–40 ms), so measure it once per setup (Audio Settings → Measure Latency).
- VST3: editor windows on macOS and Windows only; Linux (X11 embedding needs the host's run
  loop) keeps the generic parameter sliders. A window the user resizes is fitted to the
  plug-in on the next GUI frame. A plug-in's full state is saved with the project (VST3: the
  component and controller state as a `.vstpreset`; Audio Units: the ClassInfo), and the
  parameter values set in OpenTake are applied on top when it loads. Undo covers parameter
  values, not the rest of a plug-in's state: undoing a preset loaded in its editor restores
  only parameters OpenTake had already saved. VST3 has no MIDI controller events: pitch bend and CCs reach an
  instrument only as the parameters it maps them to (`IMidiMapping`), and are dropped
  otherwise. Instruments haven't been tested with many third-party plugins.
- Audio Units (macOS only): editor windows for Cocoa views (AUv2) and AUv3 view controllers
  (the latter untested so far); stereo/mono main bus plus a
  side chain on a second input element (no extra outputs); notes and controllers go in as MIDI. State is saved as the
  unit's property list; Reaper's AU state may not restore. AUv3 extensions that load only
  asynchronously wait up to 10 s and fail if they need the main thread meanwhile.
- Plugin parameter values, MIDI regions, the built-in synth/drums/effects and master-bus effects
  are saved in `.otp` projects only (`.rpp` export skips them, controller lanes included; VST3 instruments are exported).
  DAWproject and Ableton imports bring MIDI notes only, not their controllers.
- Guitar Pro: only GP7/8 (`.gp`, `.gpif`); `.gp3`–`.gp5` and `.gpx` are recognised but not
  imported yet. Bends, slides and vibrato become pitch bend, which is per track (channel): a
  bend shapes every note sounding with it, and other instruments must be set to the bend
  range by hand (the built-in synth is set for you). Whammy bar and harmonics aren't played.
  `cargo run -p opentake-project --example gpinfo -- song.gp` prints what the reader sees.
- MIDI files: notes, tempo and time signatures are imported (format 0/1; format 2 is read
  as parallel tracks) with CCs, pitch bend and channel pressure; program changes only pick
  names; polyphonic aftertouch, sysex and lyrics are skipped.
- MIDI: notes, CCs, pitch bend and channel pressure (no polyphonic aftertouch or MPE), and
  channels are merged (omni). Live, the sustain pedal also holds notes for instruments that
  ignore CC64; played back from a region it's up to the instrument (the built-in synth and AUs
  honour it, VST3 if it maps CC64). Chasing sends a controller's neutral value (centred bend,
  mod wheel and pedal off, expression full) before its first event; volume and other CCs
  without a neutral value keep what they had. Live MIDI plays one audio buffer late (constant, no jitter) and
  through any plug-in delay compensation. MIDI recording merges cycle passes (no MIDI take
  folders, no replace mode); a key held over the cycle end ends there. Quantize is
  destructive (no non-destructive region Quantize parameter like Logic's, no groove templates).
- Audio doesn't follow the tempo: audio regions, automation, markers and the tempo changes
  themselves stay in seconds, so a tempo change moves MIDI against the audio (Logic's Flex
  "follow tempo" for audio is a next step). A Guitar Pro or MIDI file imported without its
  tempo plays its notes at the project's tempo; "Use Song Tempo" adopts the song's map.
- Takes: one comp per folder (no alternative comps to switch between), and a new cycle pass
  replaces it; a region fade longer than the first or last comp segment is shortened to it.
- Tempo map: steps only (no gradual tempo ramps); Reaper ramps are imported as steps.
- Pitch/stretch: one ratio per region (no Flex markers, no varispeed mode), formants are not
  preserved, and stretch is limited to 0.25–4×. Each new setting renders the region first:
  long regions take about a second per minute, in the background in the GUI. A pitched or
  stretched region doesn't collect new cycle-recording takes.
- Regions: automatic crossfades only cover partial overlaps (a region lying inside another just
  adds to it), and merged regions bake their fades in. Fade curves are mapped to Reaper's
  nearest shapes, and fades aren't imported from other DAWs.
- Region loops: no crossfades at the loop joins (as in Logic; loop lengths are whole samples,
  so passes butt without gaps); at most 1024 passes per region. Reaper loops a whole source
  file, so `.rpp` and DAWproject export write a looped region as one item per pass. The tab view
  shows a looped region's first pass only.
- Buses: stems export what reaches the master: a bus is one stem with its sources and what
  sends bring to it inside, and a track's stem is its dry signal. Pre-fader sends ignore the
  fader and pan but not the track's mute; sends can't be reordered.
- Side chains: only effect slots take a key (not instruments); a key that would make a loop (from a track
  mixed after the keyed one) is refused. A keyed track is read late enough to line the key
  up, which can add to the total plug-in delay. VST3 side chains use the plug-in's first aux input
  bus (stereo or mono as it declares); side-chain VST3s / AUs have had little third-party testing,
  and `.rpp` / DAWproject export drops keys.
- Automation: plug-in parameters change once per 64-sample block (fader, pan and mute ramp
  within it), so changes land within ~1.3 ms of their point (plug-in editor windows show them
  at the GUI's frame rate); no automation of region gain, and Touch/Latch only record from
  the GUI and plug-in editors.

## Development

```sh
cargo test                     # unit tests + headless GUI interaction tests (egui_kittest)
OPENTAKE_SCREENSHOT=/tmp/ui.png cargo test -p opentake render_screenshot -- --ignored   # render the GUI to PNGs (README screenshots)
cargo run -p opentake-engine --example open_editor -- au:aufx:dely:appl   # open a plug-in's editor window, no audio
```

## License

OpenTake is released under the [MIT License](LICENSE).
