//! Watch what can be metered, and print the list whenever it changes.
//!
//! The window does this to fill its table; on the command line it shows what a
//! live listing costs — nothing until something actually happens.

use std::time::Duration;

use output_decibel_meter::capture::{self, SourceInfo};

fn main() {
    let listing = capture::listing();
    let mut shown: Vec<SourceInfo> = Vec::new();

    println!("watching, Ctrl+C to stop");
    loop {
        let sources = listing.sources();
        if sources != shown {
            println!();
            for source in &sources {
                let state = match source.is_active {
                    Some(true) => "active",
                    Some(false) => "idle",
                    None => "—",
                };
                println!("  {state:<7} {}", source.name);
            }
            shown = sources;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}
