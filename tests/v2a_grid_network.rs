//! V2A-grid network behavioural tests (schematic-correct 5E3 topology).
//!
//! These tests exercise the production [`V2aGridNetwork`] directly and
//! assert the documented behaviours of the real 5E3 volume/tone network:
//!
//! - **Backwards volume pots** (signal into the wiper, output from the
//!   ungrounded track end): turning a volume down inserts up to ~1 MΩ in
//!   series with the grid junction, so low volume = darker ("muddying").
//! - **Direct junction** (no mixing resistors): the unused channel's pot
//!   is part of the active channel's load. Unused volume full DOWN gives
//!   the LIGHTEST load (most gain); full UP couples the idle V1 plate's
//!   source impedance onto the junction (heavy cut).
//! - **500 pF treble bypass**: pre-volume Bright-channel signal feeds the
//!   tone pot's top lug, so with tone up the Bright channel keeps its
//!   treble even at low volume settings, and the tone control behaves
//!   like a second (treble) volume.
//! - **5 nF tone shunt**: with tone down the cap sits directly on the
//!   junction and deeply cuts treble; with tone up it hides behind the
//!   full 1 MΩ track.
//! - The coupling caps + tone caps block DC; the pot tracks give V2A a
//!   constant 500 kΩ DC grid leak, so the junction settles to ≈ 0 V DC.

use neampmod_engine::dsp::circuits::mna_circuit::MnaCircuit;
use neampmod_engine::{EngineRate, OversamplingFactor};
use the_tweed::V2aGridNetwork;

const SAMPLE_RATE: f32 = 48_000.0;
const V1_SOURCE_Z_OHMS: f32 = 21_000.0; // 12AY7 plate Thévenin ≈ R_p ∥ r_p

/// Build the production network at X1 so inner_sr == SAMPLE_RATE and the
/// frequency-domain assertions stay calibrated to the constant above.
fn build_network() -> V2aGridNetwork {
    let engine_rate = EngineRate::new(SAMPLE_RATE, OversamplingFactor::X1);
    V2aGridNetwork::new(engine_rate, V1_SOURCE_Z_OHMS)
}

/// Set pot targets and run silence through until the internal pot
/// smoothers (20 ms τ) are safely in steady state.
fn settle_pots(net: &mut V2aGridNetwork, norm: f32, bright: f32, tone: f32) {
    net.circuit.set_pot_position(net.norm_volume, norm);
    net.circuit.set_pot_position(net.bright_volume, bright);
    net.circuit.set_pot_position(net.tone, tone);
    for _ in 0..4_800 {
        let _ = net.circuit.process(&[0.0, 0.0]);
    }
}

/// Drive one V1 plate with a sinusoid (`driver` 0 = V1A/Normal,
/// 1 = V1B/Bright). Settle, then measure RMS at V2A's grid.
fn measure_rms(
    circuit: &mut MnaCircuit,
    driver: usize,
    freq_hz: f32,
    amp_volts: f32,
    settle: usize,
    measure: usize,
) -> f32 {
    let mut drives = [0.0_f32; 2];
    for i in 0..settle {
        let phase = 2.0 * std::f32::consts::PI * freq_hz * (i as f32 / SAMPLE_RATE);
        drives[driver] = amp_volts * phase.sin();
        let _ = circuit.process(&drives);
    }
    let mut sum_sq = 0.0_f32;
    for i in 0..measure {
        let phase =
            2.0 * std::f32::consts::PI * freq_hz * ((settle + i) as f32 / SAMPLE_RATE);
        drives[driver] = amp_volts * phase.sin();
        let out = circuit.process(&drives);
        sum_sq += out * out;
    }
    (sum_sq / measure as f32).sqrt()
}

/// HF (5 kHz) / LF (100 Hz) response ratio for a given driver + settings.
fn hf_lf_ratio(norm: f32, bright: f32, tone: f32, driver: usize) -> f32 {
    let mut net = build_network();
    settle_pots(&mut net, norm, bright, tone);
    let lo = measure_rms(&mut net.circuit, driver, 100.0, 1.0, 4_800, 9_600);
    let hi = measure_rms(&mut net.circuit, driver, 5_000.0, 1.0, 4_800, 9_600);
    hi / lo.max(1e-9)
}

/// At tone = 1.0 the 5 nF shunt hides behind the full 1 MΩ track, so a
/// 5 kHz sine driven through the Normal channel at full volume must not
/// be deeply cut relative to 100 Hz. (Some HF loss remains: V2A's Miller
/// capacitance plus the 500 pF path into the grounded Bright wiper.)
#[test]
fn tone_max_passes_treble() {
    let ratio = hf_lf_ratio(1.0, 0.0, 1.0, 0);
    assert!(
        ratio > 0.5,
        "tone=1.0 should pass HF without deep cut; HF/LF ratio = {}",
        ratio
    );
}

/// At tone = 0.0 the wiper sits at the 5 nF end: the cap couples V2A's
/// grid junction more or less directly to ground at HF, against the
/// Normal channel's ~21 kΩ source. 5 kHz must land many dB below 100 Hz.
#[test]
fn tone_min_cuts_treble() {
    let ratio = hf_lf_ratio(1.0, 0.0, 0.0, 0);
    let cut_db = 20.0 * ratio.log10();
    assert!(
        cut_db < -8.0,
        "tone=0.0 must deeply cut HF; measured HF/LF = {} dB",
        cut_db
    );
}

/// The pot must actually modulate the response — tone max vs min differ
/// by well over 6 dB at 5 kHz. Guards against the pot stamping degrading
/// into a fixed resistance.
#[test]
fn tone_position_modulates_hf_response() {
    let hi = hf_lf_ratio(1.0, 0.0, 1.0, 0);
    let lo = hf_lf_ratio(1.0, 0.0, 0.0, 0);
    assert!(
        hi > lo * 2.0,
        "tone=1.0 HF/LF ({}) must exceed tone=0.0 HF/LF ({}) by ≥6 dB",
        hi,
        lo
    );
}

/// THE 5E3 interaction quirk (sign matters): the unused channel's volume
/// full DOWN presents ~1 MΩ (light load, most gain); full UP couples the
/// idle V1 plate's ~21 kΩ source impedance onto the junction through the
/// 0.1 µF cap (heavy cut). Robinette: "Turning the unused channel volume
/// full down offers up the most preamp gain."
#[test]
fn unused_volume_full_down_gives_most_gain() {
    let mut net_down = build_network();
    settle_pots(&mut net_down, 0.5, 0.0, 0.5);
    let rms_down = measure_rms(&mut net_down.circuit, 0, 1_000.0, 1.0, 4_800, 9_600);

    let mut net_up = build_network();
    settle_pots(&mut net_up, 0.5, 1.0, 0.5);
    let rms_up = measure_rms(&mut net_up.circuit, 0, 1_000.0, 1.0, 4_800, 9_600);

    assert!(
        rms_down > rms_up * 2.0,
        "unused bright vol at 0 ({}) must pass much more signal than at max ({})",
        rms_down,
        rms_up
    );
}

/// The backwards-wired volume pot muddies the tone as it comes down:
/// the series track resistance forms a low-pass against the junction's
/// capacitances. HF/LF at full volume must clearly exceed HF/LF at low
/// volume on the Normal channel (which has no bright cap to mask it).
#[test]
fn volume_down_darkens_normal_channel() {
    let full = hf_lf_ratio(1.0, 0.0, 1.0, 0);
    let low = hf_lf_ratio(0.25, 0.0, 1.0, 0);
    assert!(
        full > low * 2.0,
        "norm vol=1.0 HF/LF ({}) must exceed vol=0.25 HF/LF ({}) by ≥6 dB",
        full,
        low
    );
}

/// The 500 pF bypass feeds pre-volume Bright-channel treble through the
/// tone pot straight into the junction. At low volume with tone up, the
/// Bright channel must therefore keep far more relative treble than the
/// Normal channel at identical settings.
#[test]
fn bright_channel_keeps_treble_at_low_volume() {
    let bright_ratio = hf_lf_ratio(0.0, 0.15, 1.0, 1);
    let norm_ratio = hf_lf_ratio(0.15, 0.0, 1.0, 0);
    assert!(
        bright_ratio > norm_ratio * 3.0,
        "bright HF/LF at low vol ({}) must far exceed normal HF/LF ({})",
        bright_ratio,
        norm_ratio
    );
}

/// The bypass is tone-gated: rolling the tone down inserts the full
/// 1 MΩ track into the injection path, so the Bright channel's low-volume
/// treble retention must collapse with the tone control.
#[test]
fn bright_bypass_is_tone_gated() {
    let tone_up = hf_lf_ratio(0.0, 0.15, 1.0, 1);
    let tone_down = hf_lf_ratio(0.0, 0.15, 0.0, 1);
    assert!(
        tone_up > tone_down * 2.0,
        "bright bypass with tone up ({}) must exceed tone down ({}) by ≥6 dB",
        tone_up,
        tone_down
    );
}

/// "The tone knob acts like a second volume": on the Bright channel at
/// moderate volume, treble-band level must rise appreciably as the tone
/// control opens, because the injection path carries real signal power.
#[test]
fn tone_up_boosts_bright_channel_level() {
    let mut net_up = build_network();
    settle_pots(&mut net_up, 0.0, 0.5, 1.0);
    let rms_up = measure_rms(&mut net_up.circuit, 1, 2_000.0, 1.0, 4_800, 9_600);

    let mut net_down = build_network();
    settle_pots(&mut net_down, 0.0, 0.5, 0.3);
    let rms_down = measure_rms(&mut net_down.circuit, 1, 2_000.0, 1.0, 4_800, 9_600);

    assert!(
        rms_up > rms_down * 1.3,
        "tone=1.0 level at 2 kHz ({}) must exceed tone=0.3 ({}) by ≥30%",
        rms_up,
        rms_down
    );
}

/// The coupling caps and tone caps block DC; the volume pot tracks are
/// V2A's only DC grid-leak path (1 MΩ ∥ 1 MΩ = 500 kΩ to ground). A
/// constant drive must therefore not appear at the junction as DC.
#[test]
fn network_blocks_dc_at_v2a_grid() {
    let mut net = build_network();
    settle_pots(&mut net, 0.5, 0.5, 0.5);

    for _ in 0..((1.5 * SAMPLE_RATE) as usize) {
        let _ = net.circuit.process(&[10.0, 0.0]);
    }

    let mut sum = 0.0_f32;
    let n = 480usize;
    for _ in 0..n {
        sum += net.circuit.process(&[10.0, 0.0]);
    }
    let mean = sum / n as f32;

    // The grid-conduction stamp's bias point produces a small but finite
    // residual; assert |mean| < 50 mV to keep noise off the test.
    assert!(
        mean.abs() < 0.05,
        "V2A grid DC steady-state = {} V (expected ≈ 0 with caps blocking DC)",
        mean
    );
}
