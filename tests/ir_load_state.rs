//! Integration tests for the IR loader state machine.
//!
//! These run against `the_tweed`'s public API surface — the same one the
//! audio thread and GUI consume. Goal: catch regressions in the IR-load
//! lifecycle (NO_IR / LOADING / LOADED / FAILED) without needing a real
//! WAV file or a host process.

use std::sync::atomic::Ordering;

use the_tweed::{IrLoadState, ir_load_status, load_ir_file_into_state};

/// Fresh state must report NO_IR — the audio path runs as a unity
/// passthrough until something is loaded, and the GUI status indicator
/// keys off this value.
#[test]
fn default_state_is_no_ir() {
    let state = IrLoadState::default();
    assert_eq!(state.status.load(Ordering::Relaxed), ir_load_status::NO_IR);
    assert!(state.pending.lock().unwrap().is_none());
}

/// Loading from a path that doesn't exist must transition to FAILED rather
/// than leaving the status stuck at LOADING — otherwise the GUI would lie
/// about progress and the user couldn't tell the load died.
#[test]
fn missing_file_transitions_to_failed() {
    let state = IrLoadState::default();
    state.set_audio_format(48_000.0, 512);
    load_ir_file_into_state(&state, std::path::Path::new("/nonexistent/path-that-cannot-exist.wav"));
    assert_eq!(state.status.load(Ordering::Relaxed), ir_load_status::FAILED);
    assert!(state.pending.lock().unwrap().is_none(),
        "FAILED load must not leave a stale pending convolver");
}

/// Audio format setters propagate atomically — the loader thread uses these
/// values when constructing the convolver, so a torn read would build the
/// wrong block size.
#[test]
fn set_audio_format_round_trips() {
    let state = IrLoadState::default();
    state.set_audio_format(96_000.0, 1024);
    assert_eq!(state.sample_rate.load(Ordering::Relaxed), 96_000.0);
    assert_eq!(state.block_size.load(Ordering::Relaxed), 1024);
}
