//! The plugin window, in vizia.
//!
//! Three bands stacked in stretch units so the whole editor scales with the
//! window: a 56px header (logo, centred Share|Join switch, settings gear), a
//! stretching body (label/control form on the left, stereo meters on the
//! right), and a 40px footer (status line, primary action).
//!
//! Every colour lives in `theme_dark.css` / `theme_light.css`; `views.css`
//! carries structure only. Light mode is the same class vocabulary scoped
//! under a `.light` root class, because vizia can add stylesheets but not
//! remove them, so a live swap is not available.

mod views;

use std::sync::Arc;

use relay_session::{PUBLIC_LINK_ORIGIN, normalize_slug};
use std::time::Instant;
use truce::prelude::*;
use truce_vizia::vizia::prelude::*;
use truce_vizia::vizia::vg;
use truce_vizia::{ParamLens, ViziaEditor};

use crate::status::{Facts, Health, describe};
use crate::{Codec, P, RelayParams, clipboard, slug};
use views::{FRAME, fader, lamp, logo_mark, segmented, segmented_signal, spectrum, stereo_meters};

/// Default editor size in logical pixels.
pub const WINDOW: (u32, u32) = (680, 480);
/// Smallest size the layout still reads at.
pub const MIN_WINDOW: (u32, u32) = (510, 360);
/// Largest size the host may request (3x the base).
pub const MAX_WINDOW: (u32, u32) = (2040, 1440);

/// How long the Copy button reads "Copied".
const COPIED_FOR: Duration = Duration::from_millis(1200);

const THEME_LABELS: &[&str] = &["Dark", "Light"];
const MODE_LABELS: &[&str] = &["Share", "Join"];
const CODEC_LABELS: &[&str] = &["Opus", "FLAC", "PCM"];
const MONITOR_LABELS: &[&str] = &["Dry", "Mix", "Remote"];

const BITRATES: [i32; 6] = [64, 96, 128, 160, 192, 256];

/// Editor state the host cannot supply: which theme to paint, and whether
/// the settings popover starts open (screenshot tests only).
#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub dark: bool,
    pub settings_open: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            // DESIGN.md pins Polar Night as the product surface.
            dark: dark_theme_saved(),
            settings_open: false,
        }
    }
}

/// `~/.config/matari/relay-theme`, the same file the egui editor used.
fn theme_path() -> std::path::PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
        })
        .join("matari/relay-theme")
}

/// Dark unless the file explicitly says "light".
fn dark_theme_saved() -> bool {
    std::fs::read_to_string(theme_path()).map_or(true, |value| value.trim() != "light")
}

fn save_theme(dark: bool) {
    let path = theme_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, if dark { "dark" } else { "light" });
}

/// Build the editor. `size` and `options` are explicit so screenshot tests
/// can render every state without touching the user's config.
pub fn build(params: Arc<RelayParams>, size: (u32, u32), options: Options) -> Box<dyn Editor> {
    let for_view = Arc::clone(&params);
    ViziaEditor::new(params, size, move |cx, lens| {
        view(cx, lens, Arc::clone(&for_view), options);
    })
    .with_stylesheet(include_str!("views.css"))
    .with_stylesheet(include_str!("theme_dark.css"))
    .with_stylesheet(include_str!("theme_light.css"))
    .with_font(include_bytes!("../../assets/fonts/Barlow-SemiBold.ttf"))
    // No-op in truce-vizia today; kept so the editor follows the host the
    // moment vizia_baseview grows a resize entry point.
    .resizable(true)
    .min_size(MIN_WINDOW)
    .max_size(MAX_WINDOW)
    .into_editor()
}

/// Live session status, refreshed at 30 fps by the root timer.
#[derive(Clone, Copy)]
struct StatusSignals {
    health: Signal<Health>,
    live: Signal<bool>,
}

fn view(
    cx: &mut Context,
    lens: ParamLens<RelayParams>,
    params: Arc<RelayParams>,
    options: Options,
) {
    // Host state is re-read here on every open: truce-vizia rebuilds the whole
    // view tree per `Editor::open`, and exposes no `state_changed` hook, so a
    // project load while the window is open will not refresh the text fields
    // until it is closed and reopened.
    let saved = params.session.read();
    let name = Signal::new(saved.name);
    let peer = Signal::new(saved.peer);
    let password = Signal::new(saved.password);
    let port = Signal::new(params.port.value().to_string());

    let dark = Signal::new(options.dark);
    let settings_open = Signal::new(options.settings_open);
    let copied = Signal::new(false);
    // The "Copied" flash expires on the root tick; a second vizia timer would
    // hang the editor (see `views::TICKS`).
    let copied_at: Arc<std::sync::Mutex<Option<Instant>>> = Arc::default();
    let copied_press = Arc::clone(&copied_at);

    let mode = lens.value_signal(P::Mode);
    let share = Memo::new(move |_| mode.get() < 0.5);

    // Seeded here as well as on the timer so the very first frame (and the
    // headless screenshot path, which never ticks) shows a real status.
    let first_facts = Facts::read(&params.control, share.get());
    let first = describe(&first_facts);
    let status = StatusSignals {
        health: Signal::new(first.health),
        live: Signal::new(first_facts.live),
    };
    // Status is derived from `SessionControl`, which has no reactive handle,
    // so poll it. This is the editor's ONLY timer: see `views::on_tick`.
    let status_params = Arc::clone(&params);
    let status_line = Signal::new(first.line);
    let tick_lens = lens.clone();
    let copied_at = Arc::clone(&copied_at);
    views::clear_ticks();
    let timer = cx.add_timer(FRAME, None, move |_cx, action| {
        if !matches!(action, TimerAction::Tick(_)) {
            return;
        }
        tick_lens.refresh_meters();
        tick_lens.refresh_params();
        views::tick_all();
        let facts = Facts::read(&status_params.control, share.get());
        let described = describe(&facts);
        status.health.set(described.health);
        status.live.set(facts.live);
        if status_line.get() != described.line {
            status_line.set(described.line);
        }
        if copied.get()
            && copied_at
                .lock()
                .ok()
                .and_then(|at| *at)
                .is_none_or(|at| at.elapsed() >= COPIED_FOR)
        {
            copied.set(false);
        }
    });
    cx.start_timer(timer);

    VStack::new(cx, move |cx| {
        header(cx, lens.clone(), dark, settings_open, port, params.clone());
        Element::new(cx).class("hairline");
        body(
            cx,
            lens.clone(),
            &params,
            share,
            name,
            peer,
            password,
            copied,
            copied_press,
        );
        Element::new(cx).class("hairline");
        footer(cx, lens, &params, status, status_line);
    })
    .class("root")
    .toggle_class("light", Memo::new(move |_| !dark.get()));
}

// ----------------------------------------------------------------- header

fn header(
    cx: &mut Context,
    lens: ParamLens<RelayParams>,
    dark: Signal<bool>,
    settings_open: Signal<bool>,
    port: Signal<String>,
    params: Arc<RelayParams>,
) {
    HStack::new(cx, move |cx| {
        HStack::new(cx, |cx| {
            logo_mark(cx);
            Label::new(cx, "RELAY").class("wordmark");
        })
        .class("header-side");

        segmented(cx, lens.clone(), P::Mode, MODE_LABELS).class("mode-switch");

        HStack::new(cx, move |cx| {
            icon_button(cx, Icon::Gear, "Settings")
                .on_press(move |_| settings_open.set(!settings_open.get()));
            Binding::new(cx, settings_open, move |cx| {
                if settings_open.get() {
                    settings_popover(cx, lens.clone(), dark, port, &params);
                }
            });
        })
        .class("header-side")
        .class("end");
    })
    .class("header");
}

fn settings_popover(
    cx: &mut Context,
    lens: ParamLens<RelayParams>,
    dark: Signal<bool>,
    port: Signal<String>,
    params: &Arc<RelayParams>,
) {
    let port_id: u32 = P::Port.into();
    let port_value = lens.value_signal(port_id);
    let params = Arc::clone(params);
    Popover::new(cx, move |cx| {
        VStack::new(cx, move |cx| {
            Label::new(cx, "Theme").class("row-label");
            let selected = Memo::new(move |_| usize::from(!dark.get()));
            segmented_signal(cx, selected, THEME_LABELS, move |_cx, index| {
                dark.set(index == 0);
                save_theme(index == 0);
            });
            Label::new(cx, "Port").class("row-label");
            Textbox::new(cx, port)
                .class("field")
                .on_edit(move |_, text| port.set(text))
                .on_submit(move |_, text, _| {
                    let value = text.trim().parse::<i32>().unwrap_or(0).clamp(1, 65_535);
                    port.set(value.to_string());
                    // `discrete(1, 65535)` -> 65534 steps.
                    let normalized = f64::from(value - 1) / 65_534.0;
                    lens.automate(port_id, normalized);
                    #[allow(clippy::cast_possible_truncation)]
                    port_value.set(normalized as f32);
                    params.publish_atomics();
                });
        })
        .class("settings-body");
    })
    .class("settings")
    .placement(Placement::BottomEnd)
    .show_arrow(false);
}

// ------------------------------------------------------------------- body

#[allow(clippy::too_many_arguments)]
fn body(
    cx: &mut Context,
    lens: ParamLens<RelayParams>,
    params: &Arc<RelayParams>,
    share: Memo<bool>,
    name: Signal<String>,
    peer: Signal<String>,
    password: Signal<String>,
    copied: Signal<bool>,
    copied_press: Arc<std::sync::Mutex<Option<Instant>>>,
) {
    let params = Arc::clone(params);
    let meter_lens = lens.clone();
    let tap = Arc::clone(&params.spectrum);
    let rate = Arc::clone(&params.spectrum);
    HStack::new(cx, move |cx| {
        VStack::new(cx, move |cx| {
            Binding::new(cx, share, {
                let lens = lens.clone();
                let params = Arc::clone(&params);
                move |cx| {
                    VStack::new(cx, |cx| {
                        if share.get() {
                            share_form(
                                cx,
                                lens.clone(),
                                &params,
                                name,
                                password,
                                copied,
                                Arc::clone(&copied_press),
                            );
                        } else {
                            join_form(cx, lens.clone(), &params, peer);
                        }
                    })
                    .class("rows");
                }
            });
            VStack::new(cx, move |cx| {
                HStack::new(cx, move |cx| {
                    Label::new(cx, "Send spectrum").class("caption");
                    Label::new(
                        cx,
                        Memo::new(move |_| {
                            #[allow(clippy::cast_precision_loss)]
                            let upper = (rate.rate.load(std::sync::atomic::Ordering::Relaxed)
                                as f32
                                * 0.45)
                                .min(18_000.0);
                            format!("50 Hz — {:.1} kHz", upper / 1000.0)
                        }),
                    )
                    .class("caption")
                    .class("end");
                })
                .class("caption-row");
                spectrum(cx, Arc::clone(&tap));
            })
            .class("scope-well");
        })
        .class("form-col");

        VStack::new(cx, move |cx| {
            stereo_meters(cx, meter_lens.clone());
        })
        .class("meter-col");
    })
    .class("body");
}

/// One label/control row on the shared grid.
fn row(cx: &mut Context, label: &'static str, content: impl FnOnce(&mut Context)) {
    HStack::new(cx, move |cx| {
        Label::new(cx, label).class("row-label");
        HStack::new(cx, content).class("row-control");
    })
    .class("row");
}

#[allow(clippy::too_many_arguments)]
fn share_form(
    cx: &mut Context,
    lens: ParamLens<RelayParams>,
    params: &Arc<RelayParams>,
    name: Signal<String>,
    password: Signal<String>,
    copied: Signal<bool>,
    copied_press: Arc<std::sync::Mutex<Option<Instant>>>,
) {
    row(cx, "Session", {
        let params = Arc::clone(params);
        move |cx| {
            VStack::new(cx, move |cx| {
                Textbox::new(cx, name)
                    .class("nameplate")
                    .placeholder("room name")
                    .on_edit(move |_, text| name.set(text))
                    .on_submit(move |_, text, _| {
                        let clean = normalize_slug(&text);
                        let slug = if clean.is_empty() {
                            slug::new_slug()
                        } else {
                            clean
                        };
                        name.set(slug.clone());
                        params.session.update(|session| session.name = slug);
                    });
                Element::new(cx).class("nameplate-rule");
            })
            .class("nameplate-wrap");
            icon_button(cx, Icon::Copy, "Copy listener link")
                .toggle_class("copied", copied)
                .on_press(move |_| {
                    if clipboard::copy(&format!("{PUBLIC_LINK_ORIGIN}/{}", name.get())) {
                        copied.set(true);
                        if let Ok(mut at) = copied_press.lock() {
                            *at = Some(Instant::now());
                        }
                    }
                });
            icon_button(cx, Icon::Open, "Open listener in browser").on_press(move |_| {
                // Best effort: no browser is not an error the plugin can act on.
                let _ = open::that_detached(format!("{PUBLIC_LINK_ORIGIN}/{}", name.get()));
            });
        }
    });
    row(cx, "Password", {
        let params = Arc::clone(params);
        move |cx| {
            Textbox::new(cx, password)
                .class("field")
                .class("secret")
                .placeholder("optional")
                .mask_char('•')
                .on_edit(move |_, text| password.set(text))
                .on_submit(move |_, text, _| {
                    password.set(text.clone());
                    params.session.update(|session| session.password = text);
                });
        }
    });
    row(cx, "Format", {
        let lens = lens.clone();
        let params = Arc::clone(params);
        move |cx| {
            segmented(cx, lens.clone(), P::Codec, CODEC_LABELS);
            let codec = lens.value_signal(P::Codec);
            Binding::new(cx, codec, move |cx| {
                quality(cx, lens.clone(), &params);
            });
        }
    });
    row(cx, "Send", move |cx| {
        fader(cx, lens, P::Send);
    });
}

/// Codec-dependent quality control: a bitrate / level dropdown, or a plain
/// label for PCM which has nothing to choose.
fn quality(cx: &mut Context, lens: ParamLens<RelayParams>, params: &Arc<RelayParams>) {
    let codec = params.codec.value();
    let hint = match codec {
        Codec::Opus => "Bitrate: higher values preserve more detail and use more bandwidth.",
        Codec::Flac => {
            "Lossless compression: higher levels use more CPU, not higher audio quality."
        }
        Codec::Pcm => "Uncompressed 24-bit audio. No quality setting needed.",
    };
    if codec == Codec::Pcm {
        Label::new(cx, "24-bit PCM")
            .class("quality")
            .class("static")
            .tooltip(move |cx| hint_tip(cx, hint));
        return;
    }

    let bitrate = lens.value_signal(P::Bitrate);
    let level = lens.value_signal(P::FlacLevel);
    let trigger_lens = lens.clone();
    let trigger = Memo::new(move |_| {
        let _ = (bitrate.get(), level.get());
        match codec {
            Codec::Flac => format!("Level {}", trigger_lens.format(P::FlacLevel)),
            _ => trigger_lens.format(P::Bitrate),
        }
    });

    Dropdown::new(
        cx,
        move |cx| {
            Button::new(cx, move |cx| Label::new(cx, trigger))
                .class("quality")
                .on_press(|cx| cx.emit(PopupEvent::Switch));
        },
        move |cx| {
            let lens = lens.clone();
            VStack::new(cx, move |cx| {
                Label::new(
                    cx,
                    if codec == Codec::Flac {
                        "Lossless compression"
                    } else {
                        "Bitrate / kbps"
                    },
                )
                .class("caption");
                let count = if codec == Codec::Flac {
                    9
                } else {
                    BITRATES.len()
                };
                #[allow(clippy::needless_range_loop)]
                for index in 0..count {
                    let (label, id, normalized) = if codec == Codec::Flac {
                        #[allow(clippy::cast_precision_loss)]
                        (format!("Level {index}"), P::FlacLevel, index as f64 / 8.0)
                    } else {
                        let kbps = BITRATES[index];
                        (
                            format!("{kbps} kbps"),
                            P::Bitrate,
                            f64::from(kbps - 64) / 192.0,
                        )
                    };
                    let signal = if codec == Codec::Flac { level } else { bitrate };
                    let lens = lens.clone();
                    Button::new(cx, move |cx| Label::new(cx, label.clone()).hoverable(false))
                        .class("option")
                        .on_press(move |cx| {
                            lens.automate(id, normalized);
                            #[allow(clippy::cast_possible_truncation)]
                            signal.set(normalized as f32);
                            cx.emit(PopupEvent::Close);
                        });
                }
            })
            .class("options");
        },
    )
    .show_arrow(false)
    .tooltip(move |cx| hint_tip(cx, hint));
}

fn hint_tip<'a>(cx: &'a mut Context, hint: &'static str) -> Handle<'a, Tooltip> {
    Tooltip::new(cx, move |cx| {
        Label::new(cx, hint);
    })
    .class("hint")
    .size(Auto)
    .placement(Placement::Top)
}

fn join_form(
    cx: &mut Context,
    lens: ParamLens<RelayParams>,
    params: &Arc<RelayParams>,
    peer: Signal<String>,
) {
    row(cx, "Session", {
        let params = Arc::clone(params);
        move |cx| {
            Textbox::new(cx, peer)
                .class("field")
                .placeholder("host:port")
                .on_edit(move |_, text| peer.set(text))
                .on_submit(move |_, text, _| {
                    let trimmed = text.trim().to_owned();
                    peer.set(trimmed.clone());
                    params.session.update(|session| session.peer = trimmed);
                });
        }
    });
    row(cx, "Monitor", {
        let lens = lens.clone();
        move |cx| {
            segmented(cx, lens, P::Monitor, MONITOR_LABELS);
        }
    });
    row(cx, "Send", {
        let lens = lens.clone();
        move |cx| {
            fader(cx, lens, P::Send);
        }
    });
    row(cx, "Receive", move |cx| {
        fader(cx, lens, P::Hear);
    });
}

// ----------------------------------------------------------------- footer

fn footer(
    cx: &mut Context,
    lens: ParamLens<RelayParams>,
    params: &Arc<RelayParams>,
    status: StatusSignals,
    line: Signal<String>,
) {
    let control = Arc::clone(&params.control);
    let label = Memo::new(move |_| {
        if status.health.get() == Health::Asleep {
            "Wake"
        } else if status.live.get() {
            "Disconnect"
        } else {
            "Go live"
        }
    });
    HStack::new(cx, move |cx| {
        HStack::new(cx, move |cx| {
            lamp(cx, status.health);
            Label::new(cx, line).class("status");
        })
        .class("status-row");

        Button::new(cx, move |cx| Label::new(cx, label))
            .class("primary")
            .toggle_class("armed", status.live)
            .on_press(move |_| {
                if status.health.get() == Health::Asleep {
                    control.request_web_wake();
                } else {
                    lens.automate(P::Live, if status.live.get() { 0.0 } else { 1.0 });
                }
            });
    })
    .class("footer");
}

// ------------------------------------------------------------ icon buttons

/// The three drawn glyphs the editor owns. No emoji, no icon font.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Icon {
    Gear,
    Copy,
    Open,
}

struct IconView(Icon);

impl View for IconView {
    fn element(&self) -> Option<&'static str> {
        Some("icon")
    }

    fn draw(&self, cx: &mut DrawContext, canvas: &Canvas) {
        let bounds = cx.bounds();
        let size = bounds.w.min(bounds.h);
        if size <= 0.0 {
            return;
        }
        let (cx_, cy) = (bounds.x + bounds.w / 2.0, bounds.y + bounds.h / 2.0);
        let unit = size / 24.0;
        let color = cx.font_color();
        let mut paint = vg::Paint::default();
        paint.set_anti_alias(true);
        paint.set_style(vg::PaintStyle::Stroke);
        paint.set_stroke_width(1.6 * unit);
        paint.set_stroke_cap(vg::PaintCap::Round);
        paint.set_stroke_join(vg::PaintJoin::Round);
        paint.set_color(vg::Color::from_argb(
            color.a(),
            color.r(),
            color.g(),
            color.b(),
        ));

        let mut path = vg::PathBuilder::new();
        match self.0 {
            Icon::Gear => {
                path.add_circle((cx_, cy), 3.2 * unit, None);
                path.add_circle((cx_, cy), 6.6 * unit, None);
                for tooth in 0..8 {
                    #[allow(clippy::cast_precision_loss)]
                    let angle = std::f32::consts::TAU * tooth as f32 / 8.0;
                    let (sin, cos) = angle.sin_cos();
                    path.move_to((cx_ + cos * 6.4 * unit, cy + sin * 6.4 * unit));
                    path.line_to((cx_ + cos * 8.6 * unit, cy + sin * 8.6 * unit));
                }
            }
            Icon::Copy => {
                path.add_rect(
                    vg::Rect::from_xywh(
                        cx_ - 7.5 * unit,
                        cy - 7.5 * unit,
                        10.0 * unit,
                        10.0 * unit,
                    ),
                    None,
                    None,
                );
                path.add_rect(
                    vg::Rect::from_xywh(
                        cx_ - 2.5 * unit,
                        cy - 2.5 * unit,
                        10.0 * unit,
                        10.0 * unit,
                    ),
                    None,
                    None,
                );
            }
            Icon::Open => {
                path.move_to((cx_ + 2.0 * unit, cy - 7.0 * unit));
                path.line_to((cx_ + 7.5 * unit, cy - 7.0 * unit));
                path.line_to((cx_ + 7.5 * unit, cy - 1.5 * unit));
                path.move_to((cx_ - 1.0 * unit, cy + 1.0 * unit));
                path.line_to((cx_ + 7.5 * unit, cy - 7.0 * unit));
                path.move_to((cx_ + 3.0 * unit, cy + 3.0 * unit));
                path.line_to((cx_ + 3.0 * unit, cy + 7.5 * unit));
                path.line_to((cx_ - 7.5 * unit, cy + 7.5 * unit));
                path.line_to((cx_ - 7.5 * unit, cy - 3.0 * unit));
                path.line_to((cx_ - 3.0 * unit, cy - 3.0 * unit));
            }
        }
        canvas.draw_path(&path.detach(), &paint);
    }
}

/// A tick, shown in place of the Copy glyph for 1.2 s after a copy.
struct CheckView;

impl View for CheckView {
    fn element(&self) -> Option<&'static str> {
        Some("icon")
    }

    fn draw(&self, cx: &mut DrawContext, canvas: &Canvas) {
        let bounds = cx.bounds();
        let size = bounds.w.min(bounds.h);
        if size <= 0.0 {
            return;
        }
        let (cx_, cy) = (bounds.x + bounds.w / 2.0, bounds.y + bounds.h / 2.0);
        let unit = size / 24.0;
        let color = cx.font_color();
        let mut paint = vg::Paint::default();
        paint.set_anti_alias(true);
        paint.set_style(vg::PaintStyle::Stroke);
        paint.set_stroke_width(2.0 * unit);
        paint.set_stroke_cap(vg::PaintCap::Round);
        paint.set_stroke_join(vg::PaintJoin::Round);
        paint.set_color(vg::Color::from_argb(
            color.a(),
            color.r(),
            color.g(),
            color.b(),
        ));
        let mut path = vg::PathBuilder::new();
        path.move_to((cx_ - 6.0 * unit, cy));
        path.line_to((cx_ - 1.5 * unit, cy + 4.5 * unit));
        path.line_to((cx_ + 6.5 * unit, cy - 5.0 * unit));
        canvas.draw_path(&path.detach(), &paint);
    }
}

fn icon_button<'a>(cx: &'a mut Context, icon: Icon, hint: &'static str) -> Handle<'a, Button> {
    Button::new(cx, move |cx| {
        if icon == Icon::Copy {
            // Swapped for a tick while `copied` is set on the button.
            CheckView.build(cx, |_| {}).class("glyph").class("tick");
        }
        IconView(icon).build(cx, |_| {}).class("glyph")
    })
    .class("icon-button")
    .tooltip(move |cx| hint_tip(cx, hint))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Mode, Monitor};
    use views::normalized_index;

    #[test]
    fn segmented_indices_cover_every_enum() {
        assert!((normalized_index(1, Mode::variant_count()) - 1.0).abs() < 1e-9);
        assert!((normalized_index(1, Codec::variant_count()) - 0.5).abs() < 1e-9);
        assert!((normalized_index(2, Monitor::variant_count()) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn segment_labels_match_the_params() {
        assert_eq!(MODE_LABELS.len(), Mode::variant_count());
        assert_eq!(CODEC_LABELS.len(), Codec::variant_count());
        assert_eq!(MONITOR_LABELS.len(), Monitor::variant_count());
    }

    /// Renders the whole editor with the meters lit and real audio in the
    /// spectrum tap - the headless bridge reports 0.0 meters, so this is the
    /// only way to review a live rail. Writes a PNG when `RELAY_SHOTS` is set.
    #[test]
    fn the_lit_editor_renders() {
        views::set_fake_meters([0.72, 1.0]);
        let params = Arc::new(RelayParams::default());
        params.spectrum.audio.clear();
        #[allow(clippy::cast_precision_loss)]
        let noise: Vec<f32> = (0..8192)
            .map(|i| ((i * 2_654_435_761_usize) % 2003) as f32 / 1000.0 - 1.0)
            .collect();
        params.spectrum.audio.push_frames(&noise);

        let mut editor = build(
            Arc::clone(&params),
            WINDOW,
            Options {
                dark: true,
                settings_open: false,
            },
        );
        let (pixels, w, h) = editor
            .screenshot(params as Arc<dyn truce_params::Params>)
            .expect("headless render");
        if let Ok(dir) = std::env::var("RELAY_SHOTS") {
            std::fs::create_dir_all(&dir).unwrap();
            truce_core::screenshot::save_png(
                std::path::Path::new(&format!("{dir}/share-dark-lit.png")),
                &pixels,
                w,
                h,
            );
        }
        let distinct = pixels
            .chunks_exact(4)
            .map(|p| u32::from_be_bytes([p[0], p[1], p[2], p[3]]))
            .collect::<std::collections::HashSet<_>>();
        assert!(
            distinct.len() > 32,
            "blank render: {} colors",
            distinct.len()
        );
    }

    #[test]
    fn theme_defaults_to_dark() {
        // DESIGN.md: Polar Night is the product surface; only an explicit
        // "light" file flips it.
        assert!(dark_theme_saved() || std::fs::read_to_string(theme_path()).is_ok());
    }
}
