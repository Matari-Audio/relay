//! The editor: a compact black strip. Welded pills for the mode, the room,
//! the password and the link; one fused L/R meter down the right edge.

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
const HASH: char = '\u{E2A2}';
const LINK: char = '\u{E2E6}';
const LOCK: char = '\u{E308}';
const WIFI: char = '\u{E4EA}';

const SIZE: (u32, u32) = (300, 132);
/// Pill height and corner.
const H: f64 = 24.0;
const R: f64 = 7.0;

/// sRGB from `0xRRGGBB`.
fn hex(c: u32) -> Color {
    let ch = |s: u32| ((c >> s) & 0xff) as f32 / 255.0;
    Color::srgb(ch(16), ch(8), ch(0))
}

/// Black, white, one signal colour. apps/relay-web uses the same values.
mod ink {
    use super::{Color, hex};
    pub fn bg() -> Color {
        hex(0x050505)
    }
    pub fn pill() -> Color {
        hex(0x161616)
    }
    pub fn hot() -> Color {
        hex(0x262626)
    }
    pub fn text() -> Color {
        hex(0xffffff)
    }
    pub fn dim() -> Color {
        hex(0x6e6e6e)
    }
    pub fn black() -> Color {
        hex(0x000000)
    }
    /// Live.
    pub fn volt() -> Color {
        hex(0xc8ff00)
    }
    pub fn warn() -> Color {
        hex(0xffb400)
    }
    pub fn bad() -> Color {
        hex(0xff3b30)
    }
}

const THEME: Theme = Theme {
    palette: Palette::NEUTRAL,
    corners: Corners {
        selector: R,
        field: R,
        box_: R,
        concave: 4.0,
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
    copied: bool,
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
    .resizable((260, 120))
    .into_editor()
}

/// Siblings a few pixels apart, fused where they face each other.
fn welded(children: impl IntoIterator<Item = El>) -> El {
    row(children)
        .gap(4.0)
        .weld_with(Weld::all().reach(5.0).blend(1.5))
}

fn glyph(fonts: &Fonts, c: char, ink: Color) -> El {
    icon(fonts.icons.clone(), c).text_size(12.0).fill(ink)
}

/// A square icon button, welded to the pill before it.
fn button(ui: &Ui, fonts: &Fonts, id: &'static str, c: char, label: &str) -> (El, bool) {
    let hot = ui.get(id).hovered;
    let el = row([glyph(fonts, c, if hot { ink::text() } else { ink::dim() })])
        .justify(Justify::Center)
        .center()
        .size(H, H)
        .radius(R)
        .fill(if hot { ink::hot() } else { ink::pill() })
        .role(Kind::Button)
        .label(label)
        .focusable()
        .id(id);
    (el, ui.get(id).clicked_with(Button::Primary))
}

/// A pill that leads with an icon: the field's label, without a label.
fn pill(fonts: &Fonts, c: char, body: El) -> El {
    row([glyph(fonts, c, ink::dim()), body.grow(1.0)])
        .gap(6.0)
        .center()
        .pad_xy(8.0, 0.0)
        .h(H)
        .radius(R)
        .fill(ink::pill())
        .grow(1.0)
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
        welded(
            ["Off", "Share", "Join"]
                .into_iter()
                .enumerate()
                .map(|(i, name)| {
                    let (el, clicked) = segment(ui, name, i == picked);
                    if clicked {
                        *v = i as f64 / 2.0;
                    }
                    el
                }),
        )
    });

    let mut room = Shared::text(&shared.room);
    let (input, changed) = text_input(ui, "room", &mut room);
    if changed {
        shared.set_text(&shared.room, &room);
    }
    let (dice, roll) = button(ui, fonts, "roll", DICE, "New room name");
    if roll {
        shared.set_text(&shared.room, &relay_core::room_name());
    }
    let mut rows = vec![welded([pill(fonts, HASH, bare(input)), dice])];

    let mut password = Shared::text(&shared.password);
    let (input, changed) = if view.show_password {
        text_input(ui, "pass", &mut password)
    } else {
        masked_input(ui, "pass", &mut password)
    };
    if changed {
        shared.set_text(&shared.password, &password);
    }
    let (eye, toggle) = button(
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
    rows.push(welded([pill(fonts, LOCK, bare(input)), eye]));

    let mut lan = None;
    if picked == 1 {
        let link = format!("{}/{}", relay_core::SITE, relay_core::slug(&room));
        let (copy, clicked) = button(
            ui,
            fonts,
            "copy",
            if view.copied { CHECK } else { COPY },
            "Copy link",
        );
        if clicked {
            ui.set_clipboard(format!("https://{link}"));
            view.copied = true;
        }
        let link = row![
            text(format!("{}/", relay_core::SITE))
                .text_size(10.0)
                .fill(ink::dim()),
            text(relay_core::slug(&room))
                .text_size(10.0)
                .fill(ink::text()),
        ];
        rows.push(welded([pill(fonts, LINK, link), copy]));

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
        (1, Net::Taken) => (ink::bad(), "Room taken, roll a new name".into()),
        (_, Net::Denied) => (ink::bad(), "Wrong password".into()),
        (_, Net::RateMismatch) => (ink::warn(), "Sample rates differ".into()),
        (1, Net::Offline) if peers == 0 => (ink::warn(), "LAN only".into()),
        (1, _) if peers == 0 => (ink::text(), "Waiting".into()),
        (1, _) => (ink::volt(), format!("{peers} listening")),
        (_, Net::Lan) => (ink::volt(), "Live · LAN".into()),
        (_, Net::Internet) => (ink::volt(), "Live · Internet".into()),
        (_, Net::Offline) => (ink::warn(), "Searching LAN".into()),
        _ => (ink::text(), "Searching".into()),
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
                .text_size(12.0)
                .fill(ink::text()),
            spacer().grow(1.0),
            mode,
        ]
        .center(),
        column(rows).gap(4.0),
        spacer().grow(1.0),
        row![
            leaf(5.0, 5.0).radius(2.5).fill(lit),
            text(status).text_size(10.0).fill(ink::text()),
            spacer().grow(1.0),
            tail,
        ]
        .gap(5.0)
        .center(),
    ])
    .gap(5.0)
    .grow(1.0);

    if bridge.context().is_some() {
        let p = bridge.params();
        let level = |id| f64::from(bridge.meter(id)).clamp(0.0, 1.0);
        view.levels = (level(p.left.id()), level(p.right.id()));
    }
    row([left, meter(view.levels).h(Len::Pct(100.0))])
        .gap(8.0)
        .pad(8.0)
        .fill(ink::bg())
}

/// A text input with its own chrome stripped, to sit in a [`pill`].
fn bare(input: El) -> El {
    input.fill(ink::pill()).radius(0.0).h(H - 4.0)
}

/// One mode pill: white with black ink when picked.
fn segment(ui: &Ui, name: &str, on: bool) -> (El, bool) {
    let clicked = ui.get(name).clicked_with(Button::Primary);
    let hot = ui.get(name).hovered;
    let (bg, fg) = match (on, hot) {
        (true, _) => (ink::text(), ink::black()),
        (false, true) => (ink::hot(), ink::text()),
        (false, false) => (ink::pill(), ink::dim()),
    };
    let el = row([text(name).text_size(10.5).fill(fg)])
        .justify(Justify::Center)
        .center()
        .size(44.0, 20.0)
        .radius(R)
        .fill(bg)
        .role(Kind::Button)
        .label(name)
        .focusable()
        .id(name);
    (el, clicked)
}

/// L and R rails fused into one 9px strip, -60..0 dB: white, red above -1 dB.
fn meter((l, r): (f64, f64)) -> El {
    let lit = |peak: f64| ((20.0 * peak.max(1e-6).log10() + 60.0) / 60.0).clamp(0.0, 1.0);
    let (l, r) = (lit(l), lit(r));
    canvas(move |size| {
        let h = size.height;
        let rect = |x: f64, y0: f64, y1: f64, c: Color| {
            let y1 = y1.max(y0);
            let corners = [(x, y0), (x + 4.0, y0), (x + 4.0, y1), (x, y1)];
            (y1 > y0).then(|| {
                Draw::fill(
                    Path::polyline(corners.map(|(x, y)| Point::new(x, y)), true),
                    c,
                )
            })
        };
        let clip = h / 60.0;
        [(0.0, l), (5.0, r)]
            .into_iter()
            .flat_map(|(x, lit)| {
                let top = h * (1.0 - lit);
                [
                    rect(x, 0.0, h, ink::pill()),
                    rect(x, top.max(clip), h, ink::text()),
                    rect(x, top, clip.max(top), ink::bad()),
                ]
            })
            .flatten()
            .collect()
    })
    .w(9.0)
    .radius(2.0)
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
            levels: (0.5, 0.99),
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
