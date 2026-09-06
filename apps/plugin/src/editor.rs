//! The plugin window: header, a four-row form on one grid, footer status.
//!
//! Every element sits on two vertical lines: labels and the footer start at
//! `PAD`, every control starts at `CTRL_X`, every readout ends at `RIGHT_X`.
//! The whole window is painted into fixed rectangles; no egui panels.

use std::time::{Duration, Instant};

#[cfg(test)]
use egui::Mesh;
use egui::epaint::text::{LayoutJob, TextFormat};
use egui::{
    Align, Align2, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Frame, Id,
    Margin, Pos2, Rect, Sense, Stroke, StrokeKind, TextEdit, Ui, Visuals, pos2, vec2,
};
use relay_session::{PUBLIC_LINK_ORIGIN, normalize_slug};
use truce::prelude::*;
use truce_egui::EditorUi;

use crate::meter::{PeakHold, db_to_pos, peak_to_db};
use crate::status::{Facts, Health, describe};
use crate::{Codec, Mode, Monitor, P, RelayParams, clipboard, slug};

/// Fixed editor size in logical pixels.
pub const WINDOW: (u32, u32) = (680, 480);

// RELAY white and electric-blue palette.
const PAPER: Color32 = Color32::from_rgb(0xf4, 0xf6, 0xf8);
const SURFACE: Color32 = Color32::from_rgb(0xe7, 0xee, 0xf7);
const WELL: Color32 = Color32::WHITE;
const HAIRLINE: Color32 = Color32::from_rgb(0xd9, 0xe3, 0xed);
const TEXT: Color32 = Color32::from_rgb(0x14, 0x2b, 0x3b);
const MUTED: Color32 = Color32::from_rgb(0x52, 0x6a, 0x7b);
const DIM: Color32 = Color32::from_rgb(0x60, 0x75, 0x85);
const STUDIO_BLUE: Color32 = Color32::from_rgb(0x08, 0x66, 0xe8);
const OK: Color32 = Color32::from_rgb(0x15, 0x83, 0x6e);
const WARN: Color32 = Color32::from_rgb(0x97, 0x60, 0x00);
const HOT: Color32 = Color32::from_rgb(0xc2, 0x38, 0x52);
const GYR_FLOOR: Color32 = Color32::from_rgb(0x3d, 0x8f, 0x6a);

// Grid.
const PAD: f32 = 24.0;
const HEADER_H: f32 = 74.0;
const CTRL_X: f32 = 112.0;
const RIGHT_X: f32 = WINDOW.0 as f32 - PAD;
const READOUT_W: f32 = 88.0;
const ROW_H: f32 = 42.0;
const ROW_GAP: f32 = 12.0;
const FORM_TOP: f32 = 88.0;
const FORM_RIGHT: f32 = 532.0;
#[cfg(test)]
const METER_H: f32 = 11.0;
const FOOTER_Y: f32 = WINDOW.1 as f32 - 26.0;

const R_HARDWARE: CornerRadius = CornerRadius::same(21);
const R_WELL: CornerRadius = CornerRadius::same(18);
const R_METER: CornerRadius = CornerRadius::same(2);
const COPIED_FOR: Duration = Duration::from_millis(1200);
const FRAME: Duration = Duration::from_millis(33);
const SEGMENT_ANIM_SECS: f32 = 0.12;

/// 0 dB on the -24..+12 dB gain range, normalized.
const GAIN_DEFAULT: f32 = 24.0 / 36.0;

fn font(size: f32) -> FontId {
    FontId::proportional(size)
}

/// Installs Barlow `SemiBold` as the only font. Called once per context.
pub fn setup_context(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert(
        "barlow".into(),
        std::sync::Arc::new(FontData::from_static(include_bytes!(
            "../assets/fonts/Barlow-SemiBold.ttf"
        ))),
    );
    for family in [FontFamily::Proportional, FontFamily::Monospace] {
        fonts
            .families
            .entry(family)
            .or_default()
            .insert(0, "barlow".into());
    }
    ctx.set_fonts(fonts);
}

/// Light controls with a single blue interaction accent.
pub fn visuals() -> Visuals {
    if std::fs::read_to_string(theme_path()).is_ok_and(|v| v == "dark") {
        dark_visuals()
    } else {
        light_visuals()
    }
}

fn dark_visuals() -> Visuals {
    let mut v = Visuals::dark();
    v.override_text_color = Some(Color32::from_rgb(238, 238, 238));
    v.weak_text_color = Some(Color32::from_rgb(184, 184, 184));
    v.window_fill = Color32::from_rgb(32, 32, 32);
    v.extreme_bg_color = v.window_fill;
    v.selection.bg_fill = STUDIO_BLUE;
    for w in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        w.bg_fill = Color32::from_rgb(48, 48, 48);
        w.weak_bg_fill = w.bg_fill;
        w.bg_stroke = Stroke::new(1.0, Color32::from_rgb(72, 72, 72));
        w.fg_stroke = Stroke::new(1.0, Color32::from_rgb(238, 238, 238));
        w.corner_radius = CornerRadius::same(10);
    }
    v
}

fn light_visuals() -> Visuals {
    let mut v = Visuals::light();
    v.panel_fill = PAPER;
    v.window_fill = PAPER;
    v.extreme_bg_color = WELL;
    v.override_text_color = Some(TEXT);
    v.selection.bg_fill = STUDIO_BLUE.gamma_multiply(0.35);
    v.selection.stroke = Stroke::new(1.0, STUDIO_BLUE);
    v.text_cursor.stroke = Stroke::new(1.5, STUDIO_BLUE);
    for w in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        w.bg_fill = SURFACE;
        w.weak_bg_fill = SURFACE;
        w.bg_stroke = Stroke::new(1.0, HAIRLINE);
        w.fg_stroke = Stroke::new(1.0, TEXT);
    }
    v
}

fn theme_path() -> std::path::PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
        })
        .join("matari/relay-theme")
}

fn tone(ui: &Ui, color: Color32) -> Color32 {
    if !ui.visuals().dark_mode {
        return color;
    }
    match color {
        PAPER => Color32::from_rgb(21, 21, 21),
        WELL => Color32::from_rgb(32, 32, 32),
        SURFACE => Color32::from_rgb(48, 48, 48),
        HAIRLINE => Color32::from_rgb(72, 72, 72),
        TEXT => Color32::from_rgb(238, 238, 238),
        MUTED | DIM => Color32::from_rgb(184, 184, 184),
        _ => color,
    }
}

fn relay_mark(ui: &Ui, origin: Pos2, size: f32) {
    let point = |x: f32, y: f32| origin + vec2(x, y) * size / 100.0;
    let color = if ui.visuals().dark_mode { tone(ui, TEXT) } else { STUDIO_BLUE };
    let stroke = Stroke::new(size * 0.14, color);
    ui.painter().line_segment([point(20.0, 82.0), point(20.0, 20.0)], stroke);
    ui.painter().line_segment([point(20.0, 20.0), point(56.0, 20.0)], stroke);
    for points in [
        [point(56.0, 20.0), point(71.0, 20.0), point(80.0, 28.0), point(80.0, 40.0)],
        [point(80.0, 40.0), point(80.0, 52.0), point(71.0, 60.0), point(56.0, 60.0)],
    ] {
        ui.painter().add(egui::epaint::CubicBezierShape::from_points_stroke(points, false, Color32::TRANSPARENT, stroke));
    }
    ui.painter().line_segment([point(56.0, 60.0), point(42.0, 60.0)], stroke);
    ui.painter().line_segment([point(52.0, 69.0), point(70.0, 82.0)], stroke);
}

/// Text the user is typing. Committed to the session store on Enter or blur,
/// so half-typed names never reach the network.
struct Fields {
    name: String,
    peer: String,
    password: String,
}

pub struct RelayUi {
    dark: bool,
    fields: Option<Fields>,
    copied_at: Option<Instant>,
    hold: [PeakHold; 2],
    spectrum: crate::spectrum::Spectrum,
    tap: Option<std::sync::Arc<crate::spectrum::SpectrumTap>>,
}

impl Default for RelayUi {
    fn default() -> Self {
        Self {
            dark: std::fs::read_to_string(theme_path()).is_ok_and(|v| v == "dark"),
            fields: None,
            copied_at: None,
            hold: Default::default(),
            spectrum: Default::default(),
            tap: None,
        }
    }
}

impl EditorUi<RelayParams> for RelayUi {
    fn opened(&mut self, ctx: &PluginContext<RelayParams>) {
        self.dark = std::fs::read_to_string(theme_path()).is_ok_and(|v| v == "dark");
        self.fields = None;
        ctx.params().spectrum.audio.clear();
        ctx.params()
            .spectrum
            .active
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self.tap = Some(ctx.params().spectrum.clone());
    }

    fn closed(&mut self) {
        if let Some(tap) = self.tap.take() {
            tap.active
                .store(false, std::sync::atomic::Ordering::Relaxed);
        }
    }

    fn state_changed(&mut self, _ctx: &PluginContext<RelayParams>) {
        self.fields = None;
    }

    fn ui(&mut self, ui: &mut Ui, ctx: &PluginContext<RelayParams>) {
        let params = ctx.params();
        if ui.visuals().dark_mode != self.dark {
            ui.ctx().set_visuals(if self.dark {
                dark_visuals()
            } else {
                light_visuals()
            });
        }
        let screen = ui.ctx().content_rect();
        ui.painter()
            .rect_filled(screen, CornerRadius::ZERO, tone(ui, PAPER));

        let mode = params.mode.value();
        let share = mode.is_share();
        header(ui, ctx, mode, &mut self.dark);

        let fields = self.fields.get_or_insert_with(|| {
            let saved = params.session.read();
            Fields {
                name: saved.name,
                peer: saved.peer,
                password: saved.password,
            }
        });
        let peaks = [ctx.get_meter(P::MeterLeft), ctx.get_meter(P::MeterRight)];
        let held = [self.hold[0].update(peaks[0]), self.hold[1].update(peaks[1])];
        self.spectrum.update(&params.spectrum);
        let scope = Rect::from_min_max(pos2(PAD, 326.0), pos2(RIGHT_X, 434.0));
        ui.painter()
            .rect_filled(scope, CornerRadius::same(20), if ui.visuals().dark_mode { tone(ui, WELL) } else { STUDIO_BLUE });
        ui.painter().text(
            pos2(scope.min.x + 16.0, scope.min.y + 12.0),
            Align2::LEFT_TOP,
            "Send spectrum",
            font(12.0),
            Color32::WHITE,
        );
        let upper = (params
            .spectrum
            .rate
            .load(std::sync::atomic::Ordering::Relaxed) as f32
            * 0.45)
            .min(18000.0);
        ui.painter().text(
            pos2(scope.max.x - 16.0, scope.min.y + 12.0),
            Align2::RIGHT_TOP,
            format!("50 Hz — {:.1} kHz", upper / 1000.0),
            font(11.0),
            Color32::WHITE,
        );
        for (index, level) in self.spectrum.bands.iter().enumerate() {
            let height = (*level * 62.0).max(2.0);
            let x = scope.min.x + 16.0 + index as f32 * (scope.width() - 32.0) / 40.0;
            let bar = Rect::from_min_max(
                pos2(x, scope.max.y - 14.0 - height),
                pos2(x + 7.0, scope.max.y - 14.0),
            );
            ui.painter()
                .rect_filled(bar, CornerRadius::same(3), if ui.visuals().dark_mode { STUDIO_BLUE } else { Color32::WHITE });
        }
        vertical_meters(ui, peaks, held);
        let mut rows = Rows { next_y: FORM_TOP };
        if share {
            share_form(ui, ctx, &mut rows, fields, &mut self.copied_at);
        } else {
            join_form(ui, ctx, &mut rows, fields);
        }

        let facts = Facts::read(&params.control, share);
        footer(ui, ctx, &facts);

        ui.ctx().request_repaint_after(FRAME);
    }
}

fn header(ui: &mut Ui, ctx: &PluginContext<RelayParams>, mode: Mode, dark: &mut bool) {
    ui.painter().rect_filled(
        Rect::from_min_max(pos2(0.0, 0.0), pos2(WINDOW.0 as f32, HEADER_H)),
        CornerRadius::ZERO,
        tone(ui, WELL),
    );
    relay_mark(ui, pos2(PAD, 21.0), 32.0);
    let theme = Rect::from_min_size(pos2(244.0, 19.0), vec2(36.0, 36.0));
    let response = ui.interact(theme, Id::new("theme"), Sense::click());
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Switch color theme")
    });
    ui.painter()
        .rect_filled(theme, CornerRadius::same(12), tone(ui, SURFACE));
    ui.painter()
        .circle_stroke(theme.center(), 7.0, Stroke::new(1.5, tone(ui, TEXT)));
    ui.painter().add(egui::Shape::convex_polygon(
        (0..=16)
            .map(|i| {
                let a = std::f32::consts::PI * (i as f32 / 16.0 - 0.5);
                theme.center() + vec2(a.cos(), a.sin()) * 7.0
            })
            .collect(),
        tone(ui, TEXT),
        Stroke::NONE,
    ));
    if response.on_hover_text("Switch color theme").clicked() {
        *dark = !*dark;
        let path = theme_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(path, if *dark { "dark" } else { "light" });
    }
    let mid_y = HEADER_H / 2.0;
    tracked_text(
        ui,
        pos2(PAD + 42.0, mid_y),
        Align2::LEFT_CENTER,
        "RELAY",
        28.0,
        -0.02,
        if *dark {
            Color32::from_rgb(238, 238, 238)
        } else {
            STUDIO_BLUE
        },
    );
    let seg = Rect::from_min_max(pos2(300.0, mid_y - 18.0), pos2(490.0, mid_y + 18.0));
    if let Some(index) = segmented(
        ui,
        seg,
        Id::new("mode"),
        &["Share", "Join"],
        mode.to_index(),
    ) {
        ctx.automate(P::Mode, normalized_index(index, Mode::variant_count()));
    }
    ui.painter().hline(
        0.0..=WINDOW.0 as f32,
        HEADER_H - 0.5,
        Stroke::new(1.0, tone(ui, HAIRLINE)),
    );
}

/// Session status with an explicit live control.
fn footer(ui: &mut Ui, ctx: &PluginContext<RelayParams>, facts: &Facts) {
    let status = describe(facts);
    let lamp = match status.health {
        Health::Off => tone(ui, DIM),
        Health::Failed => HOT,
        Health::Asleep => WARN,
        Health::Ready | Health::Pending => GYR_FLOOR,
        Health::Live => OK,
    };
    let button = Rect::from_min_max(pos2(548.0, 19.0), pos2(RIGHT_X, 55.0));
    let response = ui.interact(button, Id::new("live"), Sense::click());
    let label = if status.health == Health::Asleep {
        "Wake"
    } else if facts.live {
        "Disconnect"
    } else {
        "Go live"
    };
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), label)
    });
    ui.painter().rect_filled(
        button,
        R_HARDWARE,
        if response.hovered() {
            Color32::from_rgb(0, 78, 188)
        } else if facts.live {
            tone(ui, SURFACE)
        } else {
            STUDIO_BLUE
        },
    );
    ui.painter().text(
        button.center(),
        Align2::CENTER_CENTER,
        label,
        font(14.0),
        if facts.live && !response.hovered() {
            tone(ui, TEXT)
        } else {
            Color32::WHITE
        },
    );
    let color = tone(ui, MUTED);
    ui.painter()
        .circle_filled(pos2(PAD + 4.0, FOOTER_Y), 4.0, lamp);
    ui.painter().text(
        pos2(PAD + 16.0, FOOTER_Y),
        Align2::LEFT_CENTER,
        &status.line,
        font(12.0),
        color,
    );
    if response.clicked() {
        if status.health == Health::Asleep {
            ctx.params().control.request_web_wake();
        } else {
            ctx.automate(P::Live, if facts.live { 0.0 } else { 1.0 });
        }
    }
}

/// Hands out one labelled row at a time, top to bottom.
struct Rows {
    next_y: f32,
}

impl Rows {
    /// Paints the label and returns the full control rectangle.
    fn row(&mut self, ui: &Ui, label: &str) -> Rect {
        let y = self.next_y;
        self.next_y += ROW_H + ROW_GAP;
        tracked_text(
            ui,
            pos2(PAD, y + ROW_H / 2.0),
            Align2::LEFT_CENTER,
            label,
            14.0,
            0.0,
            tone(ui, MUTED),
        );
        Rect::from_min_max(pos2(CTRL_X, y), pos2(FORM_RIGHT, y + ROW_H))
    }
}

fn share_form(
    ui: &mut Ui,
    ctx: &PluginContext<RelayParams>,
    rows: &mut Rows,
    fields: &mut Fields,
    copied_at: &mut Option<Instant>,
) {
    let params = ctx.params();

    let row = rows.row(ui, "Session");
    let copied = copied_at.is_some_and(|t| t.elapsed() < COPIED_FOR);
    let labels = [if copied { "Copied" } else { "Copy" }, "Open"];
    let widths = [40.0, 40.0];
    let actions_w: f32 = widths.iter().sum();
    let field = row.with_max_x(row.max.x - actions_w - 4.0);
    if text_field(
        ui,
        field,
        Id::new("name"),
        &mut fields.name,
        "room name",
        false,
    ) {
        let clean = normalize_slug(&fields.name);
        let name = if clean.is_empty() {
            slug::new_slug()
        } else {
            clean
        };
        fields.name.clone_from(&name);
        params.session.update(|s| s.name = name);
    }
    let mut x = field.max.x + 4.0;
    for (i, (label, w)) in labels.iter().zip(widths).enumerate() {
        let rect = Rect::from_min_max(pos2(x, row.min.y), pos2(x + w, row.max.y));
        x += w;
        if !icon_button(ui, rect, Id::new(("name-action", i)), label) {
            continue;
        }
        let link = format!("{PUBLIC_LINK_ORIGIN}/{}", fields.name);
        if i == 0 {
            if clipboard::copy(&link) {
                *copied_at = Some(Instant::now());
            }
        } else {
            // Best effort: no browser is not an error the plugin can act on.
            let _ = open::that_detached(link);
        }
    }

    let row = rows.row(ui, "Password");
    if text_field(
        ui,
        row,
        Id::new("password"),
        &mut fields.password,
        "optional",
        true,
    ) {
        let password = fields.password.clone();
        params.session.update(|s| s.password = password);
    }

    let row = rows.row(ui, "Format");
    let codec = params.codec.value();
    let seg = row.with_max_x(row.max.x - 118.0);
    if let Some(index) = segmented(
        ui,
        seg,
        Id::new("codec"),
        &["Opus", "FLAC", "PCM"],
        codec.to_index(),
    ) {
        ctx.automate(P::Codec, normalized_index(index, Codec::variant_count()));
    }
    let chip = row.with_min_x(seg.max.x + 8.0);
    let (label, hint) = match codec {
        Codec::Opus => (
            format!("{} kbps", params.bitrate.value()),
            "Bitrate: higher values preserve more detail and use more bandwidth.",
        ),
        Codec::Flac => (
            format!("Level {}", params.flac_level.value()),
            "Lossless compression: higher levels use more CPU, not higher audio quality.",
        ),
        Codec::Pcm => (
            "24-bit".into(),
            "Uncompressed 24-bit audio. No quality setting needed.",
        ),
    };
    ui.scope_builder(
        egui::UiBuilder::new().max_rect(chip.shrink2(vec2(0.0, 7.0))),
        |ui| {
            if codec == Codec::Pcm {
                ui.label("24-bit PCM").on_hover_text(hint);
                return;
            }
            ui.spacing_mut().interact_size.y = 28.0;
            for widget in [&mut ui.visuals_mut().widgets.inactive] {
                widget.corner_radius = CornerRadius::same(10);
            }
            egui::ComboBox::from_id_salt("quality")
                .selected_text(label)
                .width(chip.width() - 8.0)
                .height(220.0)
                .show_ui(ui, |ui| match codec {
                    Codec::Opus => {
                        ui.label("Bitrate / kbps");
                        for value in [64, 96, 128, 160, 192, 256] {
                            if ui
                                .selectable_label(
                                    params.bitrate.value() == value,
                                    format!("{value} kbps"),
                                )
                                .clicked()
                            {
                                ctx.automate(P::Bitrate, (value - 64) as f64 / 192.0);
                            }
                        }
                    }
                    Codec::Flac => {
                        ui.label("Lossless compression");
                        for value in 0..=8 {
                            if ui
                                .selectable_label(
                                    params.flac_level.value() == value,
                                    format!("Level {value}"),
                                )
                                .clicked()
                            {
                                ctx.automate(P::FlacLevel, value as f64 / 8.0);
                            }
                        }
                    }
                    Codec::Pcm => {
                        ui.label("24-bit · uncompressed");
                    }
                })
                .response
                .on_hover_text(hint);
        },
    );

    let row = rows.row(ui, "Send");
    fader(ui, ctx, row, P::Send);
}

fn join_form(ui: &mut Ui, ctx: &PluginContext<RelayParams>, rows: &mut Rows, fields: &mut Fields) {
    let params = ctx.params();

    let row = rows.row(ui, "Session");
    if text_field(
        ui,
        row,
        Id::new("peer"),
        &mut fields.peer,
        "host:port",
        false,
    ) {
        let peer = fields.peer.trim().to_owned();
        fields.peer.clone_from(&peer);
        params.session.update(|s| s.peer = peer);
    }

    let row = rows.row(ui, "Monitor");
    let seg = row.with_max_x(row.max.x - READOUT_W);
    if let Some(index) = segmented(
        ui,
        seg,
        Id::new("monitor"),
        &["Dry", "Mix", "Remote"],
        params.monitor.value().to_index(),
    ) {
        ctx.automate(
            P::Monitor,
            normalized_index(index, Monitor::variant_count()),
        );
    }

    let row = rows.row(ui, "Send");
    fader(ui, ctx, row, P::Send);

    let row = rows.row(ui, "Receive");
    fader(ui, ctx, row, P::Hear);
}

/// Enum index → normalized parameter value.
fn normalized_index(index: usize, count: usize) -> f64 {
    let last = count.saturating_sub(1).max(1);
    index.min(last) as f64 / last as f64
}

// ---------------------------------------------------------------- widgets

/// Segmented switch. Returns the newly clicked index, if any.
fn segmented(ui: &mut Ui, rect: Rect, id: Id, labels: &[&str], selected: usize) -> Option<usize> {
    ui.painter()
        .rect_filled(rect, R_HARDWARE, tone(ui, SURFACE));

    let count = labels.len().max(1) as f32;
    let cell_w = (rect.width() - 4.0) / count;
    let target = rect.min.x + 2.0 + cell_w * selected as f32;
    let x = ui
        .ctx()
        .animate_value_with_time(id.with("indicator"), target, SEGMENT_ANIM_SECS);
    let indicator =
        Rect::from_min_size(pos2(x, rect.min.y + 2.0), vec2(cell_w, rect.height() - 4.0));
    ui.painter().rect_filled(indicator, R_WELL, STUDIO_BLUE);

    let mut clicked = None;
    for (i, label) in labels.iter().enumerate() {
        let cell = Rect::from_min_size(
            pos2(rect.min.x + 2.0 + cell_w * i as f32, rect.min.y),
            vec2(cell_w, rect.height()),
        );
        let response = ui.interact(cell, id.with(i), Sense::click());
        if response.clicked() {
            clicked = Some(i);
        }
        let color = if i == selected {
            Color32::WHITE
        } else if response.hovered() {
            tone(ui, MUTED)
        } else {
            tone(ui, DIM)
        };
        response.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), *label)
        });
        let is_mode = labels == ["Share", "Join"];
        if is_mode {
            let center = pos2(cell.min.x + 18.0, cell.center().y);
            let direction = if i == 0 { -1.0 } else { 1.0 };
            let tip = center + vec2(0.0, 6.0 * direction);
            let stroke = Stroke::new(1.6, color);
            ui.painter()
                .line_segment([center + vec2(0.0, -6.0 * direction), tip], stroke);
            ui.painter()
                .line_segment([tip + vec2(-4.0, -4.0 * direction), tip], stroke);
            ui.painter()
                .line_segment([tip + vec2(4.0, -4.0 * direction), tip], stroke);
        }
        ui.painter().text(
            cell.center() + vec2(if is_mode { 8.0 } else { 0.0 }, 0.0),
            Align2::CENTER_CENTER,
            label,
            font(14.0),
            color,
        );
    }
    clicked
}

/// Single-line input in a well. Returns `true` when the value was committed.
fn text_field(
    ui: &mut Ui,
    rect: Rect,
    id: Id,
    text: &mut String,
    hint: &str,
    secret: bool,
) -> bool {
    ui.painter().rect_filled(rect, R_WELL, tone(ui, WELL));
    let edit = TextEdit::singleline(text)
        .id(id)
        .hint_text(hint)
        .password(secret)
        .font(font(15.0))
        .text_color(tone(ui, TEXT))
        .frame(Frame::NONE.inner_margin(Margin::symmetric(10, 0)))
        .vertical_align(Align::Center)
        .desired_width(rect.width());
    let response = ui.put(rect, edit);
    if response.has_focus() {
        ui.painter().rect_stroke(
            rect,
            R_WELL,
            Stroke::new(1.0, STUDIO_BLUE),
            StrokeKind::Inside,
        );
    }
    response.lost_focus()
}

fn icon_button(ui: &mut Ui, rect: Rect, id: Id, label: &str) -> bool {
    let response = ui.interact(rect, id, Sense::click());
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), label)
    });
    let color = if response.hovered() {
        STUDIO_BLUE
    } else {
        tone(ui, MUTED)
    };
    ui.painter().rect_filled(
        rect.shrink2(vec2(2.0, 3.0)),
        CornerRadius::same(12),
        tone(ui, SURFACE),
    );
    let center = rect.center();
    let stroke = Stroke::new(1.7, color);
    let box_rect = Rect::from_center_size(center, vec2(13.0, 13.0));
    if label == "Copied" {
        ui.painter()
            .line_segment([center + vec2(-6.0, 0.0), center + vec2(-1.0, 5.0)], stroke);
        ui.painter()
            .line_segment([center + vec2(-1.0, 5.0), center + vec2(7.0, -5.0)], stroke);
    } else if label == "Open" {
        ui.painter().rect_stroke(
            box_rect.translate(vec2(-2.0, 2.0)),
            CornerRadius::same(2),
            stroke,
            StrokeKind::Inside,
        );
        ui.painter()
            .line_segment([center, center + vec2(9.0, -9.0)], stroke);
        ui.painter()
            .line_segment([center + vec2(2.0, -9.0), center + vec2(9.0, -9.0)], stroke);
        ui.painter()
            .line_segment([center + vec2(9.0, -9.0), center + vec2(9.0, -2.0)], stroke);
    } else {
        ui.painter().rect_stroke(
            box_rect.translate(vec2(-3.0, -3.0)),
            CornerRadius::same(3),
            stroke,
            StrokeKind::Inside,
        );
        ui.painter().rect_filled(
            box_rect.translate(vec2(3.0, 3.0)),
            CornerRadius::same(3),
            tone(ui, PAPER),
        );
        ui.painter().rect_stroke(
            box_rect.translate(vec2(3.0, 3.0)),
            CornerRadius::same(3),
            stroke,
            StrokeKind::Inside,
        );
    }
    if response.has_focus() {
        ui.painter().rect_stroke(
            rect.shrink(1.0),
            CornerRadius::same(12),
            Stroke::new(1.5, STUDIO_BLUE),
            StrokeKind::Inside,
        );
    }
    let clicked = response.clicked();
    response.on_hover_text(if label == "Open" {
        "Open listener in browser"
    } else if label == "Copied" {
        "Link copied"
    } else {
        "Copy listener link"
    });
    clicked
}

fn readout(ui: &Ui, rect: Rect, text: &str) {
    ui.painter().text(
        rect.right_center(),
        Align2::RIGHT_CENTER,
        text,
        font(12.0),
        tone(ui, MUTED),
    );
}

/// Horizontal gain fader with a right-aligned readout.
fn fader(ui: &mut Ui, ctx: &PluginContext<RelayParams>, row: Rect, param: P) {
    let hit = row.with_max_x(row.max.x - READOUT_W);
    let track = Rect::from_min_max(
        pos2(hit.min.x, hit.center().y - 4.0),
        pos2(hit.max.x, hit.center().y + 4.0),
    );
    let response = ui.interact(
        hit,
        Id::new(("fader", param as u32)),
        Sense::click_and_drag(),
    );

    let mut value = ctx.get_param(param);
    if response.drag_started() {
        ctx.begin_edit(param);
    }
    if response.dragged() && track.width() > 0.0 {
        value = (value + response.drag_delta().x / track.width()).clamp(0.0, 1.0);
        ctx.set_param(param, f64::from(value));
    }
    if response.drag_stopped() {
        ctx.end_edit(param);
    }
    if response.clicked()
        && let Some(point) = response.interact_pointer_pos()
    {
        value = ((point.x - track.min.x) / track.width()).clamp(0.0, 1.0);
        ctx.automate(param, f64::from(value));
    }
    if response.double_clicked() {
        value = GAIN_DEFAULT;
        ctx.automate(param, f64::from(GAIN_DEFAULT));
    }

    response.widget_info(|| {
        egui::WidgetInfo::labeled(
            egui::WidgetType::Slider,
            ui.is_enabled(),
            ctx.format_param(param),
        )
    });
    if response.has_focus() {
        let step = ui.input(|i| {
            if i.key_pressed(egui::Key::ArrowRight) || i.key_pressed(egui::Key::ArrowUp) {
                1.0
            } else if i.key_pressed(egui::Key::ArrowLeft) || i.key_pressed(egui::Key::ArrowDown) {
                -1.0
            } else {
                0.0
            }
        });
        if step != 0.0 {
            value = (value + step / 36.0).clamp(0.0, 1.0);
            ctx.automate(param, f64::from(value));
        }
        ui.painter().rect_stroke(
            hit,
            CornerRadius::same(8),
            Stroke::new(1.0, STUDIO_BLUE),
            StrokeKind::Inside,
        );
    }
    let painter = ui.painter();
    painter.rect_filled(track, CornerRadius::same(4), tone(ui, SURFACE));
    let x_value = track.min.x + track.width() * value;
    let fill = Rect::from_min_max(pos2(track.min.x, track.min.y), pos2(x_value, track.max.y));
    painter.rect_filled(fill, R_METER, STUDIO_BLUE);
    painter.circle_filled(pos2(x_value, track.center().y), 9.0, STUDIO_BLUE);
    painter.circle_filled(pos2(x_value, track.center().y), 3.0, Color32::WHITE);

    readout(ui, row.with_min_x(hit.max.x), &ctx.format_param(param));
}

fn vertical_meters(ui: &Ui, peaks: [f32; 2], held: [f32; 2]) {
    let panel = Rect::from_min_max(pos2(548.0, 88.0), pos2(RIGHT_X, 310.0));
    ui.painter()
        .rect_filled(panel, CornerRadius::same(20), tone(ui, WELL));
    for channel in 0..2 {
        let x = 572.0 + channel as f32 * 42.0;
        let rail = Rect::from_min_max(pos2(x, 118.0), pos2(x + 14.0, 274.0));
        ui.painter()
            .rect_filled(rail, CornerRadius::same(7), tone(ui, SURFACE));
        let top = rail.max.y - rail.height() * db_to_pos(peak_to_db(peaks[channel]));
        ui.painter().rect_filled(
            Rect::from_min_max(pos2(x, top), rail.max),
            CornerRadius::same(7),
            if peaks[channel] >= 1.0 {
                HOT
            } else {
                STUDIO_BLUE
            },
        );
        if held[channel] > 0.000001 {
            let y = rail.max.y - rail.height() * db_to_pos(peak_to_db(held[channel]));
            ui.painter().line_segment(
                [pos2(x - 2.0, y), pos2(x + 16.0, y)],
                Stroke::new(2.0, tone(ui, TEXT)),
            );
        }
        ui.painter().text(
            pos2(x + 7.0, 101.0),
            Align2::CENTER_CENTER,
            if channel == 0 { "L" } else { "R" },
            font(12.0),
            tone(ui, MUTED),
        );
        ui.painter().text(
            pos2(x + 7.0, 291.0),
            Align2::CENTER_CENTER,
            format!("{:.0}", peak_to_db(peaks[channel])),
            font(11.0),
            tone(ui, MUTED),
        );
    }
}

/// Horizontal four-stop gradient: floor → ok → warn → hot from left to right.
#[cfg(test)]
fn gyr_gradient(rect: Rect) -> Mesh {
    const STOPS: [(f32, Color32); 4] = [(0.0, GYR_FLOOR), (0.42, OK), (0.78, WARN), (1.0, HOT)];
    let mut mesh = Mesh::default();
    for pair in STOPS.windows(2) {
        let (lo, lo_c) = pair[0];
        let (hi, hi_c) = pair[1];
        let x_lo = rect.min.x + rect.width() * lo;
        let x_hi = rect.min.x + rect.width() * hi;
        let base = u32::try_from(mesh.vertices.len()).unwrap_or(u32::MAX);
        mesh.colored_vertex(pos2(x_lo, rect.min.y), lo_c);
        mesh.colored_vertex(pos2(x_hi, rect.min.y), hi_c);
        mesh.colored_vertex(pos2(x_lo, rect.max.y), lo_c);
        mesh.colored_vertex(pos2(x_hi, rect.max.y), hi_c);
        mesh.add_triangle(base, base + 1, base + 2);
        mesh.add_triangle(base + 1, base + 3, base + 2);
    }
    mesh
}

// ------------------------------------------------------------------- text

fn tracked_job(text: &str, size: f32, tracking_em: f32, color: Color32) -> LayoutJob {
    let mut job = LayoutJob::default();
    job.append(
        text,
        0.0,
        TextFormat {
            font_id: font(size),
            extra_letter_spacing: size * tracking_em,
            color,
            ..TextFormat::default()
        },
    );
    job
}

fn tracked_text(
    ui: &Ui,
    anchor: Pos2,
    align: Align2,
    text: &str,
    size: f32,
    tracking_em: f32,
    color: Color32,
) {
    let galley = ui
        .painter()
        .layout_job(tracked_job(text, size, tracking_em, color));
    let rect = align.anchor_size(anchor, galley.size());
    ui.painter().galley(rect.min, galley, color);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalized_index_spans_the_unit_range() {
        let close = |a: f64, b: f64| (a - b).abs() < 1e-9;
        assert!(close(normalized_index(0, 2), 0.0));
        assert!(close(normalized_index(1, 2), 1.0));
        assert!(close(normalized_index(1, 3), 0.5));
        assert!(close(normalized_index(9, 3), 1.0));
        assert!(close(normalized_index(0, 1), 0.0));
    }

    #[test]
    fn gradient_covers_the_bar_with_two_triangles_per_stop() {
        let mesh = gyr_gradient(Rect::from_min_size(Pos2::ZERO, vec2(100.0, 4.0)));
        assert_eq!(mesh.vertices.len(), 12);
        assert_eq!(mesh.indices.len(), 18);
    }

    #[test]
    fn window_fits_four_rows_a_meter_and_the_footer() {
        let form_bottom = FORM_TOP + 4.0 * ROW_H + 3.0 * ROW_GAP + METER_H + 4.0;
        assert!(form_bottom <= FOOTER_Y - 12.0);
    }
}
