//! Print a summary of Guitar Pro files: `cargo run -p opentake-project --example gpinfo -- song.gp ...`

use opentake_project::guitarpro::{self, timeline};

fn main() {
    for path in std::env::args().skip(1) {
        match guitarpro::load(&path) {
            Ok(score) => {
                let perf = timeline::build_performance(&score);
                let tracks: Vec<String> = score.tracks.iter().map(|t| format!("{} ({:?})", t.name, t.kind)).collect();
                println!(
                    "OK   {path}\n     \"{}\" — {} bars ({} played), {:.1}s, {} tempo changes, {} notes; tracks: {}",
                    score.title,
                    score.master_bars.len(),
                    perf.bars.len(),
                    perf.duration,
                    score.tempos.len(),
                    perf.events.len(),
                    tracks.join(", ")
                );
            }
            Err(e) => println!("FAIL {path}\n     {e}"),
        }
    }
}
