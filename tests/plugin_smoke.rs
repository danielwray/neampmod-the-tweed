//! Plugin construction smoke + V2A-grid rebuild coverage.
//!
//! The unit tests under `tests/v1_shared_cathode.rs` and
//! `tests/v2a_grid_tone_shunt.rs` exercise engine primitives directly
//! with the values the plugin uses. They do not catch construction-
//! time bugs in the plugin itself (missing LUT lookups, OT spec
//! mismatches, AmpTopology config gaps, sample-rate coupling bugs in
//! IIR coefficient computation) or the tube-toggle path that rebuilds
//! the V2A-grid MNA with a different V1 plate source-Z.
//!
//! `TheTweed::default()` only constructs Arc-shared / SR-independent
//! state. The heavy DSP graph (V1 pairs, V2A + grid MNA, AmpTopology)
//! is built lazily inside [`TheTweed::initialize_audio_state`] when the
//! host hands the plugin a `BufferConfig`. The smoke tests drive that
//! path explicitly so the full construction is still under test.
//!
//! Full `Plugin::process()` exercise requires a nih-plug `Buffer`, which
//! is awkward to construct in tests — that surface is left to
//! first-DAW-load.

use neampmod_engine::dsp::circuits::mna_circuit::{MnaCircuit, MnaCircuitBuilder, PotHandle, GND};
use neampmod_engine::{EngineRate, OversamplingFactor, TubeRegistry};
use the_tweed::TheTweed;

const SAMPLE_RATE: f32 = 48_000.0;

/// `TheTweed::default()` exercises the SR-independent construction
/// paths: `TheTweedParams` defaults, `InputCalibration::amp_standard()`,
/// `JackInput::new(…)`, `LoadboxDi::standard()`,
/// `PotTaperConfig`,
/// `IrLoadState`, and the meter atomics. Anything host-rate-dependent
/// (V1 pairs, V2A grid MNA, `AmpTopology`, IR convolver, DC blocker) is
/// deferred to [`audio_state_builds_at_common_sample_rates`].
#[test]
fn plugin_default_constructs_without_panic() {
    let _plugin = TheTweed::default();
}

/// `Default` should be re-callable without leaving stale state.
/// Re-constructing several times catches non-idempotent registry or
/// static state bugs that don't show up on a single build.
#[test]
fn plugin_default_is_repeatable() {
    for _ in 0..3 {
        let _plugin = TheTweed::default();
    }
}

/// Drive the full DSP-graph construction across the sample rates a
/// real DAW will hand the plugin. Exercises every LUT lookup, every
/// MNA build, the `AmpTopology` composition, the `DspEngine` boundary
/// OS, the `InputLevelMeter`, the IR convolver, and the `DCBlocker` —
/// all keyed off the host's reported sample rate, so any latent
/// SR-coupling bug in coefficient computation shows up here rather
/// than at audio load time.
#[test]
fn audio_state_builds_at_common_sample_rates() {
    for sr in [44_100.0_f32, 48_000.0, 88_200.0, 96_000.0, 176_400.0, 192_000.0] {
        let mut plugin = TheTweed::default();
        plugin.initialize_audio_state(sr, 512, OversamplingFactor::X4);
    }
}

/// `initialize_audio_state` should be idempotent — the nih-plug docs
/// say the host may call `Plugin::initialize` multiple times in rapid
/// succession during state restore, so re-calling at the same or a
/// different rate must not leave dangling state. Catches any "rebuild
/// drops a Box but forgets the field" hazard.
#[test]
fn audio_state_rebuild_is_idempotent() {
    let mut plugin = TheTweed::default();
    plugin.initialize_audio_state(48_000.0, 512, OversamplingFactor::X4);
    plugin.initialize_audio_state(96_000.0, 1024, OversamplingFactor::X2);
    plugin.initialize_audio_state(44_100.0, 256, OversamplingFactor::X8);
}

/// Reconstructs the V2A-grid MNA with the two V1-tube source impedances
/// (≈21 kΩ for 12AY7, ≈38 kΩ for 12AX7) and confirms the HF response with
/// the bright channel active differs measurably. This validates that the
/// per-toggle rebuild produces meaningful character change rather than
/// being a redundant operation.
///
/// The network mirrors `V2aGridNetwork::new` element-for-element,
/// including the tone shunt (rheostat + 5 nF cap) at V2A's grid.
fn build_mixing_network(v1_source_z_ohms: f32) -> (MnaCircuit, PotHandle, PotHandle) {
    let v2a_spec = TubeRegistry::global()
        .lookup("ge_12ax7_100k")
        .expect("12AX7 spec must be present");

    // X1 keeps inner_sr == SAMPLE_RATE so the 8 kHz measurement below
    // stays calibrated to the const defined above.
    let engine_rate = EngineRate::new(SAMPLE_RATE, OversamplingFactor::X1);
    let mut b = MnaCircuitBuilder::new(engine_rate);

    let (v1a, _) = b.add_driver("v1a_plate");
    let (v1b, _) = b.add_driver("v1b_plate");

    let v1a_after_src = b.node("v1a_after_src");
    let norm_pot_top = b.node("norm_pot_top");
    let norm_wiper = b.node("norm_wiper");
    let v2a_grid = b.node("v2a_grid");

    b.resistor(v1a, v1a_after_src, v1_source_z_ohms)
        .capacitor(v1a_after_src, norm_pot_top, 0.1e-6);
    let (norm_volume, _) = b.pot(norm_pot_top, norm_wiper, GND, 1_000_000.0, 1.0);
    b.resistor(norm_wiper, v2a_grid, 68_000.0);

    let v1b_after_src = b.node("v1b_after_src");
    let bright_pot_top = b.node("bright_pot_top");
    let bright_wiper = b.node("bright_wiper");

    b.resistor(v1b, v1b_after_src, v1_source_z_ohms)
        .capacitor(v1b_after_src, bright_pot_top, 0.1e-6);
    let (bright_volume, _) = b.pot(bright_pot_top, bright_wiper, GND, 1_000_000.0, 0.5);
    // Bright cap across upper pot section — the source-Z-sensitive element.
    b.capacitor(bright_pot_top, bright_wiper, 500e-12)
        .resistor(bright_wiper, v2a_grid, 68_000.0);

    // Tone shunt at V2A's grid (same as V2aGridNetwork). Hold at
    // tone=1.0 (rheostat at full 1 MΩ) so the shunt is mostly
    // isolated and doesn't dominate the HF measurement.
    let tone_internal = b.node("tone_internal");
    let (_tone, _) = b.pot(tone_internal, tone_internal, v2a_grid, 1_000_000.0, 1.0);
    b.capacitor(tone_internal, GND, 5e-9);

    use neampmod_engine::dsp::circuits::mna_circuit::{GridBiasType, GridConductionConfig};
    b.grid_conduction(
        v2a_grid,
        GridConductionConfig {
            grid_perveance: v2a_spec.grid_perveance,
            contact_potential: v2a_spec.threshold.abs(),
            bias_type: GridBiasType::CathodeBias {
                cathode_voltage: -v2a_spec.bias_voltage,
            },
        },
    );
    b.set_output(v2a_grid);

    (b.build().expect("network must build"), norm_volume, bright_volume)
}

fn measure_hf_rms(circuit: &mut MnaCircuit, freq_hz: f32, amp: f32, settle: usize, measure: usize) -> f32 {
    for i in 0..settle {
        let phase = 2.0 * std::f32::consts::PI * freq_hz * (i as f32 / SAMPLE_RATE);
        let drive = amp * phase.sin();
        // Drive only the bright side — that's the source-Z-sensitive path.
        let _ = circuit.process(&[0.0, drive]);
    }
    let mut sum_sq = 0.0_f32;
    for i in 0..measure {
        let phase = 2.0 * std::f32::consts::PI * freq_hz * ((settle + i) as f32 / SAMPLE_RATE);
        let drive = amp * phase.sin();
        let out = circuit.process(&[0.0, drive]);
        sum_sq += out * out;
    }
    (sum_sq / measure as f32).sqrt()
}

/// At 8 kHz with the bright channel volume mid-position, the 500 pF bright
/// cap couples HF directly to the wiper. The HF divider — V1 plate
/// source-Z in series with the path to V2A's grid — gives the 12AY7
/// (≈21 kΩ source-Z) a slight edge over the 12AX7 (≈38 kΩ). The
/// difference is smaller than back-of-envelope suggests because the
/// bright cap's low reactance at HF dilutes the source-Z effect, but it
/// is reliably present and verifies the toggle rebuild is not a no-op.
#[test]
fn v1_source_z_affects_hf_bright_response() {
    let (mut circuit_ay7, _, _) = build_mixing_network(21_000.0);
    let (mut circuit_ax7, _, _) = build_mixing_network(38_500.0);

    // Long settle (1 s) so any cap charging is past.
    let settle = SAMPLE_RATE as usize;
    let measure = (SAMPLE_RATE / 100.0) as usize; // 10 ms

    let amp = 1.0_f32;
    let rms_ay7 = measure_hf_rms(&mut circuit_ay7, 8_000.0, amp, settle, measure);
    let rms_ax7 = measure_hf_rms(&mut circuit_ax7, 8_000.0, amp, settle, measure);

    // 12AY7 should produce a measurably larger HF level than 12AX7 at
    // V2A's grid. The bright cap dilutes the source-Z effect, but the
    // delta is still reliable above noise.
    assert!(
        rms_ay7 > rms_ax7 * 1.01,
        "12AY7 HF ({}) should exceed 12AX7 HF ({}) by ≥1%",
        rms_ay7,
        rms_ax7
    );
}
