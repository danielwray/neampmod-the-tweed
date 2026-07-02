//! V1 shared-cathode pair behavioural tests.
//!
//! Assert the schematic-driven behaviours of the V1 pair: matched DC
//! Q-point on both halves, cross-channel modulation through the shared
//! cathode integrator, and the 12AY7 / 12AX7 gain difference seen by the
//! tube toggle.

use neampmod_engine::dsp::amps::tube_modeling::{
    SharedCathodeTriodePair, SharedCathodeTriodePairConfig,
};
use neampmod_engine::{EngineRate, OversamplingFactor};

const SAMPLE_RATE: f32 = 48_000.0;
const V1_CATHODE_R: f32 = 820.0;
const V1_CATHODE_CAP_UF: f32 = 25.0;
const PREAMP_BPLUS: f32 = 250.0;

fn build_pair(spec: &str) -> SharedCathodeTriodePair {
    let config = SharedCathodeTriodePairConfig {
        tube_spec: spec.into(),
        shared_cathode_resistor_ohms: V1_CATHODE_R,
        shared_cathode_bypass_cap_uf: Some(V1_CATHODE_CAP_UF),
        shared_cathode_bypass_dielectric: Some("electrolytic_vintage".into()),
        plate_resistor_a_ohms: 100_000.0,
        plate_resistor_b_ohms: 100_000.0,
        tube_mismatch: Some(0.05),
        linear_blend_threshold: None,
        plate_voltage_fraction: 1.0,
    };
    // OS factor doesn't matter for these tests, they characterise the
    // pair at SAMPLE_RATE itself, so X1 keeps inner_sr == SAMPLE_RATE.
    let engine_rate = EngineRate::new(SAMPLE_RATE, OversamplingFactor::X1);
    let mut pair = SharedCathodeTriodePair::from_config(engine_rate, config)
        .expect("V1 pair construction must succeed for the canonical 5E3 spec");
    pair.set_plate_bplus_voltage(PREAMP_BPLUS);
    pair
}

/// With both grids at 0 V AC and the cathode bypass cap fully settled, the
/// two plates should produce essentially zero AC voltage. We sample after
/// transient settling, any DC drift would show up as a non-zero AC mean,
/// indicating the AC-zero restoration offset has gone wrong.
#[test]
fn pair_settles_to_zero_ac_at_quiescent() {
    let mut pair = build_pair("ge_12ay7_100k");

    // Warm-up: 200 ms of silence at 48 kHz.
    for _ in 0..9_600 {
        let _ = pair.process_pair(0.0, 0.0, PREAMP_BPLUS);
    }

    // Measure mean over 100 ms.
    let n = 4_800usize;
    let mut sum_a = 0.0_f32;
    let mut sum_b = 0.0_f32;
    for _ in 0..n {
        let out = pair.process_pair(0.0, 0.0, PREAMP_BPLUS);
        sum_a += out.plate_a_ac_volts;
        sum_b += out.plate_b_ac_volts;
    }
    let mean_a = sum_a / n as f32;
    let mean_b = sum_b / n as f32;

    // ±50 mV either side of zero is well within the LUT's quiescent
    // calibration noise and zero-restoration tolerance.
    assert!(mean_a.abs() < 0.05, "plate A AC mean = {} V at quiescent", mean_a);
    assert!(mean_b.abs() < 0.05, "plate B AC mean = {} V at quiescent", mean_b);
}

/// The shared cathode integrator should produce cross-channel modulation:
/// driving grid A hard raises (I_a + I_b) × R_k, lifting the cathode and
/// reducing both triodes' V_gk. Triode B carries no input signal, but its
/// plate output should pick up a residual signal correlated with grid A's
/// drive; Jumpered-input 5E3 ducking. If this assertion
/// fails, either the engine's pair has regressed or the cathode-bypass
/// cap is preventing the integrator from responding at audio rate.
#[test]
fn shared_cathode_couples_a_drive_into_b_plate() {
    let mut pair = build_pair("ge_12ay7_100k");

    // Warm-up.
    for _ in 0..9_600 {
        let _ = pair.process_pair(0.0, 0.0, PREAMP_BPLUS);
    }

    // Drive grid A with a 1 kHz sine, grid B held at zero. Measure the
    // RMS at plate B — should be non-trivial under the shared cathode
    // model (it would be ~0 with independent triodes).
    let drive_volts = 0.5_f32; // 0.5 V AC at the grid — well into clipping
                               // for the 12AY7's ±8.5 V swing region
    let freq = 1_000.0_f32;
    let n = 4_800usize; // 100 ms — enough for many cycles

    let mut sum_b_sq = 0.0_f32;
    for i in 0..n {
        let phase = 2.0 * std::f32::consts::PI * freq * (i as f32 / SAMPLE_RATE);
        let v_a = drive_volts * phase.sin();
        let out = pair.process_pair(v_a, 0.0, PREAMP_BPLUS);
        sum_b_sq += out.plate_b_ac_volts * out.plate_b_ac_volts;
    }
    let b_rms = (sum_b_sq / n as f32).sqrt();

    // The ducking modulation is at 2× the drive frequency (full-wave
    // rectified envelope) and proportional to the drive-induced cathode
    // ripple. At 0.5 V drive into a 12AY7 with 820 Ω bypassed by 25 µF
    // (fc ≈ 7.7 Hz, so 1 kHz is fully bypassed), the residual at plate B
    // is small but should be measurably > 1 mV RMS.
    assert!(
        b_rms > 1e-3,
        "plate B RMS = {} V — shared cathode coupling appears absent",
        b_rms
    );
}

/// The mod tube path (12AX7) has higher μ than the stock 12AY7. At the
/// same small-signal drive, the 12AX7 should produce a larger plate AC
/// swing, call it at least ~1.5×, confirming the toggle still meaningfully
/// changes character after the migration.
#[test]
fn mod_tube_has_higher_gain_than_stock() {
    let mut stock = build_pair("ge_12ay7_100k");
    let mut mod_ = build_pair("ge_12ax7_100k");

    // Warm-up.
    for _ in 0..9_600 {
        let _ = stock.process_pair(0.0, 0.0, PREAMP_BPLUS);
        let _ = mod_.process_pair(0.0, 0.0, PREAMP_BPLUS);
    }

    // Small-signal drive — well below clip — so the gain ratio reflects
    // the tubes' μ rather than the LUT's nonlinear region.
    let drive_volts = 0.05_f32;
    let freq = 1_000.0_f32;
    let n = 4_800usize;

    let mut stock_sq = 0.0_f32;
    let mut mod_sq = 0.0_f32;
    for i in 0..n {
        let phase = 2.0 * std::f32::consts::PI * freq * (i as f32 / SAMPLE_RATE);
        let v = drive_volts * phase.sin();
        let stock_out = stock.process_pair(v, 0.0, PREAMP_BPLUS);
        let mod_out = mod_.process_pair(v, 0.0, PREAMP_BPLUS);
        stock_sq += stock_out.plate_a_ac_volts * stock_out.plate_a_ac_volts;
        mod_sq += mod_out.plate_a_ac_volts * mod_out.plate_a_ac_volts;
    }

    let stock_rms = (stock_sq / n as f32).sqrt();
    let mod_rms = (mod_sq / n as f32).sqrt();

    assert!(
        mod_rms > 1.5 * stock_rms,
        "mod RMS ({}) should be > 1.5× stock RMS ({})",
        mod_rms,
        stock_rms
    );
}
