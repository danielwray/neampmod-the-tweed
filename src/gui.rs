use nih_plug::prelude::*;
use nih_plug_egui::egui::{self, Pos2, Rect, Vec2, ColorImage, TextureHandle};
use std::sync::{Arc, Mutex, atomic};

use crate::{TheTweedParams, ChannelMode};

// Embedded image assets
const CONTROLS_IMAGE: &[u8] = include_bytes!("../gui/controls.png");
const AMP_IMAGE: &[u8] = include_bytes!("../gui/amp.png");
const SWITCH_ON: &[u8] = include_bytes!("../gui/toggle_on.png");
const SWITCH_CENTER: &[u8] = include_bytes!("../gui/toggle_centered.png");
const SWITCH_OFF: &[u8] = include_bytes!("../gui/toggle_off.png");
const LIGHT_ON: &[u8] = include_bytes!("../gui/light_on.png");
const LIGHT_OFF: &[u8] = include_bytes!("../gui/light_off.png");

// Knob animation frames (100 positions)
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

pub struct GuiState {
    pub ir_status: Arc<atomic::AtomicU8>,
    pub ir_path: Arc<Mutex<String>>,
    pub meter_peak_volts: Arc<atomic_float::AtomicF32>,

    // Circuit-stats snapshots (written per-buffer by the audio thread)
    pub meter_bplus_volts: Arc<atomic_float::AtomicF32>,
    pub meter_v1_volts: Arc<atomic_float::AtomicF32>,
    pub meter_v2_volts: Arc<atomic_float::AtomicF32>,
    pub meter_6v6_volts: Arc<atomic_float::AtomicF32>,
    pub meter_output_db: Arc<atomic_float::AtomicF32>,

    // Local GUI state (persists across frames but not sessions)
    pub show_amp_view: bool,
    pub show_circuit_stats: bool,
}

impl GuiState {
    pub fn new(
        ir_status: Arc<atomic::AtomicU8>,
        ir_path: Arc<Mutex<String>>,
        meter_peak_volts: Arc<atomic_float::AtomicF32>,
        meter_bplus_volts: Arc<atomic_float::AtomicF32>,
        meter_v1_volts: Arc<atomic_float::AtomicF32>,
        meter_v2_volts: Arc<atomic_float::AtomicF32>,
        meter_6v6_volts: Arc<atomic_float::AtomicF32>,
        meter_output_db: Arc<atomic_float::AtomicF32>,
    ) -> Self {
        Self {
            ir_status,
            ir_path,
            meter_peak_volts,
            meter_bplus_volts,
            meter_v1_volts,
            meter_v2_volts,
            meter_6v6_volts,
            meter_output_db,
            show_amp_view: false,
            show_circuit_stats: false,
        }
    }
}

pub fn create(
    egui_ctx: &egui::Context,
    setter: &ParamSetter,
    params: &Arc<TheTweedParams>,
    state: &mut GuiState,
) {
    // Bottom panel for IR loading and calibration
    egui::TopBottomPanel::bottom("menu_bar").show(egui_ctx, |ui| {
        ui.vertical(|ui| {
            // First row: IR loading
            ui.horizontal(|ui| {
                ui.label("IR Cabinet:");

                // Browse button to open file dialog
                if ui.button("Browse...").clicked() {
                    let ir_status = state.ir_status.clone();
                    let ir_path = state.ir_path.clone();

                    // Spawn file dialog using async-std runtime
                    // XDG portal uses D-Bus, so it won't conflict with host GTK
                    std::thread::spawn(move || {
                        async_std::task::block_on(async move {
                            let dialog = rfd::AsyncFileDialog::new()
                                .add_filter("WAV Audio", &["wav"])
                                .set_title("Select Impulse Response");

                            if let Some(file_handle) = dialog.pick_file().await {
                                if let Ok(mut path_lock) = ir_path.lock() {
                                    *path_lock = file_handle.path().display().to_string();
                                }
                                ir_status.store(0, atomic::Ordering::Relaxed); // Pending
                            }
                        });
                    });
                }

                ui.separator();

                // Status indicator
                let status = state.ir_status.load(atomic::Ordering::Relaxed);
                let (color, text) = match status {
                    0 => (egui::Color32::GRAY, "Loading..."),
                    1 => (egui::Color32::GREEN, "Loaded"),
                    2 => (egui::Color32::RED, "Failed"),
                    _ => (egui::Color32::GRAY, "No IR"),
                };

                ui.colored_label(color, text);

                // Display current filename
                if let Ok(path) = state.ir_path.lock() {
                    if !path.is_empty() {
                        let filename = std::path::Path::new(&*path)
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("unknown");
                        ui.label(format!("({})", filename));
                    }
                }

                ui.separator();

                // View toggle: controls panel vs. amp cabinet photo
                if ui.button("View").clicked() {
                    state.show_amp_view = !state.show_amp_view;
                }

                // Circuit stats modal
                if ui.button("Circuit Stats").clicked() {
                    state.show_circuit_stats = !state.show_circuit_stats;
                }
            });

            // Second row: Calibration controls + input level zone
            ui.horizontal(|ui| {
                // Input calibration slider
                ui.label("Input:");
                let mut input_trim = params.input_trim_db.unmodulated_plain_value();
                if ui.add(
                    egui::Slider::new(&mut input_trim, -18.0..=12.0)
                        .suffix(" dB")
                        .fixed_decimals(1)
                ).changed() {
                    setter.set_parameter(&params.input_trim_db, input_trim);
                }

                // Input level indicator — shows physical voltage at V1a grid
                // Color bands based on real-world guitar pickup output ranges
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

                // Colored zone rectangle
                let (rect, _) = ui.allocate_exact_size(
                    Vec2::new(8.0, 16.0),
                    egui::Sense::hover(),
                );
                ui.painter().rect_filled(rect, 2.0, zone_color);

                // Peak voltage — always show, default to 000 mV when silent
                let voltage_text = if peak_v >= 1.0 {
                    format!("{:.2} V", peak_v)
                } else {
                    format!("{:03.0} mV", peak_mv)
                };
                ui.colored_label(zone_color, voltage_text);

                ui.separator();

                // Output calibration slider
                ui.label("Output:");
                let mut output_trim = params.output_trim_db.unmodulated_plain_value();
                if ui.add(
                    egui::Slider::new(&mut output_trim, -24.0..=0.0)
                        .suffix(" dB")
                        .fixed_decimals(1)
                ).changed() {
                    setter.set_parameter(&params.output_trim_db, output_trim);
                }
            });
        });
    });

    egui::CentralPanel::default()
        .frame(egui::Frame::new())
        .show(egui_ctx, |ui| {
            // Set window size to match concept image proportions
            ui.set_min_size(Vec2::new(800.0, 400.0));

            if state.show_amp_view {
                // === AMP CABINET PHOTO VIEW ===
                let amp_texture = load_texture_from_bytes(egui_ctx, "amp_view", AMP_IMAGE);
                let amp_image = egui::Image::from_texture(&amp_texture)
                    .fit_to_exact_size(Vec2::new(800.0, 400.0));
                ui.add_sized([800.0, 400.0], amp_image);
                return;
            }

            // === CONTROLS VIEW (default) ===
            // Single background image regardless of power state.
            let background_texture = load_texture_from_bytes(egui_ctx, "controls", CONTROLS_IMAGE);
            let background_image = egui::Image::from_texture(&background_texture)
                .fit_to_exact_size(Vec2::new(800.0, 400.0));

            // Draw background
            ui.add_sized([800.0, 400.0], background_image);

            // Overlay controls positioned exactly like the concept image
            ui.allocate_new_ui(
                egui::UiBuilder::new().max_rect(Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 400.0))),
                |ui| {
                    // CHANNEL SELECT 3-way toggle - Normal / Both / Bright
                    {
                        let channel_mode = params.channel_select.value();
                        draw_three_way_switch(
                            ui,
                            Pos2::new(80.0, 120.0),
                            channel_mode,
                            "Channel Select",
                            "[DOWN = Normal, MID = Both (jumpered), UP = Bright]",
                            |new_mode| setter.set_parameter(&params.channel_select, new_mode),
                        );
                    }

                    // POWER switch with indicator light
                    draw_power_light(ui, Pos2::new(150.0, 120.0), params.power.value());

                    {
                        let power_value = params.power.value();
                        draw_switch_with_tooltip(
                            ui,
                            Pos2::new(200.0, 120.0),
                            power_value,
                            "Power Toggle",
                            "[This plugin has no pass-thru]",
                            || setter.set_parameter(&params.power, !power_value),
                        );
                    }

                    // TONE knob - treble/bass balance
                    draw_image_knob_with_tooltip(
                        ui,
                        Pos2::new(300.0, 120.0),
                        params.tone.value(),
                        "Tone Control",
                        "Tone Control",
                        |_ui, new_value| setter.set_parameter(&params.tone, new_value),
                    );

                    // BRIGHT VOLUME knob - Bright channel drive
                    draw_image_knob_with_tooltip(
                        ui,
                        Pos2::new(400.0, 120.0),
                        params.bright_volume.value(),
                        "Bright Volume",
                        "Bright Volume",
                        |_ui, new_value| setter.set_parameter(&params.bright_volume, new_value),
                    );

                    // NORMAL VOLUME knob - Normal channel drive
                    draw_image_knob_with_tooltip(
                        ui,
                        Pos2::new(500.0, 120.0),
                        params.normal_volume.value(),
                        "Normal Volume",
                        "Normal Volume",
                        |_ui, new_value| setter.set_parameter(&params.normal_volume, new_value),
                    );

                    // AY/AX toggle - switch preamp from 12AY7 to 12AX7
                    {
                        let tube_toggle_value = params.tube_toggle.value();
                        draw_switch_with_tooltip(
                            ui,
                            Pos2::new(645.0, 120.0),
                            tube_toggle_value,
                            "Change V1A/V1B to 12ax7",
                            "[A 12ax7 tube has more gain (60-70mu)]",
                            || setter.set_parameter(&params.tube_toggle, !tube_toggle_value),
                        );
                    }

                    // MASTER knob - Master Volume
                    draw_image_knob_with_tooltip(
                        ui,
                        Pos2::new(715.0, 120.0),
                        params.master.value(),
                        "Master Volume",
                        "Master Volume",
                        |_ui, new_value| setter.set_parameter(&params.master, new_value),
                    );

                    // Version identifier in bottom right corner
                    let build_id = env!("CARGO_PKG_VERSION");
                    ui.painter().text(
                        Pos2::new(790.0, 390.0),
                        egui::Align2::RIGHT_BOTTOM,
                        build_id,
                        egui::FontId::monospace(10.0),
                        egui::Color32::from_rgba_unmultiplied(255, 255, 255, 60),
                    );
                }
            );
        });

    // === CIRCUIT STATS MODAL ===
    if state.show_circuit_stats {
        let screen_rect = egui_ctx.screen_rect();
        let modal_size = Vec2::new(240.0, 246.0);
        let modal_rect = Rect::from_center_size(screen_rect.center(), modal_size);

        egui::Area::new(egui::Id::new("stats_overlay"))
            .fixed_pos(Pos2::ZERO)
            .order(egui::Order::Foreground)
            .show(egui_ctx, |ui| {
                let (rect, response) = ui.allocate_exact_size(screen_rect.size(), egui::Sense::click());

                // Dark overlay
                ui.painter().rect_filled(
                    rect, 0.0,
                    egui::Color32::from_rgba_unmultiplied(0, 0, 0, 180),
                );

                // Modal frame
                ui.painter().rect_filled(
                    modal_rect, 8.0,
                    egui::Color32::from_rgb(30, 30, 30),
                );
                ui.painter().rect_stroke(
                    modal_rect, 8.0,
                    egui::Stroke::new(1.0, egui::Color32::from_rgb(80, 80, 80)),
                    egui::StrokeKind::Outside,
                );

                // Read per-buffer circuit stats from audio thread
                let input_mv = state.meter_peak_volts.load(atomic::Ordering::Relaxed) * 1000.0;
                let bplus_v = state.meter_bplus_volts.load(atomic::Ordering::Relaxed);
                let v1_v = state.meter_v1_volts.load(atomic::Ordering::Relaxed);
                let v2_v = state.meter_v2_volts.load(atomic::Ordering::Relaxed);
                let v6v6_v = state.meter_6v6_volts.load(atomic::Ordering::Relaxed);
                let output_db = state.meter_output_db.load(atomic::Ordering::Relaxed);

                let text_color = egui::Color32::from_rgb(220, 220, 220);
                let label_color = egui::Color32::from_rgb(150, 150, 150);

                // Title
                ui.painter().text(
                    Pos2::new(modal_rect.center().x, modal_rect.min.y + 25.0),
                    egui::Align2::CENTER_CENTER,
                    "Circuit Stats",
                    egui::FontId::proportional(16.0),
                    text_color,
                );

                // Signal flow: B+ rail → Input → V1 → V2A → 6V6 → Output
                let left_x = modal_rect.min.x + 30.0;
                let right_x = modal_rect.max.x - 30.0;
                let mut y = modal_rect.min.y + 55.0;
                let line_h = 26.0;

                for (label, value) in [
                    ("B+:", format!("{:.0}v", bplus_v)),
                    ("Input:", format!("{:.0}mV", input_mv)),
                    ("V1:", format!("{:.0}v", v1_v)),
                    ("V2:", format!("{:.0}v", v2_v)),
                    ("V3/4:", format!("{:.0}v", v6v6_v)),
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

                // Click outside modal to close
                if response.clicked() {
                    if let Some(pos) = response.interact_pointer_pos() {
                        if !modal_rect.contains(pos) {
                            state.show_circuit_stats = false;
                        }
                    }
                }
            });
    }
}

// Helper function to load texture from PNG bytes
fn load_texture_from_bytes(ctx: &egui::Context, name: &str, bytes: &[u8]) -> TextureHandle {
    let image = image::load_from_memory(bytes).expect("Failed to load image");
    let size = [image.width() as usize, image.height() as usize];
    let image_buffer = image.to_rgba8();
    let pixels = image_buffer.as_flat_samples();
    let color_image = ColorImage::from_rgba_unmultiplied(size, pixels.as_slice());
    ctx.load_texture(name, color_image, egui::TextureOptions::default())
}

// Image-based knob widget with 100-frame animation and tooltip
fn draw_image_knob_with_tooltip<F>(
    ui: &mut egui::Ui,
    center: Pos2,
    value: f32,
    name: &str,
    tooltip: &str,
    mut on_change: F,
)
where
    F: FnMut(&mut egui::Ui, f32),
{
    // Map value (0.0-1.0) to knob frame (0-99)
    let frame_index = ((value * 99.0).round() as usize).min(99);
    let knob_bytes = KNOB_FRAMES[frame_index];

    // Create knob texture
    let knob_texture = load_texture_from_bytes(ui.ctx(), &format!("knob_{:03}", frame_index + 1), knob_bytes);

    // Position knob image at center
    let knob_rect = Rect::from_center_size(center, Vec2::new(90.0, 90.0));

    // Handle interaction FIRST to ensure it captures hover state
    let response = ui.interact(knob_rect, egui::Id::new(format!("knob_{}", name)), egui::Sense::drag());

    // Draw knob image using painter (on top of interaction layer)
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

    // Show tooltip on hover
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

// Power indicator light widget - 25% smaller than switch
fn draw_power_light(ui: &mut egui::Ui, center: Pos2, is_on: bool) {
    let light_bytes = if is_on { LIGHT_ON } else { LIGHT_OFF };

    let light_texture = load_texture_from_bytes(ui.ctx(), &format!("light_{}", if is_on { "on" } else { "off" }), light_bytes);

    let light_rect = Rect::from_center_size(center, Vec2::new(37.5, 37.5));

    // Draw light indicator image using painter
    ui.painter().image(
        light_texture.id(),
        light_rect,
        Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
        egui::Color32::WHITE,
    );
}

// Image-based switch widget with tooltip
fn draw_switch_with_tooltip<F>(
    ui: &mut egui::Ui,
    center: Pos2,
    is_on: bool,
    name: &str,
    tooltip: &str,
    mut on_click: F,
)
where
    F: FnMut(),
{
    let switch_bytes = if is_on { SWITCH_ON } else { SWITCH_OFF };

    let switch_texture = load_texture_from_bytes(ui.ctx(), &format!("switch_{}", if is_on { "on" } else { "off" }), switch_bytes);

    let switch_rect = Rect::from_center_size(center, Vec2::new(50.0, 50.0));

    // Handle interaction FIRST to ensure it captures hover state
    let response = ui.interact(switch_rect, egui::Id::new(format!("switch_{}", name)), egui::Sense::click());

    // Draw switch image using painter (on top of interaction layer)
    ui.painter().image(
        switch_texture.id(),
        switch_rect,
        Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
        egui::Color32::WHITE,
    );

    if response.clicked() {
        on_click();
    }

    // Show tooltip on hover
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

// 3-way toggle switch widget: cycles Normal -> Both -> Bright -> Normal
fn draw_three_way_switch<F>(
    ui: &mut egui::Ui,
    center: Pos2,
    current: ChannelMode,
    name: &str,
    tooltip: &str,
    mut on_change: F,
)
where
    F: FnMut(ChannelMode),
{
    let switch_bytes = match current {
        ChannelMode::Normal => SWITCH_OFF,
        ChannelMode::Both => SWITCH_CENTER,
        ChannelMode::Bright => SWITCH_ON,
    };

    let texture_key = match current {
        ChannelMode::Normal => "channel_off",
        ChannelMode::Both => "channel_center",
        ChannelMode::Bright => "channel_on",
    };

    let switch_texture = load_texture_from_bytes(ui.ctx(), texture_key, switch_bytes);
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



