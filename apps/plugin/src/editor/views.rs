//! The six custom RELAY views for the vizia editor.
//!
//! Structure and behaviour only. There is not one color literal in this file:
//! every surface reads its color from a CSS class (see `views.css` for the
//! class -> role map). Every size is stretch / percentage / px relative to
//! the parent, so the editor scales without a fixed window assumption.

use std::cell::RefCell;
use std::sync::Arc;
use std::time::Duration;

use truce_vizia::ParamLens;
use truce_vizia::vizia::prelude::*;
use truce_vizia::vizia::vg;

use crate::meter::{PeakHold, db_to_pos, peak_to_db};
use crate::status::Health;
use crate::{P, RelayParams};

/// UI refresh cadence. Matches the egui editor's 30 Hz meter fan-out.
pub const FRAME: Duration = Duration::from_millis(33);

// ponytail: ONE shared 30 Hz timer drives the whole editor, because vizia's
// `Context::modify_timer` (vizia_core/src/context/mod.rs:857) spins forever on
// any timer that isn't the head of its heap - so a *second* `start_timer` in a
// window hangs the editor. `editor::view` owns it; the views below hang their
// per-frame work off this list. Give each view its own timer once that's fixed.
thread_local! {
    static TICKS: RefCell<Vec<Box<dyn FnMut()>>> = const { RefCell::new(Vec::new()) };
}

/// Drop every registered job. Called when a view tree is (re)built.
pub fn clear_ticks() {
    TICKS.with_borrow_mut(Vec::clear);
}

/// Run `job` on every tick of the editor's timer.
pub fn on_tick(job: impl FnMut() + 'static) {
    TICKS.with_borrow_mut(|ticks| ticks.push(Box::new(job)));
}

/// Run every registered job once.
pub fn tick_all() {
    TICKS.with_borrow_mut(|ticks| {
        for job in ticks.iter_mut() {
            job();
        }
    });
}

// ------------------------------------------------------------------ logo ---

/// The RELAY mark in the SVG's `0 0 100 100` space: a source dot handing the
/// signal to two transmit chevrons. Single source of truth - `assets/logo.svg`
/// is generated from these constants and a test asserts the two agree.
const LOGO_STROKE: f32 = 13.0;
const LOGO_DOT: [f32; 3] = [24.0, 50.0, 9.0];
const LOGO_CHEVRONS: [[[f32; 2]; 3]; 2] = [
    [[42.0, 26.0], [56.0, 50.0], [42.0, 74.0]],
    [[66.0, 26.0], [80.0, 50.0], [66.0, 74.0]],
];

struct LogoMark;

impl View for LogoMark {
    fn element(&self) -> Option<&'static str> {
        Some("logo-mark")
    }

    fn draw(&self, cx: &mut DrawContext, canvas: &Canvas) {
        let bounds = cx.bounds();
        let size = bounds.w.min(bounds.h);
        if size <= 0.0 {
            return;
        }
        // Square, optically centred in whatever box CSS gave us.
        let scale = size / 100.0;
        let ox = bounds.x + (bounds.w - size) / 2.0;
        let oy = bounds.y + (bounds.h - size) / 2.0;
        let at = |p: [f32; 2]| (ox + p[0] * scale, oy + p[1] * scale);

        let mut paint = vg::Paint::default();
        paint.set_anti_alias(true);
        paint.set_color(cx.font_color());
        canvas.draw_circle(at([LOGO_DOT[0], LOGO_DOT[1]]), LOGO_DOT[2] * scale, &paint);

        paint.set_style(vg::PaintStyle::Stroke);
        paint.set_stroke_width(LOGO_STROKE * scale);
        paint.set_stroke_cap(vg::PaintCap::Round);
        paint.set_stroke_join(vg::PaintJoin::Round);
        for chevron in LOGO_CHEVRONS {
            let mut path = vg::PathBuilder::new();
            path.move_to(at(chevron[0]))
                .line_to(at(chevron[1]))
                .line_to(at(chevron[2]));
            canvas.draw_path(&path.detach(), &paint);
        }
    }
}

/// The RELAY mark. Square, strokes with the CSS `color`, scales to the
/// `.logo` width/height.
pub fn logo_mark(cx: &mut Context) -> Handle<'_, impl View> {
    LogoMark.build(cx, |_| {}).class("logo")
}

// ---------------------------------------------------------------- meters ---

// Fake meter feed for the screenshot test: the headless screenshot bridge
// hard-codes `get_meter` to 0.0, so there is no other way to render a lit rail.
// ponytail: thread-local test seam; drop it if truce grows a screenshot
// meter bridge.
#[cfg(test)]
thread_local! {
    static FAKE_METERS: std::cell::Cell<[f32; 2]> = const { std::cell::Cell::new([0.0; 2]) };
}

/// Feed the meters a fixed pair of peaks so a headless shot renders a lit rail.
#[cfg(test)]
pub fn set_fake_meters(peaks: [f32; 2]) {
    FAKE_METERS.with(|cell| cell.set(peaks));
}

fn read_peaks(lens: &ParamLens<RelayParams>) -> [f32; 2] {
    #[cfg(test)]
    {
        let fake = FAKE_METERS.with(std::cell::Cell::get);
        if fake != [0.0; 2] {
            return fake;
        }
    }
    [lens.meter(P::MeterLeft), lens.meter(P::MeterRight)]
}

/// Stereo GYR meters: two vertical rails with independent peak-hold ticks,
/// a clip lamp above each, L/R labels on top and a dB readout below.
///
/// Per DESIGN.md the rail is a full-height flat green->yellow->red gradient
/// covered from the top by the surface, not a coloured fill that grows.
pub fn stereo_meters(cx: &mut Context, lens: ParamLens<RelayParams>) -> Handle<'_, impl View> {
    let peaks = read_peaks(&lens);
    let level = [Signal::new(peaks[0]), Signal::new(peaks[1])];
    let held = [Signal::new(peaks[0]), Signal::new(peaks[1])];

    let poll = lens;
    HStack::new(cx, move |cx| {
        let holders = RefCell::new([PeakHold::default(); 2]);
        on_tick(move || {
            let peaks = read_peaks(&poll);
            let mut holders = holders.borrow_mut();
            for channel in 0..2 {
                level[channel].set_if_changed(peaks[channel]);
                held[channel].set_if_changed(holders[channel].update(peaks[channel]));
            }
        });

        for channel in 0..2 {
            let peak = level[channel];
            let hold = held[channel];
            let cover = Memo::new(move |_| Percentage(100.0 - rail_pos(peak.get()) * 100.0));
            let tick = Memo::new(move |_| Percentage(100.0 - rail_pos(hold.get()) * 100.0));
            let clipping = Memo::new(move |_| hold.get() >= 1.0);
            let db = Memo::new(move |_| format!("{:.0}", peak_to_db(peak.get())));

            VStack::new(cx, move |cx| {
                Label::new(cx, if channel == 0 { "L" } else { "R" }).class("meters-label");
                Element::new(cx)
                    .class("meters-clip")
                    .toggle_class("lit", clipping);
                ZStack::new(cx, move |cx| {
                    Element::new(cx).class("meters-fill");
                    Element::new(cx).class("meters-cover").height(cover);
                    Element::new(cx).class("meters-hold").top(tick);
                })
                .class("meters-rail");
                Label::new(cx, db).class("meters-db");
            })
            .class("meters-channel");
        }
    })
    .class("meters")
}

/// Linear peak -> `[0, 1]` rail position, through the shared dBFS scale.
fn rail_pos(peak: f32) -> f32 {
    db_to_pos(peak_to_db(peak))
}

// -------------------------------------------------------------- spectrum ---

/// Forty logarithmic bands, polled off the audio tap at ~30 fps.
pub fn spectrum(cx: &mut Context, tap: Arc<crate::spectrum::SpectrumTap>) -> Handle<'_, impl View> {
    let bands: Vec<Signal<f32>> = (0..40).map(|_| Signal::new(0.0)).collect();

    let analysis = RefCell::new(crate::spectrum::Spectrum::default());
    // Seed once at build so the first frame (and the headless screenshot,
    // where no timer ever fires) shows whatever the tap already holds.
    analysis.borrow_mut().update(&tap);
    for (signal, band) in bands.iter().zip(analysis.borrow().bands) {
        signal.set(band);
    }

    let ticking = bands.clone();
    HStack::new(cx, move |cx| {
        on_tick(move || {
            let mut analysis = analysis.borrow_mut();
            analysis.update(&tap);
            for (signal, band) in ticking.iter().zip(analysis.bands) {
                signal.set_if_changed(band);
            }
        });

        for band in bands {
            let height = Memo::new(move |_| Percentage(band.get().clamp(0.0, 1.0) * 100.0));
            Element::new(cx).class("spectrum-bar").height(height);
        }
    })
    .class("spectrum")
}

// ----------------------------------------------------------------- fader ---

/// Normalized position of 0 dB on the `-24..+12 dB` gain range.
const GAIN_DEFAULT: f64 = 24.0 / 36.0;
/// One arrow-key nudge: 1 dB.
const GAIN_STEP: f32 = 1.0 / 36.0;

/// Horizontal gain fader: 4 px slot, flat cap, right-aligned readout.
///
/// Drag sets through one `begin_edit` / `set*` / `end_edit` gesture; a bare
/// click is that gesture with a single `set`, i.e. `automate`. Double-click
/// resets to 0 dB, arrows step 1 dB.
pub fn fader(cx: &mut Context, lens: ParamLens<RelayParams>, param: P) -> Handle<'_, impl View> {
    let value = lens.value_signal(param);
    let formatter = lens.clone();
    let readout = Memo::new(move |_| {
        let _ = value.get();
        formatter.format(param)
    });
    let fill = Memo::new(move |_| Percentage(value.get().clamp(0.0, 1.0) * 100.0));
    let cap = Memo::new(move |_| Percentage(value.get().clamp(0.0, 1.0) * 100.0));

    let keys = lens.clone();
    Keys::new(move |_, code| {
        let step = match code {
            Code::ArrowRight | Code::ArrowUp => GAIN_STEP,
            Code::ArrowLeft | Code::ArrowDown => -GAIN_STEP,
            _ => return false,
        };
        let next = (value.get() + step).clamp(0.0, 1.0);
        keys.automate(param, f64::from(next));
        value.set(next);
        true
    })
    .build(cx, move |cx| {
        let down = lens.clone();
        let moved = lens.clone();
        let up = lens.clone();
        let reset = lens.clone();
        ZStack::new(cx, move |cx| {
            Element::new(cx).class("fader-fill").width(fill);
            Element::new(cx).class("fader-cap").left(cap);
        })
        .class("fader-track")
        .on_mouse_down(move |cx, button| {
            if button != MouseButton::Left {
                return;
            }
            cx.capture();
            cx.focus();
            let next = cursor_fraction(cx);
            down.begin_edit(param);
            down.set(param, f64::from(next));
            value.set(next);
        })
        .on_mouse_move(move |cx, _, _| {
            if cx.mouse().left.state != MouseButtonState::Pressed {
                return;
            }
            let next = cursor_fraction(cx);
            moved.set(param, f64::from(next));
            value.set(next);
        })
        .on_mouse_up(move |cx, button| {
            if button == MouseButton::Left {
                cx.release();
                up.end_edit(param);
            }
        })
        .on_double_click(move |_, button| {
            if button == MouseButton::Left {
                reset.automate(param, GAIN_DEFAULT);
                #[allow(clippy::cast_possible_truncation)]
                value.set(GAIN_DEFAULT as f32);
            }
        });
        Label::new(cx, readout).class("fader-readout");
    })
    .class("fader")
    .focusable(true)
    .navigable(true)
}

/// Cursor x as a `[0, 1]` fraction of the hovered entity's own bounds.
fn cursor_fraction(cx: &EventContext) -> f32 {
    let bounds = cx.bounds();
    if bounds.w <= 0.0 {
        return 0.0;
    }
    ((cx.mouse().cursor_x - bounds.x) / bounds.w).clamp(0.0, 1.0)
}

// ------------------------------------------------------------- segmented ---

/// Segmented switch over an arbitrary selection signal. The indicator eases
/// to the selected cell in ~120 ms. Used directly by the theme picker, which
/// is not a plugin parameter.
pub fn segmented_signal<'a, S: SignalGet<usize> + Copy + 'static>(
    cx: &'a mut Context,
    selected: S,
    labels: &'static [&'static str],
    pick: impl Fn(&mut EventContext, usize) + Send + Sync + 'static,
) -> Handle<'a, impl View> {
    let count = labels.len().max(1);
    let last = count - 1;
    let pick = Arc::new(pick);

    // Indicator position in cell units, eased toward the selection.
    #[allow(clippy::cast_precision_loss)]
    let slide = Signal::new(selected.get().min(last) as f32);

    let keys = Arc::clone(&pick);
    Keys::new(move |cx, code| {
        let at = selected.get().min(last);
        let next = match code {
            Code::ArrowRight if at < last => at + 1,
            Code::ArrowLeft if at > 0 => at - 1,
            _ => return false,
        };
        keys(cx, next);
        true
    })
    .build(cx, move |cx| {
        // vizia only transitions rule-derived styles, and the indicator's
        // `left` is a reactive inline style, so the ease lives here.
        on_tick(move || {
            #[allow(clippy::cast_precision_loss)]
            let target = selected.get().min(last) as f32;
            let at = slide.get();
            let next = if (target - at).abs() < 0.001 {
                target
            } else {
                at + (target - at) * 0.4
            };
            slide.set_if_changed(next);
        });

        let cell = 100.0 / index_f32(count);
        let offset = Memo::new(move |_| Percentage(slide.get() * cell));
        Element::new(cx)
            .class("segmented-indicator")
            .width(Percentage(cell))
            .left(offset);
        for (index, label) in labels.iter().enumerate() {
            let pick = Arc::clone(&pick);
            Label::new(cx, *label)
                .class("segmented-cell")
                .toggle_class("selected", Memo::new(move |_| selected.get() == index))
                .width(Percentage(cell))
                .on_press(move |cx| {
                    cx.focus();
                    pick(cx, index);
                });
        }
    })
    .class("segmented")
    .focusable(true)
    .navigable(true)
}

/// Segmented switch bound to a discrete plugin parameter.
pub fn segmented<'a>(
    cx: &'a mut Context,
    lens: ParamLens<RelayParams>,
    param: impl Into<u32> + Copy,
    labels: &'static [&'static str],
) -> Handle<'a, impl View> {
    let id: u32 = param.into();
    let count = labels.len().max(1);
    let value = lens.value_signal(id);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let selected = Memo::new(move |_| {
        let last = index_f32(count - 1).max(1.0);
        (value.get().clamp(0.0, 1.0) * last).round() as usize
    });
    segmented_signal(cx, selected, labels, move |_, index| {
        lens.automate(id, normalized_index(index, count));
        #[allow(clippy::cast_possible_truncation)]
        value.set(normalized_index(index, count) as f32);
    })
}

/// Enum index -> normalized parameter value.
pub fn normalized_index(index: usize, count: usize) -> f64 {
    let last = count.saturating_sub(1).max(1);
    #[allow(clippy::cast_precision_loss)]
    {
        index.min(last) as f64 / last as f64
    }
}

// Segment counts are single digits; the cast can't lose anything.
#[allow(clippy::cast_precision_loss)]
fn index_f32(index: usize) -> f32 {
    index as f32
}

// ------------------------------------------------------------------ lamp ---

/// Session-health lamp. Carries exactly one state class at a time.
pub fn lamp(cx: &mut Context, health: Signal<Health>) -> Handle<'_, impl View> {
    let mut element = Element::new(cx).class("lamp");
    for (name, state) in [
        ("off", Health::Off),
        ("failed", Health::Failed),
        ("asleep", Health::Asleep),
        ("ready", Health::Ready),
        ("pending", Health::Pending),
        ("live", Health::Live),
    ] {
        element = element.toggle_class(name, Memo::new(move |_| health.get() == state));
    }
    element
}

// ------------------------------------------------------------- key catch ---

/// Container view that forwards arrow keys to a closure. vizia's `Keymap`
/// only takes bare `fn` pointers, which can't carry a `ParamLens`.
type KeyHandler = dyn Fn(&mut EventContext, Code) -> bool;

struct Keys(Box<KeyHandler>);

impl Keys {
    fn new(handler: impl Fn(&mut EventContext, Code) -> bool + 'static) -> Self {
        Self(Box::new(handler))
    }
}

impl View for Keys {
    fn element(&self) -> Option<&'static str> {
        Some("keys")
    }

    fn event(&mut self, cx: &mut EventContext, event: &mut Event) {
        event.map(|window_event, meta| {
            if let WindowEvent::KeyDown(code, _) = window_event
                && (self.0)(cx, *code)
            {
                meta.consume();
            }
        });
    }
}

// ------------------------------------------------------------------ test ---

#[cfg(test)]
mod tests {
    use super::*;

    /// `assets/logo.svg` rendered from [`LOGO_CHEVRONS`] - the two must not
    /// drift apart.
    fn logo_svg() -> String {
        let mut svg = format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 100 100\" fill=\"none\" \
             stroke=\"currentColor\" stroke-width=\"{LOGO_STROKE:.0}\" stroke-linecap=\"round\" \
             stroke-linejoin=\"round\">\n  <circle cx=\"{:.0}\" cy=\"{:.0}\" r=\"{:.0}\" \
             fill=\"currentColor\" stroke=\"none\"/>\n",
            LOGO_DOT[0], LOGO_DOT[1], LOGO_DOT[2]
        );
        for c in LOGO_CHEVRONS {
            svg.push_str(&format!(
                "  <path d=\"M{:.0} {:.0}L{:.0} {:.0}L{:.0} {:.0}\"/>\n",
                c[0][0], c[0][1], c[1][0], c[1][1], c[2][0], c[2][1]
            ));
        }
        svg.push_str("</svg>\n");
        svg
    }

    #[test]
    fn logo_svg_matches_the_drawn_path() {
        let on_disk = include_str!("../../assets/logo.svg");
        assert_eq!(
            on_disk,
            logo_svg(),
            "regenerate apps/plugin/assets/logo.svg"
        );
    }

    #[test]
    fn normalized_index_spans_the_unit_range() {
        let close = |a: f64, b: f64| (a - b).abs() < 1e-9;
        assert!(close(normalized_index(0, 2), 0.0));
        assert!(close(normalized_index(1, 2), 1.0));
        assert!(close(normalized_index(1, 3), 0.5));
        assert!(close(normalized_index(9, 3), 1.0));
        assert!(close(normalized_index(0, 1), 0.0));
    }
}
