//! The editor: a compact charcoal panel. Room, password and link fields
//! with their actions inside them on the left; input and output meters,
//! each with its own fader, on the right.

use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Instant;

use mui::prelude::*;
use mui_truce::{Bridge, MuiEditor};
use relay_core::{Net, Peak, Shared};
use truce_core::editor::{Editor, IntoEditor};

use crate::{P, RelayParams};

const SEMIBOLD: &[u8] = include_bytes!("../assets/barlow-600.ttf");
const BOLD: &[u8] = include_bytes!("../assets/barlow-700.ttf");
const MONO: &[u8] = include_bytes!("../assets/martian-mono.ttf");
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

// Neutral charcoal, Oklch. apps/relay-web and apps/web use the same values.
const BG: Color = Color::oklch(0.182, 0.0, 0.0); // #121212
const FIELD: Color = Color::oklch(0.235, 0.0, 0.0); // #1e1e1e
const HOT: Color = Color::oklch(0.281, 0.0, 0.0); // #292929
const WELL: Color = Color::oklch(0.145, 0.0, 0.0); // #0a0a0a
const TEXT: Color = Color::oklch(0.961, 0.0, 0.0); // #f2f2f2
const DIM: Color = Color::oklch(0.64, 0.0, 0.0); // #8c8c8c
const GREEN: Color = Color::oklch(0.795, 0.214, 149.6); // #1fe06a, also the accent
const ON_GREEN: Color = Color::oklch(0.173, 0.032, 156.0); // #04140a
const YELLOW: Color = Color::oklch(0.877, 0.176, 92.7); // #ffd21a
const RED: Color = Color::oklch(0.647, 0.239, 22.0); // #ff2d46
/// A meter's colour down its height, 0 dB at the top.
const RAMP: [(f32, Color); 4] = [(0.0, RED), (0.15, YELLOW), (0.35, GREEN), (1.0, GREEN)];

const THEME: Theme = Theme {
    palette: Palette {
        primary: Pigment::new(150.0, 0.2),
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

struct Fonts {
    bold: Font,
    mono: Font,
    icons: Font,
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
    /// When the editor opened: the clock for the mark's motion.
    born: Instant,
    show_password: bool,
    show_address: bool,
    copied: bool,
    /// IN L, IN R, OUT L, OUT R.
    rails: [Rail; 4],
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
            born: now,
            show_password: false,
            show_address: false,
            copied: false,
            rails: [rail; 4],
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
        icons: load(ICONS),
    };
    (Ui::new(THEME).font(load(SEMIBOLD)), fonts)
}

pub fn editor(params: Arc<RelayParams>) -> Box<dyn Editor> {
    let shared = Arc::clone(&params.link.0);
    let (ui, fonts) = new_ui();
    let mut view = View::default();
    MuiEditor::new(params, ui, SIZE, move |ui, bridge| {
        build(ui, bridge, &shared, &fonts, &mut view)
    })
    .resizable((380, 150))
    .into_editor()
}

/// An icon centred in a field-high square, so its inset is the same on
/// every side.
fn glyph(fonts: &Fonts, c: char, ink: Color) -> El {
    row([icon(fonts.icons.clone(), c).text_size(12.0).fill(ink)])
        .justify(Justify::Center)
        .center()
        .size(H, H)
}

/// An icon button at a field's right end.
fn action(ui: &Ui, fonts: &Fonts, id: &'static str, c: char, label: &str) -> (El, bool) {
    let r = ui.get(id);
    let el = glyph(fonts, c, if r.hovered { TEXT } else { DIM })
        .role(Kind::Button)
        .label(label)
        .focusable()
        .id(id);
    (el, r.clicked_with(Button::Primary))
}

/// A field: leading icon, the body, then its action, all in one box.
fn field(fonts: &Fonts, c: char, body: El, action: El) -> El {
    row([glyph(fonts, c, DIM), body.grow(1.0), action])
        .center()
        .h(H)
        .radius(R)
        .fill(FIELD)
}

/// A text input with its own chrome stripped, to sit in a [`field`].
fn bare(input: El) -> El {
    input.fill(FIELD).radius(0.0).pad_xy(0.0, 0.0).h(H - 4.0)
}

fn mono(fonts: &Fonts, s: String, size: f64, ink: Color) -> El {
    text(s).font(fonts.mono.clone()).text_size(size).fill(ink)
}

fn build(
    ui: &mut Ui,
    bridge: &mut Bridge<RelayParams>,
    shared: &Shared,
    fonts: &Fonts,
    view: &mut View,
) -> El {
    let picked = (bridge.value(P::Mode) * 2.0).round() as usize;
    let mode = bridge.bind(ui, "mode", P::Mode, |ui, v| {
        let segments: Vec<El> = ["Off", "Share", "Join"]
            .into_iter()
            .enumerate()
            .map(|(i, name)| {
                let r = ui.get(name);
                if r.clicked_with(Button::Primary) {
                    *v = i as f64 / 2.0;
                }
                let (bg, fg) = match (i == picked, r.hovered) {
                    (true, _) => (GREEN, ON_GREEN),
                    (false, true) => (HOT, TEXT),
                    (false, false) => (FIELD, DIM),
                };
                row([text(name).text_size(10.5).fill(fg)])
                    .justify(Justify::Center)
                    .center()
                    .size(42.0, 18.0)
                    .radius(R - 1.0)
                    .fill(bg)
                    .role(Kind::Button)
                    .label(name)
                    .focusable()
                    .id(name)
            })
            .collect();
        row(segments).gap(2.0).pad(2.0).radius(R).fill(FIELD)
    });

    let mut room = Shared::text(&shared.room);
    let (input, changed) = text_input(ui, "room", &mut room);
    if changed {
        shared.set_text(&shared.room, &room);
    }
    let (dice, roll) = action(ui, fonts, "roll", DICE, "New room name");
    if roll {
        shared.set_text(&shared.room, &relay_core::room_name());
    }
    let mut rows = vec![field(fonts, HASH, bare(input), dice)];

    let mut password = Shared::text(&shared.password);
    let show = view.show_password;
    let (input, changed) = if show {
        text_input(ui, "pass", &mut password)
    } else {
        masked_input(ui, "pass", &mut password)
    };
    if changed {
        shared.set_text(&shared.password, &password);
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
        let link = row![
            text(format!("{}/", relay_core::SITE))
                .text_size(11.0)
                .fill(DIM),
            text(slug)
                .text_size(11.0)
                .fill(if r.hovered { GREEN } else { TEXT }),
        ]
        .role(Kind::Button)
        .label("Open the listen page")
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
        let r = ui.get("lan");
        view.show_address ^= r.clicked_with(Button::Primary);
        row![
            glyph(fonts, WIFI, DIM),
            mono(fonts, address, 9.0, if r.hovered { TEXT } else { DIM }),
        ]
        .center()
        .role(Kind::Button)
        .label("LAN address")
        .focusable()
        .id("lan")
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
        (1, _) => (GREEN, format!("{peers} listening")),
        (_, Net::Lan) => (GREEN, "Live · LAN".into()),
        (_, Net::Internet) => (GREEN, "Live · Internet".into()),
        (_, Net::Offline) => (YELLOW, "Searching LAN".into()),
        _ => (TEXT, "Searching".into()),
    };

    // The mark and wordmark open the about panel.
    view.about ^= ui.get("about").clicked_with(Button::Primary);
    let t = view.born.elapsed().as_secs_f64();
    let live = lit == GREEN;
    let brand = row![
        canvas(move |_| mark(t, live)).size(18.0, 18.0),
        text("RELAY")
            .font(fonts.bold.clone())
            .text_size(14.0)
            .fill(TEXT),
    ]
    .gap(5.0)
    .center()
    .role(Kind::Button)
    .label("About RELAY")
    .focusable()
    .id("about");
    let body = if view.about {
        about(ui)
    } else {
        column([
            column(rows).gap(4.0),
            spacer().grow(1.0),
            row![
                leaf(6.0, 6.0).radius(3.0).fill(lit),
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
    let left = column([row![brand, spacer().grow(1.0), mode].center(), body])
        .gap(6.0)
        .grow(1.0);

    let meters = meters(ui, bridge, shared, fonts, view);
    row([left, meters]).gap(12.0).pad(10.0).fill(BG)
}

/// IN and OUT pairs with a fader riding each, the scale between them,
/// max-peak readouts on top and the gains below.
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
    let [max_in, max_out] = [("IN", view.max[0]), ("OUT", view.max[1])].map(|(label, v)| {
        let ink = if v >= 1.0 { RED } else { TEXT };
        let value = if v < 1e-5 {
            minus_infinity(fonts, ink)
        } else {
            mono(fonts, format!("{:.1}", db(v)), 9.0, ink)
        };
        column([text(label).text_size(8.0).fill(DIM), value])
            .align(Align::Center)
            .w(PAIR)
    });
    let readouts = row![max_in, spacer().w(SCALE), max_out]
        .role(Kind::Button)
        .label("Reset peaks")
        .id("peaks");

    let [il, ir, ol, or] = view.rails;
    let pair = |a: Rail, b: Rail| row![rail(a), rail(b)].gap(1.0).w(PAIR);
    let input = fader(ui, bridge, "in", P::Input, "Input", pair(il, ir));
    let output = fader(ui, bridge, "out", P::Output, "Output", pair(ol, or));

    let scale = column(
        [(0, 6.0), (-6, 6.0), (-12, 12.0), (-24, 24.0), (-48, 12.0)].map(|(d, span)| {
            column([text(d.to_string()).text_size(7.5).fill(DIM)])
                .align(Align::Center)
                .grow(span)
        }),
    )
    .w(SCALE);

    let under = |p: P| {
        let db = GAIN.0 + bridge.value(p) * (GAIN.1 - GAIN.0);
        column([mono(fonts, format!("{db:+.1}"), 8.0, TEXT)])
            .align(Align::Center)
            .w(PAIR)
    };
    column([
        readouts,
        row![input, scale, output].grow(1.0),
        row![
            under(P::Input),
            spacer().w(SCALE),
            under(P::Output)
        ],
    ])
    .gap(4.0)
    .w(PAIR * 2.0 + SCALE)
    .h(Len::Pct(100.0))
}

/// A gain fader over a meter pair: drag anywhere on it, double-click for
/// 0 dB.
fn fader(
    ui: &mut Ui,
    bridge: &mut Bridge<RelayParams>,
    id: &'static str,
    param: P,
    label: &'static str,
    meter: El,
) -> El {
    bridge.bind(ui, id, param, |ui, v| {
        let h = ui
            .scene()
            .and_then(|s| s.surface(id))
            .map_or(100.0, |s| s.frame.size.height);
        ui.drag(id, v, 0.0..=1.0, h, true);
        if ui.double_click(id) {
            *v = -GAIN.0 / (GAIN.1 - GAIN.0);
        }
        let handle = row([leaf(0.0, 3.0).grow(1.0).radius(1.5).fill(TEXT)])
            .pad(1.0)
            .radius(2.5)
            .fill(BG);
        overlay([meter, at(1.0 - *v as f32, handle)])
            .role(Kind::Slider {
                value: *v,
                min: 0.0,
                max: 1.0,
            })
            .label(label)
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
fn mark(t: f64, live: bool) -> Vec<Draw> {
    let ink = if live { GREEN } else { TEXT };
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
        Point::new(4.3 + dot * a.cos(), 9.0 + dot * a.sin())
    });
    let mut shapes = vec![Draw::fill(Path::polyline(circle, true), ink)];
    for (i, x) in [7.6, 11.9].into_iter().enumerate() {
        let k = kick(0.08 + 0.1 * i as f64);
        let x = x + 1.6 * k;
        let chevron = [(x, 4.6), (x + 2.5, 9.0), (x, 13.4)].map(|(x, y)| Point::new(x, y));
        let fade = ink.with_alpha(1.0 - 0.35 * k.abs() as f32);
        shapes.push(Draw::stroke(Path::polyline(chevron, false), fade, 2.3));
    }
    shapes
}

/// Version, licence and the newest changelog entries.
fn about(ui: &Ui) -> El {
    const CHANGELOG: &str = include_str!("../../CHANGELOG.md");
    const FULL: &str = "https://github.com/Matari-Audio/relay/blob/main/CHANGELOG.md";
    let notes: Vec<El> = CHANGELOG
        .split("\n## ")
        .nth(1)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.strip_prefix("- "))
        .map(|l| text(format!("·  {l}")).text_size(10.0).fill(DIM))
        .collect();
    let r = ui.get("changelog");
    if r.clicked_with(Button::Primary) {
        open(FULL);
    }
    column([
        row![
            text(format!("Version {}", env!("CARGO_PKG_VERSION")))
                .text_size(11.0)
                .fill(TEXT),
            spacer().grow(1.0),
            text("Matari Audio · MPL-2.0").text_size(10.0).fill(DIM),
        ]
        .w(Len::Pct(100.0)),
        column(notes).gap(2.0).align(Align::Start),
        spacer().grow(1.0),
        text("Full changelog")
            .text_size(10.0)
            .fill(if r.hovered { GREEN } else { TEXT })
            .role(Kind::Button)
            .label("Open the full changelog")
            .focusable()
            .id("changelog"),
    ])
    .gap(6.0)
    .align(Align::Start)
    .pad(8.0)
    .radius(R)
    .fill(FIELD)
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
    column([
        spacer().grow(f64::from(t)),
        el,
        spacer().grow(f64::from(1.0 - t)),
    ])
    .w(Len::Pct(100.0))
    .h(Len::Pct(100.0))
}

/// One rail: a single red-yellow-green ramp, dark above the level, with a
/// thin peak-hold line.
fn rail(r: Rail) -> El {
    let cover = depth(r.shown);
    let edge = RAMP
        .windows(2)
        .find(|w| cover <= w[1].0)
        .map_or(GREEN, |w| {
            w[0].1.mix(w[1].1, (cover - w[0].0) / (w[1].0 - w[0].0))
        });
    let stops = [(0.0, WELL), (cover, WELL), (cover, edge)]
        .into_iter()
        .chain(RAMP.into_iter().filter(|s| s.0 > cover));
    let bar = leaf(0.0, 0.0)
        .w(Len::Pct(100.0))
        .h(Len::Pct(100.0))
        .radius(2.0)
        .fill(Gradient::linear(180.0, stops));
    let hold = leaf(0.0, 1.5).w(Len::Pct(100.0)).fill(TEXT);
    let mut layers = vec![bar];
    if r.hold > FLOOR {
        layers.push(at(depth(r.hold), hold));
    }
    overlay(layers).grow(1.0)
}

/// A text input that shows `•` for every character. Typing and deleting go
/// through: what changed among the dots is spliced into `value`.
fn masked_input(ui: &mut Ui, id: &str, value: &mut String) -> (El, bool) {
    let old: Vec<char> = value.chars().collect();
    let mut shown = "•".repeat(old.len());
    let (el, changed) = text_input(ui, id, &mut shown);
    if changed {
        *value = unmask(&old, &shown);
    }
    (el, changed)
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
    use mui::vello::vello_cpu::{Pixmap, RenderContext, Resources};
    use mui::vello::{Cache, Cpu};
    use truce_params::Params;

    fn render(mode: f64, net: Net, name: &str, about: bool) {
        let params = Arc::new(RelayParams::new());
        params.set_normalized(P::Mode.into(), mode);
        let shared = Arc::clone(&params.link.0);
        shared.set_text(&shared.room, "quiet-dusty-papaya");
        shared.set_text(&shared.password, "hunter2");
        shared.set_text(&shared.address, "192.168.1.20");
        shared.set_net(net);
        shared.peers.store(2, Relaxed);
        shared.rate.store(48_000, Relaxed);
        shared.latency.store(512, Relaxed);
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
            ..View::default()
        };
        let (w, h) = (SIZE.0 * 2, SIZE.1 * 2);
        ui.scale = Some(2.0);
        let root = build(&mut ui, &mut bridge, &shared, &fonts, &mut view);
        let size = mui::layout::Size::new(f64::from(SIZE.0), f64::from(SIZE.1));
        ui.frame(root, Some(size), mui::input::Input::default(), 0.0)
            .unwrap();
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
        render(1.0, Net::Lan, "join", false);
        render(0.5, Net::Internet, "about", true);
    }
}
