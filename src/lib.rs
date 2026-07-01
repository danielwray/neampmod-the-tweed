use nih_plug::prelude::*;
use std::sync::{Arc, Mutex, atomic};
use std::sync::atomic::Ordering;

#[cfg(feature = "gui")]
mod gui;

use neampmod_engine::{
    TubeStage,
    TubeRegistry,
    AmpTopology,
    AmpTopologyConfig,
    BPlusTap,
    CouplingCapacitor,
    DCBlocker,
    InputCalibration,
    InputLevelMeter,
    LoadboxDi,
    ir_loader,
    ir_convolver,
    PotTaper,
    PotTaperConfig,
    JackInput,
    enable_audio_thread_denormal_handling,
    EngineRate,
    OversamplingFactor,
    X1Boundary,
    X2Boundary,
    X4Boundary,
    X8Boundary,
    InnerDspProcessor,
    DspEngine,
    SpeakerCabRoomProcessor,
    SpeakerCabRoomConfig,
    SpeakerWiring,
    MicrophonePlacement,
};
use neampmod_engine::dsp::amps::tube_modeling::{
    SharedCathodeTriodePair, SharedCathodeTriodePairConfig, TubeSpec,
};
use neampmod_engine::dsp::circuits::mna_circuit::{
    GridBiasType, GridConductionConfig, MnaCircuit, MnaCircuitBuilder, PotHandle,
    PotSmoother, GND,
};

const IR_CROSSFADE_MS: f32 = 30.0;

// GUI metering cadence: plate/B+/level values are accumulated over this
// window and published to the GUI atomics once per window, rather than
// every buffer.
const METER_UPDATE_INTERVAL_MS: f32 = 100.0;

// Must match the OT-load speaker in `AmpTopologyConfig::fender_5e3()` —
// the 5E3 shipped with a Jensen P12R, open-back 1x12.
const DEFAULT_SPEAKER_ID: &str = "jensen_p12r";
const DEFAULT_CABINET_ID: &str = "fender_5e3_open_back_1x12";

pub struct IrLoadState {
    pub pending: Mutex<Option<ir_convolver::ZeroLatencyConvolver>>,
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

// Hot-swap slot: GUI builds a fresh SpeakerCabRoomProcessor and writes it
// here; the audio thread try_locks once per buffer and swaps it in.
pub struct CabProcessorLoadState {
    pub pending: Mutex<Option<SpeakerCabRoomProcessor>>,
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
    let (room_id, room_enabled) = room.into_engine();
    SpeakerCabRoomProcessor::new(
        sample_rate,
        max_buffer_size,
        SpeakerCabRoomConfig {
            // 5E3 is a 1x12 — single driver, matches the OT-load speaker.
            speaker_wiring: SpeakerWiring::single(speaker_id),
            cabinet_id: cabinet_id.to_string(),
            microphone_id: microphone_id.to_string(),
            placement,
            room_id: room_id.to_string(),
            // Lockstep with LoadboxDi's -10 dB pad so both cab arms land
            // in the same dBFS region.
            mic_preamp_gain_db: 35.0,
            speaker_enabled: true,
            cabinet_enabled: true,
            mic_enabled: true,
            room_enabled,
            response_enabled: true,
        },
    )
}

pub fn load_ir_file_into_state(state: &IrLoadState, path: &std::path::Path) {
    state.status.store(ir_load_status::LOADING, Ordering::Relaxed);

    let sample_rate = state.sample_rate.load(Ordering::Relaxed);
    let block_size = state.block_size.load(Ordering::Relaxed);

    let loader = ir_loader::IrLoader::new(sample_rate);
    match loader.load_from_file(path) {
        Ok((mut ir, _, _)) => {
            ir_loader::IrLoader::remove_dc_offset(&mut ir);
            ir_loader::IrLoader::normalize_response_peak(&mut ir);
            let fir_len = 128.min(block_size);
            let conv = ir_convolver::ZeroLatencyConvolver::new(&ir, block_size, fir_len);
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
    #[id = "shure_sm57"]
    #[name = "Shure SM57"]
    ShureSm57,
    #[id = "sennheiser_md421"]
    #[name = "Sennheiser MD 421-II"]
    SennheiserMd421,
    #[id = "royer_r121"]
    #[name = "Royer R-121 Ribbon"]
    RoyerR121,
    #[id = "neumann_u87"]
    #[name = "Neumann U 87 Ai (Cardioid)"]
    NeumannU87,
    #[id = "rca_44bx"]
    #[name = "RCA 44-BX"]
    Rca44Bx,
    #[id = "rca_77dx"]
    #[name = "RCA 77-DX"]
    Rca77Dx,
}

impl MicSelection {
    pub fn registry_id(self) -> &'static str {
        match self {
            MicSelection::ShureSm57 => "shure_sm57",
            MicSelection::SennheiserMd421 => "sennheiser_md421",
            MicSelection::RoyerR121 => "royer_r121",
            MicSelection::NeumannU87 => "neumann_u87",
            MicSelection::Rca44Bx => "rca_44bx",
            MicSelection::Rca77Dx => "rca_77dx",
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
    WoodenBarn,
    #[id = "wooden_barn"]
    #[name = "Wooden Barn"]
    SmallBedroom,
    #[id = "iso_box"]
    #[name = "Iso Box"]
    IsoBox,
}

impl RoomSelection {
    // `None` still resolves to a real id so the processor builds; the room
    // stage is simply disabled via the flag.
    pub fn into_engine(self) -> (&'static str, bool) {
        match self {
            RoomSelection::None => ("small_studio", false),
            RoomSelection::SmallStudio => ("small_studio", true),
            RoomSelection::LargeStudio => ("large_studio", true),
            RoomSelection::LiveRoom => ("live_room", true),
            RoomSelection::WoodenBarn => ("wooden_barn", true),
            RoomSelection::SmallBedroom => ("small_bedroom", true),
            RoomSelection::IsoBox => ("iso_box", true),
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

    #[id = "master"]
    pub master: FloatParam,

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
                0.38,
                FloatRange::Linear { min: 0.01, max: 1.0 },
            )
            .with_smoother(SmoothingStyle::Logarithmic(10.0))
            .with_value_to_string(v2s_dial_1_to_12())
            .with_string_to_value(s2v_dial_1_to_12()),

            normal_volume: FloatParam::new(
                "Normal",
                0.29,
                FloatRange::Linear { min: 0.01, max: 1.0 },
            )
            .with_smoother(SmoothingStyle::Logarithmic(10.0))
            .with_value_to_string(v2s_dial_1_to_12())
            .with_string_to_value(s2v_dial_1_to_12()),

            channel_select: EnumParam::new("Channel", ChannelMode::Both),

            tone: FloatParam::new(
                "Tone",
                0.54,
                FloatRange::Linear { min: 0.01, max: 1.0 },
            )
            .with_smoother(SmoothingStyle::Logarithmic(5.0))
            .with_value_to_string(v2s_dial_1_to_12())
            .with_string_to_value(s2v_dial_1_to_12()),

            power: BoolParam::new("Power", true),

            tube_toggle: BoolParam::new("Tube Toggle", false),

            master: FloatParam::new(
                "Master",
                0.6,
                FloatRange::Linear { min: 0.0001, max: 1.0 },
            )
            .with_smoother(SmoothingStyle::Logarithmic(10.0))
            .with_value_to_string(v2s_dial_1_to_12())
            .with_string_to_value(s2v_dial_1_to_12()),

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
                4.0,
                FloatRange::Linear { min: 0.1, max: 24.0 },
            )
            .with_unit(" in")
            .with_step_size(0.1)
            .with_smoother(SmoothingStyle::Linear(20.0))
            .with_value_to_string(formatters::v2s_f32_rounded(1))
            .with_string_to_value(Arc::new(|s: &str| s.trim().parse().ok())),

            microphone: EnumParam::new("Mic", MicSelection::Rca77Dx),
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
    let mut config = AmpTopologyConfig::fender_5e3();
    config.power_section.transformer_spec = OT_SPEC.into();
    config.power_supply.sag.rectifier_spec = Some(RECTIFIER_SPEC.into());
    config
}

const PREAMP_BPLUS_5E3: f32 = 250.0;

const V1_STOCK_SPEC: &str = "ge_12ay7_100k";
const V1_MOD_SPEC: &str = "ge_12ax7_100k";
const V2A_SPEC: &str = "ge_12ax7_100k";
const RECTIFIER_SPEC: &str = "ge_5y3";
const OT_SPEC: &str = "sst_108";
const V1_CATHODE_R: f32 = 820.0;
const V1_CATHODE_CAP: f32 = 25.0;
const V2A_CATHODE_R: f32 = 1500.0;
const V2A_CATHODE_CAP: f32 = 25.0;
const V2A_TO_V2B_COUPLING_CAP_F: f32 = 0.02e-6;
const V2B_GRID_LEAK_OHMS: f32 = 1_000_000.0;

const POT_SMOOTH_TAU_S: f32 = 0.020;

fn build_preamp_tube(
    engine_rate: EngineRate,
    spec_name: &str,
    cathode_resistor_ohms: f32,
    cathode_bypass_cap_uf: Option<f32>,
) -> TubeStage {
    let reg = TubeRegistry::global();
    let spec = reg.lookup(spec_name)
        .unwrap_or_else(|| panic!("Tube spec '{}' not found in registry", spec_name));
    let mut stage = TubeStage::from_spec(
        engine_rate,
        spec,
        cathode_resistor_ohms,
        cathode_bypass_cap_uf,
    )
    .unwrap_or_else(|e| panic!("Failed to build tube from spec '{}': {}", spec_name, e));
    stage.set_plate_bplus_voltage(PREAMP_BPLUS_5E3);
    stage
}

// V1 (12AY7 stock / 12AX7 mod) is one shared-cathode triode pair: triode A
// is the Normal grid, triode B is Bright, sharing an 820Ω/25µF cathode RC.
// The shared cathode integrator is what produces the 5E3's cross-channel
// ducking — a hard drive into one grid biases both triodes toward cutoff.
fn build_v1_pair(
    engine_rate: EngineRate,
    spec_name: &str,
) -> SharedCathodeTriodePair {
    let config = SharedCathodeTriodePairConfig {
        tube_spec: spec_name.into(),
        shared_cathode_resistor_ohms: V1_CATHODE_R,
        shared_cathode_bypass_cap_uf: Some(V1_CATHODE_CAP),
        shared_cathode_bypass_dielectric: Some("electrolytic_vintage".into()),
        plate_resistor_a_ohms: 100_000.0,
        plate_resistor_b_ohms: 100_000.0,
        tube_mismatch: Some(0.05),
        linear_blend_threshold: None,
        plate_voltage_fraction: 1.0,
    };
    let mut pair = SharedCathodeTriodePair::from_config(engine_rate, config)
        .unwrap_or_else(|e| panic!("V1 pair build for '{}': {}", spec_name, e));
    pair.set_plate_bplus_voltage(PREAMP_BPLUS_5E3);
    pair
}

// V2A's grid network (V1 mixing + bright cap + tone shunt + grid
// conduction) is attached separately via `set_grid_circuit` — see
// `V2aGridNetwork` below. V2A→V2B is a plain 0.02µF/1MΩ coupling cap.
fn build_v2a_tube(engine_rate: EngineRate) -> TubeStage {
    build_preamp_tube(
        engine_rate,
        V2A_SPEC,
        V2A_CATHODE_R,
        Some(V2A_CATHODE_CAP),
    )
}

// Tube-plate Thévenin source impedance: R_load ∥ r_p.
fn plate_source_impedance(spec: &TubeSpec) -> f32 {
    let rp = spec.rp;
    let rl = spec.plate_resistor_ohms;
    rp * rl / (rp + rl)
}

// Passive subcircuit between the V1 plates and V2A's grid: each V1 plate
// feeds a 0.1µF coupling cap into a 1MΩ volume pot (500pF bright cap
// across the Bright pot's upper half), and both wipers join through 68kΩ
// mixing resistors at V2A's grid. Tone is a shunt-to-ground rheostat at
// that same grid node — 1MΩ pot in series with a 5nF cap to ground (the
// pot's `top` and `wiper` tie to the same node to make a 2-terminal
// rheostat from the 3-terminal primitive). Grid conduction is stamped at
// V2A's grid; the volume pots double as its DC grid leak.
//
// Driver order is load-bearing: handle 0 is V1A plate, handle 1 is V1B
// plate — must match the `process_multi(&[v1a, v1b], ...)` call site.
struct V2aGridNetwork {
    circuit: MnaCircuit,
    norm_volume: PotHandle,
    bright_volume: PotHandle,
    tone: PotHandle,
}

impl V2aGridNetwork {
    const COUPLING_CAP_F: f32 = 0.1e-6;
    const VOLUME_POT_OHMS: f32 = 1_000_000.0;
    const BRIGHT_CAP_F: f32 = 500e-12;
    const MIXING_R_OHMS: f32 = 68_000.0;
    const TONE_POT_OHMS: f32 = 1_000_000.0;
    const TONE_CAP_F: f32 = 5e-9;

    fn new(engine_rate: EngineRate, v1_source_z_ohms: f32) -> Self {
        let v2a_spec = TubeRegistry::global()
            .lookup(V2A_SPEC)
            .unwrap_or_else(|| panic!("Tube spec '{}' not found in registry", V2A_SPEC));

        let mut b = MnaCircuitBuilder::new(engine_rate);

        let (v1a, _drv_v1a) = b.add_driver("v1a_plate");
        let (v1b, _drv_v1b) = b.add_driver("v1b_plate");

        let v1a_after_src = b.node("v1a_after_src");
        let norm_pot_top = b.node("norm_pot_top");
        let norm_wiper = b.node("norm_wiper");
        let v2a_grid = b.node("v2a_grid");
        b.resistor(v1a, v1a_after_src, v1_source_z_ohms)
            .capacitor(v1a_after_src, norm_pot_top, Self::COUPLING_CAP_F);
        let (norm_volume, _) =
            b.pot(norm_pot_top, norm_wiper, GND, Self::VOLUME_POT_OHMS, 1.0);
        b.resistor(norm_wiper, v2a_grid, Self::MIXING_R_OHMS);

        let v1b_after_src = b.node("v1b_after_src");
        let bright_pot_top = b.node("bright_pot_top");
        let bright_wiper = b.node("bright_wiper");
        b.resistor(v1b, v1b_after_src, v1_source_z_ohms)
            .capacitor(v1b_after_src, bright_pot_top, Self::COUPLING_CAP_F);
        let (bright_volume, _) =
            b.pot(bright_pot_top, bright_wiper, GND, Self::VOLUME_POT_OHMS, 0.0);
        b.capacitor(bright_pot_top, bright_wiper, Self::BRIGHT_CAP_F)
            .resistor(bright_wiper, v2a_grid, Self::MIXING_R_OHMS);

        let tone_internal = b.node("tone_internal");
        let (tone, _) = b.pot(
            tone_internal,
            tone_internal,
            v2a_grid,
            Self::TONE_POT_OHMS,
            1.0,
        );
        b.capacitor(tone_internal, GND, Self::TONE_CAP_F);

        // V2A's reflected Miller capacitance at the grid node forms the
        // 5E3's interactive-volume treble pole against the mixer's
        // pot-position-dependent source impedance.
        b.capacitor(v2a_grid, GND, v2a_spec.miller_c_eff_farads());

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

fn meter_ceiling_for_pair(pair: &SharedCathodeTriodePair, jack: &JackInput) -> f32 {
    pair.voltage_cal().clean_ac_ceiling_volts() / jack.dc_gain()
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
    pub grid_norm_handle: PotHandle,
    pub grid_bright_handle: PotHandle,
    pub grid_tone_handle: PotHandle,
    // Ticked at host rate in `begin_host_sample`; targets set per-buffer.
    pub norm_smoother: PotSmoother,
    pub bright_smoother: PotSmoother,
    pub tone_smoother: PotSmoother,
    // V2A plate → V2B (cathodyne) grid coupling: 0.02µF/1MΩ, ~8Hz HP.
    pub coupling_v2a: CouplingCapacitor,

    pub amp_topology: AmpTopology,
    pub preamp_tap: BPlusTap,
    pub power_tube_tap: BPlusTap,

    pub current_channel_mode: ChannelMode,

    // Per-buffer accumulators for PSU drain, drained in `end_buffer`.
    // Part of the physics (power-supply loading) — never decimated.
    pub preamp_current_sum: f32,
    pub preamp_current_count: u32,

    // GUI-only metering: plate voltages sampled once per host sample
    // (not per inner/oversampled sample) and accumulated across buffers
    // until the plugin drains them at the METER_UPDATE_INTERVAL_MS cadence.
    pub meter_this_host_sample: bool,
    pub v1_plate_sum: f32,
    pub v2_plate_sum: f32,
    pub v3v4_plate_sum: f32,
    pub plate_samples_counted: u32,
}

impl TweedInner {
    fn reset_plate_meters(&mut self) {
        self.v1_plate_sum = 0.0;
        self.v2_plate_sum = 0.0;
        self.v3v4_plate_sum = 0.0;
        self.plate_samples_counted = 0;
    }
}

impl InnerDspProcessor for TweedInner {
    fn begin_buffer(&mut self, n: usize) {
        self.amp_topology.begin_buffer(n);
        self.preamp_current_sum = 0.0;
        self.preamp_current_count = 0;
        // Plate meters deliberately NOT reset here — they accumulate
        // across buffers until the metering window closes.
    }

    fn begin_host_sample(&mut self) {
        self.amp_topology.advance_sample();
        self.meter_this_host_sample = true;

        // Wiper values hold across OS sub-samples (zero-order hold).
        let n = self.norm_smoother.tick();
        let b = self.bright_smoother.tick();
        let t = self.tone_smoother.tick();
        let h_n = self.grid_norm_handle;
        let h_b = self.grid_bright_handle;
        let h_t = self.grid_tone_handle;
        self.v2a_tube.set_pot_position(h_n, n);
        self.v2a_tube.set_pot_position(h_b, b);
        self.v2a_tube.set_pot_position(h_t, t);
    }

    fn process_inner(&mut self, input: f32) -> f32 {
        let b_plus_preamp = self.amp_topology.b_plus_at(self.preamp_tap);

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

        let v2a_out = self
            .v2a_tube
            .process_multi(
                &[v1_out.plate_a_ac_volts, v1_out.plate_b_ac_volts],
                b_plus_preamp,
            )
            .plate_ac_volts;

        let pi_input = self.coupling_v2a.process(v2a_out);

        let ot_volts = self.amp_topology.process_power_section(pi_input);

        // Meter the plates once per host sample (first inner sample only)
        // — the readout is a ~10ms mean, so oversampled resolution buys
        // nothing but per-inner-sample overhead.
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

        self.preamp_current_sum += v1_out.plate_a_current_amps
            + v1_out.plate_b_current_amps
            + self.v2a_tube.plate_current_amps();
        self.preamp_current_count += 1;

        ot_volts
    }

    fn end_buffer(&mut self) {
        let preamp_mean = if self.preamp_current_count > 0 {
            self.preamp_current_sum / self.preamp_current_count as f32
        } else {
            0.0
        };
        self.amp_topology
            .end_buffer(&[(self.preamp_tap, preamp_mean)]);
    }

    fn reset(&mut self) {
        self.v1_pair_stock.reset();
        self.v1_pair_mod.reset();
        self.v2a_tube.reset();
        self.coupling_v2a.reset();
        self.amp_topology.reset();
        self.preamp_current_sum = 0.0;
        self.preamp_current_count = 0;
        self.meter_this_host_sample = false;
        self.reset_plate_meters();
    }
}

// =============================================================================
// TweedEngine — runtime-dispatched DspEngine over OS factor
// =============================================================================

// OS factor is a const-generic-style choice fixed at construction, so this
// enum picks one `DspEngine<TweedInner, OS>` variant per supported factor.
// Changing OS factor rebuilds the whole engine (see `initialize()`).
pub enum TweedEngine {
    X1(DspEngine<TweedInner, X1Boundary>),
    X2(DspEngine<TweedInner, X2Boundary>),
    X4(DspEngine<TweedInner, X4Boundary>),
    X8(DspEngine<TweedInner, X8Boundary>),
}

impl TweedEngine {
    pub fn new(engine_rate: EngineRate, inner: TweedInner) -> Self {
        match engine_rate.oversampling {
            OversamplingFactor::X1 => Self::X1(DspEngine::new(
                engine_rate,
                X1Boundary::new(engine_rate),
                inner,
            )),
            OversamplingFactor::X2 => Self::X2(DspEngine::new(
                engine_rate,
                X2Boundary::new(engine_rate),
                inner,
            )),
            OversamplingFactor::X4 => Self::X4(DspEngine::new(
                engine_rate,
                X4Boundary::new(engine_rate),
                inner,
            )),
            OversamplingFactor::X8 => Self::X8(DspEngine::new(
                engine_rate,
                X8Boundary::new(engine_rate),
                inner,
            )),
        }
    }

    #[inline]
    pub fn engine_rate(&self) -> EngineRate {
        match self {
            Self::X1(e) => e.rate(),
            Self::X2(e) => e.rate(),
            Self::X4(e) => e.rate(),
            Self::X8(e) => e.rate(),
        }
    }

    #[inline]
    pub fn begin_buffer(&mut self, n: usize) {
        match self {
            Self::X1(e) => e.begin_buffer(n),
            Self::X2(e) => e.begin_buffer(n),
            Self::X4(e) => e.begin_buffer(n),
            Self::X8(e) => e.begin_buffer(n),
        }
    }

    #[inline]
    pub fn process_sample(&mut self, input: f32) -> f32 {
        match self {
            Self::X1(e) => e.process_sample(input),
            Self::X2(e) => e.process_sample(input),
            Self::X4(e) => e.process_sample(input),
            Self::X8(e) => e.process_sample(input),
        }
    }

    #[inline]
    pub fn end_buffer(&mut self) {
        match self {
            Self::X1(e) => e.end_buffer(),
            Self::X2(e) => e.end_buffer(),
            Self::X4(e) => e.end_buffer(),
            Self::X8(e) => e.end_buffer(),
        }
    }

    #[inline]
    pub fn reset(&mut self) {
        match self {
            Self::X1(e) => e.reset(),
            Self::X2(e) => e.reset(),
            Self::X4(e) => e.reset(),
            Self::X8(e) => e.reset(),
        }
    }

    #[inline]
    pub fn inner(&self) -> &TweedInner {
        match self {
            Self::X1(e) => e.inner(),
            Self::X2(e) => e.inner(),
            Self::X4(e) => e.inner(),
            Self::X8(e) => e.inner(),
        }
    }

    #[inline]
    pub fn inner_mut(&mut self) -> &mut TweedInner {
        match self {
            Self::X1(e) => e.inner_mut(),
            Self::X2(e) => e.inner_mut(),
            Self::X4(e) => e.inner_mut(),
            Self::X8(e) => e.inner_mut(),
        }
    }
}

// =============================================================================
// AudioState — sample-rate / block-size dependent runtime state
// =============================================================================

// Runtime state whose construction depends on the host's sample rate or
// max buffer size. Built lazily in `Plugin::initialize`; `None` before then.
pub struct AudioState {
    pub engine_rate: EngineRate,
    pub engine: TweedEngine,

    pub dc_blocker_output: DCBlocker,
    pub input_meter: InputLevelMeter,

    pub ir_convolver: ir_convolver::HotSwapConvolver,
    pub pre_ir_buffer: Vec<f32>,
    pub post_ir_buffer: Vec<f32>,
    pub ir_block_size: usize,
    pub ir_crossfade_samples: usize,

    // Always allocated, even in IR mode, so switching cab-modelling modes
    // is glitch-free and does no allocation on the audio thread.
    pub cab_processor: SpeakerCabRoomProcessor,

    // Metering window (METER_UPDATE_INTERVAL_MS of host samples): meters
    // publish to the GUI atomics only when the window closes.
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
        input_cal: &InputCalibration,
        jack_input: &JackInput,
    ) -> Self {
        let engine_rate = EngineRate::new(sample_rate, os_factor);

        let v1_pair_stock = build_v1_pair(engine_rate, V1_STOCK_SPEC);
        let v1_pair_mod = build_v1_pair(engine_rate, V1_MOD_SPEC);

        let amp_topology =
            AmpTopology::new(engine_rate, build_5e3_amp_topology_config());
        let preamp_tap = amp_topology.b_plus_tap("preamp");
        let power_tube_tap = amp_topology.b_plus_tap("power_tube");

        // V1 tube choice sets the plate source-Z baked into the V2A grid
        // network (~21kΩ 12AY7 vs ~38kΩ 12AX7), which shapes the bright
        // cap's HF lift.
        let tube_toggle = params.tube_toggle.value();
        let v1_spec_name = if tube_toggle { V1_MOD_SPEC } else { V1_STOCK_SPEC };
        let v1_spec = TubeRegistry::global()
            .lookup(v1_spec_name)
            .unwrap_or_else(|| panic!("Tube spec '{}' not found in registry", v1_spec_name));
        let v1_source_z = plate_source_impedance(v1_spec);

        let mut v2a_tube = build_v2a_tube(engine_rate);
        let V2aGridNetwork {
            circuit,
            norm_volume,
            bright_volume,
            tone,
        } = V2aGridNetwork::new(engine_rate, v1_source_z);
        v2a_tube.set_grid_circuit(circuit);

        let coupling_v2a = CouplingCapacitor::new(
            engine_rate,
            V2A_TO_V2B_COUPLING_CAP_F,
            V2B_GRID_LEAK_OHMS,
        );

        let init_norm = volume_taper.wiper_fraction(params.normal_volume.value());
        let init_bright = volume_taper.wiper_fraction(params.bright_volume.value());
        let init_tone = params.tone.value();

        // Snap wipers to current settings so the smoother starts at steady state.
        v2a_tube.set_pot_position(norm_volume, init_norm);
        v2a_tube.set_pot_position(bright_volume, init_bright);
        v2a_tube.set_pot_position(tone, init_tone);

        let inner = TweedInner {
            v1_pair_stock,
            v1_pair_mod,
            current_tube_toggle: tube_toggle,
            v2a_tube,
            grid_norm_handle: norm_volume,
            grid_bright_handle: bright_volume,
            grid_tone_handle: tone,
            norm_smoother: PotSmoother::new(sample_rate, init_norm, POT_SMOOTH_TAU_S),
            bright_smoother: PotSmoother::new(sample_rate, init_bright, POT_SMOOTH_TAU_S),
            tone_smoother: PotSmoother::new(sample_rate, init_tone, POT_SMOOTH_TAU_S),
            coupling_v2a,
            amp_topology,
            preamp_tap,
            power_tube_tap,
            current_channel_mode: params.channel_select.value(),
            preamp_current_sum: 0.0,
            preamp_current_count: 0,
            meter_this_host_sample: false,
            v1_plate_sum: 0.0,
            v2_plate_sum: 0.0,
            v3v4_plate_sum: 0.0,
            plate_samples_counted: 0,
        };
        let engine = TweedEngine::new(engine_rate, inner);

        let meter_ceiling = {
            let inner = engine.inner();
            let pair = if tube_toggle { &inner.v1_pair_mod } else { &inner.v1_pair_stock };
            meter_ceiling_for_pair(pair, jack_input)
        };
        let input_meter =
            InputLevelMeter::new(sample_rate, input_cal.input_scale(), meter_ceiling);

        let ir_convolver = ir_convolver::HotSwapConvolver::new(&[1.0], max_buffer_size, 1);
        let pre_ir_buffer = vec![0.0; max_buffer_size];
        let post_ir_buffer = vec![0.0; max_buffer_size];
        let ir_crossfade_samples = (IR_CROSSFADE_MS * sample_rate / 1000.0) as usize;

        let dc_blocker_output = DCBlocker::new(engine_rate, 10.0);

        let cab_processor = build_cab_processor(
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
            dc_blocker_output,
            input_meter,
            ir_convolver,
            pre_ir_buffer,
            post_ir_buffer,
            ir_block_size: max_buffer_size,
            ir_crossfade_samples,
            cab_processor,
            meter_window_len,
            meter_window_samples: 0,
            meter_output_peak: 0.0,
        }
    }
}

// =============================================================================
// TheTweed — Fender Deluxe 5E3 Plugin
// =============================================================================

pub struct TheTweed {
    params: Arc<TheTweedParams>,

    input_cal: InputCalibration,
    jack_input: JackInput,

    // 5E3 volume pots are 1MΩ Audio 30A taper.
    volume_taper: PotTaperConfig,

    // Output-transduction boundary: OT secondary volts -> -10dB loadbox
    // pad -> +24dBu-at-FS converter (IR arm only).
    loadbox_di: LoadboxDi,

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
    // 0 = not yet initialized; GUI falls back to X4 display.
    meter_os_ratio: Arc<atomic::AtomicU8>,
}

impl Default for TheTweed {
    fn default() -> Self {
        Self {
            params: Arc::new(TheTweedParams::default()),

            input_cal: InputCalibration::amp_standard(),
            jack_input: JackInput::new(68_000.0, 1_000_000.0),

            volume_taper: PotTaperConfig::new(PotTaper::Audio30A),

            loadbox_di: LoadboxDi::standard(),

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
        // Trim must be live in `input_cal` before `AudioState::build` reads it.
        let trim_db = self.params.input_trim_db.value();
        self.input_cal.set_user_trim_db(trim_db);
        self.cached_input_trim_db = trim_db;

        let audio_state = AudioState::build(
            sample_rate,
            max_buffer_size,
            os_factor,
            &self.params,
            &self.volume_taper,
            &self.input_cal,
            &self.jack_input,
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

    const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[AudioIOLayout {
        main_input_channels: NonZeroU32::new(1),
        main_output_channels: NonZeroU32::new(1),
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
        self.params.master.smoothed.reset(self.params.master.value());

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
        self.params.master.smoothed.reset(self.params.master.value());
        self.jack_input.reset();
        if let Some(audio) = self.audio_state.as_mut() {
            audio.engine.reset();
            audio.ir_convolver.reset();
            audio.cab_processor.reset();
            audio.pre_ir_buffer.fill(0.0);
            audio.post_ir_buffer.fill(0.0);
            audio.dc_blocker_output.reset();
            audio.input_meter.reset();
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

        if let Ok(mut pending) = self.ir_load_state.pending.try_lock() {
            if let Some(new_conv) = pending.take() {
                audio.ir_convolver.queue_swap(new_conv, audio.ir_crossfade_samples);
            }
        }

        // try_lock so the audio thread never blocks on a GUI-side build.
        if let Ok(mut pending) = self.cab_load_state.pending.try_lock() {
            if let Some(new_processor) = pending.take() {
                audio.cab_processor = new_processor;
            }
        }

        let num_samples = buffer.samples();
        let power_on = self.params.power.value();
        let mut sample_idx = 0usize;

        let current_trim_db = self.params.input_trim_db.value();
        if (current_trim_db - self.cached_input_trim_db).abs() > 0.01 {
            self.cached_input_trim_db = current_trim_db;
            self.input_cal.set_user_trim_db(current_trim_db);
            audio.input_meter.set_input_scale(self.input_cal.input_scale());
        }

        let cab_mode = self.params.cab_modelling_mode.value();
        let mic_x = self.params.mic_x_position.value();
        let mic_dist_in = self.params.mic_distance_inches.value();
        if mic_x != self.cached_mic_x_position
            || (mic_dist_in - self.cached_mic_distance_inches).abs() > 0.001
        {
            self.cached_mic_x_position = mic_x;
            self.cached_mic_distance_inches = mic_dist_in;
            audio.cab_processor.set_mic_placement(MicrophonePlacement {
                distance_m: mic_dist_in * 0.0254,
                radial_offset_cm: mic_x.radial_offset_cm(),
                off_axis_angle_deg: 0.0,
            });
        }

        // Tube swap changes V1's plate source-Z, so rebuild the V2A grid
        // network on toggle and snap it to the current pot settings.
        let current_tube_toggle = self.params.tube_toggle.value();
        if current_tube_toggle != audio.engine.inner().current_tube_toggle {
            let v1_spec_name = if current_tube_toggle { V1_MOD_SPEC } else { V1_STOCK_SPEC };
            let v1_spec = TubeRegistry::global()
                .lookup(v1_spec_name)
                .unwrap_or_else(|| panic!("Tube spec '{}' not found in registry", v1_spec_name));
            let v1_source_z = plate_source_impedance(v1_spec);
            let V2aGridNetwork {
                circuit,
                norm_volume,
                bright_volume,
                tone,
            } = V2aGridNetwork::new(audio.engine_rate, v1_source_z);

            let normal_wiper = self
                .volume_taper
                .wiper_fraction(self.params.normal_volume.value());
            let bright_wiper = self
                .volume_taper
                .wiper_fraction(self.params.bright_volume.value());
            let tone_pos = self.params.tone.value();

            let inner = audio.engine.inner_mut();
            inner.v2a_tube.set_grid_circuit(circuit);
            inner.grid_norm_handle = norm_volume;
            inner.grid_bright_handle = bright_volume;
            inner.grid_tone_handle = tone;
            inner.current_tube_toggle = current_tube_toggle;
            inner.norm_smoother.set_target(normal_wiper);
            inner.bright_smoother.set_target(bright_wiper);
            inner.tone_smoother.set_target(tone_pos);
            let h_n = inner.grid_norm_handle;
            let h_b = inner.grid_bright_handle;
            let h_t = inner.grid_tone_handle;
            inner.v2a_tube.set_pot_position(h_n, normal_wiper);
            inner.v2a_tube.set_pot_position(h_b, bright_wiper);
            inner.v2a_tube.set_pot_position(h_t, tone_pos);

            let ceiling = {
                let inner = audio.engine.inner();
                let v1_pair = if current_tube_toggle {
                    &inner.v1_pair_mod
                } else {
                    &inner.v1_pair_stock
                };
                meter_ceiling_for_pair(v1_pair, &self.jack_input)
            };
            audio.input_meter.set_clean_ceiling_v(ceiling);
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

        // Pass 1 — per-sample signal chain.
        for channel_samples in buffer.iter_samples() {
            for sample in channel_samples {
                if !power_on {
                    audio.pre_ir_buffer[sample_idx] = 0.0;
                    sample_idx += 1;
                    continue;
                }

                let input = *sample;

                // Meter reads the raw DAW signal, before calibration.
                audio.input_meter.process(input);

                let conditioned =
                    self.jack_input.process(self.input_cal.process(input));

                // Boundary OS: inner DSP fires OS_factor times per host sample.
                let ot_volts = audio.engine.process_sample(conditioned);

                // IR mode: loadbox DI converts OT secondary volts to samples.
                // Dynamic mode: SpeakerCabRoomProcessor's own mic preamp
                // performs the transduction, so pass raw volts through.
                audio.pre_ir_buffer[sample_idx] = match cab_mode {
                    CabModellingMode::Ir => self.loadbox_di.process(ot_volts),
                    CabModellingMode::Dynamic => ot_volts,
                };
                sample_idx += 1;
            }
        }

        audio.engine.end_buffer();

        // Pass 2 — cab modelling.
        let ir_block_size = audio.ir_block_size;
        match cab_mode {
            CabModellingMode::Ir => {
                for i in num_samples..ir_block_size {
                    audio.pre_ir_buffer[i] = 0.0;
                }
                audio.ir_convolver.process(
                    &audio.pre_ir_buffer[..ir_block_size],
                    &mut audio.post_ir_buffer[..ir_block_size],
                );
            }
            CabModellingMode::Dynamic => {
                for i in 0..num_samples {
                    // b_plus_sag: 1.0 (no-op) — sag is already applied upstream.
                    let (l, r) =
                        audio.cab_processor.process(audio.pre_ir_buffer[i], 1.0);
                    audio.post_ir_buffer[i] = 0.5 * (l + r);
                }
            }
        }

        // Pass 3 — output trim, master gain, DC block.
        let mut output_peak = 0.0f32;
        {
            let output_channel = &mut buffer.as_slice()[0];
            for i in 0..num_samples {
                if !power_on {
                    output_channel[i] = 0.0;
                    continue;
                }

                let mut signal = audio.post_ir_buffer[i];

                let output_trim = self.params.output_trim_db.smoothed.next();
                signal *= neampmod_engine::db_to_linear(output_trim);

                let master = self.params.master.smoothed.next();
                let master_gain = master.powf(1.5);
                signal *= master_gain;

                signal = audio.dc_blocker_output.process(signal);

                output_peak = output_peak.max(signal.abs());
                output_channel[i] = signal;
            }
        }

        // Publish meters only when the ~10ms window closes; the plate sums
        // (and output peak) keep accumulating across buffers in between.
        audio.meter_output_peak = audio.meter_output_peak.max(output_peak);
        audio.meter_window_samples += num_samples;
        if audio.meter_window_samples >= audio.meter_window_len {
            let metrics = audio.input_meter.get_metrics();
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
    const CLAP_DESCRIPTION: Option<&'static str> = Some("Vintage tweed amplifier simulator inspired by classic 1950s circuits.");
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
