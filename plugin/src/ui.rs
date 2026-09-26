//! The editor: BUFFR's Polar Night and Studio Blue in MUI. Mode buttons,
//! the room, a password, the host to join, the buffer knob and a meter.

use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

use mui::prelude::*;
use mui_truce::{Bridge, MuiEditor};
use relay_core::Shared;
use truce_core::editor::{Editor, IntoEditor};

use crate::{P, RelayParams};

const BARLOW: &[u8] = include_bytes!("../assets/barlow-latin-400.ttf");

/// Studio Blue (#00aaff) on neutral greys.
const THEME: Theme = Theme {
    palette: Palette {
        primary: Pigment::new(237.0, 0.16),
        ..Palette::NEUTRAL
    },
    text: 13.0,
    ..Theme::DEFAULT
};

const SIZE: (u32, u32) = (400, 280);

fn new_ui() -> Ui {
    let ui = Ui::new(THEME);
    match Font::new(BARLOW) {
        Ok(font) => ui.font(font),
        Err(_) => ui,
    }
}

pub fn editor(params: Arc<RelayParams>) -> Box<dyn Editor> {
    let shared = Arc::clone(&params.link.0);
    MuiEditor::new(params, new_ui(), SIZE, move |ui, bridge| {
        build(ui, bridge, &shared)
    })
    .resizable((320, 240))
    .into_editor()
}

fn build(ui: &mut Ui, bridge: &mut Bridge<RelayParams>, shared: &Shared) -> El {
    let picked = (bridge.value(P::Mode) * 2.0).round() as usize;
    let mode = bridge.bind(ui, "mode", P::Mode, |ui, v| {
        let segments = ["Off", "Share", "Join"]
            .into_iter()
            .enumerate()
            .map(|(i, name)| {
                let (el, clicked) = segment(ui, name, name, i == picked);
                if clicked {
                    *v = i as f64 / 2.0;
                }
                el.grow(1.0)
            });
        row(segments.collect::<Vec<_>>())
            .gap(2.0)
            .pad(3.0)
            .radius(8.0)
            .fill(Field)
    });

    let field = |ui: &mut Ui, id: &str, name: &str, value: &std::sync::Mutex<String>| {
        let mut text = Shared::text(value);
        let (input, changed) = text_input(ui, id, &mut text);
        if changed {
            shared.set_text(value, &text);
        }
        row![caption(name).w(72.0), input.grow(1.0)].gap(S).center()
    };
    let mut rows = vec![
        field(ui, "room", "Room", &shared.room),
        field(ui, "pass", "Password", &shared.password),
    ];
    if picked == 1 {
        let address = Shared::text(&shared.address);
        let address = if address.is_empty() {
            "…".to_owned()
        } else {
            address
        };
        rows.push(
            row![
                caption("Join with").w(72.0),
                text(address).fill(Primary).grow(1.0)
            ]
            .gap(S)
            .center(),
        );
    }
    if picked == 2 {
        rows.push(field(ui, "host", "Host", &shared.peer));
        let (rate, block) = (shared.rate.load(Relaxed).max(1), shared.block.load(Relaxed));
        let blocks = bridge.text(P::Buffer);
        let ms = blocks.parse::<f64>().unwrap_or(1.0) * f64::from(block) * 1000.0 / f64::from(rate);
        let hint = if block > 0 {
            format!("{blocks} × {block} frames · {ms:.1} ms")
        } else {
            blocks
        };
        let buffer = bridge.bind(ui, "buffer", P::Buffer, |ui, v| {
            slider(ui, "buffer", "", v, 0.0..=1.0)
                .0
                .value_text(hint)
                .el()
        });
        rows.push(
            row![caption("Buffer").w(72.0), buffer.grow(1.0)]
                .gap(S)
                .center(),
        );
    }

    let peers = shared.peers.load(Relaxed);
    let port = shared.port.load(Relaxed);
    let (lit, status) = match picked {
        1 if port == 0 => (Danger, "No free port".to_owned()),
        1 => (
            Success,
            format!("Sharing on port {port} · {peers} listening"),
        ),
        2 if shared.net() == relay_core::Net::RateMismatch => (Warning, "Sample rates differ".to_owned()),
        2 if peers > 0 => (Success, "Live".to_owned()),
        2 => (Warning, "Waiting for host".to_owned()),
        _ => (Field, "Off".to_owned()),
    };

    let level = f64::from(bridge.meter(bridge.params().level.id())).clamp(0.0, 1.0);
    let db = if level > 0.0 {
        format!("{:.1} dB", 20.0 * level.log10())
    } else {
        "-inf dB".to_owned()
    };
    // Meter on a -60..0 dB scale, like BUFFR's Out.
    let lit_w = 120.0 * (1.0 + (20.0 * level.max(1e-6).log10()) / 60.0).clamp(0.0, 1.0);
    let meter = row([leaf(lit_w, 6.0).radius(3.0).fill(Primary)])
        .size(120.0, 6.0)
        .radius(3.0)
        .fill(Field);

    col![
        row![
            title("RELAY"),
            spacer().grow(1.0),
            caption("Out"),
            meter,
            caption(db).w(60.0),
        ]
        .gap(S)
        .center(),
        mode,
        column(rows).gap(S).pad(M).radius(10.0).fill(Raised),
        row![leaf(8.0, 8.0).radius(4.0).fill(lit), caption(status)]
            .gap(S)
            .center(),
    ]
    .gap(M)
    .pad(L)
    .fill(Surface)
}

/// One segment of a BUFFR segmented control: Studio Blue when picked.
fn segment(ui: &Ui, id: &str, name: &str, on: bool) -> (El, bool) {
    let clicked = ui.get(id).clicked_with(Button::Primary);
    let el = row([text(name).fill(if on { Ink } else { Dim })])
        .justify(Justify::Center)
        .pad_xy(14.0, 7.0)
        .radius(6.0)
        .fill(if on { Primary } else { Field })
        .role(Kind::Button)
        .label(name)
        .focusable()
        .id(id);
    (el, clicked)
}

/// Renders the editor to `$TMPDIR/relay-editor-<mode>.png` on the CPU so it
/// can be looked at without a DAW.
#[cfg(test)]
mod snapshot {
    use super::*;
    use mui::vello::vello_cpu::{Pixmap, RenderContext, Resources};
    use mui::vello::{Cache, Cpu};
    use truce_params::Params;

    fn render(mode: f64, name: &str) {
        let params = Arc::new(RelayParams::new());
        params.set_normalized(P::Mode.into(), mode);
        let shared = Arc::clone(&params.link.0);
        shared.set_text(&shared.peer, "192.168.1.20");
        shared.set_text(&shared.address, "192.168.1.20");
        shared
            .port
            .store(17_492, std::sync::atomic::Ordering::Relaxed);
        shared.peers.store(2, std::sync::atomic::Ordering::Relaxed);
        shared
            .rate
            .store(48_000, std::sync::atomic::Ordering::Relaxed);
        shared
            .block
            .store(256, std::sync::atomic::Ordering::Relaxed);
        let mut bridge = Bridge::new(params);
        let mut ui = new_ui();
        let (w, h) = (SIZE.0 * 2, SIZE.1 * 2);
        ui.scale = Some(2.0);
        let root = build(&mut ui, &mut bridge, &shared);
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
        render(0.5, "share");
        render(1.0, "join");
    }
}
