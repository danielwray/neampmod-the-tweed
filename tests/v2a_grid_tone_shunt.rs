//! V2A-grid tone-shunt behavioural tests.
//!
//! The 5E3's tone control is a **shunt-to-ground at V2A's grid**: a
//! 1 MΩ pot wired as a rheostat in series with a 5 nF cap to ground.
//! At tone-max the 1 MΩ isolates the cap → HF passes; at tone-min the
//! 1 Ω floor effectively shorts the cap directly to V2A's grid → HF
//! aggressively shunted to ground.
//!
//! These tests reconstruct the schematic-correct subcircuit (V1 plate
//! driver → 0.1 µF coupling → 1 MΩ vol pot → 68 kΩ mixing → V2A grid
//! → tone rheostat → 5 nF cap → GND) and assert the invariants:
//!
//! 1. The pot used as a rheostat (same node for `top` and `wiper`)
//!    produces a measurable variable resistance between the wiper and
//!    bottom (the verification of the engine "feature" we rely on).
//! 2. At tone = 1.0 the 5 kHz / 100 Hz ratio is close to unity (HF
//!    largely unattenuated).
//! 3. At tone = 0.0 the 5 kHz response is substantially attenuated
//!    relative to 100 Hz — the audible treble-cut character.
//! 4. The 5 nF cap blocks DC indefinitely: a constant input does not
//!    leak through to the V2A grid as a constant offset.

use neampmod_engine::dsp::circuits::mna_circuit::{
    GridBiasType, GridConductionConfig, MnaCircuit, MnaCircuitBuilder, PotHandle, GND,
};
use neampmod_engine::{EngineRate, OversamplingFactor, TubeRegistry};

const SAMPLE_RATE: f32 = 48_000.0;
const V1_SOURCE_Z_OHMS: f32 = 21_000.0; // 12AY7 plate Thévenin ≈ R_p ∥ r_p
const COUPLING_CAP_F: f32 = 0.1e-6;
const VOLUME_POT_OHMS: f32 = 1_000_000.0;
const MIXING_R_OHMS: f32 = 68_000.0;
const TONE_POT_OHMS: f32 = 1_000_000.0;
const TONE_CAP_F: f32 = 5e-9;

/// Build the V2A-grid network with the schematic-correct tone shunt.
/// Returns the circuit plus handles to the Normal volume pot and the
/// tone rheostat (the tests only drive the Normal channel; the Bright
/// channel is silent at zero pot so it doesn't affect measurements).
fn build_grid_network() -> (MnaCircuit, PotHandle, PotHandle) {
    let v2a_spec = TubeRegistry::global()
        .lookup("ge_12ax7_100k")
        .expect("12AX7 spec must be present");

    // X1 keeps inner_sr == SAMPLE_RATE so the frequency-domain
    // assertions below stay calibrated to the constant defined above.
    let engine_rate = EngineRate::new(SAMPLE_RATE, OversamplingFactor::X1);
    let mut b = MnaCircuitBuilder::new(engine_rate);

    // Two drivers per the production network (V1A + V1B), but tests
    // only drive V1A. V1B's driver stays at 0 V, its branch is
    // electrically present (so the dangling-node check passes) but
    // contributes only its quiescent contribution.
    let (v1a, _) = b.add_driver("v1a_plate");
    let (v1b, _) = b.add_driver("v1b_plate");

    let v1a_after_src = b.node("v1a_after_src");
    let norm_top = b.node("norm_top");
    let norm_wiper = b.node("norm_wiper");
    let v2a_grid = b.node("v2a_grid");
    b.resistor(v1a, v1a_after_src, V1_SOURCE_Z_OHMS)
        .capacitor(v1a_after_src, norm_top, COUPLING_CAP_F);
    let (norm_volume, _) = b.pot(norm_top, norm_wiper, GND, VOLUME_POT_OHMS, 1.0);
    b.resistor(norm_wiper, v2a_grid, MIXING_R_OHMS);

    // Bright branch — present for parity with the production network.
    let v1b_after_src = b.node("v1b_after_src");
    let bright_top = b.node("bright_top");
    let bright_wiper = b.node("bright_wiper");
    b.resistor(v1b, v1b_after_src, V1_SOURCE_Z_OHMS)
        .capacitor(v1b_after_src, bright_top, COUPLING_CAP_F);
    let (_bright_volume, _) = b.pot(bright_top, bright_wiper, GND, VOLUME_POT_OHMS, 0.0);
    b.resistor(bright_wiper, v2a_grid, MIXING_R_OHMS);

    // Tone control: rheostat (top tied to wiper) + 5 nF cap to GND.
    let tone_internal = b.node("tone_internal");
    let (tone, _) = b.pot(
        tone_internal,
        tone_internal,
        v2a_grid,
        TONE_POT_OHMS,
        1.0,
    );
    b.capacitor(tone_internal, GND, TONE_CAP_F);

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

    let circuit = b.build().expect("V2A grid network must build");
    (circuit, norm_volume, tone)
}

/// Drive `v1a_plate` with a sinusoid of `freq_hz` and `amp_volts`. Settle
/// for `settle` samples, then measure RMS over `measure` samples.
fn measure_rms(circuit: &mut MnaCircuit, freq_hz: f32, amp_volts: f32, settle: usize, measure: usize) -> f32 {
    for i in 0..settle {
        let phase = 2.0 * std::f32::consts::PI * freq_hz * (i as f32 / SAMPLE_RATE);
        let drive = amp_volts * phase.sin();
        let _ = circuit.process(&[drive, 0.0]);
    }
    let mut sum_sq = 0.0_f32;
    for i in 0..measure {
        let phase = 2.0 * std::f32::consts::PI * freq_hz * ((settle + i) as f32 / SAMPLE_RATE);
        let drive = amp_volts * phase.sin();
        let out = circuit.process(&[drive, 0.0]);
        sum_sq += out * out;
    }
    (sum_sq / measure as f32).sqrt()
}

/// At tone = 1.0 the 5 nF cap is isolated by the 1 MΩ rheostat. The
/// network behaves close to "no tone shunt at all", so a 5 kHz sine at
/// V2A's grid should not be deeply cut. Measure HF / LF response ratio
/// and assert it stays close to unity.
#[test]
fn tone_max_passes_treble() {
    let (mut circuit, norm_vol, tone) = build_grid_network();
    circuit.set_pot_position(norm_vol, 1.0);
    circuit.set_pot_position(tone, 1.0);

    // Settle the volume/tone smoothers (20 ms τ ≈ 100 ms = 4800
    // samples to be safely in steady state).
    for _ in 0..4_800 {
        let _ = circuit.process(&[0.0, 0.0]);
    }

    let amp = 1.0_f32;
    let lo = measure_rms(&mut circuit, 100.0, amp, 4_800, 9_600);
    let hi = measure_rms(&mut circuit, 5_000.0, amp, 4_800, 9_600);

    // With the rheostat at full 1 MΩ, the 5 nF cap presents
    // ≈ 1 MΩ + 1/(jωC) impedance to ground at V2A's grid. At 5 kHz the
    // cap reactance is ~6.4 kΩ and the upstream mixing source-Z is
    // ~68 kΩ + the V1 plate Thévenin — the shunt isn't perfectly
    // open. We expect HF/LF ≥ 0.5 (within 6 dB of unity) here, not 1.0.
    let ratio = hi / lo.max(1e-9);
    assert!(
        ratio > 0.5,
        "tone=1.0 should pass HF without deep cut; HF/LF ratio = {} (lo={}, hi={})",
        ratio,
        lo,
        hi
    );
}

/// At tone = 0.0 the rheostat collapses to the 1 Ω floor. The 5 nF cap
/// then sits effectively directly across V2A's grid to ground, forming
/// an aggressive HF shunt with the 68 kΩ mixing source-Z. The 5 kHz
/// response should be many dB below the 100 Hz response.
#[test]
fn tone_min_cuts_treble() {
    let (mut circuit, norm_vol, tone) = build_grid_network();
    circuit.set_pot_position(norm_vol, 1.0);
    circuit.set_pot_position(tone, 0.0);

    for _ in 0..4_800 {
        let _ = circuit.process(&[0.0, 0.0]);
    }

    let amp = 1.0_f32;
    let lo = measure_rms(&mut circuit, 100.0, amp, 4_800, 9_600);
    let hi = measure_rms(&mut circuit, 5_000.0, amp, 4_800, 9_600);

    let cut_db = 20.0 * (hi / lo.max(1e-9)).log10();
    assert!(
        cut_db < -10.0,
        "tone=0.0 must deeply cut HF; measured HF/LF = {} dB (lo={}, hi={})",
        cut_db,
        lo,
        hi
    );
}

/// Sanity-check the rheostat construction itself: at tone = 1.0 the HF
/// response should be measurably higher than at tone = 0.0. This is the
/// strongest direct evidence that the same-node pot wiring (`top` ==
/// `wiper`) does produce a *variable* resistance between wiper and
/// bottom, if the engine treated the same-node terminals as a hard
/// short or as ill-defined, both pot positions would give identical
/// responses.
#[test]
fn tone_position_modulates_hf_response() {
    let amp = 1.0_f32;

    let (mut hi_circuit, hi_norm, hi_tone) = build_grid_network();
    hi_circuit.set_pot_position(hi_norm, 1.0);
    hi_circuit.set_pot_position(hi_tone, 1.0);
    for _ in 0..4_800 {
        let _ = hi_circuit.process(&[0.0, 0.0]);
    }
    let hi_hf = measure_rms(&mut hi_circuit, 5_000.0, amp, 4_800, 9_600);

    let (mut lo_circuit, lo_norm, lo_tone) = build_grid_network();
    lo_circuit.set_pot_position(lo_norm, 1.0);
    lo_circuit.set_pot_position(lo_tone, 0.0);
    for _ in 0..4_800 {
        let _ = lo_circuit.process(&[0.0, 0.0]);
    }
    let lo_hf = measure_rms(&mut lo_circuit, 5_000.0, amp, 4_800, 9_600);

    assert!(
        hi_hf > lo_hf * 2.0,
        "tone=1.0 HF ({}) must exceed tone=0.0 HF ({}) by ≥6 dB",
        hi_hf,
        lo_hf
    );
}

/// The 5 nF tone cap blocks DC indefinitely. With a constant 10 V drive
/// at V1A's plate driver, the steady-state value at V2A's grid must
/// settle to ≈ 0 V (the only DC path to ground is via the volume pots'
/// lower sections, which give a finite divider).
///
/// Settling time constant is set by the 0.1 µF coupling cap and the
/// network impedance seen by it (~ V1 source-Z + 0.5×vol pot + 68 kΩ +
/// V2A grid network), so τ ≈ 0.1 µF × ~600 kΩ = 60 ms. We settle for
/// 1.5 s (~25 τ).
#[test]
fn tone_cap_blocks_dc_at_v2a_grid() {
    let (mut circuit, norm_vol, tone) = build_grid_network();
    circuit.set_pot_position(norm_vol, 0.5);
    circuit.set_pot_position(tone, 0.5);

    for _ in 0..((1.5 * SAMPLE_RATE) as usize) {
        let _ = circuit.process(&[10.0, 0.0]);
    }

    let mut sum = 0.0_f32;
    let n = 480usize;
    for _ in 0..n {
        sum += circuit.process(&[10.0, 0.0]);
    }
    let mean = sum / n as f32;

    // The 0.1 µF V1→vol coupling cap blocks DC upstream of the
    // mixing junction, so V2A's grid sees ~0 V DC steady state. The
    // grid-conduction stamp's bias point produces a small but finite
    // residual; we assert |mean| < 50 mV to keep noise off the test.
    assert!(
        mean.abs() < 0.05,
        "V2A grid DC steady-state = {} V (expected ≈ 0 with cap blocking DC)",
        mean
    );
}
