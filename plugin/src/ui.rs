//! The editor: one flat panel in BUFFR's Polar Night with Studio Blue.
//! Mode, room, password and the share link on the left; L/R meters on the
//! right, like a limiter's.

use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

use mui::prelude::*;
use mui_truce::{Bridge, MuiEditor};
use relay_core::{Net, Shared};
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

const SIZE: (u32, u32) = (420, 196);

/// sRGB from `0xRRGGBB`.
fn hex(c: u32) -> Color {
    let ch = |s: u32| ((c >> s) & 0xff) as f32 / 255.0;
    Color::srgb(ch(16), ch(8), ch(0))
}

/// The panel's tokens; apps/relay-web and apps/web use the same values.
mod ink {
    use super::{Color, hex};
    pub fn bg() -> Color {
        hex(0x0a0b0d)
    }
    pub fn field() -> Color {
        hex(0x06070a)
    }
    pub fn line() -> Color {
        hex(0x22252c)
    }
    pub fn text() -> Color {
        hex(0xf3f5f8)
    }
    pub fn dim() -> Color {
        hex(0x8b93a1)
    }
    pub fn accent() -> Color {
        hex(0x00aaff)
    }
    pub fn on_accent() -> Color {
        hex(0x00131d)
    }
    pub fn ok() -> Color {
        hex(0x2fd67b)
    }
    pub fn warn() -> Color {
        hex(0xffb020)
    }
    pub fn bad() -> Color {
        hex(0xff4d4f)
    }
}

const THEME: Theme = Theme {
    palette: Palette {
        primary: Pigment::new(237.0, 0.16),
        ..Palette::NEUTRAL
    },
    corners: Corners {
        selector: 2.0,
        field: 3.0,
        box_: 3.0,
        concave: 2.0,
    },
    text: 12.0,
    ..Theme::DEFAULT
};

struct Fonts {
    bold: Font,
    mono: Font,
    icons: Font,
}

/// What only the editor remembers.
#[derive(Default)]
struct View {
    show_password: bool,
    show_address: bool,
    /// Which copy button was pressed last, for its check mark.
    copied: Option<&'static str>,
    /// Peak L and R, 0..1, from the audio thread's meters.
    levels: (f64, f64),
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
    .resizable((360, 180))
    .into_editor()
}

/// A square icon button for the end of a [`group`].
fn button(ui: &Ui, fonts: &Fonts, id: &'static str, glyph: char, label: &str) -> (El, bool) {
    let hot = ui.get(id).hovered;
    let el = row([icon(fonts.icons.clone(), glyph)
        .text_size(13.0)
        .fill(if hot { ink::text() } else { ink::dim() })])
    .justify(Justify::Center)
    .center()
    .w(26.0)
    .h(Len::Pct(100.0))
    .fill(if hot { ink::line() } else { ink::field() })
    .role(Kind::Button)
    .label(label)
    .focusable()
    .id(id);
    (el, ui.get(id).clicked_with(Button::Primary))
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
            .inside(2.0)
            .gap(0.0)
            .radius(3.0)
            .fill(ink::field())
            .border(ink::line(), 1.0)
    });

    // Room, with a dice to roll a fresh name.
    let mut room = Shared::text(&shared.room);
    let (input, changed) = text_input(ui, "room", &mut room);
    if changed {
        shared.set_text(&shared.room, &room);
    }
    let (dice, roll) = button(ui, fonts, "roll", DICE, "New room name");
    if roll {
        shared.set_text(&shared.room, &relay_core::room_name());
    }
    let mut rows = vec![labelled("Room", group([bare(input), dice]))];

    // Password, dots unless the eye is open.
    let mut password = Shared::text(&shared.password);
    let (input, changed) = if view.show_password {
        text_input(ui, "pass", &mut password)
    } else {
        masked_input(ui, "pass", &mut password)
    };
    if changed {
        shared.set_text(&shared.password, &password);
    }
    let (eye, toggle) = eye_button(ui, fonts, "pass-eye", view.show_password, "password");
    view.show_password ^= toggle;
    rows.push(labelled("Password", group([bare(input), eye])));

    if picked == 1 {
        let link = format!("{}/{}", relay_core::SITE, relay_core::slug(&room));
        let address = Shared::text(&shared.address);
        let shown: String = if view.show_address {
            address.clone()
        } else {
            address
                .chars()
                .map(|c| if c.is_ascii_hexdigit() { '•' } else { c })
                .collect()
        };
        let mut copy = |ui: &mut Ui, id: &'static str, value: String| {
            let glyph = if view.copied == Some(id) { CHECK } else { COPY };
            let (el, clicked) = button(ui, fonts, id, glyph, "Copy");
            if clicked {
                ui.set_clipboard(value);
                view.copied = Some(id);
            }
            el
        };
        let copy_link = copy(ui, "copy-link", format!("https://{link}"));
        let copy_address = copy(ui, "copy-lan", address);
        let (eye, toggle) = eye_button(ui, fonts, "lan-eye", view.show_address, "address");
        view.show_address ^= toggle;
        rows.push(labelled("Link", group([readout(&link, fonts), copy_link])));
        rows.push(labelled(
            "LAN",
            group([readout(&shown, fonts), eye, copy_address]),
        ));
    }

    let peers = shared.peers.load(Relaxed);
    let (lit, status) = match (picked, shared.net()) {
        (0, _) | (_, Net::Idle) => (ink::line(), "Off".to_owned()),
        (1, Net::Taken) => (
            ink::bad(),
            "Room is hosted elsewhere, roll a new name".into(),
        ),
        (_, Net::Denied) => (ink::bad(), "Wrong password".into()),
        (_, Net::RateMismatch) => (ink::warn(), "Sample rates differ".into()),
        (1, Net::Offline) if peers == 0 => (ink::warn(), "LAN only, no internet link".into()),
        (1, _) if peers == 0 => (ink::accent(), "Waiting for listeners".into()),
        (1, _) => (ink::ok(), format!("{peers} listening")),
        (_, Net::Lan) => (ink::ok(), "Live on LAN, lossless".into()),
        (_, Net::Internet) => (ink::ok(), "Live over the internet".into()),
        (_, Net::Offline) => (ink::warn(), "Looking on LAN, no internet link".into()),
        _ => (ink::accent(), "Looking for the room".into()),
    };
    let latency = if picked == 2 && peers > 0 {
        let rate = f64::from(shared.rate.load(Relaxed).max(1));
        let ms = f64::from(shared.latency.load(Relaxed)) * 1000.0 / rate;
        format!("BUF {ms:.1} ms")
    } else {
        String::new()
    };

    let left = column([
        row![
            text("RELAY")
                .font(fonts.bold.clone())
                .text_size(16.0)
                .fill(ink::text()),
            spacer().grow(1.0),
            mode.w(180.0),
        ]
        .center(),
        column(rows).gap(4.0),
        spacer().grow(1.0),
        row![
            leaf(6.0, 6.0).radius(1.0).fill(lit),
            text(status).text_size(11.0).fill(ink::text()),
            spacer().grow(1.0),
            text(latency)
                .font(fonts.mono.clone())
                .text_size(9.0)
                .fill(ink::dim()),
        ]
        .gap(6.0)
        .center(),
    ])
    .gap(8.0)
    .grow(1.0);

    if bridge.context().is_some() {
        let p = bridge.params();
        let level = |id| f64::from(bridge.meter(id)).clamp(0.0, 1.0);
        view.levels = (level(p.left.id()), level(p.right.id()));
    }
    let (l, r) = view.levels;
    let peak = l.max(r);
    let db = if peak > 1e-5 {
        format!("{:.1}", 20.0 * peak.log10())
    } else {
        "-inf".to_owned()
    };
    let bar = |level, name| {
        column([
            meter(level).w(12.0).grow(1.0),
            text(name).text_size(9.0).fill(ink::dim()),
        ])
        .gap(3.0)
        .align(Align::Center)
        .grow(1.0)
    };
    let meters = column([
        row![bar(l, "L"), bar(r, "R")].gap(3.0).grow(1.0),
        text(db)
            .font(fonts.mono.clone())
            .text_size(9.0)
            .fill(if peak >= 1.0 { ink::bad() } else { ink::text() }),
    ])
    .gap(4.0)
    .align(Align::Center)
    .w(36.0)
    .h(Len::Pct(100.0));

    row([
        left,
        leaf(1.0, 0.0).h(Len::Pct(100.0)).fill(ink::line()),
        meters,
    ])
    .gap(10.0)
    .pad(10.0)
    .fill(ink::bg())
}

fn eye_button(ui: &Ui, fonts: &Fonts, id: &'static str, open: bool, what: &str) -> (El, bool) {
    let (glyph, verb) = if open {
        (EYE_OFF, "Hide")
    } else {
        (EYE, "Show")
    };
    button(ui, fonts, id, glyph, &format!("{verb} {what}"))
}

/// A caps label in a fixed column, then the control.
fn labelled(name: &str, control: El) -> El {
    row![
        text(name.to_uppercase())
            .text_size(9.5)
            .fill(ink::dim())
            .w(62.0),
        control.grow(1.0),
    ]
    .center()
}

/// A field and its icon buttons, welded into one bordered well.
fn group(children: impl IntoIterator<Item = El>) -> El {
    row(children)
        .inside(1.0)
        .gap(0.0)
        .h(26.0)
        .radius(3.0)
        .fill(ink::field())
        .border(ink::line(), 1.0)
}

/// A text input with its own chrome stripped, to sit in a [`group`].
fn bare(input: El) -> El {
    input.grow(1.0).fill(ink::field()).radius(0.0)
}

/// Read-only text in a [`group`]; numbers (an IP) in mono.
fn readout(value: &str, fonts: &Fonts) -> El {
    let shown = if value.chars().count() > 44 {
        format!("{}…", value.chars().take(43).collect::<String>())
    } else {
        value.to_owned()
    };
    let label = if value.chars().any(|c| c.is_ascii_alphabetic()) {
        text(shown).text_size(12.0)
    } else {
        text(shown).font(fonts.mono.clone()).text_size(10.0)
    };
    row([label.fill(ink::text()), spacer().grow(1.0)])
        .center()
        .pad_xy(8.0, 0.0)
        .grow(1.0)
}

/// One segment of the mode switch: Studio Blue with dark ink when picked.
fn segment(ui: &Ui, name: &str, on: bool) -> (El, bool) {
    let clicked = ui.get(name).clicked_with(Button::Primary);
    let hot = ui.get(name).hovered;
    let (bg, fg) = match (on, hot) {
        (true, _) => (ink::accent(), ink::on_accent()),
        (false, true) => (ink::line(), ink::text()),
        (false, false) => (ink::field(), ink::dim()),
    };
    let el = row([text(name).text_size(11.0).fill(fg)])
        .justify(Justify::Center)
        .center()
        .grow(1.0)
        .h(22.0)
        .radius(2.0)
        .fill(bg)
        .role(Kind::Button)
        .label(name)
        .focusable()
        .id(name);
    (el, clicked)
}

/// A vertical peak meter on a -60..0 dB scale: green, amber over -6 dB,
/// red over -1 dB, with ticks at -6, -12, -18, -24 and -48.
fn meter(peak: f64) -> El {
    let db = 20.0 * peak.max(1e-6).log10();
    let lit = ((db + 60.0) / 60.0).clamp(0.0, 1.0);
    canvas(move |size| {
        let (w, h) = (size.width, size.height);
        let y = |db: f64| h * (-db / 60.0);
        let rect = |y0: f64, y1: f64, c: Color| {
            let y1 = y1.max(y0);
            let corners = [(0.0, y0), (w, y0), (w, y1), (0.0, y1)];
            (y1 > y0).then(|| {
                Draw::fill(
                    Path::polyline(corners.map(|(x, y)| Point::new(x, y)), true),
                    c,
                )
            })
        };
        let top = h * (1.0 - lit);
        [
            rect(0.0, h, ink::line()),
            rect(1.0, h - 1.0, ink::field()),
            rect(top.max(y(-6.0)), h, ink::ok()),
            rect(top.max(y(-1.0)), y(-6.0).max(top), ink::warn()),
            rect(top, y(-1.0).max(top), ink::bad()),
        ]
        .into_iter()
        .chain([6.0, 12.0, 18.0, 24.0, 48.0].map(|d| rect(y(-d), y(-d) + 1.0, ink::bg())))
        .flatten()
        .collect()
    })
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
        let mut view = View {
            levels: (0.5, 0.95),
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
        render(0.5, Net::Internet, "share");
        render(1.0, Net::Lan, "join");
    }
}
