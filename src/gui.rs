use nih_plug::prelude::*;
use nih_plug_egui::egui::{self, Pos2, Rect, Vec2, ColorImage, TextureHandle};
use std::sync::{Arc, Mutex, atomic};
use std::sync::atomic::Ordering;

use crate::{
    TheTweedParams, ChannelMode, CabModellingMode, MicSelection, MicXPosition, RoomSelection,
    IrLoadState, CabProcessorLoadState, ir_load_status, load_ir_file_into_state,
    build_cab_processor,
    DEFAULT_SPEAKER_ID, DEFAULT_CABINET_ID,
    parse_os_factor, os_factor_str, os_factor_label,
};
use neampmod_engine::{
    OversamplingFactor, MicrophonePlacement, SpeakerRegistry,
    CabinetRegistry,
};

const AMP_ON_IMAGE: &[u8] = include_bytes!("../gui/amp_on.png");
const AMP_OFF_IMAGE: &[u8] = include_bytes!("../gui/amp_off.png");
const AMP_IMAGE: &[u8] = include_bytes!("../gui/amp.png");
const SWITCH_ON: &[u8] = include_bytes!("../gui/toggle_on.png");
const SWITCH_CENTER: &[u8] = include_bytes!("../gui/toggle_centered.png");
const SWITCH_OFF: &[u8] = include_bytes!("../gui/toggle_off.png");

const KNOB_FRAMES: [&[u8]; 100] = [
    include_bytes!("../gui/0000.png"),
    include_bytes!("../gui/0001.png"),
    include_bytes!("../gui/0002.png"),
    include_bytes!("../gui/0003.png"),
    include_bytes!("../gui/0004.png"),
    include_bytes!("../gui/0005.png"),
    include_bytes!("../gui/0006.png"),
    include_bytes!("../gui/0007.png"),
    include_bytes!("../gui/0008.png"),
    include_bytes!("../gui/0009.png"),
    include_bytes!("../gui/0010.png"),
    include_bytes!("../gui/0011.png"),
    include_bytes!("../gui/0012.png"),
    include_bytes!("../gui/0013.png"),
    include_bytes!("../gui/0014.png"),
    include_bytes!("../gui/0015.png"),
    include_bytes!("../gui/0016.png"),
    include_bytes!("../gui/0017.png"),
    include_bytes!("../gui/0018.png"),
    include_bytes!("../gui/0019.png"),
    include_bytes!("../gui/0020.png"),
    include_bytes!("../gui/0021.png"),
    include_bytes!("../gui/0022.png"),
    include_bytes!("../gui/0023.png"),
    include_bytes!("../gui/0024.png"),
    include_bytes!("../gui/0025.png"),
    include_bytes!("../gui/0026.png"),
    include_bytes!("../gui/0027.png"),
    include_bytes!("../gui/0028.png"),
    include_bytes!("../gui/0029.png"),
    include_bytes!("../gui/0030.png"),
    include_bytes!("../gui/0031.png"),
    include_bytes!("../gui/0032.png"),
    include_bytes!("../gui/0033.png"),
    include_bytes!("../gui/0034.png"),
    include_bytes!("../gui/0035.png"),
    include_bytes!("../gui/0036.png"),
    include_bytes!("../gui/0037.png"),
    include_bytes!("../gui/0038.png"),
    include_bytes!("../gui/0039.png"),
    include_bytes!("../gui/0040.png"),
    include_bytes!("../gui/0041.png"),
    include_bytes!("../gui/0042.png"),
    include_bytes!("../gui/0043.png"),
    include_bytes!("../gui/0044.png"),
    include_bytes!("../gui/0045.png"),
    include_bytes!("../gui/0046.png"),
    include_bytes!("../gui/0047.png"),
    include_bytes!("../gui/0048.png"),
    include_bytes!("../gui/0049.png"),
    include_bytes!("../gui/0050.png"),
    include_bytes!("../gui/0051.png"),
    include_bytes!("../gui/0052.png"),
    include_bytes!("../gui/0053.png"),
    include_bytes!("../gui/0054.png"),
    include_bytes!("../gui/0055.png"),
    include_bytes!("../gui/0056.png"),
    include_bytes!("../gui/0057.png"),
    include_bytes!("../gui/0058.png"),
    include_bytes!("../gui/0059.png"),
    include_bytes!("../gui/0060.png"),
    include_bytes!("../gui/0061.png"),
    include_bytes!("../gui/0062.png"),
    include_bytes!("../gui/0063.png"),
    include_bytes!("../gui/0064.png"),
    include_bytes!("../gui/0065.png"),
    include_bytes!("../gui/0066.png"),
    include_bytes!("../gui/0067.png"),
    include_bytes!("../gui/0068.png"),
    include_bytes!("../gui/0069.png"),
    include_bytes!("../gui/0070.png"),
    include_bytes!("../gui/0071.png"),
    include_bytes!("../gui/0072.png"),
    include_bytes!("../gui/0073.png"),
    include_bytes!("../gui/0074.png"),
    include_bytes!("../gui/0075.png"),
    include_bytes!("../gui/0076.png"),
    include_bytes!("../gui/0077.png"),
    include_bytes!("../gui/0078.png"),
    include_bytes!("../gui/0079.png"),
    include_bytes!("../gui/0080.png"),
    include_bytes!("../gui/0081.png"),
    include_bytes!("../gui/0082.png"),
    include_bytes!("../gui/0083.png"),
    include_bytes!("../gui/0084.png"),
    include_bytes!("../gui/0085.png"),
    include_bytes!("../gui/0086.png"),
    include_bytes!("../gui/0087.png"),
    include_bytes!("../gui/0088.png"),
    include_bytes!("../gui/0089.png"),
    include_bytes!("../gui/0090.png"),
    include_bytes!("../gui/0091.png"),
    include_bytes!("../gui/0092.png"),
    include_bytes!("../gui/0093.png"),
    include_bytes!("../gui/0094.png"),
    include_bytes!("../gui/0095.png"),
    include_bytes!("../gui/0096.png"),
    include_bytes!("../gui/0097.png"),
    include_bytes!("../gui/0098.png"),
    include_bytes!("../gui/0099.png"),
];

// Lazy GPU-texture cache for the GUI's static image assets.
#[derive(Default)]
pub struct TextureCache {
    amp_on: Option<TextureHandle>,
    amp_off: Option<TextureHandle>,
    amp: Option<TextureHandle>,
    switch_on: Option<TextureHandle>,
    switch_center: Option<TextureHandle>,
    switch_off: Option<TextureHandle>,
    knob_frames: Vec<Option<TextureHandle>>,
}

impl TextureCache {
    fn new() -> Self {
        Self {
            knob_frames: vec![None; KNOB_FRAMES.len()],
            ..Default::default()
        }
    }
}

// Fetch a cached texture or upload it on first use.
fn get_or_load(
    slot: &mut Option<TextureHandle>,
    ctx: &egui::Context,
    name: &str,
    bytes: &[u8],
) -> TextureHandle {
    if let Some(handle) = slot {
        return handle.clone();
    }
    let handle = load_texture_from_bytes(ctx, name, bytes);
    *slot = Some(handle.clone());
    handle
}

pub struct GuiState {
    pub ir_load_state: Arc<IrLoadState>,
    pub cab_load_state: Arc<CabProcessorLoadState>,
    pub ir_path: Arc<Mutex<String>>,
    pub meter_peak_volts: Arc<atomic_float::AtomicF32>,
    pub meter_bplus_volts: Arc<atomic_float::AtomicF32>,
    pub meter_v1_volts: Arc<atomic_float::AtomicF32>,
    pub meter_v2_volts: Arc<atomic_float::AtomicF32>,
    pub meter_v3v4_volts: Arc<atomic_float::AtomicF32>,
    pub meter_output_db: Arc<atomic_float::AtomicF32>,
    // OS factor the audio thread is actually running (vs the persisted
    // selection), shown in the Settings modal.
    pub active_os_ratio: Arc<atomic::AtomicU8>,
    pub show_amp_view: bool,
    pub show_circuit_stats: bool,
    pub show_settings: bool,
    pub textures: TextureCache,
}

impl GuiState {
    // Arg list mirrors the struct's shared-Arc fields; expanding past
    // the 7-arg lint threshold is intentional rather than a structural
    // smell.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        ir_load_state: Arc<IrLoadState>,
        cab_load_state: Arc<CabProcessorLoadState>,
        ir_path: Arc<Mutex<String>>,
        meter_peak_volts: Arc<atomic_float::AtomicF32>,
        meter_bplus_volts: Arc<atomic_float::AtomicF32>,
        meter_v1_volts: Arc<atomic_float::AtomicF32>,
        meter_v2_volts: Arc<atomic_float::AtomicF32>,
        meter_v3v4_volts: Arc<atomic_float::AtomicF32>,
        meter_output_db: Arc<atomic_float::AtomicF32>,
        active_os_ratio: Arc<atomic::AtomicU8>,
    ) -> Self {
        Self {
            ir_load_state,
            cab_load_state,
            ir_path,
            meter_peak_volts,
            meter_bplus_volts,
            meter_v1_volts,
            meter_v2_volts,
            meter_v3v4_volts,
            meter_output_db,
            active_os_ratio,
            show_amp_view: false,
            show_circuit_stats: false,
            show_settings: false,
            textures: TextureCache::new(),
        }
    }
}

// Defaults to X4 for unknown values.
fn active_os_factor(ratio_u8: u8) -> OversamplingFactor {
    match ratio_u8 {
        1 => OversamplingFactor::X1,
        2 => OversamplingFactor::X2,
        8 => OversamplingFactor::X8,
        _ => OversamplingFactor::X4,
    }
}

fn mic_x_label(pos: MicXPosition) -> &'static str {
    match pos {
        MicXPosition::Cap => "Cap",
        MicXPosition::CapEdge => "Cap Edge",
        MicXPosition::Cone => "Cone",
        MicXPosition::ConeEdge => "Cone Edge",
    }
}

fn mic_label(mic: MicSelection) -> &'static str {
    match mic {
        MicSelection::ShureSm57 => "Shure SM57",
        MicSelection::SennheiserMd421 => "Sennheiser MD 421-II",
        MicSelection::RoyerR121 => "Royer R-121 Ribbon",
        MicSelection::NeumannU87 => "Neumann U 87 Ai (Cardioid)",
        MicSelection::Rca44Bx => "RCA 44-BX",
        MicSelection::Rca77Dx => "RCA 77-DX",
    }
}

fn room_selection_label(room: RoomSelection) -> &'static str {
    match room {
        RoomSelection::None => "None",
        RoomSelection::SmallStudio => "Small Studio",
        RoomSelection::LargeStudio => "Large Studio",
        RoomSelection::LiveRoom => "Live Room",
        RoomSelection::WoodenBarn => "Wooden Barn",
        RoomSelection::SmallBedroom => "Small Bedroom",
        RoomSelection::IsoBox => "Iso Box",
    }
}

// Rebuild the cab chain and hand it to the audio thread via the shared
// pending slot. Called whenever a Dynamic-mode dropdown changes.
fn request_cab_rebuild(
    state: &GuiState,
    params: &TheTweedParams,
    microphone: MicSelection,
    room: RoomSelection,
) {
    let sample_rate = state.ir_load_state.sample_rate.load(Ordering::Relaxed);
    let block_size = state.ir_load_state.block_size.load(Ordering::Relaxed);

    // Mic/Room passed in by the caller rather than read from `params`:
    // `set_parameter()` is async under CLAP/VST3, so `params.*.value()`
    // would still return the pre-change value here.
    let placement = MicrophonePlacement {
        distance_m: params.mic_distance_inches.value() * 0.0254,
        radial_offset_cm: params.mic_x_position.value().radial_offset_cm(),
        off_axis_angle_deg: 0.0,
    };

    let processor = build_cab_processor(
        sample_rate,
        block_size,
        DEFAULT_SPEAKER_ID,
        DEFAULT_CABINET_ID,
        microphone.registry_id(),
        room,
        placement,
    );

    if let Ok(mut pending) = state.cab_load_state.pending.lock() {
        *pending = Some(processor);
    }
}

// Sized so the 400px amp image + this panel exactly fill the 520px editor
// window, with no host-window bleed between them.
const BOTTOM_PANEL_HEIGHT_PX: f32 = 120.0;
// Height of the bottom-anchored IO + window-buttons row.
const IO_ROW_RESERVED_HEIGHT_PX: f32 = 28.0;
const CAB_COMBOBOX_WIDTH_PX: f32 = 150.0;
const MIC_X_COMBOBOX_WIDTH_PX: f32 = 90.0;

pub fn create(
    egui_ctx: &egui::Context,
    setter: &ParamSetter,
    params: &Arc<TheTweedParams>,
    state: &mut GuiState,
) {
    egui::TopBottomPanel::bottom("menu_bar")
        .exact_height(BOTTOM_PANEL_HEIGHT_PX)
        .show(egui_ctx, |ui| {
            ui.vertical(|ui| {
                let cab_mode = params.cab_modelling_mode.value();

                // Row 1: cab-mode-dependent content.
                ui.horizontal(|ui| {
                    match cab_mode {
                        CabModellingMode::Ir => {
                            ui.label("IR Cabinet:");
                            if ui.button("Browse...").clicked() {
                                let ir_load_state = state.ir_load_state.clone();
                                let ir_path = state.ir_path.clone();

                                std::thread::spawn(move || {
                                    async_std::task::block_on(async move {
                                        let dialog = rfd::AsyncFileDialog::new()
                                            .add_filter("WAV Audio", &["wav"])
                                            .set_title("Select Impulse Response");

                                        if let Some(file_handle) = dialog.pick_file().await {
                                            let path = file_handle.path().to_path_buf();
                                            load_ir_file_into_state(&ir_load_state, &path);
                                            if ir_load_state.status.load(Ordering::Relaxed)
                                                == ir_load_status::LOADED
                                            {
                                                if let Ok(mut path_lock) = ir_path.lock() {
                                                    *path_lock = path.display().to_string();
                                                }
                                            }
                                        }
                                    });
                                });
                            }

                            ui.separator();

                            let status = state.ir_load_state.status.load(Ordering::Relaxed);
                            let (color, text) = match status {
                                ir_load_status::LOADING => (egui::Color32::GRAY, "Loading..."),
                                ir_load_status::LOADED => (egui::Color32::GREEN, "Loaded"),
                                ir_load_status::FAILED => (egui::Color32::RED, "Failed"),
                                _ => (egui::Color32::GRAY, "No IR Loaded"),
                            };

                            ui.colored_label(color, text);

                            if let Ok(path) = state.ir_path.lock() {
                                if !path.is_empty() {
                                    let filename = std::path::Path::new(&*path)
                                        .file_name()
                                        .and_then(|n| n.to_str())
                                        .unwrap_or("unknown");
                                    ui.label(format!("({})", filename));
                                }
                            }
                        }
                        CabModellingMode::Dynamic => {
                            let cab_name = CabinetRegistry::global()
                                .lookup(DEFAULT_CABINET_ID)
                                .map(|c| c.cabinet.name.as_str())
                                .unwrap_or(DEFAULT_CABINET_ID);
                            let spk_name = SpeakerRegistry::global()
                                .lookup(DEFAULT_SPEAKER_ID)
                                .map(|s| s.speaker.name.as_str())
                                .unwrap_or(DEFAULT_SPEAKER_ID);

                            ui.label("Cab:");
                            ui.colored_label(
                                egui::Color32::from_rgb(140, 200, 140),
                                "Dynamic",
                            );
                            ui.separator();
                            ui.label(format!("Cabinet: {}", cab_name));
                            ui.separator();
                            ui.label(format!("Speaker: {}", spk_name));
                        }
                    }
                });

                // Row 2 (Dynamic only): Mic / Mic X / Mic Distance.
                if cab_mode == CabModellingMode::Dynamic {
                    ui.horizontal(|ui| {
                        ui.label("Mic:");
                        let current_mic = params.microphone.value();
                        let mut mic_changed: Option<MicSelection> = None;
                        egui::ComboBox::from_id_salt("microphone_select")
                            .width(CAB_COMBOBOX_WIDTH_PX)
                            .selected_text(mic_label(current_mic))
                            .show_ui(ui, |ui| {
                                for variant in [
                                    MicSelection::ShureSm57,
                                    MicSelection::SennheiserMd421,
                                    MicSelection::RoyerR121,
                                    MicSelection::NeumannU87,
                                    MicSelection::Rca44Bx,
                                    MicSelection::Rca77Dx,
                                ] {
                                    if ui
                                        .selectable_label(
                                            current_mic == variant,
                                            mic_label(variant),
                                        )
                                        .clicked()
                                    {
                                        mic_changed = Some(variant);
                                    }
                                }
                            });
                        if let Some(new_mic) = mic_changed {
                            setter.set_parameter(&params.microphone, new_mic);
                            request_cab_rebuild(
                                state,
                                params,
                                new_mic,
                                params.room_selection.value(),
                            );
                        }

                        ui.separator();

                        ui.label("Mic X:");
                        let current_x = params.mic_x_position.value();
                        egui::ComboBox::from_id_salt("mic_x_position")
                            .width(MIC_X_COMBOBOX_WIDTH_PX)
                            .selected_text(mic_x_label(current_x))
                            .show_ui(ui, |ui| {
                                for variant in [
                                    MicXPosition::Cap,
                                    MicXPosition::CapEdge,
                                    MicXPosition::Cone,
                                    MicXPosition::ConeEdge,
                                ] {
                                    if ui
                                        .selectable_label(
                                            current_x == variant,
                                            mic_x_label(variant),
                                        )
                                        .clicked()
                                    {
                                        setter.set_parameter(
                                            &params.mic_x_position,
                                            variant,
                                        );
                                    }
                                }
                            });

                        ui.separator();

                        ui.label("Mic Distance:");
                        let mut dist =
                            params.mic_distance_inches.unmodulated_plain_value();
                        if ui
                            .add(
                                egui::Slider::new(&mut dist, 0.1..=24.0)
                                    .suffix(" in")
                                    .fixed_decimals(1),
                            )
                            .changed()
                        {
                            setter.set_parameter(&params.mic_distance_inches, dist);
                        }
                    });

                    // Row 3 (Dynamic only): Room.
                    ui.horizontal(|ui| {
                        ui.label("Room:");
                        let current_room = params.room_selection.value();
                        let mut room_changed: Option<RoomSelection> = None;
                        egui::ComboBox::from_id_salt("room_select")
                            .width(CAB_COMBOBOX_WIDTH_PX)
                            .selected_text(room_selection_label(current_room))
                            .show_ui(ui, |ui| {
                                for variant in [
                                    RoomSelection::None,
                                    RoomSelection::SmallStudio,
                                    RoomSelection::LargeStudio,
                                    RoomSelection::LiveRoom,
                                    RoomSelection::SmallBedroom,
                                    RoomSelection::WoodenBarn,
                                    RoomSelection::IsoBox,
                                ] {
                                    if ui
                                        .selectable_label(
                                            current_room == variant,
                                            room_selection_label(variant),
                                        )
                                        .clicked()
                                    {
                                        room_changed = Some(variant);
                                    }
                                }
                            });
                        if let Some(new_room) = room_changed {
                            setter.set_parameter(
                                &params.room_selection,
                                new_room,
                            );
                            request_cab_rebuild(
                                state,
                                params,
                                params.microphone.value(),
                                new_room,
                            );
                        }
                    });
                }

                // Push the IO row down to the panel bottom.
                let remaining = ui.available_height();
                let space = (remaining - IO_ROW_RESERVED_HEIGHT_PX).max(0.0);
                if space > 0.0 {
                    ui.add_space(space);
                }

                ui.horizontal(|ui| {
                    ui.label("Input:");
                    let mut input_trim = params.input_trim_db.unmodulated_plain_value();
                    if ui.add(
                        egui::Slider::new(&mut input_trim, -18.0..=12.0)
                            .suffix(" dB")
                            .fixed_decimals(1)
                    ).changed() {
                        setter.set_parameter(&params.input_trim_db, input_trim);
                    }
                    let peak_v = state.meter_peak_volts.load(atomic::Ordering::Relaxed);
                    let peak_mv = peak_v * 1000.0;
                    let zone_color = if peak_mv < 10.0 {
                        egui::Color32::from_rgb(100, 100, 100)  // Gray: silent
                    } else if peak_mv <= 800.0 {
                        egui::Color32::from_rgb(80, 180, 80)    // Green: typical guitar range
                    } else if peak_mv <= 1500.0 {
                        egui::Color32::from_rgb(220, 200, 40)   // Yellow: hot / active pickup
                    } else {
                        egui::Color32::from_rgb(200, 50, 50)    // Red: too hot
                    };

                    ui.label("Signal:");

                    let (rect, _) = ui.allocate_exact_size(
                        Vec2::new(8.0, 16.0),
                        egui::Sense::hover(),
                    );
                    ui.painter().rect_filled(rect, 2.0, zone_color);

                    let voltage_text = if peak_v >= 1.0 {
                        format!("{:.2} V", peak_v)
                    } else {
                        format!("{:03.0} mV", peak_mv)
                    };
                    ui.colored_label(zone_color, voltage_text);

                    ui.separator();

                    ui.label("Output:");
                    let mut output_trim = params.output_trim_db.unmodulated_plain_value();
                    if ui.add(
                        egui::Slider::new(&mut output_trim, -24.0..=0.0)
                            .suffix(" dB")
                            .fixed_decimals(1)
                    ).changed() {
                        setter.set_parameter(&params.output_trim_db, output_trim);
                    }

                    ui.separator();

                    if ui.button("View").clicked() {
                        state.show_amp_view = !state.show_amp_view;
                    }

                    if ui.button("Circuit Stats").clicked() {
                        state.show_circuit_stats = !state.show_circuit_stats;
                    }

                    if ui.button("Settings").clicked() {
                        state.show_settings = !state.show_settings;
                    }
                });
            });
        });

    egui::CentralPanel::default()
        .frame(egui::Frame::new())
        .show(egui_ctx, |ui| {
            ui.set_min_size(Vec2::new(800.0, 400.0));

            if state.show_amp_view {
                let amp_texture = get_or_load(&mut state.textures.amp, egui_ctx, "amp_view", AMP_IMAGE);
                let amp_image = egui::Image::from_texture(&amp_texture)
                    .fit_to_exact_size(Vec2::new(800.0, 400.0));
                ui.add_sized([800.0, 400.0], amp_image);
                return;
            }

            let (slot, name, bytes) = if params.power.value() {
                (&mut state.textures.amp_on, "amp_on", AMP_ON_IMAGE)
            } else {
                (&mut state.textures.amp_off, "amp_off", AMP_OFF_IMAGE)
            };
            let background_texture = get_or_load(slot, egui_ctx, name, bytes);
            let background_image = egui::Image::from_texture(&background_texture)
                .fit_to_exact_size(Vec2::new(800.0, 400.0));

            ui.add_sized([800.0, 400.0], background_image);

            ui.allocate_new_ui(
                egui::UiBuilder::new().max_rect(Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 400.0))),
                |ui| {
                    {
                        let channel_mode = params.channel_select.value();
                        draw_three_way_switch(
                            ui,
                            &mut state.textures,
                            Pos2::new(80.0, 120.0),
                            channel_mode,
                            "Channel Select",
                            "[DOWN = Normal, MID = Both (jumpered), UP = Bright]",
                            |new_mode| setter.set_parameter(&params.channel_select, new_mode),
                        );
                    }

                    {
                        let power_value = params.power.value();
                        draw_switch_with_tooltip(
                            ui,
                            &mut state.textures,
                            Pos2::new(200.0, 120.0),
                            power_value,
                            "Power Toggle",
                            "[This plugin has no pass-thru]",
                            || setter.set_parameter(&params.power, !power_value),
                        );
                    }

                    draw_image_knob_with_tooltip(
                        ui,
                        &mut state.textures,
                        Pos2::new(300.0, 120.0),
                        params.tone.value(),
                        "Tone Control",
                        "Tone Control",
                        |_ui, new_value| setter.set_parameter(&params.tone, new_value),
                    );

                    draw_image_knob_with_tooltip(
                        ui,
                        &mut state.textures,
                        Pos2::new(400.0, 120.0),
                        params.bright_volume.value(),
                        "Bright Volume",
                        "Bright Volume",
                        |_ui, new_value| setter.set_parameter(&params.bright_volume, new_value),
                    );

                    draw_image_knob_with_tooltip(
                        ui,
                        &mut state.textures,
                        Pos2::new(500.0, 120.0),
                        params.normal_volume.value(),
                        "Normal Volume",
                        "Normal Volume",
                        |_ui, new_value| setter.set_parameter(&params.normal_volume, new_value),
                    );

                    {
                        let tube_toggle_value = params.tube_toggle.value();
                        draw_switch_with_tooltip(
                            ui,
                            &mut state.textures,
                            Pos2::new(645.0, 120.0),
                            tube_toggle_value,
                            "Change V1A/V1B to 12ax7",
                            "[A 12ax7 tube has more gain (60-70mu)]",
                            || setter.set_parameter(&params.tube_toggle, !tube_toggle_value),
                        );
                    }

                    draw_image_knob_with_tooltip(
                        ui,
                        &mut state.textures,
                        Pos2::new(715.0, 120.0),
                        params.master.value(),
                        "Master Volume",
                        "Master Volume",
                        |_ui, new_value| setter.set_parameter(&params.master, new_value),
                    );

                    let build_id = env!("CARGO_PKG_VERSION");
                    ui.painter().text(
                        Pos2::new(800.0, 400.0),
                        egui::Align2::RIGHT_BOTTOM,
                        build_id,
                        egui::FontId::monospace(10.0),
                        egui::Color32::from_rgba_unmultiplied(255, 255, 255, 60),
                    );
                }
            );
        });

    if state.show_circuit_stats {
        let screen_rect = egui_ctx.screen_rect();
        let modal_size = Vec2::new(240.0, 246.0);
        let modal_rect = Rect::from_center_size(screen_rect.center(), modal_size);

        egui::Area::new(egui::Id::new("stats_overlay"))
            .fixed_pos(Pos2::ZERO)
            .order(egui::Order::Foreground)
            .show(egui_ctx, |ui| {
                let (rect, response) = ui.allocate_exact_size(screen_rect.size(), egui::Sense::click());

                ui.painter().rect_filled(
                    rect, 0.0,
                    egui::Color32::from_rgba_unmultiplied(0, 0, 0, 180),
                );

                ui.painter().rect_filled(
                    modal_rect, 8.0,
                    egui::Color32::from_rgb(30, 30, 30),
                );
                ui.painter().rect_stroke(
                    modal_rect, 8.0,
                    egui::Stroke::new(1.0, egui::Color32::from_rgb(80, 80, 80)),
                    egui::StrokeKind::Outside,
                );

                let input_mv = state.meter_peak_volts.load(atomic::Ordering::Relaxed) * 1000.0;
                let bplus_v = state.meter_bplus_volts.load(atomic::Ordering::Relaxed);
                let v1_v = state.meter_v1_volts.load(atomic::Ordering::Relaxed);
                let v2_v = state.meter_v2_volts.load(atomic::Ordering::Relaxed);
                let v3v4_v = state.meter_v3v4_volts.load(atomic::Ordering::Relaxed);
                let output_db = state.meter_output_db.load(atomic::Ordering::Relaxed);

                let text_color = egui::Color32::from_rgb(220, 220, 220);
                let label_color = egui::Color32::from_rgb(150, 150, 150);

                ui.painter().text(
                    Pos2::new(modal_rect.center().x, modal_rect.min.y + 25.0),
                    egui::Align2::CENTER_CENTER,
                    "Circuit Stats",
                    egui::FontId::proportional(16.0),
                    text_color,
                );

                let left_x = modal_rect.min.x + 30.0;
                let right_x = modal_rect.max.x - 30.0;
                let mut y = modal_rect.min.y + 55.0;
                let line_h = 26.0;

                for (label, value) in [
                    ("B+:", format!("{:.0}v", bplus_v)),
                    ("Input:", format!("{:.0}mV", input_mv)),
                    ("V1:", format!("{:.0}v", v1_v)),
                    ("V2:", format!("{:.0}v", v2_v)),
                    ("V3/4:", format!("{:.0}v", v3v4_v)),
                    ("Output:", format!("{:.0}dB", output_db)),
                ] {
                    ui.painter().text(
                        Pos2::new(left_x, y), egui::Align2::LEFT_CENTER,
                        label, egui::FontId::proportional(14.0), label_color,
                    );
                    ui.painter().text(
                        Pos2::new(right_x, y), egui::Align2::RIGHT_CENTER,
                        &value, egui::FontId::proportional(14.0), text_color,
                    );
                    y += line_h;
                }

                if response.clicked() {
                    if let Some(pos) = response.interact_pointer_pos() {
                        if !modal_rect.contains(pos) {
                            state.show_circuit_stats = false;
                        }
                    }
                }
            });
    }

    // Oversampling change applies on next plugin reload, not live.
    if state.show_settings {
        let screen_rect = egui_ctx.screen_rect();
        let modal_size = Vec2::new(320.0, 320.0);
        let modal_rect = Rect::from_center_size(screen_rect.center(), modal_size);

        let selected_os = params
            .oversampling_factor
            .lock()
            .ok()
            .map(|s| parse_os_factor(&s))
            .unwrap_or(OversamplingFactor::X4);
        let active_os =
            active_os_factor(state.active_os_ratio.load(atomic::Ordering::Relaxed));
        let selected_cab_mode = params.cab_modelling_mode.value();

        egui::Area::new(egui::Id::new("settings_overlay"))
            .fixed_pos(Pos2::ZERO)
            .order(egui::Order::Foreground)
            .show(egui_ctx, |ui| {
                let (rect, response) =
                    ui.allocate_exact_size(screen_rect.size(), egui::Sense::click());

                ui.painter().rect_filled(
                    rect,
                    0.0,
                    egui::Color32::from_rgba_unmultiplied(0, 0, 0, 180),
                );

                ui.painter().rect_filled(
                    modal_rect,
                    8.0,
                    egui::Color32::from_rgb(30, 30, 30),
                );
                ui.painter().rect_stroke(
                    modal_rect,
                    8.0,
                    egui::Stroke::new(1.0, egui::Color32::from_rgb(80, 80, 80)),
                    egui::StrokeKind::Outside,
                );

                let text_color = egui::Color32::from_rgb(220, 220, 220);
                let label_color = egui::Color32::from_rgb(150, 150, 150);
                let selected_fill = egui::Color32::from_rgb(60, 90, 120);
                let unselected_fill = egui::Color32::from_rgb(50, 50, 50);

                ui.painter().text(
                    Pos2::new(modal_rect.center().x, modal_rect.min.y + 25.0),
                    egui::Align2::CENTER_CENTER,
                    "Settings",
                    egui::FontId::proportional(16.0),
                    text_color,
                );

                ui.painter().text(
                    Pos2::new(modal_rect.min.x + 25.0, modal_rect.min.y + 60.0),
                    egui::Align2::LEFT_CENTER,
                    "Oversampling",
                    egui::FontId::proportional(14.0),
                    label_color,
                );

                let factors = [
                    OversamplingFactor::X1,
                    OversamplingFactor::X2,
                    OversamplingFactor::X4,
                    OversamplingFactor::X8,
                ];
                let button_y = modal_rect.min.y + 85.0;
                let button_w = 60.0;
                let button_h = 28.0;
                let button_spacing = 8.0;
                let total_w = button_w * 4.0 + button_spacing * 3.0;
                let start_x = modal_rect.center().x - total_w * 0.5;

                let mut clicked_factor: Option<OversamplingFactor> = None;
                for (i, factor) in factors.iter().copied().enumerate() {
                    let x = start_x + i as f32 * (button_w + button_spacing);
                    let btn_rect = Rect::from_min_size(
                        Pos2::new(x, button_y),
                        Vec2::new(button_w, button_h),
                    );

                    let btn_response = ui.allocate_rect(btn_rect, egui::Sense::click());

                    let fill = if factor == selected_os {
                        selected_fill
                    } else {
                        unselected_fill
                    };
                    ui.painter().rect_filled(btn_rect, 4.0, fill);
                    ui.painter().rect_stroke(
                        btn_rect,
                        4.0,
                        egui::Stroke::new(1.0, egui::Color32::from_rgb(100, 100, 100)),
                        egui::StrokeKind::Outside,
                    );
                    ui.painter().text(
                        btn_rect.center(),
                        egui::Align2::CENTER_CENTER,
                        os_factor_label(factor),
                        egui::FontId::proportional(13.0),
                        text_color,
                    );

                    if btn_response.clicked() {
                        clicked_factor = Some(factor);
                    }
                }

                if let Some(new_factor) = clicked_factor {
                    if let Ok(mut s) = params.oversampling_factor.lock() {
                        *s = os_factor_str(new_factor).to_string();
                    }
                }

                let status_y = modal_rect.min.y + 140.0;
                let line_h = 20.0;
                ui.painter().text(
                    Pos2::new(modal_rect.center().x, status_y),
                    egui::Align2::CENTER_CENTER,
                    format!("Current: {}", os_factor_label(active_os)),
                    egui::FontId::proportional(13.0),
                    text_color,
                );
                if selected_os != active_os {
                    ui.painter().text(
                        Pos2::new(modal_rect.center().x, status_y + line_h),
                        egui::Align2::CENTER_CENTER,
                        format!("Selected: {}", os_factor_label(selected_os)),
                        egui::FontId::proportional(13.0),
                        text_color,
                    );
                }
                ui.painter().text(
                    Pos2::new(modal_rect.center().x, modal_rect.min.y + 180.0),
                    egui::Align2::CENTER_CENTER,
                    "Will apply on next plugin reload",
                    egui::FontId::proportional(11.0),
                    label_color,
                );

                let divider_y = modal_rect.min.y + 198.0;
                ui.painter().line_segment(
                    [
                        Pos2::new(modal_rect.min.x + 25.0, divider_y),
                        Pos2::new(modal_rect.max.x - 25.0, divider_y),
                    ],
                    egui::Stroke::new(1.0, egui::Color32::from_rgb(60, 60, 60)),
                );

                ui.painter().text(
                    Pos2::new(modal_rect.min.x + 25.0, modal_rect.min.y + 220.0),
                    egui::Align2::LEFT_CENTER,
                    "Cab Modelling",
                    egui::FontId::proportional(14.0),
                    label_color,
                );

                let cab_modes = [CabModellingMode::Ir, CabModellingMode::Dynamic];
                let cab_button_y = modal_rect.min.y + 245.0;
                let cab_button_w = 90.0;
                let cab_button_spacing = 12.0;
                let cab_total_w =
                    cab_button_w * 2.0 + cab_button_spacing;
                let cab_start_x = modal_rect.center().x - cab_total_w * 0.5;

                let mut clicked_cab_mode: Option<CabModellingMode> = None;
                for (i, mode) in cab_modes.iter().copied().enumerate() {
                    let x = cab_start_x
                        + i as f32 * (cab_button_w + cab_button_spacing);
                    let btn_rect = Rect::from_min_size(
                        Pos2::new(x, cab_button_y),
                        Vec2::new(cab_button_w, button_h),
                    );
                    let btn_response =
                        ui.allocate_rect(btn_rect, egui::Sense::click());

                    let fill = if mode == selected_cab_mode {
                        selected_fill
                    } else {
                        unselected_fill
                    };
                    ui.painter().rect_filled(btn_rect, 4.0, fill);
                    ui.painter().rect_stroke(
                        btn_rect,
                        4.0,
                        egui::Stroke::new(
                            1.0,
                            egui::Color32::from_rgb(100, 100, 100),
                        ),
                        egui::StrokeKind::Outside,
                    );
                    let label = match mode {
                        CabModellingMode::Ir => "IR",
                        CabModellingMode::Dynamic => "Dynamic",
                    };
                    ui.painter().text(
                        btn_rect.center(),
                        egui::Align2::CENTER_CENTER,
                        label,
                        egui::FontId::proportional(13.0),
                        text_color,
                    );

                    if btn_response.clicked() {
                        clicked_cab_mode = Some(mode);
                    }
                }

                if let Some(new_mode) = clicked_cab_mode {
                    setter.set_parameter(&params.cab_modelling_mode, new_mode);
                }

                ui.painter().text(
                    Pos2::new(modal_rect.center().x, modal_rect.max.y - 25.0),
                    egui::Align2::CENTER_CENTER,
                    "IR: load WAV impulse · Dynamic: parametric cab chain",
                    egui::FontId::proportional(11.0),
                    label_color,
                );

                // Button clicks are consumed by allocate_rect above, so
                // this only fires for clicks outside the modal.
                if response.clicked() {
                    if let Some(pos) = response.interact_pointer_pos() {
                        if !modal_rect.contains(pos) {
                            state.show_settings = false;
                        }
                    }
                }
            });
    }
}

fn load_texture_from_bytes(ctx: &egui::Context, name: &str, bytes: &[u8]) -> TextureHandle {
    let image = image::load_from_memory(bytes).expect("Failed to load image");
    let size = [image.width() as usize, image.height() as usize];
    let image_buffer = image.to_rgba8();
    let pixels = image_buffer.as_flat_samples();
    let color_image = ColorImage::from_rgba_unmultiplied(size, pixels.as_slice());
    ctx.load_texture(name, color_image, egui::TextureOptions::default())
}

fn draw_image_knob_with_tooltip<F>(
    ui: &mut egui::Ui,
    cache: &mut TextureCache,
    center: Pos2,
    value: f32,
    name: &str,
    tooltip: &str,
    mut on_change: F,
)
where
    F: FnMut(&mut egui::Ui, f32),
{
    let frame_index = ((value * 99.0).round() as usize).min(99);
    let knob_bytes = KNOB_FRAMES[frame_index];

    let knob_texture = get_or_load(
        &mut cache.knob_frames[frame_index],
        ui.ctx(),
        &format!("knob_{:03}", frame_index + 1),
        knob_bytes,
    );

    let knob_rect = Rect::from_center_size(center, Vec2::new(90.0, 90.0));

    let response = ui.interact(knob_rect, egui::Id::new(format!("knob_{}", name)), egui::Sense::drag());

    ui.painter().image(
        knob_texture.id(),
        knob_rect,
        Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
        egui::Color32::WHITE,
    );

    if response.dragged() {
        let delta = response.drag_delta();
        let new_value = (value + delta.y * -0.01).clamp(0.0, 1.0);
        on_change(ui, new_value);
    }

    if response.hovered() {
        egui::show_tooltip_at_pointer(
            ui.ctx(),
            ui.layer_id(),
            egui::Id::new(format!("tooltip_{}", name)),
            |ui| {
                ui.label(tooltip);
            },
        );
    }
}

fn draw_switch_with_tooltip<F>(
    ui: &mut egui::Ui,
    cache: &mut TextureCache,
    center: Pos2,
    is_on: bool,
    name: &str,
    tooltip: &str,
    mut on_click: F,
)
where
    F: FnMut(),
{
    let (slot, key, bytes) = if is_on {
        (&mut cache.switch_on, "switch_on", SWITCH_ON)
    } else {
        (&mut cache.switch_off, "switch_off", SWITCH_OFF)
    };
    let switch_texture = get_or_load(slot, ui.ctx(), key, bytes);

    let switch_rect = Rect::from_center_size(center, Vec2::new(50.0, 50.0));

    let response = ui.interact(switch_rect, egui::Id::new(format!("switch_{}", name)), egui::Sense::click());

    ui.painter().image(
        switch_texture.id(),
        switch_rect,
        Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
        egui::Color32::WHITE,
    );

    if response.clicked() {
        on_click();
    }

    if response.hovered() {
        let state = if is_on { "ON" } else { "OFF" };
        egui::show_tooltip_at_pointer(
            ui.ctx(),
            ui.layer_id(),
            egui::Id::new(format!("tooltip_{}", name)),
            |ui| {
                ui.label(format!("{}: {}\n{}", name, state, tooltip));
            },
        );
    }
}

fn draw_three_way_switch<F>(
    ui: &mut egui::Ui,
    cache: &mut TextureCache,
    center: Pos2,
    current: ChannelMode,
    name: &str,
    tooltip: &str,
    mut on_change: F,
)
where
    F: FnMut(ChannelMode),
{
    let (slot, key, bytes) = match current {
        ChannelMode::Normal => (&mut cache.switch_off, "switch_off", SWITCH_OFF),
        ChannelMode::Both => (&mut cache.switch_center, "switch_center", SWITCH_CENTER),
        ChannelMode::Bright => (&mut cache.switch_on, "switch_on", SWITCH_ON),
    };
    let switch_texture = get_or_load(slot, ui.ctx(), key, bytes);
    let switch_rect = Rect::from_center_size(center, Vec2::new(50.0, 50.0));

    let response = ui.interact(switch_rect, egui::Id::new(format!("switch_{}", name)), egui::Sense::click());

    ui.painter().image(
        switch_texture.id(),
        switch_rect,
        Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
        egui::Color32::WHITE,
    );

    if response.clicked() {
        let next = match current {
            ChannelMode::Normal => ChannelMode::Both,
            ChannelMode::Both => ChannelMode::Bright,
            ChannelMode::Bright => ChannelMode::Normal,
        };
        on_change(next);
    }

    if response.hovered() {
        let state = match current {
            ChannelMode::Normal => "NORMAL",
            ChannelMode::Both => "BOTH (Jumpered)",
            ChannelMode::Bright => "BRIGHT",
        };
        egui::show_tooltip_at_pointer(
            ui.ctx(),
            ui.layer_id(),
            egui::Id::new(format!("tooltip_{}", name)),
            |ui| {
                ui.label(format!("{}: {}\n{}", name, state, tooltip));
            },
        );
    }
}



