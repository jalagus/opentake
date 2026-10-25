# Working with an agent: ideas and examples

OpenTake exposes the whole session over MCP, so an agent (Claude Code, Claude Desktop or any
other MCP client) works in the same project you have open. Changes appear live in the
window, and every step can be undone with ⌘Z. This page collects ideas to get you started:
prompts you can type, and what the agent does with the tools behind them.

Connect first (see [Connect an agent](../README.md#connect-an-agent)):

```sh
claude mcp add --transport http opentake http://127.0.0.1:8808/mcp
```

## Things to ask

Type these as they are, or adapt them. The agent picks the tools.

**Getting a song in**
- "Import `~/Songs/riff.gp` with the built-in synth for guitars and the drum kit for drums,
  and use the song's tempo map."
- "This project was imported without its tempo. Make the timeline follow the song."
- "Open `~/Desktop/demo.otp` and tell me what's in it: tracks, length, tempo changes."

**Practising and recording**
- "Loop bars 17 to 24 and turn on a two-bar count-in so I can practise the solo."
- "Arm the Guitar track on input 1, monitor it, and record from bar 9 with a one-bar count-in."
- "Slow the whole song down to 80 % for practice. Keep the tempo changes in proportion."
- "Mute everything except drums and bass in the chorus so I can play along."

**Writing**
- "Add a synth pad that holds the root of each bar's chord for the first 16 bars."
- "Write a simple four-on-the-floor drum pattern for bars 1–8 at 120 BPM."
- "Double the bass line an octave up on a new track, quieter, panned slightly right."
- "The song changes to 3/4 at bar 33 and slows to 96 BPM. Set that up."

**Mixing**
- "Do a balanced mix that works on phones and studio monitors."
- "The vocals get lost on laptop speakers. Fix that without making them harsh on headphones."
- "Compare the verse and the chorus: which one is louder, and is the low end consistent?"
- "Put a gentle low cut on everything except kick and bass, then re-check the master."

**Housekeeping**
- "Save this as `~/Music/song-v2.otp` and also export a Reaper project next to it."
- "What did you change in the last ten steps? Undo the EQ changes but keep the levels."

## Worked example: a mix by numbers

The agent can't hear the audio, but `analyze_mix` gives it numbers to work from. It renders
the mix offline and reports, per track and for the master: the peak, the loudness (LUFS),
the energy in each frequency band, and how much loudness survives a phone speaker. A
typical loop:

1. **Look.** `get_project` (tracks, plug-ins, tempo; MIDI as note counts) and `analyze_mix`.
2. **Decide.** For example, "drums are 6 LU louder than the guitars", "most of the master's
   energy is below 50 Hz (sub-bass phones can't play)", "the bass loses 7 LU on a phone".
3. **Change** levels, pans and EQs:

   ```jsonc
   // update_track: guitars apart and down
   { "track": 0, "volume_db": 1.6, "pan": -0.55 }
   { "track": 1, "volume_db": 4.6, "pan": 0.55 }

   // insert_plugin, then set_plugin_param (normalized 0..1; list_plugin_params shows the
   // ids, current values and readable displays like "900" / "+6.0")
   { "track": 2, "plugin": "eq" }                                  // -> slot 0
   { "track": 2, "slot": 0, "param_id": 25, "value": 0.551 }       // High Mid frequency: 900 Hz
   { "track": 2, "slot": 0, "param_id": 26, "value": 0.625 }       // gain +6 dB

   // master: safety low cut, then the fader for headroom
   { "plugin": "eq" }                                              // insert_master_plugin
   { "master": true, "slot": 0, "param_id": 0, "value": 1 }        // low cut on
   { "volume_db": -3.5 }                                           // set_master_volume
   ```

4. **Check** with `analyze_mix` again, and repeat until the master peaks below −1 dBFS with
   `clipped_samples: 0`, the tracks sit where they should, and `phone_drop` is similar across
   tracks (so the balance holds on small speakers).

A trimmed `analyze_mix` result:

```json
{
  "tracks": [
    { "name": "Guitar I", "peak_db": -4.2, "lufs": -23.3, "phone_drop": -2.6 },
    { "name": "Bass",     "peak_db": -5.8, "lufs": -24.0, "phone_drop": -4.6,
      "bands": [["sub <50", 29.0], ["bass 50-120", 35.3], ["low-mid 120-300", 32.5], ["...", 0]] },
    { "name": "Drums",    "peak_db": -6.1, "lufs": -21.5, "phone_drop": -4.4 }
  ],
  "master": { "name": "Master", "peak_db": -1.1, "lufs": -21.3, "phone_drop": -3.8 },
  "clipped_samples": 0,
  "warnings": []
}
```

Built-in instruments (synth, drum kit) and the built-in EQ are rendered exactly. VST3
plug-ins can't be rendered offline yet; they're listed in `warnings`, so the numbers
exclude them.

## Worked example: a practice setup from a Guitar Pro file

> "Import `~/Songs/Ashen Litany.gp`, use the drum kit for drums, loop the slow section
> around bar 97 and give me a one-bar count-in."

1. `inspect_guitar_pro` → the tracks, tempo and length, to choose instruments.
2. `import_guitar_pro` `{ "path": "~/Songs/Ashen Litany.gp", "instrument": "synth",
   "drum_instrument": "drums", "set_tempo": true }` → one instrument track per part, the
   song's tempo map adopted, and the tablature view opens.
3. `get_tempo_map` `{ "bars_from_secs": 140, "bars_to_secs": 170 }` → where the bars and
   tempo changes fall (the song slows from 150 to 118 BPM around bar 97).
4. `convert_time` `{ "bar": 97 }` and `{ "bar": 101 }` → the loop's start and end in seconds.
5. `set_loop` `{ "enabled": true, "start_secs": 153.6, "end_secs": 161.5 }` and
   `set_count_in` `{ "bars": 1 }`.

Times are always in seconds; the tempo map does the bar/beat conversion, so the loop stays
on the bar lines even across tempo changes.

## Worked example: writing MIDI

> "Add a bass synth playing root notes, one per bar, for an A–F–C–G progression, bars 1–8."

```jsonc
// add_instrument_track
{ "name": "Synth Bass", "instrument": "synth" }                 // -> track 4
// add_midi_clip (8 bars at 120 BPM in 4/4 = 16 s)
{ "track": 4, "position_secs": 0, "length_secs": 16, "name": "Roots" }
// set_midi_notes (times relative to the region; pitch 45 = A2)
{ "track": 4, "clip": 0, "notes": [
  { "start_secs": 0,  "length_secs": 1.9, "pitch": 45, "velocity": 100 },
  { "start_secs": 2,  "length_secs": 1.9, "pitch": 41, "velocity": 100 },
  { "start_secs": 4,  "length_secs": 1.9, "pitch": 48, "velocity": 100 },
  { "start_secs": 6,  "length_secs": 1.9, "pitch": 43, "velocity": 100 }
  // ...and again for bars 5–8
] }
```

With tempo changes in the song, the agent calls `convert_time` for each bar instead of
assuming 2 s per bar.

## Tips

- **Small answers.** `get_project` lists MIDI regions with note counts; use
  `get_midi_notes` for one region's notes, so a long song doesn't flood the conversation.
- **Undo is your friend.** Agent edits are normal undo steps (`get_history` lists them), so
  you can ask for bold changes and step back with ⌘Z.
- **Watch it happen.** With the GUI open, agent changes appear as they're made: faders
  move, EQ curves change, regions appear.
- **Headless.** `opentake serve` runs without a window over stdio, for agents that launch
  OpenTake themselves (batch imports, analysis, conversions).
