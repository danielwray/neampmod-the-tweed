use nih_plug::prelude::*;
use std::sync::{Arc, Mutex, atomic};
use std::collections::HashMap;

#[cfg(feature = "gui")]
mod gui;

// Import all DSP from neampmod-engine
use neampmod_engine::{
    // Tube modeling
    TubeStage,
    TubeStageConfig,
    PreampTubeType,
    PowerTubeStage,
    PowerTubeType,
    GridCurrentConfig,
    // Power supply
    PowerSupplySag,
    PowerSupplySagConfig,
    PowerSupplyRipple,
    PowerSupplyRippleConfig,
    PowerSupplyTopology,
    FilterChainSpec,
    FilterChainNodeSpec,
    FilterCapSpec,
    FilterResistorSpec,
    ScreenGridSag,
    // Bias modeling
    CathodeBias,
    CathodeBiasConfig,
    PowerTubeBias,
    PowerTubeBiasConfig,
    // Filters
    DCBlocker,
    NthOrderTdfii,
    // MNA mixing + tone network
    MnaSystem,
    PassiveNetworkSpec,
    // Output transformer
    OutputTransformer,
    TransformerType,
    // Calibration
    InputCalibration,
    OutputCalibration,
    // Coupling capacitors and grid coupling
    CouplingCapacitor,
    GridCouplingNetwork,
    GridBiasType,
    // Phase inverter
    PhaseInverter,
    PhaseInverterTopology,
    // Speaker impedance and dynamics
    SpeakerImpedanceCurve,
    SpeakerPreset,
    SpeakerNormalizer,
    SpeakerModel,
    // IR loader and convolver (as modules)
    ir_loader,
    ir_convolver,
    // Attenuation
    InterstageAttenuator,
    // Pot taper modeling
    PotTaper,
    PotTaperConfig,
};

// Embedded IR from assets/ir/default.wav (compiled into binary)
const CABINET_IR_BYTES: &[u8] = include_bytes!("../assets/ir/default.wav");

/// Maps internal 0.0–1.0 parameter value to 5E3 faceplate numbering (1–12).
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

/// Parses 5E3 faceplate numbering (1–12) back to internal 0.0–1.0.
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

#[derive(Params)]
struct TheTweedParams {
    /// Bright channel volume - drives V1B preamp saturation
    #[id = "bright_volume"]
    pub bright_volume: FloatParam,

    /// Normal channel volume - drives V1A preamp saturation
    #[id = "normal_volume"]
    pub normal_volume: FloatParam,

    /// Channel selector - Normal / Both (jumpered) / Bright
    #[id = "channel_select"]
    pub channel_select: EnumParam<ChannelMode>,

    /// Tone control - treble/bass balance
    #[id = "tone"]
    pub tone: FloatParam,

    /// Master power switch
    #[id = "power"]
    pub power: BoolParam,

    /// V1 tube toggle - switches between 12AY7 (off) and 12AX7 (on)
    /// Real-world mod: 12AX7 provides more gain and earlier breakup
    #[id = "tube_toggle"]
    pub tube_toggle: BoolParam,

    /// Master volume - final output level control
    #[id = "master"]
    pub master: FloatParam,

    /// Input calibration trim - adjusts input sensitivity
    /// Range: -12 dB to +12 dB for interface matching
    #[id = "input_trim"]
    pub input_trim_db: FloatParam,

    /// Output calibration trim - adjusts final output level
    /// Range: -24 dB to 0 dB for mixing headroom
    #[id = "output_trim"]
    pub output_trim_db: FloatParam,

    /// IR file path - persisted with DAW session state
    #[persist = "ir_path"]
    pub ir_file_path: Arc<Mutex<String>>,
}


impl Default for TheTweedParams {
    fn default() -> Self {
        Self {
            // Bright channel volume - drives V1B preamp saturation
            bright_volume: FloatParam::new(
                "Bright",
                0.42,
                FloatRange::Linear { min: 0.01, max: 1.0 },
            )
            .with_smoother(SmoothingStyle::Logarithmic(10.0))
            .with_value_to_string(v2s_dial_1_to_12())
            .with_string_to_value(s2v_dial_1_to_12()),

            // Normal channel volume - drives V1A preamp saturation
            normal_volume: FloatParam::new(
                "Normal",
                0.35,
                FloatRange::Linear { min: 0.01, max: 1.0 },
            )
            .with_smoother(SmoothingStyle::Logarithmic(10.0))
            .with_value_to_string(v2s_dial_1_to_12())
            .with_string_to_value(s2v_dial_1_to_12()),

            // Channel selector - Normal / Both (jumpered, default) / Bright
            channel_select: EnumParam::new("Channel", ChannelMode::Both),

            // Tone control
            tone: FloatParam::new(
                "Tone",
                0.4,
                FloatRange::Linear { min: 0.01, max: 1.0 },
            )
            .with_smoother(SmoothingStyle::Logarithmic(5.0))
            .with_value_to_string(v2s_dial_1_to_12())
            .with_string_to_value(s2v_dial_1_to_12()),

            power: BoolParam::new("Power", true),

            // Tube toggle - switches V1 between 12AY7 (stock) and 12AX7 (mod)
            tube_toggle: BoolParam::new("Tube Toggle", false),

            // Master volume - final output level control
            master: FloatParam::new(
                "Master",
                0.3,
                FloatRange::Linear { min: 0.0001, max: 1.0 },
            )
            .with_smoother(SmoothingStyle::Logarithmic(10.0))
            .with_value_to_string(v2s_dial_1_to_12())
            .with_string_to_value(s2v_dial_1_to_12()),

            // Input calibration trim
            input_trim_db: FloatParam::new(
                "Input Trim",
                0.0,  // No adjustment by default
                FloatRange::Linear { min: -18.0, max: 12.0 },
            )
            .with_unit(" dB")
            .with_step_size(0.1)
            .with_smoother(SmoothingStyle::Linear(5.0))
            .with_value_to_string(formatters::v2s_f32_rounded(1))
            .with_string_to_value(Arc::new(|s: &str| s.trim().parse().ok())),

            // Output calibration trim
            output_trim_db: FloatParam::new(
                "Output Trim",
                0.0,  // Additional trim on top of engine's 0db pro_audio_headroom()
                FloatRange::Linear { min: -24.0, max: -3.0 },
            )
            .with_unit(" dB")
            .with_step_size(0.1)
            .with_smoother(SmoothingStyle::Linear(5.0))
            .with_value_to_string(formatters::v2s_f32_rounded(1))
            .with_string_to_value(Arc::new(|s: &str| s.trim().parse().ok())),

            // IR file path - persisted with DAW session
            ir_file_path: Arc::new(Mutex::new("default.wav".to_string())),
        }
    }
}

/// Build speaker impedance curve for Jensen P12Q 1x12 open-back cabinet
/// Jensen P12Q in open-back 1x12 configuration (vintage tweed amp style)
fn build_speaker_impedance(sample_rate: f32) -> SpeakerImpedanceCurve {
    let curve = SpeakerImpedanceCurve::from_preset(sample_rate, SpeakerPreset::JensenP12);
    let mut config = curve.config().clone();
    config.cabinet_factor = 0.75; // Open-back 1x12 reduces LF resonance
    SpeakerImpedanceCurve::new(sample_rate, config)
}

/// Build 5E3 Tweed Deluxe power supply filter chain
///
/// Real 5E3 topology:
/// ```text
/// 5Y3 Rectifier
///   ↓
/// B+1: 8µF/450V ─-> Power tube plates (via OT center tap)
///   ↓ 5kΩ dropping resistor
/// B+2: 8µF/450V ─-> Power tube screens
///   ↓ 22kΩ dropping resistor
/// B+3: 8µF/450V ─-> All preamp plates (V1A, V1B, V2A, V2B)
/// ```
///
/// Voltage levels (approximate under load):
/// - B+1: ~360-380V (highest, least filtered)
/// - B+2: ~340-360V (5kΩ drop from B+1)
/// - B+3: ~320-340V (22kΩ drop from B+2, most filtered)
fn build_5e3_power_supply_spec() -> FilterChainSpec {
    FilterChainSpec {
        nodes: vec![
            // B+1: First filter cap (power tube plates)
            FilterChainNodeSpec::Capacitor(FilterCapSpec {
                instance_id: "b1_8uf".to_string(),
                capacitance_uf: 8.0,
                voltage_rating: 450.0,
            }),
            // 5kΩ dropping resistor (B+1 -> B+2)
            FilterChainNodeSpec::Resistor(FilterResistorSpec {
                resistance_ohms: 5_000.0,
            }),
            // B+2: Second filter cap (power tube screens)
            FilterChainNodeSpec::Capacitor(FilterCapSpec {
                instance_id: "b2_8uf".to_string(),
                capacitance_uf: 8.0,
                voltage_rating: 450.0,
            }),
            // 22kΩ dropping resistor (B+2 -> B+3)
            FilterChainNodeSpec::Resistor(FilterResistorSpec {
                resistance_ohms: 22_000.0,
            }),
            // B+3: Third filter cap (all preamp stages)
            FilterChainNodeSpec::Capacitor(FilterCapSpec {
                instance_id: "b3_8uf".to_string(),
                capacitance_uf: 8.0,
                voltage_rating: 450.0,
            }),
        ],
        b_plus_assignments: HashMap::from([
            // TODO: look at how to map B+1/ B+2 to respective parts of the power tube circuit... (research if that makes sense)
            // Power tubes use B+1 (highest voltage, least filtered)
            ("power".to_string(), "b1_8uf".to_string()),
            // All preamp stages use B+3 (lowest voltage, most filtered)
            ("preamp".to_string(), "b3_8uf".to_string()),
        ]),
        // 5E3 nominal B+ from rectified 325-0-325 power transformer
        nominal_b_plus_volts: 360.0,
    }
}

pub struct TheTweed {
    params: Arc<TheTweedParams>,
    sample_rate: f32,

    // === Calibration ===
    input_cal: InputCalibration,
    output_cal: OutputCalibration,

    // Unified MNA mixing + tone network (replaces BrightChannelFilter, manual mixing, ToneStack5E3)
    // Two transfer functions — one per input channel — solved from the same 5E3 passive netlist.
    // Controls: [normal_wiper, bright_wiper, tone] — updated per-sample with change detection.
    mna_normal: MnaSystem,      // H(s): V1A coupling cap -> V2A grid
    mna_bright: MnaSystem,      // H(s): V1B coupling cap -> V2A grid
    filter_normal: NthOrderTdfii,
    filter_bright: NthOrderTdfii,
    mixing_tone_controls: [f64; 3],  // cached for change detection

    // Tube bias modeling
    preamp_bias: CathodeBias,
    power_bias: PowerTubeBias,

    // Preamp tube stages (Koren physics-based LUTs)
    // V1A (Normal channel) — two halves of the same tube
    v1a_tube_12ay7: TubeStage,  // 12AY7 first triode (V1a) - stock tube
    v1a_tube_12ax7: TubeStage,  // 12AX7 first triode (V1a) - mod tube
    // V1B (Bright channel) — other half of V1
    v1b_tube_12ay7: TubeStage,  // 12AY7 second triode (V1b) - stock tube
    v1b_tube_12ax7: TubeStage,  // 12AX7 second triode (V1b) - mod tube
    v2a_tube: TubeStage,        // 12AX7 Gain stage (V2a)

    // Cathodyne phase inverter (V2B equivalent)
    phase_inverter: PhaseInverter,

    // Passive coupling capacitors (DC blocking, no grid conduction)
    // V1 plate → volume/mixing network: 0.1µF into 1MΩ
    coupling_v1: CouplingCapacitor,
    coupling_v1b: CouplingCapacitor,
    // V2A plate → PI grid: passive coupling (cathodyne PI cathode at ~165V,
    // so V_gk ≈ -165V — grid conduction is physically impossible)
    coupling_v2a: CouplingCapacitor,
    // Grid coupling networks (DC blocking + nonlinear grid conduction)
    // PI → 6V6 grids: 0.1µF, 220kΩ grid leak, 1.5kΩ grid stopper per phase
    coupling_power_pos: GridCouplingNetwork,
    coupling_power_neg: GridCouplingNetwork,

    // Push-pull power stage (two 6V6 tubes)
    power_tube_1: PowerTubeStage,
    power_tube_2: PowerTubeStage,

    // Screen grid sag (independent per push-pull tube)
    screen_sag_1: ScreenGridSag,
    screen_sag_2: ScreenGridSag,

    // Power supply sag (5Y3 rectifier)
    power_supply_sag: PowerSupplySag,
    // B+ ripple injection (5Y3 mains ripple at 120Hz for 60Hz American mains)
    power_supply_ripple: PowerSupplyRipple,
    // Power supply topology (B+1, B+2, B+3 filter chain)
    power_supply_topology: PowerSupplyTopology,

    // Output transformer (Small American style for 5E3)
    output_transformer: OutputTransformer,

    // === Speaker Impedance ===
    speaker_impedance: SpeakerImpedanceCurve,

    // === Speaker Normalizer (physical secondary volts → normalized ±1 for IR) ===
    speaker_normalizer: SpeakerNormalizer,

    // === IR Convolution (block-based, matched to DAW buffer size) ===
    ir_convolver: ir_convolver::ZeroLatencyConvolver,
    pre_ir_buffer: Vec<f32>,
    post_ir_buffer: Vec<f32>,
    ir_block_size: usize,

    // PI-to-power interstage: separate per phase (each 6V6 has its own
    // 1.5kΩ grid stopper + 220kΩ grid leak + Miller capacitance path)
    pi_to_power_pos: InterstageAttenuator,
    pi_to_power_neg: InterstageAttenuator,

    // Volume pot taper (1MΩ 15A audio taper, same for both channels)
    volume_taper: PotTaperConfig,
    dc_blocker_output: DCBlocker,

    // IR loading state (shared with GUI)
    ir_load_status: Arc<atomic::AtomicU8>,  // 0=pending, 1=success, 2=failed
}

impl Default for TheTweed {
    fn default() -> Self {
        let sample_rate = 48000.0;

        // 5E3-specific grid config: 22nF coupling caps
        let v1_grid_12ay7 = GridCurrentConfig {
            coupling_cap: 100e-9,
            charge_multiplier: 0.004,
            ..PreampTubeType::Triode12AY7.grid_config()
        };
        let v1_grid_12ax7 = GridCurrentConfig {
            coupling_cap: 100e-9,
            charge_multiplier: 0.004,
            ..PreampTubeType::Triode12AX7.grid_config()
        };

        // Create V2A and power tubes before struct literal to obtain ac_swing values
        // for GridCouplingNetwork construction.
        let v2a_tube = TubeStage::from_config(
            sample_rate,
            TubeStageConfig::new(PreampTubeType::Triode12AX7)
                .with_plate_voltage_fraction(250.0 / 330.0)
                .with_cathode_circuit(1500.0, Some(25.0)),
        );

        // Power tubes with internal grid model disabled (GridCouplingNetwork models it externally)
        let mut power_tube_1 = PowerTubeStage::new(sample_rate, PowerTubeType::BeamTetrode6V6);
        power_tube_1.disable_internal_grid_model();
        let power_ac_swing = power_tube_1.voltage_cal().ac_swing;
        let mut power_tube_2 = PowerTubeStage::new(sample_rate, PowerTubeType::BeamTetrode6V6);
        power_tube_2.disable_internal_grid_model();

        Self {
            params: Arc::new(TheTweedParams::default()),
            sample_rate,

            input_cal: InputCalibration::amp_standard(),
            output_cal: OutputCalibration::pro_audio_headroom(),

            // Full MNA model of the 5E3 mixing + tone passive network.
            mna_normal: {
                let spec = PassiveNetworkSpec::mixing_5e3_normal(
                    100_000.0, 1_000_000.0, 68_000.0, 1_000_000.0, 500e-12, 4.7e-9,
                );
                MnaSystem::from_netlist(&spec).expect("5E3 normal mixing netlist")
            },
            mna_bright: {
                let spec = PassiveNetworkSpec::mixing_5e3_bright(
                    100_000.0, 1_000_000.0, 68_000.0, 1_000_000.0, 500e-12, 4.7e-9,
                );
                MnaSystem::from_netlist(&spec).expect("5E3 bright mixing netlist")
            },
            filter_normal: NthOrderTdfii::new(2, sample_rate),
            filter_bright: NthOrderTdfii::new(2, sample_rate),
            mixing_tone_controls: [-1.0; 3],

            preamp_bias: CathodeBias::new(CathodeBiasConfig::fender_5e3()),
            power_bias: PowerTubeBias::new_cathode(
                CathodeBiasConfig::fender_5e3(),
                PowerTubeBiasConfig::fender_5e3(),
            ),

            // V1A (Normal channel) — 820Ω shared cathode, 25µF bypass (fc≈7.7Hz, fully bypassed)
            v1a_tube_12ay7: TubeStage::from_config(
                sample_rate,
                TubeStageConfig::new(PreampTubeType::Triode12AY7)
                    .with_plate_voltage_fraction(250.0 / 330.0)
                    .with_cathode_circuit(820.0, Some(25.0))
                    .with_grid_config(v1_grid_12ay7.clone()),
            ),
            v1a_tube_12ax7: TubeStage::from_config(
                sample_rate,
                TubeStageConfig::new(PreampTubeType::Triode12AX7)
                    .with_plate_voltage_fraction(250.0 / 330.0)
                    .with_cathode_circuit(820.0, Some(25.0))
                    .with_grid_config(v1_grid_12ax7.clone()),
            ),
            // V1B (Bright channel) — same physical tube, shared cathode
            v1b_tube_12ay7: TubeStage::from_config(
                sample_rate,
                TubeStageConfig::new(PreampTubeType::Triode12AY7)
                    .with_plate_voltage_fraction(250.0 / 330.0)
                    .with_cathode_circuit(820.0, Some(25.0))
                    .with_grid_config(v1_grid_12ay7),
            ),
            v1b_tube_12ax7: TubeStage::from_config(
                sample_rate,
                TubeStageConfig::new(PreampTubeType::Triode12AX7)
                    .with_plate_voltage_fraction(250.0 / 330.0)
                    .with_cathode_circuit(820.0, Some(25.0))
                    .with_grid_config(v1_grid_12ax7),
            ),
            // V2A (12AX7 gain stage) — 1500Ω cathode, 25µF bypass (fc≈4.2Hz, fully bypassed)
            v2a_tube,

            phase_inverter: PhaseInverter::from_topology(sample_rate, PhaseInverterTopology::Cathodyne),

            // Passive coupling caps (V1 plate → volume/mixing, no grid conduction)
            coupling_v1: CouplingCapacitor::new(sample_rate, 0.1e-6, 1_000_000.0),
            coupling_v1b: CouplingCapacitor::new(sample_rate, 0.1e-6, 1_000_000.0),
            // V2A → PI: passive coupling (cathodyne PI grid never conducts — cathode at ~165V)
            coupling_v2a: CouplingCapacitor::new(sample_rate, 0.02e-6, 1_000_000.0),
            // Grid coupling: PI → 6V6 (cathode-biased, grid leak to ground)
            coupling_power_pos: GridCouplingNetwork::from_power_tube(
                sample_rate, PowerTubeType::BeamTetrode6V6,
                0.1e-6, 220_000.0, 1_500.0,
                GridBiasType::CathodeBias { cathode_voltage: 22.0 },
                power_ac_swing,
            ),
            coupling_power_neg: GridCouplingNetwork::from_power_tube(
                sample_rate, PowerTubeType::BeamTetrode6V6,
                0.1e-6, 220_000.0, 1_500.0,
                GridBiasType::CathodeBias { cathode_voltage: 22.0 },
                power_ac_swing,
            ),

            // 1.5kΩ grid stopper into 220kΩ 6V6 grid leak, ~150pF Miller cap
            // Separate per 6V6 — each power tube has its own grid stopper circuit
            pi_to_power_pos: {
                let mut att = InterstageAttenuator::with_grid_stopper(1500.0, 220_000.0, 150e-12);
                att.initialize(sample_rate);
                att
            },
            pi_to_power_neg: {
                let mut att = InterstageAttenuator::with_grid_stopper(1500.0, 220_000.0, 150e-12);
                att.initialize(sample_rate);
                att
            },

            // 5E3 volume pots: 1MΩ CTS 15A audio taper (both channels identical)
            volume_taper: PotTaperConfig::new(PotTaper::Audio15A),

            // Power tubes: 6V6 with internal grid model disabled (GridCouplingNetwork handles it)
            power_tube_1,
            power_tube_2,

            screen_sag_1: ScreenGridSag::new(sample_rate, PowerTubeType::BeamTetrode6V6),
            screen_sag_2: ScreenGridSag::new(sample_rate, PowerTubeType::BeamTetrode6V6),

            power_supply_sag: PowerSupplySag::with_config(
                sample_rate,
                PowerSupplySagConfig::vintage_american().with_current_tracking(80.0),
            ),
            power_supply_ripple: PowerSupplyRipple::with_config(
                sample_rate,
                PowerSupplyRippleConfig::vintage_american(),
            ),
            power_supply_topology: PowerSupplyTopology::from_spec(&build_5e3_power_supply_spec()),

            output_transformer: OutputTransformer::from_type(sample_rate, TransformerType::SmallAmerican),

            speaker_impedance: build_speaker_impedance(sample_rate),

            speaker_normalizer: SpeakerNormalizer::from_speaker_model(SpeakerModel::JensenP),

            // IR convolution - load and process embedded default.wav
            ir_convolver: {
                let ir_loader = ir_loader::IrLoader::new(sample_rate);
                match ir_loader.load_from_bytes(CABINET_IR_BYTES) {
                    Ok((ir, _, _)) => {
                        let mut processed_ir = ir;
                        // Remove DC offset and normalize
                        ir_loader::IrLoader::remove_dc_offset(&mut processed_ir);
                        ir_loader::IrLoader::normalize_rms(&mut processed_ir, -12.0);
                        // Zero-latency convolver: block_size=512, FIR=128
                        ir_convolver::ZeroLatencyConvolver::new(&processed_ir, 512, 128)
                    }
                    Err(_) => {
                        // Fallback: unity impulse (bypass)
                        ir_convolver::ZeroLatencyConvolver::new(&[1.0], 512, 1)
                    }
                }
            },
            pre_ir_buffer: vec![0.0; 512],
            post_ir_buffer: vec![0.0; 512],
            ir_block_size: 512,

            dc_blocker_output: DCBlocker::new(sample_rate, 10.0),

            // IR loading state (shared with GUI)
            ir_load_status: Arc::new(atomic::AtomicU8::new(1)),  // Start with success (embedded IR)
        }
    }
}

impl TheTweed {
    /// Load IR from file path
    /// Returns true if successful
    // TODO: Identify why loading sometimes takes to attempts
    pub fn load_ir_from_file(&mut self, path: &std::path::Path) -> bool {
        use neampmod_engine::{ir_loader::IrLoader, ir_convolver::ZeroLatencyConvolver};

        let ir_loader = IrLoader::new(self.sample_rate);

        match ir_loader.load_from_file(path) {
            Ok((mut ir, _, _)) => {
                // Process IR: remove DC and normalize
                IrLoader::remove_dc_offset(&mut ir);
                IrLoader::normalize_rms(&mut ir, -12.0);

                // Create new convolver matched to DAW buffer size
                let fir_len = 128.min(self.ir_block_size);
                self.ir_convolver = ZeroLatencyConvolver::new(&ir, self.ir_block_size, fir_len);

                // Update status
                self.ir_load_status.store(1, atomic::Ordering::Relaxed);
                if let Ok(mut path_str) = self.params.ir_file_path.lock() {
                    *path_str = path.display().to_string();
                }

                true
            }
            Err(_) => {
                self.ir_load_status.store(2, atomic::Ordering::Relaxed);
                false
            }
        }
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
        self.sample_rate = buffer_config.sample_rate;
        self.ir_block_size = buffer_config.max_buffer_size as usize;

        // Resize IR processing buffers to match DAW buffer size
        self.pre_ir_buffer.resize(self.ir_block_size, 0.0);
        self.post_ir_buffer.resize(self.ir_block_size, 0.0);

        // Initialize parameter smoothing
        self.params.bright_volume.smoothed.reset(self.params.bright_volume.value());
        self.params.normal_volume.smoothed.reset(self.params.normal_volume.value());
        self.params.tone.smoothed.reset(self.params.tone.value());
        self.params.master.smoothed.reset(self.params.master.value());

        // Rebuild MNA mixing + tone network at new sample rate.
        {
            let spec = PassiveNetworkSpec::mixing_5e3_normal(
                100_000.0, 1_000_000.0, 68_000.0, 1_000_000.0, 500e-12, 4.7e-9,
            );
            self.mna_normal = MnaSystem::from_netlist(&spec)
                .expect("5E3 normal mixing netlist");
        }
        {
            let spec = PassiveNetworkSpec::mixing_5e3_bright(
                100_000.0, 1_000_000.0, 68_000.0, 1_000_000.0, 500e-12, 4.7e-9,
            );
            self.mna_bright = MnaSystem::from_netlist(&spec)
                .expect("5E3 bright mixing netlist");
        }
        self.filter_normal = NthOrderTdfii::new(2, self.sample_rate);
        self.filter_bright = NthOrderTdfii::new(2, self.sample_rate);
        self.mixing_tone_controls = [-1.0; 3];

        // Initialize bias modeling
        self.preamp_bias.initialize(self.sample_rate);
        self.power_bias.initialize(self.sample_rate);

        // 5E3-specific grid configs
        let v1_grid_12ay7 = GridCurrentConfig {
            coupling_cap: 100e-9,
            charge_multiplier: 0.004,
            ..PreampTubeType::Triode12AY7.grid_config()
        };
        let v1_grid_12ax7 = GridCurrentConfig {
            coupling_cap: 100e-9,
            charge_multiplier: 0.004,
            ..PreampTubeType::Triode12AX7.grid_config()
        };

        // V1A (Normal) — 820Ω shared cathode, 25µF bypass (fc≈7.7Hz)
        self.v1a_tube_12ay7 = TubeStage::from_config(
            self.sample_rate,
            TubeStageConfig::new(PreampTubeType::Triode12AY7)
                .with_plate_voltage_fraction(250.0 / 330.0)
                .with_cathode_circuit(820.0, Some(25.0))
                .with_grid_config(v1_grid_12ay7.clone()),
        );
        self.v1a_tube_12ax7 = TubeStage::from_config(
            self.sample_rate,
            TubeStageConfig::new(PreampTubeType::Triode12AX7)
                .with_plate_voltage_fraction(250.0 / 330.0)
                .with_cathode_circuit(820.0, Some(25.0))
                .with_grid_config(v1_grid_12ax7.clone()),
        );
        // V1B (Bright) — same physical tube, shared cathode
        self.v1b_tube_12ay7 = TubeStage::from_config(
            self.sample_rate,
            TubeStageConfig::new(PreampTubeType::Triode12AY7)
                .with_plate_voltage_fraction(250.0 / 330.0)
                .with_cathode_circuit(820.0, Some(25.0))
                .with_grid_config(v1_grid_12ay7),
        );
        self.v1b_tube_12ax7 = TubeStage::from_config(
            self.sample_rate,
            TubeStageConfig::new(PreampTubeType::Triode12AX7)
                .with_plate_voltage_fraction(250.0 / 330.0)
                .with_cathode_circuit(820.0, Some(25.0))
                .with_grid_config(v1_grid_12ax7),
        );
        // V2A (12AX7 gain stage) — 1500Ω cathode, 25µF bypass (fc≈4.2Hz)
        self.v2a_tube = TubeStage::from_config(
            self.sample_rate,
            TubeStageConfig::new(PreampTubeType::Triode12AX7)
                .with_plate_voltage_fraction(250.0 / 330.0)
                .with_cathode_circuit(1500.0, Some(25.0)),
        );

        self.phase_inverter = PhaseInverter::from_topology(self.sample_rate, PhaseInverterTopology::Cathodyne);
        // 1.5kΩ grid stopper into 220kΩ 6V6 grid leak, ~150pF Miller cap
        // Separate per 6V6 — each power tube has its own grid stopper circuit
        self.pi_to_power_pos = InterstageAttenuator::with_grid_stopper(1500.0, 220_000.0, 150e-12);
        self.pi_to_power_pos.initialize(self.sample_rate);
        self.pi_to_power_neg = InterstageAttenuator::with_grid_stopper(1500.0, 220_000.0, 150e-12);
        self.pi_to_power_neg.initialize(self.sample_rate);

        // Passive coupling caps (V1 plate → volume/mixing)
        self.coupling_v1 = CouplingCapacitor::new(self.sample_rate, 0.1e-6, 1_000_000.0);
        self.coupling_v1b = CouplingCapacitor::new(self.sample_rate, 0.1e-6, 1_000_000.0);
        // V2A → PI: passive coupling (cathodyne PI grid never conducts)
        self.coupling_v2a = CouplingCapacitor::new(self.sample_rate, 0.02e-6, 1_000_000.0);

        // Power tubes (6V6) — internal grid model disabled, GridCouplingNetwork handles it
        self.power_tube_1 = PowerTubeStage::new(self.sample_rate, PowerTubeType::BeamTetrode6V6);
        self.power_tube_1.disable_internal_grid_model();
        let power_ac_swing = self.power_tube_1.voltage_cal().ac_swing;
        self.power_tube_2 = PowerTubeStage::new(self.sample_rate, PowerTubeType::BeamTetrode6V6);
        self.power_tube_2.disable_internal_grid_model();
        // Grid coupling: PI → 6V6 (cathode-biased, grid leak to ground)
        self.coupling_power_pos = GridCouplingNetwork::from_power_tube(
            self.sample_rate, PowerTubeType::BeamTetrode6V6,
            0.1e-6, 220_000.0, 1_500.0,
            GridBiasType::CathodeBias { cathode_voltage: 22.0 },
            power_ac_swing,
        );
        self.coupling_power_neg = GridCouplingNetwork::from_power_tube(
            self.sample_rate, PowerTubeType::BeamTetrode6V6,
            0.1e-6, 220_000.0, 1_500.0,
            GridBiasType::CathodeBias { cathode_voltage: 22.0 },
            power_ac_swing,
        );

        self.screen_sag_1 = ScreenGridSag::new(self.sample_rate, PowerTubeType::BeamTetrode6V6);
        self.screen_sag_2 = ScreenGridSag::new(self.sample_rate, PowerTubeType::BeamTetrode6V6);

        self.power_supply_sag = PowerSupplySag::with_config(
            self.sample_rate,
            PowerSupplySagConfig::vintage_american().with_current_tracking(80.0),
        );
        self.power_supply_ripple = PowerSupplyRipple::with_config(
            self.sample_rate,
            PowerSupplyRippleConfig::vintage_american(),
        );

        // Reinitialize power supply topology
        self.power_supply_topology = PowerSupplyTopology::from_spec(&build_5e3_power_supply_spec());

        self.output_transformer = OutputTransformer::from_type(self.sample_rate, TransformerType::SmallAmerican);
        self.speaker_impedance = build_speaker_impedance(self.sample_rate);
        self.speaker_normalizer = SpeakerNormalizer::from_speaker_model(SpeakerModel::JensenP);

        // Reload IR convolver with new sample rate and DAW buffer size
        // Check if a custom IR was persisted; if so, reload it from file
        let persisted_ir_path = self.params.ir_file_path.lock()
            .map(|p| p.clone())
            .unwrap_or_else(|_| "default.wav".to_string());

        let ir_reloaded = if persisted_ir_path != "default.wav" {
            // Try to reload the custom IR from its original file path
            let path = std::path::PathBuf::from(&persisted_ir_path);
            if path.exists() {
                self.load_ir_from_file(&path)
            } else {
                false
            }
        } else {
            false
        };

        if !ir_reloaded {
            // Fall back to embedded default IR
            let ir_loader = ir_loader::IrLoader::new(self.sample_rate);
            if let Ok((ir, _, _)) = ir_loader.load_from_bytes(CABINET_IR_BYTES) {
                let mut processed_ir = ir;
                ir_loader::IrLoader::remove_dc_offset(&mut processed_ir);
                ir_loader::IrLoader::normalize_rms(&mut processed_ir, -12.0);
                let fir_len = 128.min(self.ir_block_size);
                self.ir_convolver = ir_convolver::ZeroLatencyConvolver::new(&processed_ir, self.ir_block_size, fir_len);
            }
            // Reset path to default if custom failed to load
            if persisted_ir_path != "default.wav" {
                if let Ok(mut p) = self.params.ir_file_path.lock() {
                    *p = "default.wav".to_string();
                }
                self.ir_load_status.store(2, atomic::Ordering::Relaxed);
            }
        }

        self.dc_blocker_output = DCBlocker::new(self.sample_rate, 10.0);

        true
    }

    fn reset(&mut self) {
        // Reset parameter smoothing
        self.params.bright_volume.smoothed.reset(self.params.bright_volume.value());
        self.params.normal_volume.smoothed.reset(self.params.normal_volume.value());
        self.params.tone.smoothed.reset(self.params.tone.value());
        self.params.master.smoothed.reset(self.params.master.value());

        // Reset MNA mixing + tone filters
        self.filter_normal.reset();
        self.filter_bright.reset();
        self.mixing_tone_controls = [-1.0; 3];

        // Reset bias modeling
        self.preamp_bias.reset();
        self.power_bias.reset();

        // Reset tube stages (both channels)
        self.v1a_tube_12ay7.reset();
        self.v1a_tube_12ax7.reset();
        self.v1b_tube_12ay7.reset();
        self.v1b_tube_12ax7.reset();
        self.v2a_tube.reset();
        self.phase_inverter.reset();
        self.power_tube_1.reset();
        self.power_tube_2.reset();

        // Reset coupling capacitors
        self.coupling_v1.reset();
        self.coupling_v1b.reset();
        self.coupling_v2a.reset();
        self.coupling_power_pos.reset();
        self.coupling_power_neg.reset();

        // Reset interstage attenuators (per-phase grid stopper filters)
        self.pi_to_power_pos.reset();
        self.pi_to_power_neg.reset();

        // Reset screen grid sag
        self.screen_sag_1.reset();
        self.screen_sag_2.reset();

        // Reset power supply, ripple, topology, and transformer
        self.power_supply_sag.reset();
        self.power_supply_ripple.reset();
        self.power_supply_topology.reset();
        self.output_transformer.reset();

        // Reset speaker impedance and IR convolver
        self.speaker_impedance.reset();
        // speaker_normalizer is stateless — no reset needed
        self.ir_convolver.reset();
        self.pre_ir_buffer.fill(0.0);
        self.post_ir_buffer.fill(0.0);

        self.dc_blocker_output.reset();
    }

    fn process(
        &mut self,
        buffer: &mut Buffer,
        _aux: &mut AuxiliaryBuffers,
        _context: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        // Check for pending IR load (once per buffer)
        if self.ir_load_status.load(atomic::Ordering::Relaxed) == 0 {
            let path_opt = self.params.ir_file_path.try_lock()
                .ok()
                .map(|guard| std::path::PathBuf::from(guard.as_str()));

            if let Some(path) = path_opt {
                self.load_ir_from_file(&path);
            }
        }

        // Reset power supply topology load tracking at start of buffer
        self.power_supply_topology.reset_loads();
        let mut power_current_accumulator = 0.0_f32;
        let mut sample_count = 0_usize;


        // === V2A VARIABLE GRID LEAK (5E3 Cross-Channel Interaction) ===
        // In the 5E3, V2A has no dedicated grid leak resistor — the volume pots
        // serve as grid leak through their wiper-to-ground DC resistance.
        // Coupling caps block DC, so only the wiper-to-ground path counts.
        // DC path per channel: 68kΩ mixing R + wiper_frac × 1MΩ pot
        //
        //TODO: Volume interactions is incorrect, from testing it feels like the
        //      volume interaction is backwards, also tone control feels like it's
        //      subtracting/ adding too much gain
        //- Unused vol DOWN -> grid leak drops -> more dirt
        //- Unused vol UP   -> grid leak rises -> cleans up the channel in use
        {
            let bright_wiper_dc = self.volume_taper.wiper_fraction(self.params.bright_volume.value());
            let normal_wiper_dc = self.volume_taper.wiper_fraction(self.params.normal_volume.value());
            let bright_dc_path = 68_000.0 + bright_wiper_dc * 1_000_000.0;
            let normal_dc_path = 68_000.0 + normal_wiper_dc * 1_000_000.0;
            let effective_grid_leak = (bright_dc_path * normal_dc_path) / (bright_dc_path + normal_dc_path);
            // V1 -> volume coupling cap (0.1µF) sets the blocking distortion time constant
            self.v2a_tube.set_grid_leak(effective_grid_leak, 0.1e-6, self.sample_rate);

            // Charge fraction: at each volume pot wiper, grid current divides between
            // ground path (R_down = wiper_frac × 1MΩ) and coupling cap path
            // (R_up = (1-wiper_frac) × 1MΩ + 100kΩ plate load).
            // cap_frac = R_down / (R_down + R_up) — higher wiper -> more goes to cap
            const POT_R_DC: f32 = 1_000_000.0;
            const PLATE_LOAD_DC: f32 = 100_000.0;
            const MIXING_R_DC: f32 = 68_000.0;

            let normal_cap_frac = {
                let r_down = normal_wiper_dc * POT_R_DC;
                let r_up = (1.0 - normal_wiper_dc) * POT_R_DC + PLATE_LOAD_DC;
                if r_down < 1.0 { 0.0 } else { r_down / (r_down + r_up) }
            };
            let bright_cap_frac = {
                let r_down = bright_wiper_dc * POT_R_DC;
                let r_up = (1.0 - bright_wiper_dc) * POT_R_DC + PLATE_LOAD_DC;
                if r_down < 1.0 { 0.0 } else { r_down / (r_down + r_up) }
            };

            // Grid current splits between the two 68kΩ mixing paths
            let normal_shunt_dc = MIXING_R_DC + normal_wiper_dc * POT_R_DC;
            let bright_shunt_dc = MIXING_R_DC + bright_wiper_dc * POT_R_DC;
            let total_path_z = normal_shunt_dc + bright_shunt_dc;
            let i_frac_normal = bright_shunt_dc / total_path_z;
            let i_frac_bright = normal_shunt_dc / total_path_z;

            // Weighted charge fraction — how much of V2A's grid current charges caps
            let charge_fraction = i_frac_normal * normal_cap_frac
                                + i_frac_bright * bright_cap_frac;
            self.v2a_tube.set_charge_fraction(charge_fraction);
        }

        let num_samples = buffer.samples();
        let power_on = self.params.power.value();
        let mut sample_idx = 0usize;

        // === PASS 1: Per-sample signal chain up to output transformer ===
        for channel_samples in buffer.iter_samples() {
            for sample in channel_samples {
                if !power_on {
                    self.pre_ir_buffer[sample_idx] = 0.0;
                    sample_idx += 1;
                    continue;
                }

                let input = *sample;

                // === Get smoothed parameters ===
                // Volume knob rotation (0-1 in perceptual/mechanical space)
                let bright_vol_raw = self.params.bright_volume.smoothed.next();
                let normal_vol_raw = self.params.normal_volume.smoothed.next();
                // Convert to physical wiper fraction (0-1 in resistance space)
                // 1MΩ 15A audio taper: 50% rotation -> 15% of resistance at wiper
                // This single value drives both signal attenuation AND impedance
                let bright_wiper = self.volume_taper.wiper_fraction(bright_vol_raw);
                let normal_wiper = self.volume_taper.wiper_fraction(normal_vol_raw);
                let tone = self.params.tone.smoothed.next();
                let channel_mode = self.params.channel_select.value();

                // === INPUT CALIBRATION ===
                let mut signal = self.input_cal.process(input);
                // Apply user input trim
                let input_trim = self.params.input_trim_db.smoothed.next();
                signal *= neampmod_engine::db_to_linear(input_trim);

                // === B+ SAG + RIPPLE ===
                let sag_state = self.power_supply_sag.sag_state();
                let ripple_mod = self.power_supply_ripple.process(sag_state);

                // 5E3 Power Supply Topology:
                // - B+1 (highest voltage, least filtered) -> Power tubes
                // - B+3 (lowest voltage, most filtered) -> All preamp stages
                // Topology handles sag filtering internally (per-buffer RC lowpass).
                // Ripple is applied per-sample on top of the topology's voltage.
                let b_plus_preamp = self.power_supply_topology.b_plus_with_load("preamp") + ripple_mod;
                let b_plus_power = self.power_supply_topology.b_plus_with_load("power") + ripple_mod;

                let preamp_bias_response = self.preamp_bias.process(signal, 1.0);

                // === DUAL-CHANNEL PREAMP (V1A Normal + V1B Bright) ===
                // Route input based on channel selector:
                //   Normal: guitar -> V1A only
                //   Both (jumpered): guitar -> V1A + V1B
                //   Bright: guitar -> V1B only
                let v1a_input = if channel_mode != ChannelMode::Bright { signal } else { 0.0 };
                let v1b_input = if channel_mode != ChannelMode::Normal { signal } else { 0.0 };

                let bias = preamp_bias_response.bias_voltage;

                // V1A (Normal) — uses B+3 (preamp rail, most filtered)
                let v1a_out = if self.params.tube_toggle.value() {
                    self.v1a_tube_12ax7.process(v1a_input, bias, b_plus_preamp)
                } else {
                    self.v1a_tube_12ay7.process(v1a_input, bias, b_plus_preamp)
                };
                let v1a_coupled = self.coupling_v1.process(v1a_out);

                // V1B (Bright) — uses B+3 (preamp rail, most filtered)
                let v1b_out = if self.params.tube_toggle.value() {
                    self.v1b_tube_12ax7.process(v1b_input, bias, b_plus_preamp)
                } else {
                    self.v1b_tube_12ay7.process(v1b_input, bias, b_plus_preamp)
                };
                let v1b_coupled = self.coupling_v1b.process(v1b_out);

                // === MNA MIXING + TONE NETWORK ===
                // Unified 2nd-order MNA model of the complete 5E3 passive network:
                //   volume pots -> 500pF bright cap -> 68kΩ mixing Rs -> tone pot + 4.7nF
                //
                // Controls: [normal_wiper, bright_wiper, tone] — all physical pot fractions.
                // Two independent transfer functions are solved from the same netlist, one
                // per input channel, then summed. This should capture:
                //   • 500pF bright cap interaction with both volume pots and the tone circuit
                //   • Frequency-dependent source impedance into the tone pot (not a fixed 34kΩ)
                //   • HF shunt to ground when bright_vol = 0 (cap discharges through grounded wiper)
                //   • Cross-channel loading at all frequencies, including HF via the bright cap
                let controls = [normal_wiper as f64, bright_wiper as f64, tone as f64];
                let controls_changed = controls.iter().zip(self.mixing_tone_controls.iter())
                    .any(|(a, b)| (a - b).abs() > 0.001);
                if controls_changed {
                    self.mixing_tone_controls = controls;
                    if let Ok(coeffs) = self.mna_normal.compute_coefficients(&controls) {
                        self.filter_normal.set_analog_coefficients(&coeffs, self.sample_rate);
                    }
                    if let Ok(coeffs) = self.mna_bright.compute_coefficients(&controls) {
                        self.filter_bright.set_analog_coefficients(&coeffs, self.sample_rate);
                    }
                }
                // Channel mode: v1a_coupled / v1b_coupled are already zero for inactive channels.
                // The 100kΩ plate load shunts in the netlist always model the correct AC loading.
                signal = self.filter_normal.process(v1a_coupled)
                       + self.filter_bright.process(v1b_coupled);

                // === V2A Gain Stage (12AX7) — uses B+3 (preamp rail) ===
                signal = self.v2a_tube.process(signal, preamp_bias_response.bias_voltage, b_plus_preamp);
                signal = self.coupling_v2a.process(signal);

                // === PHASE INVERTER (Cathodyne) ===
                // V2A couples directly to PI through 0.02µF cap + 1MΩ grid leak
                // No grid stopper between V2A and PI in the real 5E3 circuit
                let pi_output = self.phase_inverter.process(signal);

                // PI coupling caps (100nF + 220kΩ grid leak per phase)
                let pi_coupled_pos = self.coupling_power_pos.process(pi_output.positive);
                let pi_coupled_neg = self.coupling_power_neg.process(pi_output.negative);

                // Phase Inverter to Power interstage attenuation (separate per 6V6)
                let positive_phase = self.pi_to_power_pos.process(pi_coupled_pos);
                let negative_phase = self.pi_to_power_neg.process(pi_coupled_neg);

                // === PUSH-PULL 6V6 POWER STAGE — uses B+1 (power rail, highest voltage) ===
                let power_bias_response = self.power_bias.process(positive_phase, 1.0);

                let tube_1_out = self.power_tube_1.process(positive_phase, power_bias_response.sag_amount, b_plus_power);
                let tube_2_out = self.power_tube_2.process(negative_phase, power_bias_response.sag_amount, b_plus_power);

                // Dynamic Miller capacitance: each power tube's gain modulates its own
                // grid stopper filter (one-sample-delay pattern, per-tube independent)
                let cgp = PowerTubeType::BeamTetrode6V6.cgp_pf();
                let cgk = PowerTubeType::BeamTetrode6V6.cgk_pf();
                self.pi_to_power_pos.update_miller_for_gain(
                    self.power_tube_1.instantaneous_gain(), cgp, cgk,
                );
                self.pi_to_power_neg.update_miller_for_gain(
                    self.power_tube_2.instantaneous_gain(), cgp, cgk,
                );

                // Accumulate power tube current for topology tracking (per buffer)
                power_current_accumulator += (tube_1_out.abs() + tube_2_out.abs()) * 0.5;
                sample_count += 1;

                let screen_state_1 = self.screen_sag_1.process(tube_1_out.abs());
                let screen_state_2 = self.screen_sag_2.process(tube_2_out.abs());
                self.power_tube_1.set_screen_sag(screen_state_1);
                self.power_tube_2.set_screen_sag(screen_state_2);

                // Push-pull differential
                let power_combined = tube_1_out - tube_2_out;

                // === SPEAKER IMPEDANCE ===
                let signal_with_impedance_eq = self.speaker_impedance.process_audio(power_combined);
                let impedance_mods = self.speaker_impedance.get_modifiers();

                // === POWER SUPPLY SAG (current-based tracking) ===
                self.power_supply_sag.update_current(signal_with_impedance_eq);
                let (_, _) = self.power_supply_sag.process_with_signal_and_impedance(
                    signal_with_impedance_eq,
                    0.5,
                    impedance_mods.sag_mod,
                );

                signal = signal_with_impedance_eq;

                // === OUTPUT TRANSFORMER ===
                signal = self.output_transformer.process(signal);

                // === NORMALIZE SPEAKER (from voltage to -/+1.0 float range) ===
                signal = self.speaker_normalizer.process(signal);

                // Store pre-IR signal for block convolution
                self.pre_ir_buffer[sample_idx] = signal;
                sample_idx += 1;
            }
        }

        // === PASS 2: Block IR convolution (zero-latency, matched to DAW buffer) ===
        // Zero-pad if buffer is smaller than block_size (rare: end of offline render)
        for i in num_samples..self.ir_block_size {
            self.pre_ir_buffer[i] = 0.0;
        }
        self.ir_convolver.process(
            &self.pre_ir_buffer[..self.ir_block_size],
            &mut self.post_ir_buffer[..self.ir_block_size],
        );

        // === PASS 3: Post-IR processing (output cal, master, DC block) ===
        {
            let output_channel = &mut buffer.as_slice()[0];
            for i in 0..num_samples {
                if !power_on {
                    output_channel[i] = 0.0;
                    continue;
                }

                let mut signal = self.post_ir_buffer[i];

                // Output calibration
                signal = self.output_cal.process(signal);
                let output_trim = self.params.output_trim_db.smoothed.next();
                signal *= neampmod_engine::db_to_linear(output_trim);

                // Master volume
                let master = self.params.master.smoothed.next();
                let master_gain = master.powf(1.5);
                signal *= master_gain;

                // DC blocking and safety limiting
                signal = self.dc_blocker_output.process(signal);

                output_channel[i] = signal;
            }
        }

        // Update power supply topology with accumulated power tube current (per-buffer)
        if sample_count > 0 {
            // Average current over buffer, scale to amperes (approximate)
            // Power tube current varies widely (idle ~20mA to peak ~100mA for 6V6)
            // We scale the normalized output level to estimate current draw
            let avg_current = (power_current_accumulator / sample_count as f32) * 0.001;
            self.power_supply_topology.update_tube_load("power", avg_current);
            let buffer_sag = self.power_supply_sag.sag_state();
            self.power_supply_topology.process_dynamics(buffer_sag, sample_count, self.sample_rate);
        }

        ProcessStatus::Normal
    }

    fn editor(&mut self, _async_executor: AsyncExecutor<Self>) -> Option<Box<dyn Editor>> {
        #[cfg(feature = "gui")]
        {
            use nih_plug_egui::{create_egui_editor, EguiState};

            let params = self.params.clone();
            let ir_status = self.ir_load_status.clone();
            let ir_path = self.params.ir_file_path.clone();

            create_egui_editor(
                EguiState::from_size(800, 450),
                gui::GuiState::new(ir_status, ir_path),
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
        ClapFeature::Stereo,
        ClapFeature::Mono,
    ];
}

impl Vst3Plugin for TheTweed {
    const VST3_CLASS_ID: [u8; 16] = *b"TheTweed........";
    const VST3_SUBCATEGORIES: &'static [Vst3SubCategory] =
        &[Vst3SubCategory::Fx, Vst3SubCategory::Distortion];
}

// Export as CLAP plugin
nih_export_clap!(TheTweed);

// Export as VST3 plugin
nih_export_vst3!(TheTweed);
