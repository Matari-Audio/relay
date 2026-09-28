//! The editor: a compact charcoal panel. Room, password and link fields
//! with their actions inside them on the left; input and output meters,
//! each with its own fader, on the right.

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use moose::mui::{Bridge, MuiEditor};
use moose_core::editor::{Editor, IntoEditor};
use mui::geometry::kurbo::Shape as _;
use mui::prelude::*;
use relay_core::{Net, Peak, Shared};

use crate::{P, RelayParams};

const SEMIBOLD: &[u8] = include_bytes!("../assets/barlow-600.ttf");
const BOLD: &[u8] = include_bytes!("../assets/barlow-700.ttf");
const MONO: &[u8] = include_bytes!("../assets/martian-mono.ttf");
const PIXEL: &[u8] = include_bytes!("../assets/silkscreen.ttf");
const DEPARTURE: &[u8] = include_bytes!("../assets/departure-mono.ttf");
/// Phosphor Bold, cut down to the glyphs below.
const ICONS: &[u8] = include_bytes!("../assets/phosphor-bold-relay.ttf");

const COPY: char = '\u{E1CA}';
const DICE: char = '\u{E1EE}';
const EYE: char = '\u{E220}';
const EYE_OFF: char = '\u{E224}';
const CHECK: char = '\u{E182}';
const HASH: char = '\u{E2A2}';
const LINK: char = '\u{E2E6}';
const LOCK: char = '\u{E308}';
const WIFI: char = '\u{E4EA}';
const MIC: char = '\u{E326}';
const MIC_OFF: char = '\u{E328}';
const SPEAKER: char = '\u{E44A}';
const SPEAKER_OFF: char = '\u{E45A}';

const SIZE: (u32, u32) = (440, 156);
/// Field height and corner.
const H: f64 = 24.0;
const R: f64 = 4.0;
/// Meter floor, dB.
const FLOOR: f32 = -60.0;
/// Fader range, dB, as the params declare it.
const GAIN: (f64, f64) = (-24.0, 12.0);
/// Width of an L/R meter pair and of the scale between pairs.
const PAIR: f64 = 28.0;
const SCALE: f64 = 24.0;
const NAME_RAIL: f64 = 19.0;

// Matari's Signal palette, Oklch. The listen page uses the matching sRGB.
const BG: Color = Color::oklch(0.1735, 0.002, 286.2); // #101011
const FIELD: Color = Color::oklch(0.2273, 0.0038, 286.1); // #1c1c1e
const HOT: Color = Color::oklch(0.278, 0.0055, 286.0); // #28282b
const WELL: Color = Color::oklch(0.1452, 0.0021, 286.1); // #0a0a0b
const TEXT: Color = Color::oklch(0.961, 0.0, 0.0); // #f2f2f2
const DIM: Color = Color::oklch(0.7202, 0.0072, 286.2); // #a4a4a9
const LIME: Color = Color::oklch(0.9273, 0.2266, 124.65); // #c6ff1f
const ON_LIME: Color = BG;
const YELLOW: Color = Color::oklch(0.877, 0.176, 92.7); // #ffd21a
const RED: Color = Color::oklch(0.647, 0.239, 22.0); // #ff2d46
/// A meter's colour down its height, 0 dB at the top.
const RAMP: [(f32, Color); 4] = [(0.0, RED), (0.15, YELLOW), (0.35, LIME), (1.0, LIME)];

const STANDARD_THEME: Theme = Theme {
    palette: Palette {
        primary: Pigment::new(125.0, 0.2),
        ..Palette::NEUTRAL
    },
    corners: Corners {
        selector: R,
        field: R,
        box_: R,
        concave: R,
    },
    text: 12.0,
    ..Theme::DEFAULT
};
const PIXEL_THEME: Theme = Theme {
    corners: Corners {
        selector: 1.0,
        field: 1.0,
        box_: 1.0,
        concave: 1.0,
    },
    ..STANDARD_THEME
};

struct Fonts {
    bold: Font,
    mono: Font,
    semibold: Font,
    pixel_head: Font,
    departure: Font,
    icons: Font,
    pixel: Cell<bool>,
}

/// One meter rail's ballistics, in dB.
#[derive(Clone, Copy)]
struct Rail {
    /// What is drawn: jumps up, falls at 24 dB/s.
    shown: f32,
    /// Peak hold: sits 1.5 s, then falls at 20 dB/s.
    hold: f32,
    held: Instant,
}

impl Rail {
    fn feed(&mut self, peak: f32, now: Instant, dt: f32) {
        let db = db(peak);
        self.shown = db.max(self.shown - 24.0 * dt).max(FLOOR);
        if db >= self.hold {
            (self.hold, self.held) = (db, now);
        } else if now.duration_since(self.held).as_secs_f32() > 1.5 {
            self.hold = (self.hold - 20.0 * dt).max(FLOOR);
        }
    }
}

/// What only the editor remembers.
struct View {
    about: bool,
    mic_panel: bool,
    /// When the editor opened: the clock for the mark's motion.
    born: Instant,
    show_password: bool,
    show_address: bool,
    copied: bool,
    room: String,
    password: String,
    room_edit: Option<Instant>,
    password_edit: Option<Instant>,
    /// IN L, IN R, OUT L, OUT R.
    rails: [Rail; 4],
    mic_rails: HashMap<String, [Rail; 2]>,
    mic_scroll: usize,
    /// Highest IN and OUT since the last reset, linear.
    max: [f32; 2],
    last: Instant,
}

impl Default for View {
    fn default() -> Self {
        let now = Instant::now();
        let rail = Rail {
            shown: FLOOR,
            hold: FLOOR,
            held: now,
        };
        Self {
            about: false,
            mic_panel: false,
            born: now,
            show_password: false,
            show_address: false,
            copied: false,
            room: String::new(),
            password: String::new(),
            room_edit: None,
            password_edit: None,
            rails: [rail; 4],
            mic_rails: HashMap::new(),
            mic_scroll: 0,
            max: [0.0; 2],
            last: now,
        }
    }
}

fn db(linear: f32) -> f32 {
    20.0 * linear.max(1e-6).log10()
}

/// How far down a rail `db` sits, 0 at the top.
fn depth(db: f32) -> f32 {
    (db / FLOOR).clamp(0.0, 1.0)
}

fn load(bytes: &'static [u8]) -> Font {
    Font::new(bytes).expect("bundled font parses")
}

fn new_ui() -> (Ui, Fonts) {
    let fonts = Fonts {
        bold: load(BOLD),
        mono: load(MONO),
        semibold: load(SEMIBOLD),
        pixel_head: load(PIXEL),
        departure: load(DEPARTURE),
        icons: load(ICONS),
        pixel: Cell::new(true),
    };
    (Ui::new(PIXEL_THEME).font(fonts.departure.clone()), fonts)
}

pub fn editor(params: Arc<RelayParams>) -> Box<dyn Editor> {
    let shared = Arc::clone(&params.link.0);
    let settings = Arc::clone(&params);
    let (ui, fonts) = new_ui();
    let mut view = View {
        room: Shared::text(&shared.room),
        password: Shared::text(&shared.password),
        ..View::default()
    };
    let mut editor = MuiEditor::new(params, ui, SIZE, move |ui, bridge| {
        build(
            ui,
            bridge,
            &shared,
            &settings.standard_ui,
            &fonts,
            &mut view,
        )
    })
    .resizable((380, 150))
    // Meters decay and the link status moves while nothing is touched.
    .changed(|| true);
    let _ = editor.set_size(616, 218);
    editor.into_editor()
}

/// An icon centred in a field-high square, so its inset is the same on
/// every side.
fn glyph(fonts: &Fonts, c: char, ink: Color) -> El {
    row([icon(c).font(fonts.icons.clone()).text_size(12.0).fill(ink)])
        .justify(Justify::Center)
        .center()
        .size(H, H)
}

/// An icon button at a field's right end.
fn action(ui: &Ui, fonts: &Fonts, id: &'static str, c: char, label: &str) -> (El, bool) {
    let r = ui.get(id);
    let el = glyph(fonts, c, if r.hovered { TEXT } else { DIM })
        .a11y(A11y::Button)
        .named(label)
        .focusable()
        .id(id);
    (el, r.clicked_with(Button::Primary))
}

/// A field: leading icon, the body, then its action, all in one box.
fn field(fonts: &Fonts, c: char, body: El, action: El) -> El {
    row([glyph(fonts, c, DIM), body.grow(1.0), action])
        .center()
        .h(H)
        .radius(corner(fonts))
        .fill(FIELD)
}

/// A text input with its own chrome stripped, to sit in a [`field`].
fn bare(input: El) -> El {
    input.fill(FIELD).radius(0.0).pad((0.0, 0.0)).h(H - 4.0)
}

fn mono(fonts: &Fonts, s: String, size: f64, ink: Color) -> El {
    let font = if fonts.pixel.get() {
        &fonts.departure
    } else {
        &fonts.mono
    };
    text(s).font(font.clone()).text_size(size).fill(ink)
}

fn corner(fonts: &Fonts) -> f64 {
    if fonts.pixel.get() { 1.0 } else { R }
}

fn build(
    ui: &mut Ui,
    bridge: &mut Bridge<RelayParams>,
    shared: &Shared,
    standard_ui: &RwLock<bool>,
    fonts: &Fonts,
    view: &mut View,
) -> El {
    let pixel = !*standard_ui.read().unwrap();
    fonts.pixel.set(pixel);
    ui.set_theme(if pixel { PIXEL_THEME } else { STANDARD_THEME });
    ui.set_font(Some(if pixel {
        fonts.departure.clone()
    } else {
        fonts.semibold.clone()
    }));
    let picked = (bridge.value(P::Mode) * 2.0).round() as usize;
    let mode = bridge.bind(ui, P::Mode, |ui, _, v| {
        let segments: Vec<El> = ["Off", "Share", "Join"]
            .into_iter()
            .enumerate()
            .map(|(i, name)| {
                let r = ui.get(name);
                if r.clicked_with(Button::Primary) {
                    *v = i as f64 / 2.0;
                }
                let (bg, fg) = match (i == picked, r.hovered) {
                    (true, _) => (LIME, ON_LIME),
                    (false, true) => (HOT, TEXT),
                    (false, false) => (FIELD, DIM),
                };
                row([text(name)
                    .font(if pixel {
                        fonts.pixel_head.clone()
                    } else {
                        fonts.semibold.clone()
                    })
                    .text_size(if pixel { 9.0 } else { 10.5 })
                    .fill(fg)])
                .justify(Justify::Center)
                .center()
                .size(42.0, 18.0)
                .radius((corner(fonts) - 1.0).max(0.0))
                .fill(bg)
                .a11y(A11y::Button)
                .named(name)
                .focusable()
                .id(name)
            })
            .collect();
        row(segments)
            .gap(2.0)
            .pad(2.0)
            .radius(corner(fonts))
            .fill(FIELD)
    });

    let now = Instant::now();
    if view.room_edit.is_none() {
        view.room = Shared::text(&shared.room);
    }
    let Response { el: input, changed } = text_input(ui, "room", &mut view.room);
    if changed {
        view.room_edit = Some(now);
    }
    if view.room_edit.is_some_and(|at| {
        now.duration_since(at) >= Duration::from_millis(300)
            || ui.keys("room").iter().any(|k| k.key == Key::Enter)
    }) {
        shared.set_text(&shared.room, &view.room);
        view.room_edit = None;
    }
    let (dice, roll) = action(ui, fonts, "roll", DICE, "New room name");
    if roll {
        view.room = relay_core::room_name();
        shared.set_text(&shared.room, &view.room);
        view.room_edit = None;
    }
    let room = view.room.clone();
    let mut rows = vec![field(fonts, HASH, bare(input), dice)];

    if view.password_edit.is_none() {
        view.password = Shared::text(&shared.password);
    }
    let show = view.show_password;
    let Response { el: input, changed } = if show {
        text_input(ui, "pass", &mut view.password)
    } else {
        masked_input(ui, "pass", &mut view.password)
    };
    if changed {
        view.password_edit = Some(now);
    }
    if view.password_edit.is_some_and(|at| {
        now.duration_since(at) >= Duration::from_millis(300)
            || ui.keys("pass").iter().any(|k| k.key == Key::Enter)
    }) {
        shared.set_text(&shared.password, &view.password);
        view.password_edit = None;
    }
    let (eye, toggle) = action(
        ui,
        fonts,
        "pass-eye",
        if show { EYE_OFF } else { EYE },
        if show {
            "Hide password"
        } else {
            "Show password"
        },
    );
    view.show_password ^= toggle;
    rows.push(field(fonts, LOCK, bare(input), eye));

    let peers = shared.peers.load(Relaxed);
    let tail = if picked == 1 {
        let slug = relay_core::slug(&room);
        let url = format!("https://{}/{slug}", relay_core::SITE);
        let icon = if view.copied { CHECK } else { COPY };
        let (copy, clicked) = action(ui, fonts, "copy", icon, "Copy link");
        if clicked {
            ui.set_clipboard(url.clone());
            view.copied = true;
        }
        let r = ui.get("link");
        if r.clicked_with(Button::Primary) {
            open(&url);
        }
        let link = if pixel {
            text("OPEN LISTEN PAGE")
                .font(fonts.pixel_head.clone())
                .text_size(9.0)
                .fill(if r.hovered { LIME } else { TEXT })
        } else {
            row![
                text(format!("{}/", relay_core::SITE))
                    .text_size(11.0)
                    .fill(DIM),
                text(slug)
                    .text_size(11.0)
                    .fill(if r.hovered { LIME } else { TEXT }),
            ]
        }
        .a11y(A11y::Button)
        .named("Open the listen page")
        .focusable()
        .id("link");
        rows.push(field(fonts, LINK, link, copy));

        // The LAN address rides in the status line, hidden until asked.
        let mut address = Shared::text(&shared.address);
        if !view.show_address {
            address = address
                .chars()
                .map(|c| if c.is_ascii_hexdigit() { '•' } else { c })
                .collect();
        }
        // Internet bitrate: what Opus runs at, adapting to the listeners,
        // over the ceiling. A click steps the ceiling down, then round.
        let quality = bridge.bind(ui, P::Quality, |ui, id, v| {
            let r = ui.get(&id);
            let mut step = (*v * 3.0).round() as usize % 4;
            if r.clicked_with(Button::Primary) {
                step = (step + 1) % 4;
                *v = step as f64 / 3.0;
            }
            let cap = [510, 256, 128, 64][step];
            let label = if shared.net() == Net::Internet {
                format!("{}/{cap}k", shared.bitrate.load(Relaxed) / 1000)
            } else {
                format!("{cap}k")
            };
            mono(fonts, label, 9.0, if r.hovered { TEXT } else { DIM })
                .a11y(A11y::Button)
                .named("Internet quality ceiling")
                .focusable()
                .id(id)
        });
        let r = ui.get("lan");
        view.show_address ^= r.clicked_with(Button::Primary);
        let lan = row![
            glyph(fonts, WIFI, DIM),
            mono(fonts, address, 9.0, if r.hovered { TEXT } else { DIM }),
        ]
        .center()
        .a11y(A11y::Button)
        .named("LAN address")
        .focusable()
        .id("lan");
        row![quality, lan].gap(8.0).center()
    } else if picked == 2 && peers > 0 {
        let rate = f64::from(shared.rate.load(Relaxed).max(1));
        let ms = f64::from(shared.latency.load(Relaxed)) * 1000.0 / rate;
        mono(fonts, format!("{ms:.1} ms"), 9.0, DIM)
    } else {
        spacer()
    };

    let (lit, status) = match (picked, shared.net()) {
        (0, _) | (_, Net::Idle) => (DIM, "Off".to_owned()),
        (1, Net::Taken) => (RED, "Room taken, roll a new name".into()),
        (_, Net::Denied) => (RED, "Wrong password".into()),
        (_, Net::RateMismatch) => (YELLOW, "Sample rates differ".into()),
        (1, Net::Offline) if peers == 0 => (YELLOW, "LAN only".into()),
        (1, _) if peers == 0 => (TEXT, "Waiting for listeners".into()),
        (1, _) => (LIME, format!("{peers} listening")),
        (_, Net::Lan) => (LIME, "Live · LAN".into()),
        (_, Net::Internet) => (LIME, "Live · Internet".into()),
        (_, Net::Offline) => (YELLOW, "Searching LAN".into()),
        _ => (TEXT, "Searching".into()),
    };

    // The mark and wordmark open the about panel.
    if ui.get("about").clicked_with(Button::Primary) {
        view.about = !view.about;
        view.mic_panel = false;
    }
    let t = view.born.elapsed().as_secs_f64();
    let live = lit == LIME;
    let sources = shared.talkers.lock().unwrap().len();
    let talk = if picked == 1 {
        if ui.get("mics").clicked_with(Button::Primary) {
            view.mic_panel = !view.mic_panel;
            view.about = false;
        }
        row![
            icon(SPEAKER)
                .font(fonts.icons.clone())
                .text_size(12.0)
                .fill(if sources > 0 || view.mic_panel {
                    LIME
                } else {
                    DIM
                }),
            text(if sources > 0 {
                sources.to_string()
            } else {
                String::new()
            })
            .text_size(10.0)
            .fill(TEXT),
        ]
        .gap(2.0)
        .center()
        .h(H)
        .a11y(A11y::Button)
        .named("Audio source levels")
        .focusable()
        .id("mics")
    } else {
        spacer().w(0.0)
    };
    let brand = row![
        canvas(move |_| mark(t, live, pixel)).size(18.0, 18.0),
        text("RELAY")
            .font(if pixel {
                fonts.pixel_head.clone()
            } else {
                fonts.bold.clone()
            })
            .text_size(if pixel { 12.0 } else { 14.0 })
            .fill(TEXT),
    ]
    .gap(5.0)
    .center()
    .a11y(A11y::Button)
    .named("RELAY changelog and settings")
    .focusable()
    .id("about");
    let body = if view.about {
        about(ui, standard_ui, fonts)
    } else if picked == 1 && view.mic_panel {
        microphones(ui, shared, fonts, view)
    } else {
        col([
            col(rows).gap(4.0),
            spacer().grow(1.0),
            row![
                block(6.0, 6.0).radius(3.0).fill(lit),
                text(status).text_size(10.5).fill(TEXT),
                spacer().grow(1.0),
                tail,
            ]
            .gap(6.0)
            .center(),
        ])
        .gap(6.0)
        .grow(1.0)
    };
    let left = col([
        row![brand, spacer().grow(1.0), talk, mode]
            .gap(6.0)
            .center(),
        body,
    ])
    .gap(6.0)
    .min_w(0.0)
    .basis(0.0)
    .grow(1.0)
    .clip();

    let meters = meters(ui, bridge, shared, fonts, view);
    row([left, meters]).gap(12.0).pad(10.0).fill(BG)
}

/// One narrow channel per return source, mixed on the network thread.
fn microphones(ui: &mut Ui, shared: &Shared, fonts: &Fonts, view: &mut View) -> El {
    let mut talkers = shared.talkers.lock().unwrap();
    let now = Instant::now();
    let dt = now.duration_since(view.last).as_secs_f32().min(0.1);
    view.mic_rails
        .retain(|id, _| talkers.iter().any(|t| &t.id == id));
    let panel_w = ui
        .scene()
        .and_then(|s| s.surface("microphones"))
        .map_or(328.0, |s| s.frame.size.width);
    let strip_w = PAIR + NAME_RAIL + 2.0 + 8.0;
    let visible = ((panel_w + 4.0) / (strip_w + 4.0)).floor().max(1.0) as usize;
    let max_scroll = talkers.len().saturating_sub(visible);
    if let Some(wheel) = ui.wheel("microphones") {
        let delta = if wheel.y.abs() > wheel.x.abs() {
            wheel.y
        } else {
            wheel.x
        };
        if delta > 0.0 {
            view.mic_scroll = view.mic_scroll.saturating_add(1);
        } else if delta < 0.0 {
            view.mic_scroll = view.mic_scroll.saturating_sub(1);
        }
    }
    view.mic_scroll = view.mic_scroll.min(max_scroll);
    let strips: Vec<El> = talkers
        .iter_mut()
        .skip(view.mic_scroll)
        .take(visible)
        .map(|t| {
            let id = format!("mic-{}", t.id);
            let label = if t.name.is_empty() {
                format!("Listener {}", t.slot)
            } else {
                t.name.clone()
            };
            let mut gain = f64::from(t.gain_db);
            let extent = ui
                .scene()
                .and_then(|s| s.surface(&id))
                .map_or(40.0, |s| s.frame.size.height)
                .max(1.0);
            if ui.get(&id).pressed
                && let Some(p) = ui.local(&id)
            {
                let unit = 1.0 - p.y / extent;
                gain = GAIN.0 + unit.clamp(0.0, 1.0) * (GAIN.1 - GAIN.0);
            }
            ui.drag(&id, &mut gain, GAIN.0..=GAIN.1, extent, true);
            if ui.double_click(&id) {
                gain = 0.0;
            }
            if gain != f64::from(t.gain_db) {
                t.gain_db = gain as f32;
            }

            let mute_id = format!("mute-{}", t.id);
            if ui.get(&mute_id).clicked_with(Button::Primary) {
                t.muted = !t.muted;
            }
            let meter = view.mic_rails.entry(t.id.clone()).or_insert(
                [Rail {
                    shown: FLOOR,
                    hold: FLOOR,
                    held: now,
                }; 2],
            );
            for (rail, peak) in meter.iter_mut().zip(std::mem::take(&mut t.peak)) {
                rail.feed(peak, now, dt);
            }
            let handle = row([block(0.0, 3.0).grow(1.0).radius(1.5).fill(if t.muted {
                DIM
            } else if gain > 0.0 {
                RED
            } else {
                TEXT
            })])
            .pad(1.0)
            .radius(2.5)
            .fill(BG);
            let position = 1.0 - ((gain - GAIN.0) / (GAIN.1 - GAIN.0)) as f32;
            let level = if t.stereo {
                row![rail(meter[0], t.muted), rail(meter[1], t.muted)]
                    .gap(1.0)
                    .w(PAIR)
            } else {
                rail(meter[0], t.muted).w(PAIR)
            };
            let zero = at(
                1.0 / 3.0,
                block(PAIR, 1.0).fill(if t.muted { DIM } else { YELLOW }),
            );
            let meter = stack([level, zero, at(position, handle)])
                .w(PAIR)
                .h(Len::Pct(100.0));
            let meter = row([meter])
                .justify(Justify::Center)
                .w(PAIR)
                .h(Len::Pct(100.0))
                .a11y(A11y::Slider {
                    value: gain,
                    min: GAIN.0,
                    max: GAIN.1,
                })
                .named(format!("{} source gain, dB", label))
                .focusable()
                .id(id);

            let glyph = match (t.plugin, t.muted) {
                (false, false) => MIC,
                (false, true) => MIC_OFF,
                (true, false) => SPEAKER,
                (true, true) => SPEAKER_OFF,
            };
            let mute = centered_icon(ui, fonts, glyph, if t.muted { RED } else { LIME })
                .fill(if t.muted { HOT } else { WELL })
                .a11y(A11y::Button)
                .named(format!(
                    "{} {}",
                    if t.muted { "Unmute" } else { "Mute" },
                    label
                ))
                .focusable()
                .id(mute_id);
            let name = vertical_name(ui, fonts, &label, if t.muted { DIM } else { TEXT });
            let label_rail = col([mute, name]).gap(4.0).w(NAME_RAIL).h(Len::Pct(100.0));
            let strip = row![label_rail, meter].gap(2.0).pad(4.0);
            strip
                .fill(FIELD)
                .stroke(DIM.with_alpha(0.38))
                .stroke_width(1.0)
        })
        .collect();
    let body = if strips.is_empty() {
        text("Audio sources appear when peers connect.")
            .text_size(10.5)
            .fill(DIM)
            .grow(1.0)
    } else {
        row(strips)
            .gap(4.0)
            .align(Align::Stretch)
            .justify(Justify::Start)
            .grow(1.0)
    };
    body.id("microphones")
}

fn ink_box(path: &Path) -> Option<mui::geometry::Rect> {
    Some(mui::geometry::bez_path(path, 0.1).ok()?.bounding_box())
}

fn centered_icon(ui: &Ui, fonts: &Fonts, glyph: char, ink: Color) -> El {
    let (runs, font, glyph) = (ui.text_runs(), fonts.icons.clone(), glyph.to_string());
    canvas(move |size| {
        let Some(run) = runs.get(&font, &glyph, 12.0, &[]) else {
            return vec![];
        };
        let Some(b) = ink_box(&run.path) else {
            return vec![];
        };
        vec![Draw::fill(run.path.clone(), ink).at(Point::new(
            (size.width - b.width()) / 2.0 - b.x0,
            (size.height - b.height()) / 2.0 - b.y0,
        ))]
    })
    .size(NAME_RAIL, NAME_RAIL)
}

fn vertical_name(ui: &Ui, fonts: &Fonts, label: &str, ink: Color) -> El {
    let (runs, font, label) = (ui.text_runs(), fonts.departure.clone(), label.to_owned());
    canvas(move |size| {
        let mut shown = label.clone();
        let mut point_size = 9.0;
        loop {
            let text = if shown.len() == label.len() {
                shown.clone()
            } else {
                format!("{shown}…")
            };
            let Some(run) = runs.get(&font, &text, point_size, &[]) else {
                return vec![];
            };
            let Some(b) = ink_box(&run.path) else {
                return vec![];
            };
            if b.width() <= size.height - 8.0 && b.height() <= size.width - 4.0 {
                let x = (size.width - b.height()) / 2.0 - b.y0;
                let y = size.height - 4.0 + b.x0;
                let path = run
                    .path
                    .rigid_transform(mui::geometry::Vec2::new(x, y), -std::f64::consts::FRAC_PI_2);
                return path.map_or_else(|_| vec![], |p| vec![Draw::fill(p, ink)]);
            }
            if point_size > 7.0 {
                point_size -= 1.0;
            } else if shown.pop().is_none() {
                return vec![];
            }
        }
    })
    .w(NAME_RAIL)
    .grow(1.0)
    .clip()
}

/// IN and OUT pairs with a fader riding each, the scale between them,
/// peak readouts on top and mute controls below.
fn meters(
    ui: &mut Ui,
    bridge: &mut Bridge<RelayParams>,
    shared: &Shared,
    fonts: &Fonts,
    view: &mut View,
) -> El {
    let now = Instant::now();
    let dt = now.duration_since(view.last).as_secs_f32().min(0.1);
    view.last = now;
    let takes = [Peak::InL, Peak::InR, Peak::OutL, Peak::OutR].map(|p| shared.take_peak(p));
    for (rail, peak) in view.rails.iter_mut().zip(takes) {
        rail.feed(peak, now, dt);
    }
    view.max[0] = view.max[0].max(takes[0]).max(takes[1]);
    view.max[1] = view.max[1].max(takes[2]).max(takes[3]);

    // Readouts: max IN and OUT. A click clears them.
    if ui.get("peaks").clicked_with(Button::Primary) {
        view.max = [0.0; 2];
    }
    let muted = [
        bridge.value(P::MuteInput) >= 0.5,
        bridge.value(P::MuteOutput) >= 0.5,
    ];
    let [max_in, max_out] = [
        ("IN", view.max[0], muted[0]),
        ("OUT", view.max[1], muted[1]),
    ]
    .map(|(label, v, off)| {
        let ink = if off {
            DIM
        } else if v >= 1.0 {
            RED
        } else {
            TEXT
        };
        let value = if v < 1e-5 {
            minus_infinity(fonts, ink)
        } else {
            mono(fonts, format!("{:.1}", db(v)), 9.0, ink)
        };
        col([text(label).text_size(8.0).fill(DIM), value])
            .align(Align::Center)
            .w(PAIR)
    });
    let readouts = row![max_in, spacer().w(SCALE), max_out]
        .a11y(A11y::Button)
        .named("Reset peaks")
        .id("peaks");

    let [il, ir, ol, or] = view.rails;
    let pair = |a: Rail, b: Rail, off| row![rail(a, off), rail(b, off)].gap(1.0).w(PAIR);
    let input = fader(
        ui,
        bridge,
        P::Input,
        "Input",
        pair(il, ir, muted[0]),
        muted[0],
    );
    let output = fader(
        ui,
        bridge,
        P::Output,
        "Output",
        pair(ol, or, muted[1]),
        muted[1],
    );

    let scale = col(
        [(0, 6.0), (-6, 6.0), (-12, 12.0), (-24, 24.0), (-48, 12.0)].map(|(d, span)| {
            col([text(d.to_string()).text_size(7.5).fill(DIM)])
                .align(Align::Center)
                .grow(span)
        }),
    )
    .w(SCALE);

    let input_mute = mute(ui, bridge, P::MuteInput, "input");
    let output_mute = mute(ui, bridge, P::MuteOutput, "output");
    let thru = bridge.bind(ui, P::Passthrough, |ui, id, v| {
        if ui.get(&id).clicked_with(Button::Primary) {
            *v = 1.0 - *v;
        }
        row([text("THRU")
            .text_size(7.0)
            .fill(if *v >= 0.5 { LIME } else { DIM })])
        .justify(Justify::Center)
        .center()
        .size(SCALE, 16.0)
        .fill(FIELD)
        .stroke(DIM.with_alpha(0.38))
        .stroke_width(1.0)
        .a11y(A11y::Toggle { on: *v >= 0.5 })
        .named("Pass local input to output")
        .focusable()
        .id(id)
    });
    col([
        readouts,
        row![input, scale, output].grow(1.0),
        row![input_mute, thru, output_mute],
    ])
    .gap(4.0)
    .w(PAIR * 2.0 + SCALE)
    .h(Len::Pct(100.0))
    .id("meters")
}

fn mute(ui: &mut Ui, bridge: &mut Bridge<RelayParams>, param: P, name: &'static str) -> El {
    bridge.bind(ui, param, |ui, id, v| {
        if ui.get(&id).clicked_with(Button::Primary) {
            *v = 1.0 - *v;
        }
        let muted = *v >= 0.5;
        row([text("MUTE")
            .text_size(7.0)
            .fill(if muted { BG } else { DIM })])
        .justify(Justify::Center)
        .center()
        .size(PAIR, 16.0)
        .fill(if muted { RED } else { FIELD })
        .stroke(if muted { RED } else { DIM.with_alpha(0.38) })
        .stroke_width(1.0)
        .a11y(A11y::Toggle { on: muted })
        .named(format!("Mute {name}"))
        .focusable()
        .id(id)
    })
}

/// A gain fader over a meter pair: drag anywhere on it, double-click for
/// 0 dB.
fn fader(
    ui: &mut Ui,
    bridge: &mut Bridge<RelayParams>,
    param: P,
    label: &'static str,
    meter: El,
    muted: bool,
) -> El {
    bridge.bind(ui, param, |ui, id, v| {
        let h = ui
            .scene()
            .and_then(|s| s.surface(&id))
            .map_or(100.0, |s| s.frame.size.height);
        ui.drag(&id, v, 0.0..=1.0, h, true);
        if ui.double_click(&id) {
            *v = -GAIN.0 / (GAIN.1 - GAIN.0);
        }
        let handle =
            row([block(0.0, 3.0)
                .grow(1.0)
                .radius(1.5)
                .fill(if muted { DIM } else { TEXT })])
            .pad(1.0)
            .radius(2.5)
            .fill(BG);
        stack([meter, at(1.0 - *v as f32, handle)])
            .a11y(A11y::Slider {
                value: *v,
                min: 0.0,
                max: 1.0,
            })
            .named(label)
            .focusable()
            .id(id)
    })
}

/// "−∞" for a readout that has seen nothing. The fonts have no ∞, so it
/// is drawn: a lemniscate the height of a digit.
fn minus_infinity(fonts: &Fonts, ink: Color) -> El {
    let loop_ = canvas(move |_| {
        let curve = (0..24).map(|i| {
            let t = f64::from(i) * std::f64::consts::TAU / 24.0;
            let d = 1.0 + t.sin().powi(2);
            Point::new(5.0 + 4.2 * t.cos() / d, 5.5 + 4.2 * t.sin() * t.cos() / d)
        });
        vec![Draw::stroke(Path::polyline(curve, true), ink, 1.1)]
    })
    .size(10.0, 11.0);
    row![mono(fonts, "\u{2212}".into(), 9.0, ink), loop_].center()
}

/// The RELAY mark, a dot sending two chevrons. While live the chevrons
/// ripple out of the dot on a damped spring, one after the other.
fn mark(t: f64, live: bool, pixel: bool) -> Vec<Draw> {
    let ink = if live { LIME } else { TEXT };
    let snap = |v: f64| if pixel { v.round() } else { v };
    let kick = |delay: f64| {
        let s = (t % 1.8 - delay).max(0.0);
        if live {
            (-5.0 * s).exp() * (11.0 * s).sin()
        } else {
            0.0
        }
    };
    let dot = 1.7 * (1.0 + 0.3 * kick(0.0));
    let circle = (0..16).map(|i| {
        let a = f64::from(i) * std::f64::consts::TAU / 16.0;
        Point::new(snap(4.3 + dot * a.cos()), snap(9.0 + dot * a.sin()))
    });
    let mut shapes = vec![Draw::fill(Path::polyline(circle, true), ink)];
    for (i, x) in [7.6, 11.9].into_iter().enumerate() {
        let k = kick(0.08 + 0.1 * i as f64);
        let x = x + 1.6 * k;
        let chevron =
            [(x, 4.6), (x + 2.5, 9.0), (x, 13.4)].map(|(x, y)| Point::new(snap(x), snap(y)));
        let fade = ink.with_alpha(1.0 - 0.35 * k.abs() as f32);
        shapes.push(Draw::stroke(Path::polyline(chevron, false), fade, 2.3));
    }
    shapes
}

/// Changelog and the editor's saved appearance preference.
fn about(ui: &Ui, standard_ui: &RwLock<bool>, fonts: &Fonts) -> El {
    const CHANGELOG: &str = include_str!("../../CHANGELOG.md");
    const FULL: &str = "https://github.com/Matari-Audio/relay/blob/main/CHANGELOG.md";
    let notes: Vec<El> = CHANGELOG
        .split("\n## ")
        .nth(1)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.strip_prefix("- "))
        .enumerate()
        .map(|(i, l)| {
            text(format!("·  {l}"))
                .text_size(10.0)
                .fill(DIM)
                .w(Len::Pct(100.0))
                .id(format!("note-{i}"))
        })
        .collect();
    let r = ui.get("changelog");
    if r.clicked_with(Button::Primary) {
        open(FULL);
    }
    let standard = *standard_ui.read().unwrap();
    let choice = |name: &'static str, selected: bool| {
        let r = ui.get(name);
        if r.clicked_with(Button::Primary) {
            *standard_ui.write().unwrap() = name == "standard-ui";
        }
        row([text(if name == "pixel-ui" {
            "Pixel"
        } else {
            "Standard"
        })
        .font(if fonts.pixel.get() {
            fonts.pixel_head.clone()
        } else {
            fonts.semibold.clone()
        })
        .text_size(if fonts.pixel.get() { 9.0 } else { 10.0 })
        .fill(if selected { ON_LIME } else { TEXT })])
        .justify(Justify::Center)
        .center()
        .size(if name == "pixel-ui" { 52.0 } else { 70.0 }, 19.0)
        .radius(corner(fonts))
        .fill(if selected {
            LIME
        } else if r.hovered {
            HOT
        } else {
            BG
        })
        .a11y(A11y::Button)
        .named(if name == "pixel-ui" {
            "Pixel appearance"
        } else {
            "Standard appearance"
        })
        .focusable()
        .id(name)
    };
    col([
        row![
            text(format!("Version {}", env!("CARGO_PKG_VERSION")))
                .text_size(11.0)
                .fill(TEXT)
        ]
        .w(Len::Pct(100.0)),
        row![
            text("Appearance").text_size(10.0).fill(DIM),
            spacer().grow(1.0),
            choice("pixel-ui", !standard),
            choice("standard-ui", standard),
        ]
        .gap(3.0)
        .center()
        .w(Len::Pct(100.0)),
        // Scrolls when the notes outgrow the window; the link stays below.
        col(notes)
            .gap(2.0)
            .align(Align::Start)
            .w(Len::Pct(100.0))
            .scroll()
            .grow(1.0)
            .id("notes"),
        text("Full changelog")
            .text_size(10.0)
            .fill(if r.hovered { LIME } else { TEXT })
            .a11y(A11y::Button)
            .named("Open the full changelog")
            .focusable()
            .id("changelog"),
    ])
    .gap(6.0)
    .align(Align::Start)
    .pad(8.0)
    .radius(corner(fonts))
    .fill(FIELD)
    .w(Len::Pct(100.0))
    .grow(1.0)
}

/// Opens `url` in the default browser.
fn open(url: &str) {
    #[cfg(target_os = "windows")]
    let (cmd, args) = ("cmd", ["/C", "start", ""].as_slice());
    #[cfg(target_os = "macos")]
    let (cmd, args): (_, &[&str]) = ("open", &[]);
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let (cmd, args): (_, &[&str]) = ("xdg-open", &[]);
    let _ = std::process::Command::new(cmd).args(args).arg(url).spawn();
}

/// `el` placed `t` of the way down a full-height column.
fn at(t: f32, el: El) -> El {
    col([
        spacer().grow(f64::from(t)),
        el,
        spacer().grow(f64::from(1.0 - t)),
    ])
    .w(Len::Pct(100.0))
    .h(Len::Pct(100.0))
}

/// One rail: a single red-yellow-green ramp, dark above the level, with a
/// thin peak-hold line.
fn rail(r: Rail, muted: bool) -> El {
    let cover = depth(r.shown);
    let ramp = RAMP.map(|(at, color)| (at, if muted { DIM } else { color }));
    let edge = ramp.windows(2).find(|w| cover <= w[1].0).map_or(LIME, |w| {
        w[0].1.mix(w[1].1, (cover - w[0].0) / (w[1].0 - w[0].0))
    });
    let stops = [(0.0, WELL), (cover, WELL), (cover, edge)]
        .into_iter()
        .chain(ramp.into_iter().filter(|s| s.0 > cover));
    let bar = block(0.0, 0.0)
        .w(Len::Pct(100.0))
        .h(Len::Pct(100.0))
        .radius(2.0)
        .fill(Gradient::linear(180.0, stops));
    let hold = block(0.0, 1.5)
        .w(Len::Pct(100.0))
        .fill(if muted { DIM } else { TEXT });
    let mut layers = vec![bar];
    if r.hold > FLOOR {
        layers.push(at(depth(r.hold), hold));
    }
    stack(layers).grow(1.0)
}

/// A text input that shows `•` for every character. Typing and deleting go
/// through: what changed among the dots is spliced into `value`.
fn masked_input(ui: &mut Ui, id: &str, value: &mut String) -> Response {
    let old: Vec<char> = value.chars().collect();
    let mut shown = "•".repeat(old.len());
    let Response { el, changed } = text_input(ui, id, &mut shown);
    if changed {
        *value = unmask(&old, &shown);
    }
    Response { el, changed }
}

/// `old` after the edit that turned its dots into `shown`.
fn unmask(old: &[char], shown: &str) -> String {
    let new: Vec<char> = shown.chars().collect();
    let head = new.iter().take_while(|c| **c == '•').count().min(old.len());
    let tail = new[head..]
        .iter()
        .rev()
        .take_while(|c| **c == '•')
        .count()
        .min(old.len() - head);
    old[..head]
        .iter()
        .chain(&new[head..new.len() - tail])
        .chain(&old[old.len() - tail..])
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mark_keeps_dot_and_two_moving_chevrons() {
        let still = mark(0.0, true, true);
        let moving = mark(0.3, true, true);
        assert_eq!(still.len(), 3);
        assert_ne!(format!("{still:?}"), format!("{moving:?}"));
    }

    #[test]
    fn unmask_splices_edits() {
        let old: Vec<char> = "secret".chars().collect();
        assert_eq!(unmask(&old, "••••••x"), "secretx");
        assert_eq!(unmask(&old, "•••••"), "secre");
        assert_eq!(unmask(&old, "••X••••"), "seXcret");
        assert_eq!(unmask(&old, "ab"), "ab");
        assert_eq!(unmask(&[], "pw"), "pw");
    }
}

/// Renders the editor to `$TMPDIR/relay-editor-<mode>.png` on the CPU so it
/// can be looked at without a DAW.
#[cfg(test)]
mod snapshot {
    use super::*;
    use moose_params::Params;
    use mui::vello::vello_cpu::{Pixmap, RenderContext, Resources};
    use mui::vello::{Cache, Cpu};

    fn render(mode: f64, net: Net, name: &str, about: bool) {
        let params = Arc::new(RelayParams::new());
        params.set_normalized(P::Mode.into(), mode);
        if name.ends_with("muted") {
            params.set_normalized(P::MuteInput.into(), 1.0);
            params.set_normalized(P::MuteOutput.into(), 1.0);
        }
        *params.standard_ui.write().unwrap() = name == "standard";
        let settings = Arc::clone(&params);
        let shared = Arc::clone(&params.link.0);
        shared.set_text(&shared.room, "quiet-dusty-papaya");
        shared.set_text(&shared.password, "hunter2");
        shared.set_text(&shared.address, "192.168.1.20");
        shared.set_net(net);
        shared.peers.store(2, Relaxed);
        shared.rate.store(48_000, Relaxed);
        shared.latency.store(512, Relaxed);
        shared.talking.store(1, Relaxed);
        if name.starts_with("mics") || name.starts_with("compact-mics") {
            shared.talkers.lock().unwrap().extend([
                relay_core::Talker {
                    id: "1".into(),
                    slot: 1,
                    name: "Maya Chen".into(),
                    plugin: false,
                    stereo: false,
                    active: true,
                    peak: [0.16, 0.1],
                    gain_db: 3.0,
                    muted: false,
                },
                relay_core::Talker {
                    id: "2".into(),
                    slot: 2,
                    name: "Jonah".into(),
                    plugin: false,
                    stereo: false,
                    active: false,
                    peak: [0.0, 0.0],
                    gain_db: -6.0,
                    muted: false,
                },
                relay_core::Talker {
                    id: "3".into(),
                    slot: 3,
                    name: "Plugin 2".into(),
                    plugin: true,
                    stereo: true,
                    active: true,
                    peak: [0.04, 0.08],
                    gain_db: 0.0,
                    muted: true,
                },
            ]);
        }
        shared.bitrate.store(312_000, Relaxed);
        let mut bridge = Bridge::new(params);
        let (mut ui, fonts) = new_ui();
        for (p, v) in [
            (Peak::InL, 0.7),
            (Peak::InR, 0.5),
            (Peak::OutL, 0.2),
            (Peak::OutR, 1.02),
        ] {
            // Join shows an empty IN: the "−∞" readout.
            if name == "join" && matches!(p, Peak::InL | Peak::InR) {
                continue;
            }
            shared.note_peak(p, v);
        }
        let mut view = View {
            about,
            mic_panel: name.starts_with("mics") || name.starts_with("compact-mics"),
            ..View::default()
        };
        let logical = if name == "share-default" {
            (616, 218)
        } else if name.starts_with("compact-mics") {
            (380, 150)
        } else if name.contains("large") {
            (792, 432)
        } else {
            SIZE
        };
        let (w, h) = (logical.0 * 2, logical.1 * 2);
        ui.set_scale(Some(2.0));
        let size = mui::layout::Size::new(f64::from(logical.0), f64::from(logical.1));
        for _ in 0..3 {
            let root = build(
                &mut ui,
                &mut bridge,
                &shared,
                &settings.standard_ui,
                &fonts,
                &mut view,
            );
            ui.frame(root, Some(size), mui::input::Input::default(), 0.0)
                .unwrap();
        }
        assert!(
            ui.scene().unwrap().layout.frame("meters").unwrap().right() <= f64::from(logical.0)
        );
        if about {
            assert!(
                ui.scene()
                    .unwrap()
                    .layout
                    .frame("note-0")
                    .unwrap()
                    .size
                    .height
                    > 15.0
            );
        }
        if name.starts_with("mics") || name.starts_with("compact-mics") {
            assert!(ui.scene().unwrap().layout.frame("mic-1").is_some());
            assert!(ui.scene().unwrap().layout.frame("mute-1").is_some());
            if name.starts_with("compact-mics") {
                assert!(
                    ui.scene()
                        .unwrap()
                        .layout
                        .frame("mic-1")
                        .unwrap()
                        .size
                        .height
                        >= 38.0
                );
            }
        }
        let mut ctx = RenderContext::new(w as u16, h as u16);
        let mut resources = Resources::default();
        let cpu = &mut Cpu {
            ctx: &mut ctx,
            resources: &mut resources,
            cache: &mut Cache::default(),
        };
        mui::vello::paint(
            cpu,
            ui.scene().unwrap(),
            mui::vello::kurbo::Affine::scale(2.0),
        )
        .unwrap();
        ctx.flush();
        let mut pix = Pixmap::new(w as u16, h as u16);
        ctx.render(&mut pix, &mut resources);
        let rgba: Vec<u8> = pix
            .take_unpremultiplied()
            .iter()
            .flat_map(|p| [p.r, p.g, p.b, p.a])
            .collect();
        let path = std::env::temp_dir().join(format!("relay-editor-{name}.png"));
        let mut enc = png::Encoder::new(std::fs::File::create(path).unwrap(), w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.write_header().unwrap().write_image_data(&rgba).unwrap();
    }

    #[test]
    fn editor_renders() {
        render(0.5, Net::Internet, "share", false);
        render(0.5, Net::Internet, "share-default", false);
        render(0.5, Net::Internet, "standard", false);
        render(1.0, Net::Lan, "join", false);
        render(0.5, Net::Internet, "about", true);
        render(0.5, Net::Internet, "mics-large", false);
        render(0.5, Net::Internet, "compact-mics", false);
        render(0.5, Net::Internet, "compact-mics-muted", false);
    }

    #[test]
    fn wheel_moves_overflowing_listener_strips_sideways() {
        let shared = Shared::default();
        shared
            .talkers
            .lock()
            .unwrap()
            .extend((0..6).map(|n| relay_core::Talker {
                id: n.to_string(),
                slot: n + 1,
                name: format!("Listener {n}"),
                plugin: false,
                stereo: false,
                active: true,
                peak: [0.0, 0.0],
                gain_db: 0.0,
                muted: false,
            }));
        let (mut ui, fonts) = new_ui();
        let mut view = View::default();
        let size = mui::layout::Size::new(250.0, 100.0);
        let mut draw = |input| {
            let root = microphones(&mut ui, &shared, &fonts, &mut view);
            ui.frame(root, Some(size), input, 0.016).unwrap();
        };
        draw(mui::input::Input::default());
        draw(mui::input::Input {
            pointer: mui::input::PointerInput {
                pos: Some(Point::new(20.0, 30.0)),
                ..Default::default()
            },
            wheel: mui::geometry::Vec2::new(0.0, 40.0),
            ..Default::default()
        });
        draw(mui::input::Input::default());
        assert_eq!(view.mic_scroll, 1);
        assert!(ui.scene().unwrap().surface("mic-3").is_some());
        assert!(ui.scene().unwrap().surface("mic-0").is_none());
    }
}
