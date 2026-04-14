use nih_plug::prelude::*;
use std::sync::{Arc, Mutex, atomic};

#[cfg(feature = "gui")]
mod gui;

// Import DSP from neampmod-engine
use neampmod_engine::{
    // Tube modeling (preamp only — power section handled by AmpTopology)
    TubeStage,
    TubeRegistry,
    // AmpTopology (replaces manual power section + power supply + speaker impedance)
    AmpTopology,
    AmpTopologyConfig,
    ImpedanceConfig,
    // Bias modeling (preamp only — power tube bias handled by AmpTopology)
    CathodeBias,
    CathodeBiasConfig,
    // Filters
    DCBlocker,
    NthOrderTdfii,
    // MNA mixing + tone network
    MnaSystem,
    PassiveNetworkSpec,
    // Calibration
    InputCalibration,
    OutputCalibration,
    // Input level metering
    InputLevelMeter,
    // Coupling capacitors (preamp only — PI-to-power coupling handled by AmpTopology)
    CouplingCapacitor,
    // Speaker normalizer (domain transition, plugin-side)
    SpeakerNormalizer,
    SpeakerModel,
    // IR loader and convolver
    ir_loader,
    ir_convolver,
    // Pot taper modeling
    PotTaper,
    PotTaperConfig,
    // Input jack modeling
    JackInput,
};

// Embedded IR from assets/ir/default.wav (compiled into binary)
// TODO: Replace this with a hihger quality generated IR file
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

    /// V1 tube toggle - switches between stock tube (off) and mod tube (on)
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

            // Tube toggle - switches V1 between stock and mod
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

/// Build the 5E3 AmpTopology configuration.
///
/// Uses the engine's fender_5e3() preset (cathodyne PI, 6V6 PP, SmallAmerican OT,
/// 5Y3 rectifier, JensenP12 impedance with 0.75 open-back factor, 3-tap B+ topology)
/// with current tracking enabled for authentic sag response.
fn build_5e3_amp_topology_config() -> AmpTopologyConfig {
    let mut config = AmpTopologyConfig::fender_5e3();
    // Enable current-based sag tracking for more responsive feel
    config.power_supply.sag = config.power_supply.sag.with_current_tracking(80.0);
    config.power_section.power_tube_spec = Some(POWER_TUBE_SPEC.into());
    config.power_section.pi_spec = Some(PI_SPEC.into());
    config.power_supply.sag.rectifier_spec = Some(RECTIFIER_SPEC.into());
    config.impedance = Some(ImpedanceConfig {
        speaker_model: Some(SpeakerModel::JensenP),
        cabinet_factor_override: Some(0.75),
        ..Default::default()
    });
    config
}

pub struct TheTweed {
    params: Arc<TheTweedParams>,
    sample_rate: f32,

    // === Input ===
    input_cal: InputCalibration,
    jack_input: JackInput,
    output_cal: OutputCalibration,

    // Unified MNA mixing + tone network (replaces BrightChannelFilter, manual mixing, ToneStack5E3)
    // Two transfer functions — one per input channel — solved from the same 5E3 passive netlist.
    // Controls: [normal_wiper, bright_wiper, tone] — updated per-sample with change detection.
    mna_normal: MnaSystem,      // H(s): V1A coupling cap -> V2A grid
    mna_bright: MnaSystem,      // H(s): V1B coupling cap -> V2A grid
    filter_normal: NthOrderTdfii,
    filter_bright: NthOrderTdfii,
    mixing_tone_controls: [f64; 3],  // cached for change detection

    // Preamp bias modeling (power tube bias handled by AmpTopology)
    preamp_bias: CathodeBias,

    // Preamp tube stages (Koren physics-based LUTs)
    // V1A (Normal channel) — two halves of the same tube
    v1a_tube_stock: TubeStage,
    v1a_tube_mod: TubeStage,
    // V1B (Bright channel) — other half of V1
    v1b_tube_stock: TubeStage,
    v1b_tube_mod: TubeStage,
    v2a_tube: TubeStage,

    // Passive coupling capacitors (DC blocking, preamp only)
    // V1 plate → volume/mixing network: 0.1µF into 1MΩ
    coupling_v1: CouplingCapacitor,
    coupling_v1b: CouplingCapacitor,
    // V2A plate → PI grid: passive coupling (cathodyne PI cathode at ~165V,
    // so V_gk ≈ -165V — grid conduction is physically impossible)
    coupling_v2a: CouplingCapacitor,

    // === AmpTopology: PI → power tubes → OT → speaker impedance ===
    // Replaces manual power section wiring. Handles:
    //   - Cathodyne Phase inverter
    //   - PI / 6V6 coupling caps + interstage attenuation
    //   - Push-pull 6V6 power tubes with screen sag + cathode bias
    //   - Output transformer (SmallAmerican) with core saturation + leakage
    //   - Speaker impedance EQ (JensenP12, open-back 0.75)
    //   - Power supply (5Y3 sag + ripple + 3-tap B+ topology)
    //   - All internal feedback loops (screen sag, cathode bias, sag→B+)
    amp_topology: AmpTopology,

    // === Speaker Normalizer (physical secondary volts → normalized ±1 for IR) ===
    speaker_normalizer: SpeakerNormalizer,

    // === IR Convolution (block-based, matched to DAW buffer size) ===
    ir_convolver: ir_convolver::ZeroLatencyConvolver,
    pre_ir_buffer: Vec<f32>,
    post_ir_buffer: Vec<f32>,
    ir_block_size: usize,

    // Volume pot taper (1MΩ 15A audio taper, same for both channels)
    volume_taper: PotTaperConfig,
    dc_blocker_output: DCBlocker,

    // Input level meter (measures raw DAW signal, classifies operating zone)
    input_meter: InputLevelMeter,
    cached_input_trim_db: f32,
    cached_tube_toggle: bool,

    // Shared with GUI (written once per buffer from audio thread)
    meter_peak_volts: Arc<atomic_float::AtomicF32>,

    // IR loading state (shared with GUI)
    ir_load_status: Arc<atomic::AtomicU8>,  // 0=pending, 1=success, 2=failed
}

// -- Power Supply ---
/// 5E3 preamp B+ voltage (B+3 tap after filter chain)
const PREAMP_BPLUS_5E3: f32 = 250.0;

// --- Tubes ---
/// V1 stock tube — General Electric 12ay7
const V1_STOCK_SPEC: &str = "ge_12ay7_100k";
/// V1 mod tube — RCA 12AX7A
const V1_MOD_SPEC: &str = "rca_12ax7a_100k";
/// V2A gain stage — RCA 12ax7A
const V2A_SPEC: &str = "rca_12ax7a_100k";
/// Phase inverter — General Electric 12ax7 cathodyne
const PI_SPEC: &str = "ge_12ax7_cathodyne_56k";
/// Power tubes — RCA 6V6GTA configured for 5E3
const POWER_TUBE_SPEC: &str = "rca_6v6gta_5e3";
/// Rectifier — 5Y3
const RECTIFIER_SPEC: &str = "5y3";

// --- 5E3 cathode circuit values ---
/// V1 shared cathode resistor (Ω)
const V1_CATHODE_R: f32 = 820.0;
/// V1 cathode bypass cap (µF)
const V1_CATHODE_CAP: f32 = 25.0;
/// V2A cathode resistor (Ω)
const V2A_CATHODE_R: f32 = 1500.0;
/// V2A cathode bypass cap (µF)
const V2A_CATHODE_CAP: f32 = 25.0;

/// Build a preamp TubeStage from the registry with 5E3 plate voltage.
fn build_preamp_tube(
    sample_rate: f32,
    spec_name: &str,
    cathode_resistor_ohms: f32,
    cathode_bypass_cap_uf: Option<f32>,
) -> TubeStage {
    let reg = TubeRegistry::global();
    let spec = reg.lookup(spec_name)
        .unwrap_or_else(|| panic!("Tube spec '{}' not found in registry", spec_name));
    let mut stage = TubeStage::from_spec(sample_rate, spec, cathode_resistor_ohms, cathode_bypass_cap_uf)
        .unwrap_or_else(|e| panic!("Failed to build tube from spec '{}': {}", spec_name, e));
    stage.set_plate_bplus_voltage(PREAMP_BPLUS_5E3);
    stage
}

/// Compute the meter ceiling at the amp jack for a given V1 tube stage.
/// ceiling = clean_ac_ceiling_volts / jack.dc_gain()
fn meter_ceiling_for_tube(tube: &TubeStage, jack: &JackInput) -> f32 {
    tube.voltage_cal().clean_ac_ceiling_volts() / jack.dc_gain()
}

impl Default for TheTweed {
    fn default() -> Self {
        let sample_rate = 48000.0;

        // Build input chain components first — meter needs references to these
        let input_cal = InputCalibration::amp_standard();
        let jack_input = JackInput::new(0.0, 1_000_000.0);

        // V1 tubes — build before struct so meter can read ceiling from stock tube
        let v1a_tube_stock = build_preamp_tube(sample_rate, V1_STOCK_SPEC, V1_CATHODE_R, Some(V1_CATHODE_CAP));
        let v1a_tube_mod = build_preamp_tube(sample_rate, V1_MOD_SPEC, V1_CATHODE_R, Some(V1_CATHODE_CAP));

        // Input meter — default tube_toggle=false, so use 12AY7 ceiling
        let meter_ceiling = meter_ceiling_for_tube(&v1a_tube_stock, &jack_input);
        let input_meter = InputLevelMeter::new(sample_rate, input_cal.input_scale(), meter_ceiling);

        Self {
            params: Arc::new(TheTweedParams::default()),
            sample_rate,

            input_cal,
            jack_input,
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

            // V1A (Normal channel)
            v1a_tube_stock,
            v1a_tube_mod,
            // V1B (Bright channel) — same physical tube, shared cathode
            v1b_tube_stock: build_preamp_tube(sample_rate, V1_STOCK_SPEC, V1_CATHODE_R, Some(V1_CATHODE_CAP)),
            v1b_tube_mod: build_preamp_tube(sample_rate, V1_MOD_SPEC, V1_CATHODE_R, Some(V1_CATHODE_CAP)),
            // V2A (gain stage)
            v2a_tube: build_preamp_tube(sample_rate, V2A_SPEC, V2A_CATHODE_R, Some(V2A_CATHODE_CAP)),

            // Passive coupling caps (V1 plate → volume/mixing, no grid conduction)
            coupling_v1: CouplingCapacitor::new(sample_rate, 0.1e-6, 1_000_000.0),
            coupling_v1b: CouplingCapacitor::new(sample_rate, 0.1e-6, 1_000_000.0),
            // V2A → PI: passive coupling (cathodyne PI grid never conducts — cathode at ~165V)
            coupling_v2a: CouplingCapacitor::new(sample_rate, 0.02e-6, 1_000_000.0),

            // AmpTopology: PI → power tubes → OT → speaker impedance + power supply
            // This uses the 5e3 preset in the engine (it is configurable so any topology, within
            // reason can be passed in).. Smooth. Nice.
            amp_topology: AmpTopology::new(sample_rate, build_5e3_amp_topology_config()),

            // 5E3 volume pots: 1MΩ CTS 15A audio taper (both channels identical)
            volume_taper: PotTaperConfig::new(PotTaper::Audio30A),

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

            // Input level meter
            input_meter,
            cached_input_trim_db: 0.0,
            cached_tube_toggle: false,

            // Shared meter state (written by audio thread, read by GUI)
            meter_peak_volts: Arc::new(atomic_float::AtomicF32::new(0.0)),

            // IR loading state (shared with GUI)
            ir_load_status: Arc::new(atomic::AtomicU8::new(1)),  // Start with success (embedded IR)
        }
    }
}

impl TheTweed {
    /// Load IR from file path
    /// Returns true if successful
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

        // V1A (Normal)
        self.v1a_tube_stock = build_preamp_tube(self.sample_rate, V1_STOCK_SPEC, V1_CATHODE_R, Some(V1_CATHODE_CAP));
        self.v1a_tube_mod = build_preamp_tube(self.sample_rate, V1_MOD_SPEC, V1_CATHODE_R, Some(V1_CATHODE_CAP));
        // V1B (Bright) — same physical tube, shared cathode
        self.v1b_tube_stock = build_preamp_tube(self.sample_rate, V1_STOCK_SPEC, V1_CATHODE_R, Some(V1_CATHODE_CAP));
        self.v1b_tube_mod = build_preamp_tube(self.sample_rate, V1_MOD_SPEC, V1_CATHODE_R, Some(V1_CATHODE_CAP));
        // V2A (gain stage)
        self.v2a_tube = build_preamp_tube(self.sample_rate, V2A_SPEC, V2A_CATHODE_R, Some(V2A_CATHODE_CAP));

        // Passive coupling caps (V1 plate → volume/mixing)
        self.coupling_v1 = CouplingCapacitor::new(self.sample_rate, 0.1e-6, 1_000_000.0);
        self.coupling_v1b = CouplingCapacitor::new(self.sample_rate, 0.1e-6, 1_000_000.0);
        // V2A → PI: passive coupling (cathodyne PI grid never conducts)
        self.coupling_v2a = CouplingCapacitor::new(self.sample_rate, 0.02e-6, 1_000_000.0);

        // Reinitialize AmpTopology (PI → power tubes → OT → impedance + power supply)
        self.amp_topology = AmpTopology::new(self.sample_rate, build_5e3_amp_topology_config());
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

        // Sync input trim into InputCalibration and rebuild meter
        let trim_db = self.params.input_trim_db.value();
        self.input_cal.set_user_trim_db(trim_db);
        self.cached_input_trim_db = trim_db;
        let tube_toggle = self.params.tube_toggle.value();
        self.cached_tube_toggle = tube_toggle;
        let v1_tube = if tube_toggle { &self.v1a_tube_mod } else { &self.v1a_tube_stock };
        let ceiling = meter_ceiling_for_tube(v1_tube, &self.jack_input);
        self.input_meter = InputLevelMeter::new(
            self.sample_rate,
            self.input_cal.input_scale(),
            ceiling,
        );

        true
    }

    fn reset(&mut self) {
        // Reset parameter smoothing
        self.params.bright_volume.smoothed.reset(self.params.bright_volume.value());
        self.params.normal_volume.smoothed.reset(self.params.normal_volume.value());
        self.params.tone.smoothed.reset(self.params.tone.value());
        self.params.master.smoothed.reset(self.params.master.value());

        // Reset input jack
        self.jack_input.reset();

        // Reset MNA mixing + tone filters
        self.filter_normal.reset();
        self.filter_bright.reset();
        self.mixing_tone_controls = [-1.0; 3];

        // Reset preamp bias
        self.preamp_bias.reset();

        // Reset preamp tube stages
        self.v1a_tube_stock.reset();
        self.v1a_tube_mod.reset();
        self.v1b_tube_stock.reset();
        self.v1b_tube_mod.reset();
        self.v2a_tube.reset();

        // Reset preamp coupling capacitors
        self.coupling_v1.reset();
        self.coupling_v1b.reset();
        self.coupling_v2a.reset();

        // Reset AmpTopology (PI, power tubes, OT, power supply, speaker impedance)
        self.amp_topology.reset();

        // Reset IR convolver and output
        self.ir_convolver.reset();
        self.pre_ir_buffer.fill(0.0);
        self.post_ir_buffer.fill(0.0);
        self.dc_blocker_output.reset();

        // Reset input meter
        self.input_meter.reset();
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

        // Propagate input jack series resistance to V1 grid current models.
        // Z_source is currently fixed (no guitar volume param), so set once per buffer.
        let grid_series_r = self.jack_input.source_series_resistance();
        self.v1a_tube_stock.set_grid_series_resistance(grid_series_r);
        self.v1a_tube_mod.set_grid_series_resistance(grid_series_r);
        self.v1b_tube_stock.set_grid_series_resistance(grid_series_r);
        self.v1b_tube_mod.set_grid_series_resistance(grid_series_r);

        let num_samples = buffer.samples();
        let power_on = self.params.power.value();
        let mut sample_idx = 0usize;

        // === INPUT TRIM → InputCalibration (once per buffer, change-detected) ===
        let current_trim_db = self.params.input_trim_db.value();
        if (current_trim_db - self.cached_input_trim_db).abs() > 0.01 {
            self.cached_input_trim_db = current_trim_db;
            self.input_cal.set_user_trim_db(current_trim_db);
            self.input_meter.set_input_scale(self.input_cal.input_scale());
        }

        // === TUBE TOGGLE → meter ceiling (once per buffer, change-detected) ===
        let current_tube_toggle = self.params.tube_toggle.value();
        if current_tube_toggle != self.cached_tube_toggle {
            self.cached_tube_toggle = current_tube_toggle;
            let v1_tube = if current_tube_toggle { &self.v1a_tube_mod } else { &self.v1a_tube_stock };
            self.input_meter.set_clean_ceiling_v(meter_ceiling_for_tube(v1_tube, &self.jack_input));
        }

        // === AmpTopology: begin buffer ===
        // Routes impedance feedback from previous buffer, prepares power supply interpolation
        self.amp_topology.begin_buffer(num_samples);

        // === PASS 1: Per-sample signal chain ===
        for channel_samples in buffer.iter_samples() {
            for sample in channel_samples {
                if !power_on {
                    self.pre_ir_buffer[sample_idx] = 0.0;
                    sample_idx += 1;
                    continue;
                }

                let input = *sample;

                // Advance power supply interpolation
                self.amp_topology.advance_sample();

                // === Get smoothed parameters ===
                let bright_vol_raw = self.params.bright_volume.smoothed.next();
                let normal_vol_raw = self.params.normal_volume.smoothed.next();
                let bright_wiper = self.volume_taper.wiper_fraction(bright_vol_raw);
                let normal_wiper = self.volume_taper.wiper_fraction(normal_vol_raw);
                let tone = self.params.tone.smoothed.next();
                let channel_mode = self.params.channel_select.value();

                // === INPUT LEVEL METER (raw DAW signal, before calibration) ===
                self.input_meter.process(input);

                // === INPUT CALIBRATION (includes user trim via set_user_trim_db) ===
                let mut signal = self.input_cal.process(input);

                // === INPUT JACK VOLTAGE DIVIDER ===
                signal = self.jack_input.process(signal);

                // === B+ for preamp (from AmpTopology power supply) ===
                let b_plus_preamp = self.amp_topology.b_plus_for_stage("preamp");

                let preamp_bias_response = self.preamp_bias.process(signal, 1.0);

                // === DUAL-CHANNEL PREAMP (V1A Normal + V1B Bright) ===
                let v1a_input = if channel_mode != ChannelMode::Bright { signal } else { 0.0 };
                let v1b_input = if channel_mode != ChannelMode::Normal { signal } else { 0.0 };

                let bias = preamp_bias_response.bias_voltage;

                // V1A (Normal) — uses B+3 (preamp rail, most filtered)
                let v1a_out = if self.params.tube_toggle.value() {
                    self.v1a_tube_mod.process(v1a_input, bias, b_plus_preamp)
                } else {
                    self.v1a_tube_stock.process(v1a_input, bias, b_plus_preamp)
                };
                let v1a_coupled = self.coupling_v1.process(v1a_out);

                // V1B (Bright) — uses B+3 (preamp rail, most filtered)
                let v1b_out = if self.params.tube_toggle.value() {
                    self.v1b_tube_mod.process(v1b_input, bias, b_plus_preamp)
                } else {
                    self.v1b_tube_stock.process(v1b_input, bias, b_plus_preamp)
                };
                let v1b_coupled = self.coupling_v1b.process(v1b_out);

                // === MNA MIXING + TONE NETWORK ===
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
                signal = self.filter_normal.process(v1a_coupled)
                       + self.filter_bright.process(v1b_coupled);

                // === V2A Gain Stage — uses B+3 (preamp rail) ===
                signal = self.v2a_tube.process(signal, preamp_bias_response.bias_voltage, b_plus_preamp);
                signal = self.coupling_v2a.process(signal);

                // === POWER SECTION (PI → power tubes → OT → speaker impedance) ===
                // AmpTopology handles: cathodyne PI, PI-to-6V6 coupling, push-pull 6V6,
                // screen sag, cathode bias, OT (SmallAmerican), speaker impedance EQ,
                // power supply sag driving.
                let ot_volts = self.amp_topology.process_power_section(signal);

                // === NORMALIZE SPEAKER (physical OT secondary volts → ±1 for IR) ===
                signal = self.speaker_normalizer.process(ot_volts);

                // Store pre-IR signal for block convolution
                self.pre_ir_buffer[sample_idx] = signal;
                sample_idx += 1;
            }
        }

        // === AmpTopology: end buffer ===
        // Updates tube load estimates for next buffer's power supply dynamics.
        // Power tube current is tracked internally per-sample by process_power_section().
        self.amp_topology.end_buffer(&[]);

        // === PASS 2: Block IR convolution (zero-latency, matched to DAW buffer) ===
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

        // === METER: snapshot metrics for GUI (once per buffer) ===
        let metrics = self.input_meter.get_metrics();
        self.meter_peak_volts.store(metrics.peak_volts, atomic::Ordering::Relaxed);

        ProcessStatus::Normal
    }

    fn editor(&mut self, _async_executor: AsyncExecutor<Self>) -> Option<Box<dyn Editor>> {
        #[cfg(feature = "gui")]
        {
            use nih_plug_egui::{create_egui_editor, EguiState};

            let params = self.params.clone();
            let ir_status = self.ir_load_status.clone();
            let ir_path = self.params.ir_file_path.clone();
            let meter_peak_volts = self.meter_peak_volts.clone();

            create_egui_editor(
                EguiState::from_size(800, 450),
                gui::GuiState::new(ir_status, ir_path, meter_peak_volts),
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
