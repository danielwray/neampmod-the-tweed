use nih_plug::prelude::*;
use std::sync::{Arc, Mutex, atomic};
use std::sync::atomic::Ordering;

#[cfg(feature = "gui")]
mod gui;

// SDK building blocks (crate root) — engine wrapper, rate identity, amp
// electrical core, preamp supply brokerage, acoustic chain, RT contract.
use thermionicdsp::{
    AmpIo,
    AmpTopology,
    AmpTopologyConfig,
    CabBackEnd,
    PreampSupply,
    PreampTap,
    enable_audio_thread_denormal_handling,
    EngineRate,
    OversamplingFactor,
    InnerDspProcessor,
    AnyDspEngine,
    SpeakerCabRoomProcessor,
    SpeakerCabRoomConfig,
    MicChannel,
    MicChannelConfig,
};
// Amp domain — electrical path at the oversampled inner rate, physical volts.
use thermionicdsp::amp::{
    BiasSpec,
    LoadLineConfig,
    LoadLineTopology,
    TubeStage,
    TubeStageConfig,
    SharedCathodeTriodePair,
    SharedCathodeTriodePairConfig,
    BPlusTap,
    InputCalibration,
    InputLevelMeter,
    JackInput,
    PotTaper,
    PotTaperConfig,
    SpeakerWiring,
};
// Acoustic domain — output transduction, mic placement, IR convolution.
use thermionicdsp::acoustic::{
    HotSwapConvolver,
    IrLoader,
    LoadboxDi,
    MicrophonePlacement,
    ZeroLatencyConvolver,
};
// Circuit primitives — MNA solver and passive couplings.
use thermionicdsp::circuit::{
    CouplingCapacitor, GridBiasType, GridConductionConfig, MnaCircuit,
    MnaCircuitBuilder, PotHandle, PotSmoother, GND,
};
// Tube specimen catalogue — per-specimen physics for construction-time LUTs.
use thermionicdsp::catalogue::specimen_physics;

const IR_CROSSFADE_MS: f32 = 30.0;

// GUI metering cadence: plate/B+/level values are accumulated over this
// window and published to the GUI atomics once per window, rather than
// every buffer.
const METER_UPDATE_INTERVAL_MS: f32 = 100.0;

// Must match the OT-load speaker in `AmpTopologyConfig::tweed_5e3()` —
// the 5E3 shipped with a Jensen P12R, open-back 1x12.
const DEFAULT_SPEAKER_ID: &str = "p12r";
const DEFAULT_CABINET_ID: &str = "tweed_5e3_open_back_1x12";

// 5E3 input jack: each jack carries a 68kΩ grid stopper between tip and grid,
// with a 1MΩ grid leak at the grid node.
const JACK_SERIES_OHMS: f32 = 68_000.0;
const JACK_GRID_LEAK_OHMS: f32 = 1_000_000.0;

// Exit-boundary DC blocker corner. Host rate — `AmpIo` builds it through
// `DCBlocker::host_rate`.
const OUTPUT_DC_BLOCK_HZ: f32 = 10.0;

// `OutputHealth::run_length` above this means an interior integrator has
// latched and the amp is silent until rebuilt, rather than a one-off
// non-finite sample that already recovered (contract §5).
const LATCHED_SAMPLES: u32 = 512;

pub struct IrLoadState {
    pub pending: Mutex<Option<ZeroLatencyConvolver>>,
    pub sample_rate: atomic_float::AtomicF32,
    pub block_size: atomic::AtomicUsize,
    pub status: atomic::AtomicU8,
}

pub mod ir_load_status {
    pub const LOADING: u8 = 0;
    pub const LOADED: u8 = 1;
    pub const FAILED: u8 = 2;
    pub const NO_IR: u8 = 3;
}

impl IrLoadState {
    pub fn new() -> Self {
        Self {
            pending: Mutex::new(None),
            sample_rate: atomic_float::AtomicF32::new(48_000.0),
            block_size: atomic::AtomicUsize::new(512),
            status: atomic::AtomicU8::new(ir_load_status::NO_IR),
        }
    }

    pub fn set_audio_format(&self, sample_rate: f32, block_size: usize) {
        self.sample_rate.store(sample_rate, Ordering::Relaxed);
        self.block_size.store(block_size, Ordering::Relaxed);
    }
}

impl Default for IrLoadState {
    fn default() -> Self { Self::new() }
}

// Hot-swap slot: the GUI builds a fresh Dynamic cab arm and writes it here;
// the audio thread try_locks once per buffer and moves it in. The whole
// `CabBackEnd` is assembled off-thread — including its `Box` — so the audio
// thread only ever performs a move (contract §4).
pub struct CabProcessorLoadState {
    pub pending: Mutex<Option<CabBackEnd>>,
}

impl CabProcessorLoadState {
    pub fn new() -> Self {
        Self { pending: Mutex::new(None) }
    }
}

impl Default for CabProcessorLoadState {
    fn default() -> Self { Self::new() }
}

// Panics on unknown registry ids — callers must pass ids present in the
// relevant compile-time registry.
pub fn build_cab_processor(
    sample_rate: f32,
    max_buffer_size: usize,
    speaker_id: &str,
    cabinet_id: &str,
    microphone_id: &str,
    room: RoomSelection,
    placement: MicrophonePlacement,
) -> SpeakerCabRoomProcessor {
    let room_id = room.registry_id().map(str::to_string);
    let room_enabled = room_id.is_some();
    SpeakerCabRoomProcessor::new(
        sample_rate,
        max_buffer_size,
        SpeakerCabRoomConfig {
            // 5E3 is a 1x12 — single driver, matches the OT-load speaker.
            speaker_wiring: SpeakerWiring::single(speaker_id),
            cabinet_id: cabinet_id.to_string(),
            mic_primary: MicChannelConfig {
                microphone_id: microphone_id.to_string(),
                placement,
                preamp_gain_db: 35.0,
                polarity_inverted: false,
                pan: 0.0,
            },
            // The 5E3 is a one-mic rig; `None` is the real absence.
            mic_secondary: None,
            room_id,
            // Lockstep with LoadboxDi's -10 dB pad so both cab arms land
            // in the same dBFS region.
            speaker_enabled: true,
            cabinet_enabled: true,
            mic_enabled: true,
            room_enabled,
            response_enabled: true,
        },
    )
}

/// The `Dynamic` cab arm, assembled whole. Boxing happens here — off the
/// audio thread — so installing a rebuilt arm is a move, not an allocation.
///
/// `CabBackEnd` is what makes the transduction pairing unforgeable: this arm
/// transduces internally (mic sensitivity × strip × converter) and therefore
/// must never carry a `LoadboxDi`, while the `Ir` arm cannot exist without
/// one. That used to be a comment; now the type holds it.
pub fn build_cab_arm(
    sample_rate: f32,
    max_buffer_size: usize,
    speaker_id: &str,
    cabinet_id: &str,
    microphone_id: &str,
    room: RoomSelection,
    placement: MicrophonePlacement,
) -> CabBackEnd {
    CabBackEnd::Dynamic(Box::new(build_cab_processor(
        sample_rate,
        max_buffer_size,
        speaker_id,
        cabinet_id,
        microphone_id,
        room,
        placement,
    )))
}

pub fn load_ir_file_into_state(state: &IrLoadState, path: &std::path::Path) {
    state.status.store(ir_load_status::LOADING, Ordering::Relaxed);

    let sample_rate = state.sample_rate.load(Ordering::Relaxed);
    let block_size = state.block_size.load(Ordering::Relaxed);

    let loader = IrLoader::new(sample_rate);
    match loader.load_from_file(path) {
        Ok((mut ir, _, _)) => {
            IrLoader::remove_dc_offset(&mut ir);
            IrLoader::normalize_response_peak(&mut ir);
            let fir_len = 128.min(block_size);
            let conv = ZeroLatencyConvolver::new(&ir, block_size, fir_len);
            if let Ok(mut pending) = state.pending.lock() {
                *pending = Some(conv);
            }
            state.status.store(ir_load_status::LOADED, Ordering::Relaxed);
        }
        Err(_) => {
            state.status.store(ir_load_status::FAILED, Ordering::Relaxed);
        }
    }
}

fn v2s_dial_1_to_12() -> Arc<dyn Fn(f32) -> String + Send + Sync> {
    Arc::new(move |value: f32| {
        let dial = 1.0 + value * 11.0;
        if dial < 10.0 {
            format!("{:.1}", dial)
        } else {
            format!("{:.0}", dial.round())
        }
    })
}

fn s2v_dial_1_to_12() -> Arc<dyn Fn(&str) -> Option<f32> + Send + Sync> {
    Arc::new(|string: &str| {
        let dial: f32 = string.trim().parse().ok()?;
        Some(((dial - 1.0) / 11.0).clamp(0.0, 1.0))
    })
}

#[derive(Enum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelMode {
    #[id = "normal"]
    #[name = "Normal"]
    Normal,
    #[id = "both"]
    #[name = "Both"]
    Both,
    #[id = "bright"]
    #[name = "Bright"]
    Bright,
}

#[derive(Enum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum CabModellingMode {
    #[id = "ir"]
    #[name = "IR"]
    Ir,
    #[id = "dynamic"]
    #[name = "Dynamic"]
    Dynamic,
}

#[derive(Enum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicXPosition {
    #[id = "cap"]
    #[name = "Cap"]
    Cap,
    #[id = "cap_edge"]
    #[name = "Cap Edge"]
    CapEdge,
    #[id = "cone"]
    #[name = "Cone"]
    Cone,
    #[id = "cone_edge"]
    #[name = "Cone Edge"]
    ConeEdge,
}

impl MicXPosition {
    // Radial offset from speaker centre, in cm — calibrated for a 12" driver.
    pub fn radial_offset_cm(self) -> f32 {
        match self {
            MicXPosition::Cap => 0.0,
            MicXPosition::CapEdge => 3.0,
            MicXPosition::Cone => 8.0,
            MicXPosition::ConeEdge => 14.0,
        }
    }
}

// Kept in sync with `assets/config/microphones/v1/*.toml` in the engine.
#[derive(Enum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicSelection {
    #[id = "dynamic_57"]
    #[name = "Shure SM57"]
    Dynamic57,
    #[id = "dynamic_421"]
    #[name = "Sennheiser MD 421-II"]
    Dynamic421,
    #[id = "ribbon_121"]
    #[name = "Royer R-121 Ribbon"]
    Ribbon121,
    #[id = "condenser_87"]
    #[name = "Neumann U 87 Ai (Cardioid)"]
    Condenser87,
    #[id = "ribbon_44"]
    #[name = "RCA 44-BX"]
    Ribbon44,
    #[id = "ribbon_77"]
    #[name = "RCA 77-DX"]
    Ribbon77,
}

impl MicSelection {
    pub fn registry_id(self) -> &'static str {
        match self {
            MicSelection::Dynamic57 => "dynamic_57",
            MicSelection::Dynamic421 => "dynamic_421",
            MicSelection::Ribbon121 => "ribbon_121",
            MicSelection::Condenser87 => "condenser_87",
            MicSelection::Ribbon44 => "ribbon_44",
            MicSelection::Ribbon77 => "ribbon_77",
        }
    }
}

// Maps each variant to an engine room-registry id plus a wet on/off flag.
#[derive(Enum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoomSelection {
    #[id = "none"]
    #[name = "None"]
    None,
    #[id = "small_studio"]
    #[name = "Small Studio"]
    SmallStudio,
    #[id = "large_studio"]
    #[name = "Large Studio"]
    LargeStudio,
    #[id = "live_room"]
    #[name = "Live Room"]
    LiveRoom,
    #[id = "small_bedroom"]
    #[name = "Small Bedroom"]
    SmallBedroom,
    #[id = "wooden_barn"]
    #[name = "Wooden Barn"]
    WoodenBarn,
    #[id = "iso_box"]
    #[name = "Iso Box"]
    IsoBox,
}

impl RoomSelection {
    /// The engine `room_id` this selection names, or `None` for no room at
    /// all — the engine then builds no FDN, rather than constructing one and
    /// bypassing it behind a flag.
    pub fn registry_id(self) -> Option<&'static str> {
        match self {
            RoomSelection::None => None,
            RoomSelection::SmallStudio => Some("small_studio"),
            RoomSelection::LargeStudio => Some("large_studio"),
            RoomSelection::LiveRoom => Some("live_room"),
            RoomSelection::SmallBedroom => Some("small_bedroom"),
            RoomSelection::WoodenBarn => Some("wooden_barn"),
            RoomSelection::IsoBox => Some("iso_box"),
        }
    }
}

#[derive(Params)]
struct TheTweedParams {
    #[id = "bright_volume"]
    pub bright_volume: FloatParam,

    #[id = "normal_volume"]
    pub normal_volume: FloatParam,

    #[id = "channel_select"]
    pub channel_select: EnumParam<ChannelMode>,

    #[id = "tone"]
    pub tone: FloatParam,

    #[id = "power"]
    pub power: BoolParam,

    #[id = "tube_toggle"]
    pub tube_toggle: BoolParam,

    #[id = "input_trim"]
    pub input_trim_db: FloatParam,

    #[id = "output_trim"]
    pub output_trim_db: FloatParam,

    #[id = "cab_modelling_mode"]
    pub cab_modelling_mode: EnumParam<CabModellingMode>,

    #[id = "mic_x_position"]
    pub mic_x_position: EnumParam<MicXPosition>,

    #[id = "mic_distance_inches"]
    pub mic_distance_inches: FloatParam,

    // Cabinet and speaker are locked to the 5E3 defaults, not exposed as params.
    #[id = "microphone"]
    pub microphone: EnumParam<MicSelection>,

    #[id = "room_selection"]
    pub room_selection: EnumParam<RoomSelection>,

    #[persist = "ir_path"]
    pub ir_file_path: Arc<Mutex<String>>,

    // Persisted as a string proxy since OversamplingFactor doesn't
    // implement Serialize/Deserialize. Applied on next plugin reload.
    #[persist = "oversampling_factor"]
    pub oversampling_factor: Arc<Mutex<String>>,
}


impl Default for TheTweedParams {
    fn default() -> Self {
        Self {
            bright_volume: FloatParam::new(
                "Bright",
                0.96,
                FloatRange::Linear { min: 0.01, max: 1.0 },
            )
            .with_smoother(SmoothingStyle::Logarithmic(10.0))
            .with_value_to_string(v2s_dial_1_to_12())
            .with_string_to_value(s2v_dial_1_to_12()),

            normal_volume: FloatParam::new(
                "Normal",
                0.25,
                FloatRange::Linear { min: 0.01, max: 1.0 },
            )
            .with_smoother(SmoothingStyle::Logarithmic(10.0))
            .with_value_to_string(v2s_dial_1_to_12())
            .with_string_to_value(s2v_dial_1_to_12()),

            channel_select: EnumParam::new("Channel", ChannelMode::Normal),

            tone: FloatParam::new(
                "Tone",
                0.45,
                FloatRange::Linear { min: 0.01, max: 1.0 },
            )
            .with_smoother(SmoothingStyle::Logarithmic(5.0))
            .with_value_to_string(v2s_dial_1_to_12())
            .with_string_to_value(s2v_dial_1_to_12()),

            power: BoolParam::new("Power", true),

            tube_toggle: BoolParam::new("Tube Toggle", false),

            input_trim_db: FloatParam::new(
                "Input Trim",
                0.0,
                FloatRange::Linear { min: -18.0, max: 12.0 },
            )
            .with_unit(" dB")
            .with_step_size(0.1)
            .with_smoother(SmoothingStyle::Linear(5.0))
            .with_value_to_string(formatters::v2s_f32_rounded(1))
            .with_string_to_value(Arc::new(|s: &str| s.trim().parse().ok())),

            // Default −6 dB, and it is compensation for a known engine
            // defect, not a voicing choice. The dynamic arm's fixed 35 dB
            // channel strip ignores mic sensitivity, so a full-crank 5E3
            // burst lands past digital full scale on four of the six
            // selectable mics (ThermionicDSP
            // `tests/output_transduction_landing_probe.rs`, ignored, and
            // `docs/plans/cabs/mic-room-audit.md` §1 — peak dBFS: U87 +18.2,
            // R-121 +11.3, 77-DX +5.3, 44-BX +2.3, MD 421 −4.1, SM57 −9.7).
            // −6 dB is sized for the DEFAULT mic (RCA 77-DX, +5.3), putting
            // the default configuration just under FS; a U87 still needs
            // roughly −19 dB from the user until the engine lands per-mic
            // gain anchors (multi-mic.md §8.2), after which this default
            // should return to 0.0.
            //
            // This is deliberately a visible, attributable trim at the
            // transduction boundary rather than the fictional Master pot it
            // replaces: the 5E3 has no master volume, and per-amp output
            // trims and loudness normalisation are forbidden inside the
            // signal path (the-contract.md §6).
            output_trim_db: FloatParam::new(
                "Output Trim",
                0.0,
                FloatRange::Linear { min: -24.0, max: 0.0 },
            )
            .with_unit(" dB")
            .with_step_size(0.1)
            .with_smoother(SmoothingStyle::Linear(5.0))
            .with_value_to_string(formatters::v2s_f32_rounded(1))
            .with_string_to_value(Arc::new(|s: &str| s.trim().parse().ok())),

            cab_modelling_mode: EnumParam::new(
                "Cab Modelling",
                CabModellingMode::Dynamic,
            ),

            mic_x_position: EnumParam::new("Mic X", MicXPosition::CapEdge),

            mic_distance_inches: FloatParam::new(
                "Mic Distance",
                3.0,
                FloatRange::Linear { min: 0.1, max: 24.0 },
            )
            .with_unit(" in")
            .with_step_size(0.1)
            .with_smoother(SmoothingStyle::Linear(20.0))
            .with_value_to_string(formatters::v2s_f32_rounded(1))
            .with_string_to_value(Arc::new(|s: &str| s.trim().parse().ok())),

            microphone: EnumParam::new("Mic", MicSelection::Dynamic57),
            room_selection: EnumParam::new(
                "Room",
                RoomSelection::SmallStudio,
            ),

            ir_file_path: Arc::new(Mutex::new(String::new())),

            // Default OS factor: X4 (matches the engine's production default).
            oversampling_factor: Arc::new(Mutex::new(
                os_factor_str(OversamplingFactor::X4).to_string(),
            )),
        }
    }
}

// =============================================================================
// OS-factor helpers (string proxy used for NIH-plug `#[persist]` serialisation)
// =============================================================================

pub fn parse_os_factor(s: &str) -> OversamplingFactor {
    match s {
        "X1" => OversamplingFactor::X1,
        "X4" => OversamplingFactor::X4,
        "X8" => OversamplingFactor::X8,
        // "X2" is default
        _ => OversamplingFactor::X2,
    }
}

pub fn os_factor_str(f: OversamplingFactor) -> &'static str {
    match f {
        OversamplingFactor::X1 => "X1",
        OversamplingFactor::X2 => "X2",
        OversamplingFactor::X4 => "X4",
        OversamplingFactor::X8 => "X8",
    }
}

pub fn os_factor_label(f: OversamplingFactor) -> &'static str {
    match f {
        OversamplingFactor::X1 => "1x (none)",
        OversamplingFactor::X2 => "2x",
        OversamplingFactor::X4 => "4x",
        OversamplingFactor::X8 => "8x",
    }
}

fn build_5e3_amp_topology_config() -> AmpTopologyConfig {
    let mut config = AmpTopologyConfig::tweed_5e3();
    config.power_section.transformer_spec = OT_SPEC.into();
    config.power_supply.sag.rectifier_spec = Some(RECTIFIER_SPEC.into());
    config
}

const PREAMP_BPLUS_5E3: f32 = 250.0;

// Atomic specimen ids — MLUTs generate at construction and memoise process-wide.
const V1_STOCK_SPECIMEN: &str = "ge_12ay7";
const V1_MOD_SPECIMEN: &str = "ge_12ax7";
const V2A_SPECIMEN: &str = "ge_12ax7";
const RECTIFIER_SPEC: &str = "ge_5y3";
const OT_SPEC: &str = "sst_108";
const V1_CATHODE_R: f32 = 820.0;
const V1_CATHODE_CAP: f32 = 25.0;
const V2A_CATHODE_R: f32 = 1500.0;
const V2A_CATHODE_CAP: f32 = 25.0;
const V2A_TO_V2B_COUPLING_CAP_F: f32 = 0.02e-6;
const V2B_GRID_LEAK_OHMS: f32 = 1_000_000.0;

// 5E3 preamp plate load — 100 kΩ on both V1 triodes and V2A per the print.
const PREAMP_PLATE_R_OHMS: f32 = 100_000.0;
// Quiescent V_gk at the 100 kΩ / 330 V generation operating point.
const V1_STOCK_BIAS_V: f64 = -1.5; // 12AY7
const V1_MOD_BIAS_V: f64 = -1.2; // 12AX7
const V2A_BIAS_V: f64 = -1.2; // 12AX7

const POT_SMOOTH_TAU_S: f32 = 0.020;

/// 5E3 preamp-triode load line: 100 kΩ plate load, 330 V LUT reference B+.
/// `cathode_resistor_ohms` stays 0.0 — cathode degeneration is modelled at
/// runtime from each stage's own Rk‖Ck config; baking it into the LUT would
/// double-count it.
fn preamp_load_line(reference_bias: f64) -> LoadLineConfig {
    LoadLineConfig {
        topology: LoadLineTopology::Triode {
            plate_operating_points: vec![
                1.0, 0.95, 0.9, 0.85, 0.8, 0.75, 0.71, 0.67,
                0.63, 0.59, 0.55, 0.51, 0.47, 0.43, 0.39, 0.35,
            ],
        },
        plate_resistor_ohms: PREAMP_PLATE_R_OHMS as f64,
        cathode_resistor_ohms: 0.0,
        reference_bplus: 330.0,
        reference_bias: BiasSpec::Authored(reference_bias),
    }
}

fn build_preamp_tube(
    engine_rate: EngineRate,
    specimen_id: &str,
    load_line: &LoadLineConfig,
    cathode_resistor_ohms: f32,
    cathode_bypass_cap_uf: Option<f32>,
) -> TubeStage {
    let config = TubeStageConfig::default()
        .with_cathode_circuit(cathode_resistor_ohms, cathode_bypass_cap_uf);
    let mut stage = TubeStage::from_specimen_id(engine_rate, specimen_id, load_line, config)
        .unwrap_or_else(|e| {
            panic!("Failed to build tube from specimen '{}': {}", specimen_id, e)
        });
    stage.set_plate_bplus_voltage(PREAMP_BPLUS_5E3);
    stage
}

// V1 (12AY7 stock / 12AX7 mod) is one shared-cathode triode pair: triode A
// is the Normal grid, triode B is Bright, sharing an 820Ω/25µF cathode RC.
// The shared cathode integrator is what produces the 5E3's cross-channel
// ducking — a hard drive into one grid biases both triodes toward cutoff.
fn build_v1_pair(
    engine_rate: EngineRate,
    specimen_id: &str,
    reference_bias: f64,
) -> SharedCathodeTriodePair {
    let config = SharedCathodeTriodePairConfig {
        shared_cathode_resistor_ohms: V1_CATHODE_R,
        shared_cathode_bypass_cap_uf: Some(V1_CATHODE_CAP),
        shared_cathode_bypass_dielectric: Some("electrolytic_vintage".into()),
        plate_resistor_a_ohms: PREAMP_PLATE_R_OHMS,
        plate_resistor_b_ohms: PREAMP_PLATE_R_OHMS,
        tube_mismatch: Some(0.05),
        linear_blend_threshold: None,
        plate_voltage_fraction: 1.0,
    };
    let mut pair = SharedCathodeTriodePair::from_specimen_id(
        engine_rate,
        specimen_id,
        &preamp_load_line(reference_bias),
        config,
    )
    .unwrap_or_else(|e| panic!("V1 pair build for '{}': {}", specimen_id, e));
    pair.set_plate_bplus_voltage(PREAMP_BPLUS_5E3);
    pair
}

// V2A's grid network (backwards-wired volume pots + bright-cap treble
// bypass + 3-terminal tone pot + grid conduction) is solved standalone and
// its output fed to this stage — see `V2aGridNetwork` below. V2A→V2B is a
// plain 0.02µF/1MΩ coupling cap.
pub fn build_v2a_tube(engine_rate: EngineRate) -> TubeStage {
    build_preamp_tube(
        engine_rate,
        V2A_SPECIMEN,
        &preamp_load_line(V2A_BIAS_V),
        V2A_CATHODE_R,
        Some(V2A_CATHODE_CAP),
    )
}

// Tube-plate Thévenin source impedance: R_load ∥ r_p — r_p from the atomic
// specimen (datasheet typical operation), plate load from the 5E3 print.
fn plate_source_impedance(specimen_id: &str) -> f32 {
    let rp = specimen_physics(specimen_id)
        .unwrap_or_else(|e| panic!("V1 specimen '{}': {}", specimen_id, e))
        .rp
        .unwrap_or_else(|| panic!("V1 specimen '{}' cites no [circuit].rp", specimen_id));
    rp * PREAMP_PLATE_R_OHMS / (rp + PREAMP_PLATE_R_OHMS)
}

// Passive subcircuit between the V1 plates and V2A's grid
pub struct V2aGridNetwork {
    pub circuit: MnaCircuit,
    pub norm_volume: PotHandle,
    pub bright_volume: PotHandle,
    pub tone: PotHandle,
}

impl V2aGridNetwork {
    pub const COUPLING_CAP_F: f32 = 0.1e-6;
    pub const VOLUME_POT_OHMS: f32 = 1_000_000.0;
    pub const BRIGHT_CAP_F: f32 = 500e-12;
    pub const TONE_POT_OHMS: f32 = 1_000_000.0;
    pub const TONE_CAP_F: f32 = 5e-9;

    /// `v2a_quiescent_bias_volts` is V2A's solved quiescent V_gk — read it
    /// from the built stage (`lut_quiescent_bias_volts`), don't author a
    /// second copy. The circuit must stay standalone — never pass it to
    /// `set_grid_circuit`; see [`TweedInner::grid_circuit`].
    pub fn new(
        engine_rate: EngineRate,
        v1_source_z_ohms: f32,
        v2a_quiescent_bias_volts: f32,
    ) -> Self {
        let v2a = specimen_physics(V2A_SPECIMEN)
            .unwrap_or_else(|e| panic!("V2A specimen '{}': {}", V2A_SPECIMEN, e));

        let mut b = MnaCircuitBuilder::new(engine_rate);

        let (v1a, _drv_v1a) = b.add_driver("v1a_plate");
        let (v1b, _drv_v1b) = b.add_driver("v1b_plate");

        let v2a_grid = b.node("v2a_grid");

        let v1a_after_src = b.node("v1a_after_src");
        let norm_in = b.node("norm_in");
        b.resistor(v1a, v1a_after_src, v1_source_z_ohms)
            .capacitor(v1a_after_src, norm_in, Self::COUPLING_CAP_F);
        let (norm_volume, _) =
            b.pot(v2a_grid, norm_in, GND, Self::VOLUME_POT_OHMS, 1.0);

        let v1b_after_src = b.node("v1b_after_src");
        let bright_in = b.node("bright_in");
        b.resistor(v1b, v1b_after_src, v1_source_z_ohms)
            .capacitor(v1b_after_src, bright_in, Self::COUPLING_CAP_F);
        let (bright_volume, _) =
            b.pot(v2a_grid, bright_in, GND, Self::VOLUME_POT_OHMS, 0.0);

        let tone_top = b.node("tone_top");
        let tone_bottom = b.node("tone_bottom");
        b.capacitor(bright_in, tone_top, Self::BRIGHT_CAP_F);
        let (tone, _) = b.pot(
            tone_top,
            v2a_grid,
            tone_bottom,
            Self::TONE_POT_OHMS,
            1.0,
        );
        b.capacitor(tone_bottom, GND, Self::TONE_CAP_F);

        b.capacitor(v2a_grid, GND, v2a.miller_c_eff_farads(PREAMP_PLATE_R_OHMS));

        b.grid_conduction(
            v2a_grid,
            GridConductionConfig {
                grid_perveance: v2a.grid_perveance,
                contact_potential: v2a.grid_threshold.abs(),
                bias_type: GridBiasType::CathodeBias {
                    cathode_voltage: -v2a_quiescent_bias_volts,
                },
            },
        );
        b.set_output(v2a_grid);

        let circuit = b
            .build()
            .expect("5E3 V2A grid network is well-formed");

        Self {
            circuit,
            norm_volume,
            bright_volume,
            tone,
        }
    }
}

fn meter_ceiling_for_pair(pair: &SharedCathodeTriodePair, jack_dc_gain: f32) -> f32 {
    pair.voltage_cal().clean_ac_ceiling_volts() / jack_dc_gain
}

// =============================================================================
// TweedInner — per-amp inner-rate DSP processor
// =============================================================================

// Per-inner-sample DSP graph: V1 dual-triode pair (Normal/Bright split) →
// V2A (with attached grid network) → V2A→V2B coupling cap → AmpTopology
// power section (cathodyne PI → push-pull 6V6 → OT).
pub struct TweedInner {
    // Both halves of the pair always process per sample so the shared
    // cathode integrator captures cross-channel ducking correctly.
    pub v1_pair_stock: SharedCathodeTriodePair,
    pub v1_pair_mod: SharedCathodeTriodePair,
    pub current_tube_toggle: bool,

    pub v2a_tube: TubeStage,
    /// The volume/tone network, held standalone rather than attached to V2A
    /// via `set_grid_circuit`: an attached circuit gets its grid-conduction
    /// clamp overwritten each solve with the stage's cathode *deviation*
    /// (zero at idle), so the grid conducts ~1.2 V early and latches V2A
    /// into cutoff.
    pub grid_circuit: MnaCircuit,
    pub grid_norm_handle: PotHandle,
    pub grid_bright_handle: PotHandle,
    pub grid_tone_handle: PotHandle,
    /// The *other* tube's V2A grid network, held ready. The toggle changes
    /// V1's plate source impedance, so there are exactly two possible
    /// networks; both are built at construction and swapped on toggle —
    /// the same shape `v1_pair_stock`/`v1_pair_mod` already uses. Building
    /// one on the toggle would allocate an `MnaCircuit` inside `process`,
    /// which the RT contract forbids and `assert_process_allocs` panics on.
    pub grid_spare: V2aGridNetwork,
    // Ticked at host rate in `begin_host_sample`; targets set per-buffer.
    pub norm_smoother: PotSmoother,
    pub bright_smoother: PotSmoother,
    pub tone_smoother: PotSmoother,
    // V2A plate → V2B (cathodyne) grid coupling: 0.02µF/1MΩ, ~8Hz HP.
    pub coupling_v2a: CouplingCapacitor,

    pub amp_topology: AmpTopology,
    /// Rail reads and the per-buffer sag return for the caller-owned preamp
    /// graph (V1 pair + V2A, all on the B+3 node).
    pub supply: PreampSupply,
    pub preamp_tap: PreampTap,
    pub power_tube_tap: BPlusTap,

    pub current_channel_mode: ChannelMode,

    pub meter_this_host_sample: bool,
    pub v1_plate_sum: f32,
    pub v2_plate_sum: f32,
    pub v3v4_plate_sum: f32,
    pub plate_samples_counted: u32,
}

impl TweedInner {
    /// Exchange the live V2A grid network for the other tube's. The
    /// incoming circuit is reset so a toggle starts from rest, matching the
    /// freshly-built circuit this replaced; the caller re-applies the
    /// current wiper positions afterwards.
    pub fn swap_grid_network(&mut self) {
        std::mem::swap(&mut self.grid_circuit, &mut self.grid_spare.circuit);
        std::mem::swap(&mut self.grid_norm_handle, &mut self.grid_spare.norm_volume);
        std::mem::swap(
            &mut self.grid_bright_handle,
            &mut self.grid_spare.bright_volume,
        );
        std::mem::swap(&mut self.grid_tone_handle, &mut self.grid_spare.tone);
        self.grid_circuit.reset();
    }

    fn reset_plate_meters(&mut self) {
        self.v1_plate_sum = 0.0;
        self.v2_plate_sum = 0.0;
        self.v3v4_plate_sum = 0.0;
        self.plate_samples_counted = 0;
    }
}

impl InnerDspProcessor for TweedInner {
    fn begin_buffer(&mut self, n: usize) {
        self.supply.begin_buffer(&mut self.amp_topology, n);
        // Plate meters deliberately NOT reset here — they accumulate
        // across buffers until the metering window closes.
    }

    fn begin_host_sample(&mut self) {
        self.supply.begin_host_sample(&mut self.amp_topology);
        self.meter_this_host_sample = true;

        // Wiper values hold across OS sub-samples (zero-order hold).
        let n = self.norm_smoother.tick();
        let b = self.bright_smoother.tick();
        let t = self.tone_smoother.tick();
        self.grid_circuit.set_pot_position(self.grid_norm_handle, n);
        self.grid_circuit.set_pot_position(self.grid_bright_handle, b);
        self.grid_circuit.set_pot_position(self.grid_tone_handle, t);
    }

    fn process_inner(&mut self, input: f32) -> f32 {
        let b_plus_preamp = self.supply.rail(&mut self.amp_topology, self.preamp_tap);

        // Split host input into per-triode feeds by channel mode.
        let v1a_input = if self.current_channel_mode != ChannelMode::Bright {
            input
        } else {
            0.0
        };
        let v1b_input = if self.current_channel_mode != ChannelMode::Normal {
            input
        } else {
            0.0
        };

        let v1_out = if self.current_tube_toggle {
            self.v1_pair_mod
                .process_pair(v1a_input, v1b_input, b_plus_preamp)
        } else {
            self.v1_pair_stock
                .process_pair(v1a_input, v1b_input, b_plus_preamp)
        };

        // Driver order matches `V2aGridNetwork::new`; the output node is V2A's grid.
        let v2a_grid = self
            .grid_circuit
            .process(&[v1_out.plate_a_ac_volts, v1_out.plate_b_ac_volts]);

        let v2a_out = self
            .v2a_tube
            .process(v2a_grid, b_plus_preamp)
            .plate_ac_volts;

        let pi_input = self.coupling_v2a.process(v2a_out);

        let ot_volts = self.amp_topology.process_power_section(pi_input);

        // Meter the plates
        if self.meter_this_host_sample {
            self.meter_this_host_sample = false;
            let v1_active = if self.current_tube_toggle {
                &self.v1_pair_mod
            } else {
                &self.v1_pair_stock
            };
            self.v1_plate_sum += v1_active.instantaneous_plate_a_volts();
            self.v2_plate_sum += self.v2a_tube.instantaneous_plate_volts();
            self.v3v4_plate_sum += self
                .amp_topology
                .last_diag()
                .power_section
                .power_tube_pos
                .plate_voltage_volts;
            self.plate_samples_counted += 1;
        }

        // All three preamp triodes hang off the same B+3 node, so their
        // currents sum into one drain.
        self.supply.draw(
            self.preamp_tap,
            v1_out.plate_a_current_amps
                + v1_out.plate_b_current_amps
                + self.v2a_tube.plate_current_amps(),
        );

        ot_volts
    }

    fn end_buffer(&mut self) {
        self.supply.end_buffer(&mut self.amp_topology);
    }

    fn reset(&mut self) {
        self.v1_pair_stock.reset();
        self.v1_pair_mod.reset();
        self.grid_circuit.reset();
        self.v2a_tube.reset();
        self.coupling_v2a.reset();
        self.amp_topology.reset();
        self.meter_this_host_sample = false;
        self.reset_plate_meters();
    }
}

pub struct AudioState {
    pub engine_rate: EngineRate,
    pub engine: AnyDspEngine<TweedInner>,

    /// Entry and exit boundary, owned whole: input meter (raw sample) →
    /// calibration → jack divider on the way in; output trim + NaN/Inf guard
    /// → host-rate DC blockers on the way out. Also the only route to
    /// [`AmpIo::output_health`], which a hand-composed exit cannot see.
    pub amp_io: AmpIo,

    /// Both arms stay resident and the active one is fed, so a mode switch is
    /// instant rather than a rebuild (contract §4).
    pub cab_dynamic: CabBackEnd,
    pub cab_ir: CabBackEnd,

    pub pre_cab_buffer: Vec<f32>,
    pub post_cab_left: Vec<f32>,
    pub post_cab_right: Vec<f32>,
    pub ir_block_size: usize,
    pub ir_crossfade_samples: usize,

    pub meter_window_len: usize,
    pub meter_window_samples: usize,
    pub meter_output_peak: f32,
}

impl AudioState {
    pub(crate) fn build(
        sample_rate: f32,
        max_buffer_size: usize,
        os_factor: OversamplingFactor,
        params: &TheTweedParams,
        volume_taper: &PotTaperConfig,
        input_trim_db: f32,
    ) -> Self {
        let engine_rate = EngineRate::new(sample_rate, os_factor);

        // The entry pieces are constructed here and handed to `AmpIo`, which
        // owns them from then on — there is no plugin-side copy to drift.
        let mut input_cal = InputCalibration::amp_standard();
        input_cal.set_user_trim_db(input_trim_db);
        let jack_input = JackInput::new(JACK_SERIES_OHMS, JACK_GRID_LEAK_OHMS);

        let v1_pair_stock = build_v1_pair(engine_rate, V1_STOCK_SPECIMEN, V1_STOCK_BIAS_V);
        let v1_pair_mod = build_v1_pair(engine_rate, V1_MOD_SPECIMEN, V1_MOD_BIAS_V);

        let amp_topology =
            AmpTopology::new(engine_rate, build_5e3_amp_topology_config());
        let supply = PreampSupply::new(&amp_topology, &["preamp"])
            .expect("the 5E3 filter chain publishes a 'preamp' rail");
        let preamp_tap = supply.tap("preamp");
        let power_tube_tap = amp_topology.b_plus_tap("power_tube");

        // V1 tube choice sets the plate source-Z baked into the V2A grid
        // network (~21kΩ 12AY7 vs ~38kΩ 12AX7), which shapes the bright
        // cap's HF lift.
        let tube_toggle = params.tube_toggle.value();
        let v1_specimen = if tube_toggle { V1_MOD_SPECIMEN } else { V1_STOCK_SPECIMEN };
        let v1_source_z = plate_source_impedance(v1_specimen);

        let v2a_tube = build_v2a_tube(engine_rate);
        let V2aGridNetwork {
            mut circuit,
            norm_volume,
            bright_volume,
            tone,
        } = V2aGridNetwork::new(
            engine_rate,
            v1_source_z,
            v2a_tube.lut_quiescent_bias_volts(),
        );

        // The toggle's only effect on this network is V1's plate source
        // impedance, so the alternate is built here rather than on the
        // audio thread when the user flips the switch.
        let spare_specimen = if tube_toggle { V1_STOCK_SPECIMEN } else { V1_MOD_SPECIMEN };
        let grid_spare = V2aGridNetwork::new(
            engine_rate,
            plate_source_impedance(spare_specimen),
            v2a_tube.lut_quiescent_bias_volts(),
        );

        let coupling_v2a = CouplingCapacitor::new(
            engine_rate,
            V2A_TO_V2B_COUPLING_CAP_F,
            V2B_GRID_LEAK_OHMS,
        );

        let init_norm = volume_taper.wiper_fraction(params.normal_volume.value());
        let init_bright = volume_taper.wiper_fraction(params.bright_volume.value());
        let init_tone = params.tone.value();

        // Snap wipers to current settings so the smoother starts at steady state.
        circuit.set_pot_position(norm_volume, init_norm);
        circuit.set_pot_position(bright_volume, init_bright);
        circuit.set_pot_position(tone, init_tone);

        let inner = TweedInner {
            v1_pair_stock,
            v1_pair_mod,
            current_tube_toggle: tube_toggle,
            v2a_tube,
            grid_circuit: circuit,
            grid_norm_handle: norm_volume,
            grid_bright_handle: bright_volume,
            grid_tone_handle: tone,
            grid_spare,
            norm_smoother: PotSmoother::new(sample_rate, init_norm, POT_SMOOTH_TAU_S),
            bright_smoother: PotSmoother::new(sample_rate, init_bright, POT_SMOOTH_TAU_S),
            tone_smoother: PotSmoother::new(sample_rate, init_tone, POT_SMOOTH_TAU_S),
            coupling_v2a,
            amp_topology,
            supply,
            preamp_tap,
            power_tube_tap,
            current_channel_mode: params.channel_select.value(),
            meter_this_host_sample: false,
            v1_plate_sum: 0.0,
            v2_plate_sum: 0.0,
            v3v4_plate_sum: 0.0,
            plate_samples_counted: 0,
        };
        let engine = AnyDspEngine::new(engine_rate, inner);

        let meter_ceiling = {
            let inner = engine.inner();
            let pair = if tube_toggle { &inner.v1_pair_mod } else { &inner.v1_pair_stock };
            meter_ceiling_for_pair(pair, jack_input.dc_gain())
        };
        let input_meter =
            InputLevelMeter::new(sample_rate, input_cal.input_scale(), meter_ceiling);

        // `AmpIo::new` takes `host_sr` and keys its exit DC blockers through
        // `DCBlocker::host_rate` internally — they sit after the boundary
        // downsample, so an amp-domain rate would shift the 10 Hz corner by
        // the OS factor (contract §5a).
        let mut amp_io = AmpIo::new(
            sample_rate,
            input_cal,
            jack_input,
            input_meter,
            OUTPUT_DC_BLOCK_HZ,
        );
        amp_io.set_output_trim_db(params.output_trim_db.value());

        let ir_crossfade_samples = (IR_CROSSFADE_MS * sample_rate / 1000.0) as usize;

        // Both arms resident from construction. The `Ir` arm carries its own
        // loadbox and the `Dynamic` arm structurally cannot have one, which
        // is the level-pairing invariant that used to live in a comment.
        let cab_ir = CabBackEnd::Ir {
            convolver: Box::new(HotSwapConvolver::new(&[1.0], max_buffer_size, 1)),
            loadbox: LoadboxDi::standard(),
        };
        let cab_dynamic = build_cab_arm(
            sample_rate,
            max_buffer_size,
            DEFAULT_SPEAKER_ID,
            DEFAULT_CABINET_ID,
            params.microphone.value().registry_id(),
            params.room_selection.value(),
            MicrophonePlacement {
                distance_m: params.mic_distance_inches.value() * 0.0254,
                radial_offset_cm: params
                    .mic_x_position
                    .value()
                    .radial_offset_cm(),
                off_axis_angle_deg: 0.0,
            },
        );

        let meter_window_len =
            ((METER_UPDATE_INTERVAL_MS * sample_rate / 1000.0) as usize).max(1);

        Self {
            engine_rate,
            engine,
            amp_io,
            cab_dynamic,
            cab_ir,
            pre_cab_buffer: vec![0.0; max_buffer_size],
            post_cab_left: vec![0.0; max_buffer_size],
            post_cab_right: vec![0.0; max_buffer_size],
            ir_block_size: max_buffer_size,
            ir_crossfade_samples,
            meter_window_len,
            meter_window_samples: 0,
            meter_output_peak: 0.0,
        }
    }

}

// =============================================================================
// The Tweed 5E3 Plugin
// =============================================================================

pub struct TheTweed {
    params: Arc<TheTweedParams>,

    // 5E3 volume pots are 1MΩ Audio 30A taper.
    volume_taper: PotTaperConfig,

    // The entry/exit boundary and the loadbox live inside `AudioState` now —
    // `AmpIo` owns the calibration, jack and meter, and the `Ir` cab arm owns
    // its `LoadboxDi`. Nothing here mirrors them.
    ir_load_state: Arc<IrLoadState>,

    // Hot-swap slot for the parametric cab chain, populated by the GUI.
    cab_load_state: Arc<CabProcessorLoadState>,

    // `None` until `Plugin::initialize` runs.
    audio_state: Option<AudioState>,

    cached_input_trim_db: f32,

    // Latched mic placement, used to detect param changes per-buffer.
    cached_mic_x_position: MicXPosition,
    cached_mic_distance_inches: f32,

    meter_peak_volts: Arc<atomic_float::AtomicF32>,
    meter_bplus_volts: Arc<atomic_float::AtomicF32>,
    meter_v1_volts: Arc<atomic_float::AtomicF32>,
    meter_v2_volts: Arc<atomic_float::AtomicF32>,
    meter_v3v4_volts: Arc<atomic_float::AtomicF32>,
    meter_output_db: Arc<atomic_float::AtomicF32>,
    // Sticky: set when `AmpIo::output_health` reports a latched run, i.e. an
    // interior integrator has gone non-finite and the amp is silent until
    // rebuilt. Only reachable because `AmpIo` owns the exit boundary.
    meter_output_latched: Arc<atomic::AtomicBool>,
    // 0 = not yet initialized; GUI falls back to X4 display.
    meter_os_ratio: Arc<atomic::AtomicU8>,
}

impl Default for TheTweed {
    fn default() -> Self {
        Self {
            params: Arc::new(TheTweedParams::default()),

            volume_taper: PotTaperConfig::new(PotTaper::Audio30A),

            ir_load_state: Arc::new(IrLoadState::new()),

            cab_load_state: Arc::new(CabProcessorLoadState::new()),

            audio_state: None,

            cached_input_trim_db: 0.0,

            // Match the param defaults so the first buffer doesn't see a
            // spurious change-detect and push a no-op mic-placement update.
            cached_mic_x_position: MicXPosition::CapEdge,
            cached_mic_distance_inches:4.0,

            meter_peak_volts: Arc::new(atomic_float::AtomicF32::new(0.0)),
            meter_bplus_volts: Arc::new(atomic_float::AtomicF32::new(0.0)),
            meter_v1_volts: Arc::new(atomic_float::AtomicF32::new(0.0)),
            meter_v2_volts: Arc::new(atomic_float::AtomicF32::new(0.0)),
            meter_v3v4_volts: Arc::new(atomic_float::AtomicF32::new(0.0)),
            meter_output_db: Arc::new(atomic_float::AtomicF32::new(-120.0)),
            meter_output_latched: Arc::new(atomic::AtomicBool::new(false)),
            // 0 = "not yet initialized"; GUI falls back to X4 display.
            meter_os_ratio: Arc::new(atomic::AtomicU8::new(0)),
        }
    }
}

impl TheTweed {
    pub fn load_ir_from_file(&self, path: &std::path::Path) {
        load_ir_file_into_state(&self.ir_load_state, path);
        if self.ir_load_state.status.load(Ordering::Relaxed) == ir_load_status::LOADED {
            if let Ok(mut p) = self.params.ir_file_path.lock() {
                *p = path.display().to_string();
            }
        }
    }

    // Public so smoke tests can drive construction without nih-plug's
    // InitContext. Idempotent — safe to call repeatedly during state restore.
    pub fn initialize_audio_state(
        &mut self,
        sample_rate: f32,
        max_buffer_size: usize,
        os_factor: OversamplingFactor,
    ) {
        // `AudioState::build` applies the trim to the calibration it hands to
        // `AmpIo`, so the meter's scale is correct on the first buffer.
        let trim_db = self.params.input_trim_db.value();
        self.cached_input_trim_db = trim_db;

        let audio_state = AudioState::build(
            sample_rate,
            max_buffer_size,
            os_factor,
            &self.params,
            &self.volume_taper,
            trim_db,
        );

        self.meter_os_ratio.store(
            audio_state.engine_rate.oversampling.ratio() as u8,
            atomic::Ordering::Relaxed,
        );

        self.audio_state = Some(audio_state);
    }
}

impl Plugin for TheTweed {
    const NAME: &'static str = "The Tweed";
    const VENDOR: &'static str = "neampmod";
    const URL: &'static str = env!("CARGO_PKG_HOMEPAGE");
    const EMAIL: &'static str = env!("CARGO_PKG_AUTHORS");
    const VERSION: &'static str = env!("CARGO_PKG_VERSION");

    // Mono in, stereo out. The amp is a single-ended electrical path, but the
    // acoustic chain is natively stereo — two mic branches, balance-law pan,
    // and a room whose late tail is deliberately decorrelated — so collapsing
    // it to mono discards the engine's own output. There is deliberately no
    // mono path in the engine (SDK-D11) and one will not be added.
    const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[AudioIOLayout {
        main_input_channels: NonZeroU32::new(1),
        main_output_channels: NonZeroU32::new(2),
        ..AudioIOLayout::const_default()
    }];

    const MIDI_INPUT: MidiConfig = MidiConfig::None;
    const SAMPLE_ACCURATE_AUTOMATION: bool = true;

    type SysExMessage = ();
    type BackgroundTask = ();

    fn params(&self) -> Arc<dyn Params> {
        self.params.clone()
    }

    fn initialize(
        &mut self,
        _audio_io_layout: &AudioIOLayout,
        buffer_config: &BufferConfig,
        _context: &mut impl InitContext<Self>,
    ) -> bool {
        let sample_rate = buffer_config.sample_rate;
        let max_buffer_size = buffer_config.max_buffer_size as usize;

        self.params.bright_volume.smoothed.reset(self.params.bright_volume.value());
        self.params.normal_volume.smoothed.reset(self.params.normal_volume.value());
        self.params.tone.smoothed.reset(self.params.tone.value());

        let os_factor = self
            .params
            .oversampling_factor
            .lock()
            .ok()
            .map(|s| parse_os_factor(&s))
            .unwrap_or(OversamplingFactor::X4);

        self.initialize_audio_state(sample_rate, max_buffer_size, os_factor);

        self.ir_load_state.set_audio_format(sample_rate, max_buffer_size);
        self.ir_load_state.status.store(ir_load_status::NO_IR, Ordering::Relaxed);
        if let Ok(mut p) = self.ir_load_state.pending.lock() {
            *p = None;
        }

        if let Ok(mut p) = self.cab_load_state.pending.lock() {
            *p = None;
        }

        let persisted_ir_path = self
            .params
            .ir_file_path
            .lock()
            .map(|p| p.clone())
            .unwrap_or_default();
        if !persisted_ir_path.is_empty() {
            let path = std::path::PathBuf::from(&persisted_ir_path);
            if path.exists() {
                load_ir_file_into_state(&self.ir_load_state, &path);
            } else {
                self.ir_load_state
                    .status
                    .store(ir_load_status::FAILED, Ordering::Relaxed);
            }
        }

        true
    }

    fn reset(&mut self) {
        self.params.bright_volume.smoothed.reset(self.params.bright_volume.value());
        self.params.normal_volume.smoothed.reset(self.params.normal_volume.value());
        self.params.tone.smoothed.reset(self.params.tone.value());
        if let Some(audio) = self.audio_state.as_mut() {
            audio.engine.reset();
            // Both arms reset: the inactive one must not resume with stale
            // convolution or speaker state when the mode is switched back.
            audio.cab_ir.reset();
            audio.cab_dynamic.reset();
            audio.pre_cab_buffer.fill(0.0);
            audio.post_cab_left.fill(0.0);
            audio.post_cab_right.fill(0.0);
            // Resets the input meter and the exit DC blockers; calibration
            // and trim settings are deliberately untouched.
            audio.amp_io.reset();
            audio.amp_io.reset_output_health();
            audio.meter_window_samples = 0;
            audio.meter_output_peak = 0.0;
        }
    }

    fn process(
        &mut self,
        buffer: &mut Buffer,
        _aux: &mut AuxiliaryBuffers,
        _context: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        // Linux hosts (Pipewire, JACK) don't set MXCSR — denormals can
        // otherwise stall the IIR/oversampler/envelope state.
        enable_audio_thread_denormal_handling();

        let audio = self
            .audio_state
            .as_mut()
            .expect("Plugin::process called before successful Plugin::initialize");

        let crossfade = audio.ir_crossfade_samples;
        if let Ok(mut pending) = self.ir_load_state.pending.try_lock() {
            if let Some(new_conv) = pending.take() {
                if let Some(convolver) = audio.cab_ir.convolver_mut() {
                    convolver.queue_swap(new_conv, crossfade);
                }
            }
        }

        // try_lock so the audio thread never blocks on a GUI-side build. The
        // arm arrives fully assembled, so this is a move, not an allocation.
        if let Ok(mut pending) = self.cab_load_state.pending.try_lock() {
            if let Some(new_arm) = pending.take() {
                audio.cab_dynamic = new_arm;
            }
        }

        let num_samples = buffer.samples();
        let power_on = self.params.power.value();

        let current_trim_db = self.params.input_trim_db.value();
        if (current_trim_db - self.cached_input_trim_db).abs() > 0.01 {
            self.cached_input_trim_db = current_trim_db;
            // Moves the calibration and the meter's scale together — the
            // meter converts raw magnitudes with `input_scale` only at
            // read time, so the two cannot be updated separately.
            audio.amp_io.set_input_trim_db(current_trim_db);
        }

        let cab_mode = self.params.cab_modelling_mode.value();
        let mic_x = self.params.mic_x_position.value();
        let mic_dist_in = self.params.mic_distance_inches.value();
        if mic_x != self.cached_mic_x_position
            || (mic_dist_in - self.cached_mic_distance_inches).abs() > 0.001
        {
            self.cached_mic_x_position = mic_x;
            self.cached_mic_distance_inches = mic_dist_in;
            // Placement is a live, alloc-free setter on the Dynamic arm;
            // only identity changes (mic, speaker, cabinet, room) rebuild.
            if let Some(chain) = audio.cab_dynamic.dynamic_mut() {
                chain.set_mic_placement(MicChannel::Primary, MicrophonePlacement {
                    distance_m: mic_dist_in * 0.0254,
                    radial_offset_cm: mic_x.radial_offset_cm(),
                    off_axis_angle_deg: 0.0,
                });
            }
        }

        // Tube swap changes V1's plate source-Z, so the V2A grid network
        // changes with it. Both networks are resident: swap, don't build —
        // constructing an MnaCircuit here would allocate on the audio thread.
        let current_tube_toggle = self.params.tube_toggle.value();
        if current_tube_toggle != audio.engine.inner().current_tube_toggle {
            audio.engine.inner_mut().swap_grid_network();

            let normal_wiper = self
                .volume_taper
                .wiper_fraction(self.params.normal_volume.value());
            let bright_wiper = self
                .volume_taper
                .wiper_fraction(self.params.bright_volume.value());
            let tone_pos = self.params.tone.value();

            let inner = audio.engine.inner_mut();
            inner.current_tube_toggle = current_tube_toggle;
            inner.norm_smoother.set_target(normal_wiper);
            inner.bright_smoother.set_target(bright_wiper);
            inner.tone_smoother.set_target(tone_pos);
            // Snap the incoming network's wipers so the swap does not ramp
            // from the netlist default.
            let (n, b, t) = (
                inner.grid_norm_handle,
                inner.grid_bright_handle,
                inner.grid_tone_handle,
            );
            inner.grid_circuit.set_pot_position(n, normal_wiper);
            inner.grid_circuit.set_pot_position(b, bright_wiper);
            inner.grid_circuit.set_pot_position(t, tone_pos);

            let jack_dc_gain = audio.amp_io.jack_mut().dc_gain();
            let ceiling = {
                let inner = audio.engine.inner();
                let v1_pair = if current_tube_toggle {
                    &inner.v1_pair_mod
                } else {
                    &inner.v1_pair_stock
                };
                meter_ceiling_for_pair(v1_pair, jack_dc_gain)
            };
            audio.amp_io.input_meter_mut().set_clean_ceiling_v(ceiling);
        }

        {
            let normal_wiper = self
                .volume_taper
                .wiper_fraction(self.params.normal_volume.value());
            let bright_wiper = self
                .volume_taper
                .wiper_fraction(self.params.bright_volume.value());
            let tone_pos = self.params.tone.value();
            let channel_mode = self.params.channel_select.value();
            let inner = audio.engine.inner_mut();
            inner.current_channel_mode = channel_mode;
            inner.norm_smoother.set_target(normal_wiper);
            inner.bright_smoother.set_target(bright_wiper);
            inner.tone_smoother.set_target(tone_pos);
        }

        audio.engine.begin_buffer(num_samples);

        // The output trim is a per-buffer control, not a per-sample one: it
        // lives inside `AmpIo`'s exit chain and there is no audible artefact
        // from stepping it once per block.
        audio.amp_io.set_output_trim_db(self.params.output_trim_db.value());

        // Disjoint field borrows: the three passes need the boundary, the
        // engine, one cab arm and the scratch buffers simultaneously, and
        // selecting the arm through a method would borrow all of `audio`.
        let output_peak = {
            let AudioState {
                engine,
                amp_io,
                cab_dynamic,
                cab_ir,
                pre_cab_buffer,
                post_cab_left,
                post_cab_right,
                ir_block_size,
                ..
            } = &mut *audio;

            // Selected once. Both arms stay resident, so this is a pick, not
            // a rebuild — and the transduction pairing rides on the arm.
            let active_cab: &mut CabBackEnd = match cab_mode {
                CabModellingMode::Ir => cab_ir,
                CabModellingMode::Dynamic => cab_dynamic,
            };
            let ir_block_size = *ir_block_size;

            // Pass 1 — per host sample. Mono in: the input is channel 0, and
            // the output channels are not written until pass 3, so reading
            // here cannot alias what we later overwrite.
            if power_on {
                let input_channel = &buffer.as_slice()[0];
                for i in 0..num_samples {
                    // Entry boundary: meters the raw DAW sample, then
                    // calibration → jack divider → grid volts.
                    let grid_volts = amp_io.process_input(input_channel[i]);

                    // Boundary OS: inner DSP fires OS_factor times per host
                    // sample and returns OT-secondary volts.
                    let ot_volts = engine.process_sample(grid_volts);

                    // The arm's own transduction: `Ir` applies its loadbox
                    // (volts → samples), `Dynamic` passes volts through and
                    // transduces inside `process_block`.
                    pre_cab_buffer[i] = active_cab.route_sample(ot_volts);
                }
            } else {
                pre_cab_buffer[..num_samples].fill(0.0);
            }

            engine.end_buffer();

            // Pass 2 — cab domain, host rate, stereo out.
            //
            // b_plus_sag = 1.0: sag is already embodied in the OT volts.
            //
            // The IR arm's convolver has a fixed block size, so a shorter
            // host buffer is zero-padded to it. That desynchronises the
            // convolution stream rather than degrading gracefully, and it is
            // recorded as an engine responsibility — SDK-D13:
            // `ZeroLatencyConvolver` should buffer internally. Remove this
            // padding when the engine lands that fix; do not make it
            // permanent.
            let block = match cab_mode {
                CabModellingMode::Ir => {
                    if num_samples < ir_block_size {
                        pre_cab_buffer[num_samples..ir_block_size].fill(0.0);
                    }
                    ir_block_size
                }
                CabModellingMode::Dynamic => num_samples,
            };
            active_cab.process_block(
                &pre_cab_buffer[..block],
                &mut post_cab_left[..block],
                &mut post_cab_right[..block],
                1.0,
            );

            // Pass 3 — exit boundary, in place over the host buffer.
            let n = num_samples;
            let slice = buffer.as_slice();
            if !power_on {
                for channel in slice.iter_mut() {
                    channel[..n].fill(0.0);
                }
                0.0
            } else if slice.len() >= 2 {
                let (left_part, right_part) = slice.split_at_mut(1);
                let left = &mut left_part[0][..n];
                let right = &mut right_part[0][..n];
                left.copy_from_slice(&post_cab_left[..n]);
                right.copy_from_slice(&post_cab_right[..n]);
                // Output trim + NaN/Inf guard, then the host-rate DC
                // blockers; returns the block peak for the output meter.
                amp_io.process_output_block(left, right)
            } else {
                // A host that ignored the negotiated stereo layout. Publish
                // the left channel, but still run the right through the exit
                // chain so the two DC blockers' states cannot diverge —
                // `post_cab_right` is regenerated every buffer, so using it
                // as the sink allocates nothing.
                let left = &mut slice[0][..n];
                left.copy_from_slice(&post_cab_left[..n]);
                amp_io.process_output_block(left, &mut post_cab_right[..n])
            }
        };

        // Numerical health, once per buffer and off the hot loop (contract
        // §5). A run that keeps growing across buffers means an interior
        // integrator has latched and the amp is silent until rebuilt; the
        // exit guard has already stopped it reaching the host.
        let health = audio.amp_io.output_health();
        if health.run_length > LATCHED_SAMPLES {
            // The sanctioned remedy is an off-thread rebuild swapped in here.
            // Not yet wired (`BackgroundTask` is unused), so this publishes
            // the condition for the GUI and clears the counters so a later
            // occurrence is distinguishable from the one already reported.
            self.meter_output_latched.store(true, atomic::Ordering::Relaxed);
            audio.amp_io.reset_output_health();
        }

        // Publish meters
        audio.meter_output_peak = audio.meter_output_peak.max(output_peak);
        audio.meter_window_samples += num_samples;
        if audio.meter_window_samples >= audio.meter_window_len {
            let metrics = audio.amp_io.input_metrics();
            self.meter_peak_volts.store(metrics.peak_volts, atomic::Ordering::Relaxed);

            if power_on {
                let inner = audio.engine.inner();
                let bplus_v = inner.amp_topology.b_plus_mean_at(inner.power_tube_tap);
                self.meter_bplus_volts.store(bplus_v, atomic::Ordering::Relaxed);

                if inner.plate_samples_counted > 0 {
                    let n = inner.plate_samples_counted as f32;
                    self.meter_v1_volts.store(inner.v1_plate_sum / n, atomic::Ordering::Relaxed);
                    self.meter_v2_volts.store(inner.v2_plate_sum / n, atomic::Ordering::Relaxed);
                    self.meter_v3v4_volts.store(inner.v3v4_plate_sum / n, atomic::Ordering::Relaxed);
                }
            }
            let output_db = if audio.meter_output_peak > 1e-10 {
                20.0 * audio.meter_output_peak.log10()
            } else {
                -120.0
            };
            self.meter_output_db.store(output_db, atomic::Ordering::Relaxed);

            audio.engine.inner_mut().reset_plate_meters();
            audio.meter_window_samples = 0;
            audio.meter_output_peak = 0.0;
        }

        ProcessStatus::Normal
    }

    fn editor(&mut self, _async_executor: AsyncExecutor<Self>) -> Option<Box<dyn Editor>> {
        #[cfg(feature = "gui")]
        {
            use nih_plug_egui::{create_egui_editor, EguiState};

            let params = self.params.clone();
            let ir_load_state = self.ir_load_state.clone();
            let cab_load_state = self.cab_load_state.clone();
            let ir_path = self.params.ir_file_path.clone();
            let meter_peak_volts = self.meter_peak_volts.clone();
            let meter_bplus_volts = self.meter_bplus_volts.clone();
            let meter_v1_volts = self.meter_v1_volts.clone();
            let meter_v2_volts = self.meter_v2_volts.clone();
            let meter_v3v4_volts = self.meter_v3v4_volts.clone();
            let meter_output_db = self.meter_output_db.clone();
            let meter_os_ratio = self.meter_os_ratio.clone();

            create_egui_editor(
                EguiState::from_size(800, 520),
                gui::GuiState::new(
                    ir_load_state, cab_load_state, ir_path, meter_peak_volts,
                    meter_bplus_volts, meter_v1_volts, meter_v2_volts,
                    meter_v3v4_volts, meter_output_db, meter_os_ratio,
                ),
                |_, _| {},
                move |egui_ctx, setter, state| {
                    gui::create(egui_ctx, setter, &params, state)
                },
            )
        }
        #[cfg(not(feature = "gui"))]
        {
            None
        }
    }
}

impl ClapPlugin for TheTweed {
    const CLAP_ID: &'static str = "com.neampmod.the-tweed";
    const CLAP_DESCRIPTION: Option<&'static str> = Some("Physics-based amplifier simulator inspired by the 1957 Fender Tweed Deluxe 5e3 amplifier.");
    const CLAP_MANUAL_URL: Option<&'static str> = Some(Self::URL);
    const CLAP_SUPPORT_URL: Option<&'static str> = None;
    const CLAP_FEATURES: &'static [ClapFeature] = &[
        ClapFeature::AudioEffect,
        ClapFeature::Distortion,
        ClapFeature::Mono,
    ];
}

impl Vst3Plugin for TheTweed {
    const VST3_CLASS_ID: [u8; 16] = *b"TheTweed........";
    const VST3_SUBCATEGORIES: &'static [Vst3SubCategory] =
        &[Vst3SubCategory::Fx, Vst3SubCategory::Distortion];
}

nih_export_clap!(TheTweed);
nih_export_vst3!(TheTweed);

// =============================================================================
// Diagnostic probes — play-test decay-buzz attribution; dumps stage taps to
// /tmp/tweed_probe/. Run:
//   cargo test --release decay_buzz_probe -- --ignored --nocapture
// =============================================================================
#[cfg(test)]
mod decay_buzz_probe {
    use super::*;
    use thermionicdsp::circuit::BiquadFilter;
    // The probes instantiate concrete boundaries directly to compare OS
    // factors; the plugin's own path is the `AnyDspEngine` runtime dispatch.
    use thermionicdsp::{DspEngine, X4Boundary, X8Boundary};

    const SR: f32 = 48_000.0;
    const BLOCK: usize = 256;
    const RATE: EngineRate = EngineRate::new(SR, OversamplingFactor::X4);

    // Minimal PCM 48 kHz WAV reader (16/32-bit int, first channel).
    fn read_wav(path: &str) -> Vec<f32> {
        let bytes = std::fs::read(path).expect("read WAV");
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        let mut pos = 12;
        let (mut channels, mut bits) = (1u16, 16u16);
        let mut data = Vec::new();
        while pos + 8 <= bytes.len() {
            let id = &bytes[pos..pos + 4];
            let sz = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap()) as usize;
            if id == b"fmt " {
                channels = u16::from_le_bytes(bytes[pos + 10..pos + 12].try_into().unwrap());
                let rate = u32::from_le_bytes(bytes[pos + 12..pos + 16].try_into().unwrap());
                bits = u16::from_le_bytes(bytes[pos + 22..pos + 24].try_into().unwrap());
                assert_eq!(rate, 48_000, "WAV must be 48 kHz");
                assert!(bits == 16 || bits == 32, "16/32-bit PCM only");
            } else if id == b"data" {
                let stride = (bits / 8) as usize * channels as usize;
                data = bytes[pos + 8..pos + 8 + sz]
                    .chunks_exact(stride)
                    .map(|frame| match bits {
                        16 => i16::from_le_bytes([frame[0], frame[1]]) as f32 / 32768.0,
                        _ => {
                            i32::from_le_bytes([frame[0], frame[1], frame[2], frame[3]]) as f32
                                / 2_147_483_648.0
                        }
                    })
                    .collect();
            }
            pos += 8 + sz + (sz & 1);
        }
        assert!(!data.is_empty(), "no data chunk");
        data
    }

    /// `x` is interleaved by `channels`. The probes render the plugin's real
    /// stereo output, so they write stereo files — collapsing to mono here
    /// would hide exactly the mic-pan and decorrelated-room-tail content the
    /// acoustic chain produces.
    fn write_wav_f32(path: &std::path::Path, x: &[f32], channels: u16) {
        let frames = (x.len() / channels as usize) as u32;
        let block_align = 4 * u32::from(channels);
        let data_bytes = block_align * frames;
        let mut b = Vec::with_capacity(44 + data_bytes as usize);
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&(36 + data_bytes).to_le_bytes());
        b.extend_from_slice(b"WAVEfmt ");
        b.extend_from_slice(&16u32.to_le_bytes());
        b.extend_from_slice(&3u16.to_le_bytes()); // IEEE float
        b.extend_from_slice(&channels.to_le_bytes());
        b.extend_from_slice(&48_000u32.to_le_bytes());
        b.extend_from_slice(&(48_000 * block_align).to_le_bytes());
        b.extend_from_slice(&(block_align as u16).to_le_bytes());
        b.extend_from_slice(&32u16.to_le_bytes());
        b.extend_from_slice(b"data");
        b.extend_from_slice(&data_bytes.to_le_bytes());
        for &v in x {
            b.extend_from_slice(&v.to_le_bytes());
        }
        std::fs::write(path, b).expect("write WAV");
    }

    /// Interleave a stereo pair, normalised to `peak_target` of full scale
    /// across BOTH channels so the balance between them is preserved.
    fn interleave_normalised(l: &[f32], r: &[f32], peak_target: f32) -> Vec<f32> {
        let peak = l
            .iter()
            .chain(r.iter())
            .fold(0.0f32, |m, &v| m.max(v.abs()))
            .max(1e-9);
        let g = peak_target / peak;
        l.iter()
            .zip(r.iter())
            .flat_map(|(&a, &b)| [a * g, b * g])
            .collect()
    }

    /// Mirror of `AudioState::build` at the reported knob settings.
    fn build_playtest_inner() -> TweedInner {
        let taper = PotTaperConfig::new(PotTaper::Audio30A);
        // Front-panel 1–12 scale → normalised param value.
        let init_norm = taper.wiper_fraction(3.0 / 12.0);
        let init_bright = taper.wiper_fraction(1.5 / 12.0);
        let init_tone = 4.5 / 12.0;

        let v1_pair_stock = build_v1_pair(RATE, V1_STOCK_SPECIMEN, V1_STOCK_BIAS_V);
        let v1_pair_mod = build_v1_pair(RATE, V1_MOD_SPECIMEN, V1_MOD_BIAS_V);
        let amp_topology = AmpTopology::new(RATE, build_5e3_amp_topology_config());
        let supply = PreampSupply::new(&amp_topology, &["preamp"])
            .expect("the 5E3 filter chain publishes a 'preamp' rail");
        let preamp_tap = supply.tap("preamp");
        let power_tube_tap = amp_topology.b_plus_tap("power_tube");
        let v1_source_z = plate_source_impedance(V1_STOCK_SPECIMEN);
        let v2a_tube = build_v2a_tube(RATE);
        let V2aGridNetwork {
            mut circuit,
            norm_volume,
            bright_volume,
            tone,
        } = V2aGridNetwork::new(RATE, v1_source_z, v2a_tube.lut_quiescent_bias_volts());
        let grid_spare = V2aGridNetwork::new(
            RATE,
            plate_source_impedance(V1_STOCK_SPECIMEN),
            v2a_tube.lut_quiescent_bias_volts(),
        );
        circuit.set_pot_position(norm_volume, init_norm);
        circuit.set_pot_position(bright_volume, init_bright);
        circuit.set_pot_position(tone, init_tone);

        TweedInner {
            v1_pair_stock,
            v1_pair_mod,
            current_tube_toggle: false, // AY (stock)
            v2a_tube,
            grid_circuit: circuit,
            grid_norm_handle: norm_volume,
            grid_bright_handle: bright_volume,
            grid_tone_handle: tone,
            grid_spare,
            norm_smoother: PotSmoother::new(SR, init_norm, POT_SMOOTH_TAU_S),
            bright_smoother: PotSmoother::new(SR, init_bright, POT_SMOOTH_TAU_S),
            tone_smoother: PotSmoother::new(SR, init_tone, POT_SMOOTH_TAU_S),
            coupling_v2a: CouplingCapacitor::new(
                RATE,
                V2A_TO_V2B_COUPLING_CAP_F,
                V2B_GRID_LEAK_OHMS,
            ),
            amp_topology,
            supply,
            preamp_tap,
            power_tube_tap,
            current_channel_mode: ChannelMode::Bright,
            meter_this_host_sample: false,
            v1_plate_sum: 0.0,
            v2_plate_sum: 0.0,
            v3v4_plate_sum: 0.0,
            plate_samples_counted: 0,
        }
    }

    /// Windowed (50 ms) RMS + fraction of energy above 3 kHz.
    fn window_metrics(x: &[f32]) -> Vec<(f32, f32)> {
        let win = (SR * 0.05) as usize;
        let mut hp = BiquadFilter::highpass(3_000.0, std::f32::consts::FRAC_1_SQRT_2, SR);
        x.chunks(win)
            .map(|chunk| {
                let (mut total, mut high) = (0.0f64, 0.0f64);
                for &s in chunk {
                    let h = hp.process(s);
                    total += (s as f64) * (s as f64);
                    high += (h as f64) * (h as f64);
                }
                (
                    (total / chunk.len() as f64).sqrt() as f32,
                    if total > 0.0 { (high / total) as f32 } else { 0.0 },
                )
            })
            .collect()
    }

    /// A/B V2A against the 5E3 print (plate 164 V, V_gk −1.2 V): the plugin
    /// frame (LUT Rk=0, runtime 1.5 kΩ‖25 µF — bypassed at audio) vs a
    /// TriodeX-style frame (Rk baked into the LUT, unbypassed).
    /// Theory: bypassed ≈ 59×, unbypassed ≈ 30×.
    #[test]
    #[ignore = "diagnostic probe; run explicitly with --ignored --nocapture"]
    fn v2a_frame_ab() {
        let inner_sr = RATE.inner_sr();
        let mut frames: Vec<(&str, TubeStage)> = Vec::new();
        frames.push(("plugin (bypassed)", build_v2a_tube(RATE)));
        let tx_ll = LoadLineConfig {
            topology: LoadLineTopology::Triode {
                plate_operating_points: vec![
                    1.0, 0.95, 0.9, 0.85, 0.8, 0.75, 0.71, 0.67, 0.63, 0.59, 0.55, 0.51,
                    0.47, 0.43, 0.39, 0.35,
                ],
            },
            plate_resistor_ohms: PREAMP_PLATE_R_OHMS as f64,
            cathode_resistor_ohms: 1500.0,
            reference_bplus: 330.0,
            reference_bias: BiasSpec::Authored(0.0),
        };
        let mut tx = TubeStage::from_specimen_id(
            RATE,
            V2A_SPECIMEN,
            &tx_ll,
            TubeStageConfig::default(),
        )
        .expect("TriodeX-style V2A frame must build");
        tx.set_plate_bplus_voltage(PREAMP_BPLUS_5E3);
        frames.push(("triodex (baked 1.5k)", tx));

        eprintln!("\n=== V2A frame A/B — print: plate 164 V, V_gk −1.2 V, rail 247 V ===");
        for (label, mut stage) in frames {
            // Settle to DC.
            for _ in 0..(inner_sr * 0.5) as usize {
                stage.process(0.0, PREAMP_BPLUS_5E3);
            }
            let v_plate_dc = stage.instantaneous_plate_volts();
            let q_bias = stage.lut_quiescent_bias_volts();
            // Small-signal gain at 220 Hz, 10 mV pk.
            let w = 2.0 * std::f32::consts::PI * 220.0 / inner_sr;
            let n = (inner_sr * 2.0) as usize;
            let (mut re, mut im) = (0.0f64, 0.0f64);
            for i in 0..n {
                let x = 0.010 * (w * i as f32).sin();
                let y = stage.process(x, PREAMP_BPLUS_5E3).plate_ac_volts;
                if i >= n / 2 {
                    let p = w as f64 * i as f64;
                    re += y as f64 * p.cos();
                    im += y as f64 * p.sin();
                }
            }
            let amp = 2.0 * (re * re + im * im).sqrt() / (n - n / 2) as f64;
            eprintln!(
                "  {label:<22} plate DC {v_plate_dc:>7.1} V   LUT Q {q_bias:>6.2} V   gain {:>6.1}x ({:.1} dB)",
                amp / 0.010,
                20.0 * (amp / 0.010).log10()
            );
        }
    }

    /// Small-signal gain ladder at the same knob settings: 10 mV, 220 Hz
    /// and 2 kHz (above/below the bright-cap corner) — no stage clips, so
    /// the numbers are clean stage gains.
    #[test]
    #[ignore = "diagnostic probe; run explicitly with --ignored --nocapture"]
    fn small_signal_ladder() {
        for f0 in [220.0f32, 2_000.0] {
            let input_cal = InputCalibration::amp_standard();
            let jack = JackInput::new(68_000.0, 1_000_000.0);
            let mut engine: DspEngine<TweedInner, X4Boundary> =
                DspEngine::new(RATE, X4Boundary::new(RATE), build_playtest_inner());
            let w = 2.0 * std::f32::consts::PI * f0 / SR;
            let total = (SR * 3.0) as usize;
            let (mut ot, mut v2a, mut v1a) =
                (Vec::new(), Vec::new(), Vec::new());
            let vin_pk = 0.010f32; // volts at the jack — pre-calibration input
            let g = vin_pk / (jack.dc_gain() * input_cal.input_scale());
            let mut n = 0usize;
            while n < total {
                let block = BLOCK.min(total - n);
                engine.begin_buffer(block);
                for _ in 0..block {
                    let x = g * (w * n as f32).sin();
                    let conditioned = jack.process(input_cal.process(x));
                    ot.push(engine.process_sample(conditioned));
                    let inner = engine.inner();
                    v2a.push(inner.v2a_tube.instantaneous_plate_volts());
                    v1a.push(inner.v1_pair_stock.instantaneous_plate_a_volts());
                    n += 1;
                }
                engine.end_buffer();
            }
            let amp = |x: &[f32]| -> f32 {
                let tail = &x[(SR * 1.5) as usize..];
                let mean = tail.iter().map(|&v| v as f64).sum::<f64>() / tail.len() as f64;
                let om = 2.0 * std::f64::consts::PI * f0 as f64 / SR as f64;
                let (mut re, mut im) = (0.0f64, 0.0f64);
                for (i, &v) in tail.iter().enumerate() {
                    let p = om * i as f64;
                    re += (v as f64 - mean) * p.cos();
                    im += (v as f64 - mean) * p.sin();
                }
                (2.0 * (re * re + im * im).sqrt() / tail.len() as f64) as f32
            };
            let (a1, a2, ao) = (amp(&v1a), amp(&v2a), amp(&ot));
            eprintln!(
                "\n=== small-signal ladder, {f0} Hz, {:.1} mV pk at V1 grid ===",
                vin_pk * 1e3
            );
            eprintln!("  V1a plate: {a1:>9.4} V  (gain {:.1}x)", a1 / vin_pk);
            eprintln!(
                "  V2A plate: {a2:>9.4} V  (V1a->V2A grid transfer x V2A gain: {:.2}x)",
                a2 / a1
            );
            eprintln!("  OT sec:    {ao:>9.4} V  (total {:.0}x)", ao / vin_pk);
        }
    }

    /// Render the playtest chain. `dynamic_clamp` updates the grid network's
    /// conduction clamp from V2A's live absolute cathode once per host sample.
    /// Returns (ot, mic_l, mic_r, per-window (fizz, conduction µA·s, cathode
    /// V)). The mic render is stereo — the cab chain's real output.
    fn render_playtest(
        di: &[f32],
        os: OversamplingFactor,
        dynamic_clamp: bool,
    ) -> (Vec<f32>, Vec<f32>, Vec<f32>, Vec<(f32, f32, f32)>) {
        let rate = EngineRate::new(SR, os);
        let peak = di.iter().fold(0.0f32, |m, &x| m.max(x.abs())).max(1e-9);
        let g = 10f32.powf(-12.0 / 20.0) / peak;
        let input_cal = InputCalibration::amp_standard();
        let jack = JackInput::new(68_000.0, 1_000_000.0);

        fn build_inner_at(rate: EngineRate) -> TweedInner {
            // Same construction as `build_playtest_inner`, parameterised rate.
            let taper = PotTaperConfig::new(PotTaper::Audio30A);
            let init_norm = taper.wiper_fraction(3.0 / 12.0);
            let init_bright = taper.wiper_fraction(1.5 / 12.0);
            let init_tone = 4.5 / 12.0;
            let v1_pair_stock = build_v1_pair(rate, V1_STOCK_SPECIMEN, V1_STOCK_BIAS_V);
            let v1_pair_mod = build_v1_pair(rate, V1_MOD_SPECIMEN, V1_MOD_BIAS_V);
            let amp_topology = AmpTopology::new(rate, build_5e3_amp_topology_config());
            let supply = PreampSupply::new(&amp_topology, &["preamp"])
            .expect("the 5E3 filter chain publishes a 'preamp' rail");
        let preamp_tap = supply.tap("preamp");
            let power_tube_tap = amp_topology.b_plus_tap("power_tube");
            let v1_source_z = plate_source_impedance(V1_STOCK_SPECIMEN);
            let v2a_tube = build_v2a_tube(rate);
            let V2aGridNetwork {
                mut circuit,
                norm_volume,
                bright_volume,
                tone,
            } = V2aGridNetwork::new(rate, v1_source_z, v2a_tube.lut_quiescent_bias_volts());
            let grid_spare = V2aGridNetwork::new(
                rate,
                plate_source_impedance(V1_STOCK_SPECIMEN),
                v2a_tube.lut_quiescent_bias_volts(),
            );
            circuit.set_pot_position(norm_volume, init_norm);
            circuit.set_pot_position(bright_volume, init_bright);
            circuit.set_pot_position(tone, init_tone);
            TweedInner {
                v1_pair_stock,
                v1_pair_mod,
                current_tube_toggle: false,
                v2a_tube,
                grid_circuit: circuit,
                grid_norm_handle: norm_volume,
                grid_bright_handle: bright_volume,
                grid_tone_handle: tone,
                grid_spare,
                norm_smoother: PotSmoother::new(SR, init_norm, POT_SMOOTH_TAU_S),
                bright_smoother: PotSmoother::new(SR, init_bright, POT_SMOOTH_TAU_S),
                tone_smoother: PotSmoother::new(SR, init_tone, POT_SMOOTH_TAU_S),
                coupling_v2a: CouplingCapacitor::new(
                    rate,
                    V2A_TO_V2B_COUPLING_CAP_F,
                    V2B_GRID_LEAK_OHMS,
                ),
                amp_topology,
                supply,
                preamp_tap,
                power_tube_tap,
                current_channel_mode: ChannelMode::Bright,
                meter_this_host_sample: false,
                v1_plate_sum: 0.0,
                v2_plate_sum: 0.0,
                v3v4_plate_sum: 0.0,
                plate_samples_counted: 0,
            }
        }

        macro_rules! run {
            ($boundary:ty) => {{
                let mut engine: DspEngine<TweedInner, $boundary> = DspEngine::new(
                    rate,
                    <$boundary>::new(rate),
                    build_inner_at(rate),
                );
                let mut cab = build_cab_processor(
                    SR,
                    BLOCK,
                    DEFAULT_SPEAKER_ID,
                    DEFAULT_CABINET_ID,
                    "dynamic_421",
                    RoomSelection::SmallStudio,
                    MicrophonePlacement {
                        distance_m: 5.5 * 0.0254,
                        radial_offset_cm: 4.0,
                        off_axis_angle_deg: 0.0,
                    },
                );
                let mut ot = Vec::with_capacity(di.len());
                let win = (SR * 0.05) as usize;
                let mut winstats: Vec<(f32, f32, f32)> = Vec::new();
                let (mut cond_acc, mut vk_max) = (0.0f64, 0.0f32);
                for block in di.chunks(BLOCK) {
                    engine.begin_buffer(block.len());
                    for &x in block {
                        let conditioned = jack.process(input_cal.process(x * g));
                        ot.push(engine.process_sample(conditioned));
                        let inner = engine.inner_mut();
                        let vk_abs =
                            inner.v2a_tube.grid_voltage_volts() - inner.v2a_tube.vgk_volts();
                        vk_max = vk_max.max(vk_abs);
                        cond_acc += inner.grid_circuit.grid_conduction_current(0) as f64;
                        if dynamic_clamp {
                            inner.grid_circuit.set_grid_bias_all(vk_abs);
                        }
                        if ot.len() % win == 0 {
                            winstats.push((
                                0.0, // fizz filled later
                                (cond_acc / SR as f64 * 1e6) as f32, // µA·s this window
                                vk_max,
                            ));
                            cond_acc = 0.0;
                            vk_max = 0.0;
                        }
                    }
                    engine.end_buffer();
                }
                let mut mic_l = Vec::with_capacity(ot.len());
                let mut mic_r = Vec::with_capacity(ot.len());
                for &v in &ot {
                    let (l, r) = cab.process(v, 1.0);
                    mic_l.push(l);
                    mic_r.push(r);
                }
                // The fizz metric wants one signal; the left channel is that
                // signal. Summing the pair to get it would reintroduce the
                // downmix and comb the decorrelated room tail.
                let m = window_metrics(&mic_l);
                for (i, s) in winstats.iter_mut().enumerate() {
                    s.0 = m.get(i).map_or(0.0, |&(_, f)| f);
                }
                (ot, mic_l, mic_r, winstats)
            }};
        }
        match os {
            OversamplingFactor::X4 => run!(X4Boundary),
            OversamplingFactor::X8 => run!(X8Boundary),
            other => panic!("probe supports X4/X8, got {other:?}"),
        }
    }

    /// A/B frozen vs dynamic conduction clamp, and X4 vs X8 oversampling, at
    /// the playtest settings. All four mic renders dumped for listening.
    #[test]
    #[ignore = "diagnostic probe; run explicitly with --ignored --nocapture"]
    fn clamp_and_os_ab() {
        let dir = std::path::Path::new("/tmp/tweed_probe");
        std::fs::create_dir_all(dir).expect("mkdir");
        let samples_dir = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../thermionic-products/ThermionicDSP/samples"
        );
        let mut di = vec![0.0f32; (SR * 1.0) as usize];
        di.extend(read_wav(&format!("{samples_dir}/di_chord.wav")));

        let arms: [(&str, OversamplingFactor, bool); 4] = [
            ("x4_frozen", OversamplingFactor::X4, false),
            ("x4_dynamic", OversamplingFactor::X4, true),
            ("x8_frozen", OversamplingFactor::X8, false),
            ("x8_dynamic", OversamplingFactor::X8, true),
        ];
        let mut results = Vec::new();
        for (name, os, dynamic) in arms {
            let (_ot, mic_l, mic_r, stats) = render_playtest(&di, os, dynamic);
            let norm = interleave_normalised(&mic_l, &mic_r, 0.891);
            write_wav_f32(&dir.join(format!("ab_{name}.wav")), &norm, 2);
            eprintln!("  wrote /tmp/tweed_probe/ab_{name}.wav");
            results.push((name, stats));
        }

        // Aligned fizz/conduction/cathode table around the fizziest window
        // (the attack region — where clamp error and aliasing both peak).
        let peak_w = results[0]
            .1
            .iter()
            .enumerate()
            .skip(20)
            .max_by(|a, b| a.1 .0.partial_cmp(&b.1 .0).unwrap())
            .map(|(i, _)| i)
            .unwrap_or(24);
        eprintln!(
            "\n  {:>7} | {:>8} {:>9} {:>7} | {:>8} {:>9} {:>7} | {:>8} {:>8}",
            "t (s)", "frz f%", "frz µA·s", "frz Vk", "dyn f%", "dyn µA·s", "dyn Vk", "x8frz f%", "x8dyn f%"
        );
        let lo = peak_w.saturating_sub(4);
        let hi = (peak_w + 44).min(results[0].1.len());
        for w in lo..hi {
            let a = results[0].1[w];
            let b = results[1].1[w];
            let c = results[2].1[w];
            let d = results[3].1[w];
            eprintln!(
                "  {:>7.2} | {:>8.2} {:>9.3} {:>7.2} | {:>8.2} {:>9.3} {:>7.2} | {:>8.2} {:>8.2}",
                w as f32 * 0.05,
                100.0 * a.0,
                a.1,
                a.2,
                100.0 * b.0,
                b.1,
                b.2,
                100.0 * c.0,
                100.0 * d.0,
            );
        }
    }

    /// Bisect the acoustic chain against the Ardour capture: render the
    /// exported DI through the amp once, then through per-stage cab-chain
    /// arms, to locate which stage owns a given spectral feature.
    #[test]
    #[ignore = "diagnostic probe; run explicitly with --ignored --nocapture"]
    fn ardour_bisect() {
        let dir = std::path::Path::new("/tmp/tweed_probe");
        std::fs::create_dir_all(dir).expect("mkdir");
        // Plain-PCM mono conversion of the Ardour export (same samples).
        let di = read_wav("/tmp/tweed_probe/di_in_mono.wav");

        let input_cal = InputCalibration::amp_standard();
        let jack = JackInput::new(68_000.0, 1_000_000.0);
        let mut engine: DspEngine<TweedInner, X4Boundary> =
            DspEngine::new(RATE, X4Boundary::new(RATE), build_playtest_inner());
        let mut ot = Vec::with_capacity(di.len());
        for block in di.chunks(BLOCK) {
            engine.begin_buffer(block.len());
            for &x in block {
                let conditioned = jack.process(input_cal.process(x));
                ot.push(engine.process_sample(conditioned));
            }
            engine.end_buffer();
        }
        let peak = ot.iter().fold(0.0f32, |m, &v| m.max(v.abs())).max(1e-9);
        write_wav_f32(
            &dir.join("bisect_ot.wav"),
            &ot.iter().map(|&v| 0.891 * v / peak).collect::<Vec<_>>(),
            1,
        );
        eprintln!("  OT secondary peak: {peak:.2} V");

        // (label, speaker, response, cabinet, mic, room)
        let arms: [(&str, bool, bool, bool, bool, bool); 6] = [
            ("full", true, true, true, true, true),
            ("no_room", true, true, true, true, false),
            ("no_mic", true, true, true, false, true),
            ("no_cab", true, true, false, true, true),
            ("no_response", true, false, true, true, true),
            ("no_speaker", false, true, true, true, true),
        ];
        for (label, speaker, response, cabinet, mic, room) in arms {
            let mut chain = SpeakerCabRoomProcessor::new(
                SR,
                BLOCK,
                SpeakerCabRoomConfig {
                    speaker_wiring: SpeakerWiring::single(DEFAULT_SPEAKER_ID),
                    cabinet_id: DEFAULT_CABINET_ID.into(),
                    mic_primary: MicChannelConfig {
                        microphone_id: "dynamic_421".into(),
                        placement: MicrophonePlacement {
                            distance_m: 5.5 * 0.0254,
                            radial_offset_cm: 4.0,
                            off_axis_angle_deg: 0.0,
                        },
                        preamp_gain_db: 35.0,
                        polarity_inverted: false,
                        pan: 0.0,
                    },
                    mic_secondary: None,
                    room_id: Some("small_studio".into()),
                    speaker_enabled: speaker,
                    response_enabled: response,
                    cabinet_enabled: cabinet,
                    mic_enabled: mic,
                    room_enabled: room,
                },
            );
            let mut out_l = Vec::with_capacity(ot.len());
            let mut out_r = Vec::with_capacity(ot.len());
            for &v in &ot {
                let (l, r) = chain.process(v, 1.0);
                out_l.push(l);
                out_r.push(r);
            }
            let peak = out_l
                .iter()
                .chain(out_r.iter())
                .fold(0.0f32, |m, &v| m.max(v.abs()))
                .max(1e-9);
            write_wav_f32(
                &dir.join(format!("bisect_{label}.wav")),
                &interleave_normalised(&out_l, &out_r, 0.891),
                2,
            );
            eprintln!("  wrote bisect_{label}.wav (peak {peak:.4})");
        }
    }

    /// Wall time per 256-sample buffer (amp + cab, same thread) against the
    /// 5.33 ms real-time budget — over-budget buffers are audible dropouts.
    #[test]
    #[ignore = "diagnostic probe; run explicitly with --ignored --nocapture"]
    fn buffer_time_profile() {
        let samples_dir = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../thermionic-products/ThermionicDSP/samples"
        );
        let mut di = vec![0.0f32; (SR * 1.0) as usize];
        di.extend(read_wav(&format!("{samples_dir}/di_chord.wav")));
        let peak = di.iter().fold(0.0f32, |m, &x| m.max(x.abs())).max(1e-9);
        let g = 10f32.powf(-12.0 / 20.0) / peak;
        let input_cal = InputCalibration::amp_standard();
        let jack = JackInput::new(68_000.0, 1_000_000.0);
        let mut engine: DspEngine<TweedInner, X4Boundary> =
            DspEngine::new(RATE, X4Boundary::new(RATE), build_playtest_inner());
        let mut cab = build_cab_processor(
            SR,
            BLOCK,
            DEFAULT_SPEAKER_ID,
            DEFAULT_CABINET_ID,
            "dynamic_421",
            RoomSelection::SmallStudio,
            MicrophonePlacement {
                distance_m: 5.5 * 0.0254,
                radial_offset_cm: 4.0,
                off_axis_angle_deg: 0.0,
            },
        );

        let budget_us = BLOCK as f64 / SR as f64 * 1e6;
        let mut times_us: Vec<f64> = Vec::new();
        for block in di.chunks(BLOCK) {
            let t0 = std::time::Instant::now();
            engine.begin_buffer(block.len());
            for &x in block {
                let conditioned = jack.process(input_cal.process(x * g));
                let v = engine.process_sample(conditioned);
                let _ = cab.process(v, 1.0);
            }
            engine.end_buffer();
            times_us.push(t0.elapsed().as_secs_f64() * 1e6);
        }
        let mut sorted = times_us.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let pct = |p: f64| sorted[((sorted.len() - 1) as f64 * p) as usize];
        let over = times_us.iter().filter(|&&t| t > budget_us).count();
        let over80 = times_us.iter().filter(|&&t| t > 0.8 * budget_us).count();
        eprintln!("\n=== Buffer time profile, playtest settings, 256 @ 48 kHz ===");
        eprintln!("  budget {budget_us:.0} µs/buffer, {} buffers", times_us.len());
        eprintln!(
            "  p50 {:.0}  p90 {:.0}  p99 {:.0}  max {:.0} µs  ({:.0}% of budget at p99)",
            pct(0.5),
            pct(0.9),
            pct(0.99),
            sorted[sorted.len() - 1],
            100.0 * pct(0.99) / budget_us
        );
        eprintln!(
            "  buffers over budget: {over}; over 80% of budget: {over80}"
        );
        // Worst 10 buffers with their positions in the performance.
        let mut idx: Vec<usize> = (0..times_us.len()).collect();
        idx.sort_by(|&a, &b| times_us[b].partial_cmp(&times_us[a]).unwrap());
        eprintln!("  worst buffers (t, µs):");
        for &i in idx.iter().take(10) {
            eprintln!(
                "    {:>7.2} s  {:>7.0} µs ({:>3.0}%)",
                i as f32 * BLOCK as f32 / SR,
                times_us[i],
                100.0 * times_us[i] / budget_us
            );
        }
    }

    #[test]
    #[ignore = "diagnostic probe; run explicitly with --ignored --nocapture"]
    fn playtest_render() {
        let dir = std::path::Path::new("/tmp/tweed_probe");
        std::fs::create_dir_all(dir).expect("mkdir");
        let samples_dir = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../thermionic-products/ThermionicDSP/samples"
        );

        for stem in ["di_chord", "di_low_e"] {
            let mut di = vec![0.0f32; (SR * 1.0) as usize];
            di.extend(read_wav(&format!("{samples_dir}/{stem}.wav")));
            // Input peaks −12 dBFS at the DAW, trims at 0 dB.
            let peak = di.iter().fold(0.0f32, |m, &x| m.max(x.abs())).max(1e-9);
            let g = 10f32.powf(-12.0 / 20.0) / peak;

            let input_cal = InputCalibration::amp_standard();
            let jack = JackInput::new(68_000.0, 1_000_000.0);
            let mut engine: DspEngine<TweedInner, X4Boundary> =
                DspEngine::new(RATE, X4Boundary::new(RATE), build_playtest_inner());
            let mut cab = build_cab_processor(
                SR,
                BLOCK,
                DEFAULT_SPEAKER_ID,
                DEFAULT_CABINET_ID,
                "dynamic_421",
                RoomSelection::SmallStudio,
                MicrophonePlacement {
                    distance_m: 5.5 * 0.0254,
                    radial_offset_cm: 4.0,
                    off_axis_angle_deg: 0.0,
                },
            );

            let mut ot = Vec::with_capacity(di.len());
            let mut v2a = Vec::with_capacity(di.len());
            let mut v1a = Vec::with_capacity(di.len());
            let mut mic = Vec::with_capacity(di.len());
            for block in di.chunks(BLOCK) {
                engine.begin_buffer(block.len());
                for &x in block {
                    let conditioned = jack.process(input_cal.process(x * g));
                    let v = engine.process_sample(conditioned);
                    ot.push(v);
                    let inner = engine.inner();
                    v2a.push(inner.v2a_tube.instantaneous_plate_volts());
                    v1a.push(inner.v1_pair_stock.instantaneous_plate_a_volts());
                }
                engine.end_buffer();
            }
            let mut mic_r = Vec::with_capacity(ot.len());
            for &v in &ot {
                let (l, r) = cab.process(v, 1.0);
                mic.push(l);
                mic_r.push(r);
            }

            // ot/v2a/v1a are circuit-node voltages — single-ended by nature,
            // so they stay mono. Only the mic render is an acoustic output.
            for (tag, x) in [("ot", &ot), ("v2a", &v2a), ("v1a", &v1a)] {
                let peak = x.iter().fold(0.0f32, |m, &v| m.max(v.abs())).max(1e-9);
                let norm: Vec<f32> = x.iter().map(|&v| 0.891 * v / peak).collect();
                write_wav_f32(&dir.join(format!("plugin_{stem}_{tag}.wav")), &norm, 1);
            }
            write_wav_f32(
                &dir.join(format!("plugin_{stem}_mic.wav")),
                &interleave_normalised(&mic, &mic_r, 0.891),
                2,
            );

            // Gain ladder: absolute AC levels per tap over the loudest 100 ms.
            let conditioned_pk = di
                .iter()
                .map(|&x| jack.dc_gain() * input_cal.input_scale() * (x * g).abs())
                .fold(0.0f32, f32::max);
            let ac_stats = |x: &[f32], label: &str| {
                let mean = x.iter().map(|&v| v as f64).sum::<f64>() / x.len() as f64;
                let win = (SR * 0.1) as usize;
                let (mut pk, mut rms_best) = (0.0f32, 0.0f64);
                for chunk in x.chunks(win) {
                    let mut acc = 0.0f64;
                    for &v in chunk {
                        let a = (v as f64 - mean).abs();
                        pk = pk.max(a as f32);
                        acc += a * a;
                    }
                    rms_best = rms_best.max(acc / chunk.len() as f64);
                }
                eprintln!(
                    "    {label:<28} peak {pk:>9.3} V   max-window RMS {:>8.3} V",
                    rms_best.sqrt()
                );
            };
            eprintln!("  gain ladder ({stem}):");
            eprintln!(
                "    {:<28} peak {conditioned_pk:>9.3} V",
                "V1 grid (conditioned in)"
            );
            ac_stats(&v1a, "V1a plate (AC)");
            ac_stats(&v2a, "V2A plate (AC)");
            ac_stats(&ot, "OT secondary");

            let m_ot = window_metrics(&ot);
            let m_mic = window_metrics(&mic);
            let m_v2a = window_metrics(&v2a);
            let peak_w = m_ot
                .iter()
                .enumerate()
                .skip(10)
                .max_by(|a, b| a.1 .0.partial_cmp(&b.1 .0).unwrap())
                .map(|(i, _)| i)
                .unwrap();
            eprintln!("\n=== plugin chain, {stem}, user settings ===");
            eprintln!(
                "  {:>7} {:>9} {:>8} {:>8} {:>8}",
                "t (s)", "ot dBV", "ot f%", "mic f%", "v2a f%"
            );
            let lo = peak_w.saturating_sub(4);
            let hi = (peak_w + 50).min(m_ot.len());
            for w in lo..hi {
                let mark = if w == peak_w { "  <-- peak" } else { "" };
                eprintln!(
                    "  {:>7.2} {:>9.1} {:>8.2} {:>8.2} {:>8.2}{mark}",
                    w as f32 * 0.05,
                    20.0 * m_ot[w].0.max(1e-9).log10(),
                    100.0 * m_ot[w].1,
                    100.0 * m_mic[w].1,
                    100.0 * m_v2a[w].1,
                );
            }
        }
    }
}
