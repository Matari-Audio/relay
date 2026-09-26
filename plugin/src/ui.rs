//! The editor: a compact charcoal panel. Room, password and link fields
//! with their actions inside them on the left; input and output meters with
//! the output fader on the right, like a mastering limiter.

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
/// Output fader range, dB, as the param declares it.
const GAIN: (f64, f64) = (-24.0, 12.0);
/// Width of an L/R meter pair and of the scale between pairs.
const PAIR: f64 = 28.0;
const SCALE: f64 = 24.0;

/// sRGB from `0xRRGGBB`.
fn hex(c: u32) -> Color {
    let ch = |s: u32| ((c >> s) & 0xff) as f32 / 255.0;
    Color::srgb(ch(16), ch(8), ch(0))
}

/// apps/relay-web and apps/web use the same values.
mod ink {
    use super::{Color, hex};
    pub fn bg() -> Color {
        hex(0x0e1014)
    }
    pub fn field() -> Color {
        hex(0x1b1f27)
    }
    pub fn hot() -> Color {
        hex(0x262b36)
    }
    pub fn well() -> Color {
        hex(0x07080a)
    }
    pub fn text() -> Color {
        hex(0xf4f6fa)
    }
    pub fn dim() -> Color {
        hex(0x8b93a4)
    }
    /// Selection, the fader, focus.
    pub fn accent() -> Color {
        hex(0xff6b1a)
    }
    pub fn on_accent() -> Color {
        hex(0x140700)
    }
    pub fn green() -> Color {
        hex(0x1fe06a)
    }
    pub fn yellow() -> Color {
        hex(0xffd21a)
    }
    pub fn red() -> Color {
        hex(0xff2d46)
    }
}

const THEME: Theme = Theme {
    palette: Palette {
        primary: Pigment::new(45.0, 0.2),
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

/// One meter rail's ballistics.
#[derive(Clone, Copy)]
struct Rail {
    /// What is drawn, dB: jumps up, falls at 24 dB/s.
    shown: f32,
    /// Peak hold, dB, and when it was set.
    hold: f32,
    held: Instant,
}

impl Rail {
    fn new(now: Instant) -> Self {
        Self {
            shown: FLOOR,
            hold: FLOOR,
            held: now,
        }
    }

    fn feed(&mut self, peak: f32, now: Instant, dt: f32) {
        let db = db(peak);
        self.shown = db.max(self.shown - 24.0 * dt).max(FLOOR);
        if db >= self.hold || now.duration_since(self.held).as_secs_f32() > 1.5 {
            self.hold = db.max(self.hold - 20.0 * dt).max(FLOOR);
            if db >= self.hold {
                self.held = now;
            }
        }
    }
}

/// What only the editor remembers.
struct View {
    show_password: bool,
    show_address: bool,
    copied: bool,
    /// IN L, IN R, OUT L, OUT R.
    rails: [Rail; 4],
    /// Highest IN, OUT and true peak since the last reset, linear.
    max: [f32; 3],
    last: Instant,
}

impl Default for View {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            show_password: false,
            show_address: false,
            copied: false,
            rails: [Rail::new(now); 4],
            max: [0.0; 3],
            last: now,
        }
    }
}

fn db(linear: f32) -> f32 {
    20.0 * linear.max(1e-6).log10()
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

fn glyph(fonts: &Fonts, c: char, ink: Color) -> El {
    icon(fonts.icons.clone(), c).text_size(12.0).fill(ink)
}

/// An icon button that lives inside a field, at its right end.
fn action(ui: &Ui, fonts: &Fonts, id: &'static str, c: char, label: &str) -> (El, bool) {
    let hot = ui.get(id).hovered;
    let el = row([glyph(
        fonts,
        c,
        if hot { ink::accent() } else { ink::dim() },
    )])
    .justify(Justify::Center)
    .center()
    .size(22.0, H)
    .role(Kind::Button)
    .label(label)
    .focusable()
    .id(id);
    (el, ui.get(id).clicked_with(Button::Primary))
}

/// A field: leading icon, the body, then its actions, all inside one box.
fn field(fonts: &Fonts, c: char, body: El, actions: impl IntoIterator<Item = El>) -> El {
    let mut children = vec![glyph(fonts, c, ink::dim()), body.grow(1.0)];
    children.extend(actions);
    row(children)
        .gap(4.0)
        .center()
        .pad_xy(8.0, 0.0)
        .h(H)
        .radius(R)
        .fill(ink::field())
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
        let segments = ["Off", "Share", "Join"]
            .into_iter()
            .enumerate()
            .map(|(i, name)| {
                let (el, clicked) = segment(ui, name, i == picked);
                if clicked {
                    *v = i as f64 / 2.0;
                }
                el
            });
        row(segments.collect::<Vec<_>>())
            .gap(2.0)
            .pad(2.0)
            .radius(R)
            .fill(ink::field())
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
    let mut rows = vec![field(fonts, HASH, bare(input), [dice])];

    let mut password = Shared::text(&shared.password);
    let (input, changed) = if view.show_password {
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
        if view.show_password { EYE_OFF } else { EYE },
        if view.show_password {
            "Hide password"
        } else {
            "Show password"
        },
    );
    view.show_password ^= toggle;
    rows.push(field(fonts, LOCK, bare(input), [eye]));

    let mut lan = None;
    if picked == 1 {
        let slug = relay_core::slug(&room);
        let (copy, clicked) = action(
            ui,
            fonts,
            "copy",
            if view.copied { CHECK } else { COPY },
            "Copy link",
        );
        if clicked {
            ui.set_clipboard(format!("https://{}/{slug}", relay_core::SITE));
            view.copied = true;
        }
        let link = row![
            text(format!("{}/", relay_core::SITE))
                .text_size(11.0)
                .fill(ink::dim()),
            text(slug).text_size(11.0).fill(ink::text()),
        ];
        rows.push(field(fonts, LINK, link, [copy]));

        // The LAN address rides in the status line, hidden until asked.
        let address = Shared::text(&shared.address);
        let shown: String = if view.show_address {
            address
        } else {
            address
                .chars()
                .map(|c| if c.is_ascii_hexdigit() { '•' } else { c })
                .collect()
        };
        let id = "lan-eye";
        let hot = ui.get(id).hovered;
        view.show_address ^= ui.get(id).clicked_with(Button::Primary);
        lan = Some(
            row![
                glyph(fonts, WIFI, ink::dim()),
                text(shown)
                    .font(fonts.mono.clone())
                    .text_size(9.0)
                    .fill(if hot { ink::text() } else { ink::dim() }),
            ]
            .gap(4.0)
            .center()
            .role(Kind::Button)
            .label(if view.show_address {
                "Hide LAN address"
            } else {
                "Show LAN address"
            })
            .focusable()
            .id(id),
        );
    }

    let peers = shared.peers.load(Relaxed);
    let (lit, status) = match (picked, shared.net()) {
        (0, _) | (_, Net::Idle) => (ink::dim(), "Off".to_owned()),
        (1, Net::Taken) => (ink::red(), "Room taken, roll a new name".into()),
        (_, Net::Denied) => (ink::red(), "Wrong password".into()),
        (_, Net::RateMismatch) => (ink::yellow(), "Sample rates differ".into()),
        (1, Net::Offline) if peers == 0 => (ink::yellow(), "LAN only".into()),
        (1, _) if peers == 0 => (ink::accent(), "Waiting for listeners".into()),
        (1, _) => (ink::green(), format!("{peers} listening")),
        (_, Net::Lan) => (ink::green(), "Live · LAN".into()),
        (_, Net::Internet) => (ink::green(), "Live · Internet".into()),
        (_, Net::Offline) => (ink::yellow(), "Searching LAN".into()),
        _ => (ink::accent(), "Searching".into()),
    };
    let tail = match lan {
        Some(lan) => lan,
        None if picked == 2 && peers > 0 => {
            let rate = f64::from(shared.rate.load(Relaxed).max(1));
            let ms = f64::from(shared.latency.load(Relaxed)) * 1000.0 / rate;
            text(format!("{ms:.1} ms"))
                .font(fonts.mono.clone())
                .text_size(9.0)
                .fill(ink::dim())
        }
        None => spacer(),
    };

    let left = column([
        row![
            text("RELAY")
                .font(fonts.bold.clone())
                .text_size(14.0)
                .fill(ink::text()),
            spacer().grow(1.0),
            mode,
        ]
        .center(),
        column(rows).gap(4.0),
        spacer().grow(1.0),
        row![
            leaf(6.0, 6.0).radius(3.0).fill(lit),
            text(status).text_size(10.5).fill(ink::text()),
            spacer().grow(1.0),
            tail,
        ]
        .gap(6.0)
        .center(),
    ])
    .gap(6.0)
    .grow(1.0);

    let meters = meters(ui, bridge, shared, fonts, view);
    row([left, meters]).gap(12.0).pad(10.0).fill(ink::bg())
}

/// IN and OUT pairs, the scale between them, the output fader riding the
/// OUT pair, max-peak readouts on top and the gain below.
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
    let tp = shared.take_peak(Peak::TruePeak);
    for (max, v) in view
        .max
        .iter_mut()
        .zip([takes[0].max(takes[1]), takes[2].max(takes[3]), tp])
    {
        *max = max.max(v);
    }

    // Readouts: max IN, max OUT, max true peak. A click clears them.
    let reset = "peaks";
    if ui.get(reset).clicked_with(Button::Primary) {
        view.max = [0.0; 3];
    }
    let readout = |label: &str, v: f32| {
        let d = db(v);
        let value = if v < 1e-5 {
            "-inf".to_owned()
        } else {
            format!("{d:.1}")
        };
        column([
            text(label.to_owned()).text_size(8.0).fill(ink::dim()),
            text(value)
                .font(fonts.mono.clone())
                .text_size(9.0)
                .fill(if d > -0.05 { ink::red() } else { ink::text() }),
        ])
        .align(Align::Center)
        .w((PAIR * 2.0 + SCALE) / 3.0)
    };
    let top = row([
        readout("IN", view.max[0]),
        readout("OUT", view.max[1]),
        readout("TP", view.max[2]),
    ])
    .role(Kind::Button)
    .label("Reset peaks")
    .id(reset);

    let [il, ir, ol, or] = view.rails;
    let pair = |a: Rail, b: Rail| row![rail(a).grow(1.0), rail(b).grow(1.0)].gap(1.0).w(PAIR);

    // The fader: drag anywhere on the OUT pair, double-click for 0 dB.
    let gain = bridge.bind(ui, "fader", P::Output, |ui, v| {
        let h = ui
            .scene()
            .and_then(|s| s.surface("fader"))
            .map_or(100.0, |s| s.frame.size.height);
        ui.drag("fader", v, 0.0..=1.0, h, true);
        if ui.double_click("fader") {
            *v = -GAIN.0 / (GAIN.1 - GAIN.0);
        }
        let t = *v;
        overlay([pair(ol, or), canvas(move |size| fader(size, t))])
            .role(Kind::Slider {
                value: t,
                min: 0.0,
                max: 1.0,
            })
            .label("Output")
            .focusable()
            .id("fader")
    });
    let gain_db = GAIN.0 + bridge.value(P::Output) * (GAIN.1 - GAIN.0);

    // Scale labels centred on the rails' ticks: the rail body starts 6 px
    // down, and a label is about 10 px tall.
    let scale = column(std::iter::once(spacer().h(1.0)).chain(
        [(0, 6.0), (-6, 6.0), (-12, 12.0), (-24, 24.0), (-48, 12.0)].map(|(d, span)| {
            column([text(d.to_string()).text_size(7.5).fill(ink::dim())])
                .align(Align::Center)
                .grow(span)
        }),
    ))
    .w(SCALE);

    let label = |s: &str| text(s.to_owned()).text_size(8.0).fill(ink::dim());
    column([
        top,
        row![pair(il, ir), scale, gain].grow(1.0),
        row![
            column([label("IN")]).align(Align::Center).w(PAIR),
            spacer().w(SCALE),
            column([text(format!("{gain_db:+.1}"))
                .font(fonts.mono.clone())
                .text_size(8.0)
                .fill(ink::accent())])
            .align(Align::Center)
            .w(PAIR),
        ],
    ])
    .gap(4.0)
    .w(PAIR * 2.0 + SCALE)
    .h(Len::Pct(100.0))
}

/// The output fader: an orange bar across the OUT pair with a notch at
/// each end, on a dark shadow so it reads over any meter colour.
fn fader(size: Size, t: f64) -> Vec<Draw> {
    let (w, h) = (size.width, size.height);
    let y = (1.0 - t) * (h - 2.0) + 1.0;
    let poly = |pts: &[(f64, f64)], c: Color| {
        Draw::fill(
            Path::polyline(pts.iter().map(|&(x, y)| Point::new(x, y)), true),
            c,
        )
    };
    vec![
        poly(
            &[(0.0, y - 2.0), (w, y - 2.0), (w, y + 2.0), (0.0, y + 2.0)],
            ink::bg(),
        ),
        poly(
            &[(0.0, y - 1.0), (w, y - 1.0), (w, y + 1.0), (0.0, y + 1.0)],
            ink::accent(),
        ),
        poly(&[(0.0, y - 4.0), (4.0, y), (0.0, y + 4.0)], ink::accent()),
        poly(&[(w, y - 4.0), (w - 4.0, y), (w, y + 4.0)], ink::accent()),
    ]
}

/// One fat rail: green to -12 dB, yellow to -3, red above, a white
/// peak-hold line and a clip lamp on top.
fn rail(r: Rail) -> El {
    canvas(move |size| {
        let (w, h) = (size.width, size.height);
        let lamp = 4.0;
        let body = h - lamp - 2.0;
        let y = |d: f32| lamp + 2.0 + body * f64::from(d / FLOOR);
        let rect = |y0: f64, y1: f64, c: Color| {
            (y1 > y0).then(|| {
                let corners = [(0.0, y0), (w, y0), (w, y1), (0.0, y1)];
                Draw::fill(
                    Path::polyline(corners.map(|(x, y)| Point::new(x, y)), true),
                    c,
                )
            })
        };
        let top = y(r.shown.min(0.0));
        let clip = r.hold > -0.05;
        [
            rect(0.0, lamp, if clip { ink::red() } else { ink::well() }),
            rect(lamp + 2.0, h, ink::well()),
            rect(top.max(y(-12.0)), h, ink::green()),
            rect(top.max(y(-3.0)), y(-12.0).max(top), ink::yellow()),
            rect(top, y(-3.0).max(top), ink::red()),
            (r.hold > FLOOR)
                .then(|| rect(y(r.hold.min(0.0)), y(r.hold.min(0.0)) + 1.5, ink::text()))
                .flatten(),
        ]
        .into_iter()
        .chain([-6.0, -12.0, -24.0, -48.0].map(|d| rect(y(d), y(d) + 1.0, ink::bg())))
        .flatten()
        .collect()
    })
}

/// A text input with its own chrome stripped, to sit in a [`field`].
fn bare(input: El) -> El {
    input.fill(ink::field()).radius(0.0).h(H - 4.0)
}

/// One mode segment: orange with dark ink when picked.
fn segment(ui: &Ui, name: &str, on: bool) -> (El, bool) {
    let clicked = ui.get(name).clicked_with(Button::Primary);
    let hot = ui.get(name).hovered;
    let (bg, fg) = match (on, hot) {
        (true, _) => (ink::accent(), ink::on_accent()),
        (false, true) => (ink::hot(), ink::text()),
        (false, false) => (ink::field(), ink::dim()),
    };
    let el = row([text(name).text_size(10.5).fill(fg)])
        .justify(Justify::Center)
        .center()
        .size(42.0, 18.0)
        .radius(R - 1.0)
        .fill(bg)
        .role(Kind::Button)
        .label(name)
        .focusable()
        .id(name);
    (el, clicked)
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

    fn render(mode: f64, net: Net, name: &str) {
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
            (Peak::TruePeak, 1.1),
        ] {
            shared.note_peak(p, v);
        }
        let mut view = View::default();
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
        render(0.5, Net::Internet, "share");
        render(1.0, Net::Lan, "join");
    }
}
