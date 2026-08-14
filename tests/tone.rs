//! The chain, end to end: play a tone of a known level and meter it back.
//!
//! The unit tests feed samples straight into the meter, which proves the
//! arithmetic and nothing else. This one goes through the audio server, so it
//! covers what cannot be reasoned about — that the tap is on the audio that is
//! playing, at the level it was played.
//!
//! It needs a server and something to play into, so it is `#[ignore]` by
//! default:
//!
//! ```sh
//! cargo test --release -- --ignored
//! ```
//!
//! The same check is also a flag, `output-decibel-meter --self-test`, which is
//! what the CI job `audio` runs and what to reach for on a machine where the
//! numbers look wrong.

use output_decibel_meter::capture;
use output_decibel_meter::selftest;

#[test]
#[ignore = "plays a tone through the audio server; run with --ignored"]
fn a_tone_of_a_known_level_reads_back_at_that_level() {
    let report = selftest::run().expect("playing and metering a tone");
    for check in &report.checks {
        assert!(
            check.passed(),
            "{} measured {:.1} {}, expected {:.1} ± {:.1} (metering {})",
            check.what,
            check.measured,
            check.unit,
            check.expected,
            check.tolerance,
            report.source
        );
    }
}

#[test]
fn listing_holds_up_without_any_audio_device() {
    // CI runners have no sound card, which is a case worth covering rather than
    // skipping: a listing may be empty or fail, but it may not panic, and
    // nothing may claim a default output that is not in it.
    match capture::sources() {
        Ok(sources) => {
            for source in &sources {
                assert!(!source.name.is_empty(), "a source with no name at all");
                assert!(!source.key().is_empty(), "a source that cannot be reopened");
            }
        }
        Err(e) => eprintln!("no sources here, which is allowed: {e:#}"),
    }
    if let Ok(default) = capture::default_output() {
        assert!(
            capture::by_key(&default.key()).is_ok(),
            "the default output is not in the listing it came from"
        );
    }
}
