//! Custom drawn views for the RELAY editor.
//!
//! ponytail: placeholder implementations, replaced by `feat/vizia-views`.
//! They satisfy the agreed contract (one function per view, every colour
//! resolved from a CSS class, structure-only rules in `views.css`) with the
//! smallest composition that renders and behaves correctly.

use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use truce_vizia::ParamLens;
use truce_vizia::vizia::prelude::*;
use truce_vizia::vizia::vg;

use crate::meter::{PeakHold, db_to_pos, peak_to_db};
use crate::spectrum::SpectrumTap;
use crate::status::Health;
use crate::{P, RelayParams};

/// UI refresh cadence, matching the editor's meter fan-out.
pub const FRAME: Duration = Duration::from_millis(33);

/// Number of spectrum bands `crate::spectrum::Spectrum` produces.
const BANDS: usize = 40;

/// 0 dB on the -24..+12 dB gain range, normalized.
pub const GAIN_DEFAULT: f32 = 24.0 / 36.0;

// ------------------------------------------------------------------- logo

/// The RELAY mark: a square stroked glyph that inherits `color` from CSS.
pub struct LogoMark;

impl View for LogoMark {
    fn element(&self) -> Option<&'static str> {
        Some("logo")
    }

    fn draw(&self, cx: &mut DrawContext, canvas: &Canvas) {
        let bounds = cx.bounds();
        let size = bounds.w.min(bounds.h);
        if size <= 0.0 {
            return;
        }
        let unit = size / 100.0;
        let at = |x: f32, y: f32| {
            vg::Point::new(
                bounds.x + (bounds.w - size) / 2.0 + x * unit,
                bounds.y + (bounds.h - size) / 2.0 + y * unit,
            )
        };
        let mut path = vg::PathBuilder::new();
        path.move_to(at(20.0, 82.0));
        path.line_to(at(20.0, 20.0));
        path.line_to(at(56.0, 20.0));
        path.cubic_to(at(71.0, 20.0), at(80.0, 28.0), at(80.0, 40.0));
        path.cubic_to(at(80.0, 52.0), at(71.0, 60.0), at(56.0, 60.0));
        path.line_to(at(42.0, 60.0));
        path.move_to(at(52.0, 69.0));
        path.line_to(at(70.0, 82.0));

        let color = cx.font_color();
        let mut paint = vg::Paint::default();
        paint.set_anti_alias(true);
        paint.set_style(vg::PaintStyle::Stroke);
        paint.set_stroke_width(size * 0.13);
        paint.set_stroke_cap(vg::PaintCap::Round);
        paint.set_stroke_join(vg::PaintJoin::Round);
        paint.set_color(vg::Color::from_argb(
            color.a(),
            color.r(),
            color.g(),
            color.b(),
        ));
        canvas.draw_path(&path.detach(), &paint);
    }
}

/// Square logo mark; strokes with the inherited `font_color()`.
pub fn logo_mark(cx: &mut Context) -> Handle<'_, impl View> {
    LogoMark.build(cx, |_| {}).class("logo")
}

// ------------------------------------------------------------------ tick

thread_local! {
    /// Per-tick work registered by the views below.
    ///
    /// ponytail: vizia 0.4's `Context::modify_timer` spins forever when a
    /// second timer is started (`vizia_core/src/context/mod.rs:857` peeks the
    /// running-timer heap without popping when the top isn't the requested
    /// timer), so the whole editor gets exactly one timer, owned by
    /// `editor::view`, and every view hangs its 30 fps work off this list.
    /// Drop it for per-view timers once vizia is fixed.
    static TICKS: RefCell<Vec<Box<dyn FnMut()>>> = const { RefCell::new(Vec::new()) };
}

/// Drop every registered tick. Called when a view tree is (re)built.
pub fn clear_ticks() {
    TICKS.with_borrow_mut(Vec::clear);
}

/// Register 30 fps work for the editor's single timer.
pub fn on_tick(work: impl FnMut() + 'static) {
    TICKS.with_borrow_mut(|ticks| ticks.push(Box::new(work)));
}

/// Run every registered tick once.
pub fn tick_all() {
    TICKS.with_borrow_mut(|ticks| {
        for tick in ticks.iter_mut() {
            tick();
        }
    });
}

// ----------------------------------------------------------------- meters

/// Stereo GYR peak meters with peak-hold ticks, L/R captions and dB
/// readouts. Fills whatever the parent hands it.
pub fn stereo_meters(cx: &mut Context, lens: ParamLens<RelayParams>) -> Handle<'_, impl View> {
    let signals = [
        lens.meter_signal(P::MeterLeft),
        lens.meter_signal(P::MeterRight),
    ];
    let held = Signal::new([0.0f32; 2]);
    let mut holds = [PeakHold::default(); 2];
    on_tick(move || {
        held.set([
            holds[0].update(signals[0].get()),
            holds[1].update(signals[1].get()),
        ]);
    });

    HStack::new(cx, move |cx| {
        for (channel, peak) in signals.into_iter().enumerate() {
            VStack::new(cx, move |cx| {
                Label::new(cx, if channel == 0 { "L" } else { "R" }).class("meters-label");
                // Rail carries the GYR gradient; the cover masks it from the
                // top down to the current level, so the colour at any height
                // is fixed to the scale rather than stretched to the fill.
                VStack::new(cx, move |cx| {
                    let cover = Memo::new(move |_| {
                        Percentage(100.0 - db_to_pos(peak_to_db(peak.get())) * 100.0)
                    });
                    Element::new(cx).class("meters-cover").height(cover);
                    Element::new(cx)
                        .class("meters-hold")
                        .top(Memo::new(move |_| {
                            Percentage(100.0 - db_to_pos(peak_to_db(held.get()[channel])) * 100.0)
                        }))
                        .position_type(PositionType::Absolute)
                        .toggle_class("meters-clip", Memo::new(move |_| peak.get() >= 1.0));
                })
                .class("meters-rail")
                .toggle_class("meters-fill", Memo::new(move |_| peak.get() > 0.0));
                Label::new(
                    cx,
                    Memo::new(move |_| format!("{:.0}", peak_to_db(peak.get()))),
                )
                .class("meters-db");
            })
            .class("meters-channel");
        }
    })
    .class("meters")
}

// --------------------------------------------------------------- spectrum

/// Owns the analysis tap for the editor's lifetime: arms it on build and
/// disarms it when the view tree is dropped (editor close).
pub struct SpectrumWell {
    tap: Arc<SpectrumTap>,
}

impl View for SpectrumWell {
    fn element(&self) -> Option<&'static str> {
        Some("spectrum")
    }
}

impl Drop for SpectrumWell {
    fn drop(&mut self) {
        self.tap.active.store(false, Ordering::Relaxed);
    }
}

/// Send spectrum: 40 logarithmic bands refreshed at 30 fps.
pub fn spectrum(cx: &mut Context, tap: Arc<SpectrumTap>) -> Handle<'_, impl View> {
    tap.audio.clear();
    tap.active.store(true, Ordering::Relaxed);

    let bands = Signal::new([0.0f32; BANDS]);
    let mut analyzer = crate::spectrum::Spectrum::default();
    let tick_tap = Arc::clone(&tap);
    on_tick(move || {
        analyzer.update(&tick_tap);
        bands.set(analyzer.bands);
    });

    SpectrumWell { tap }
        .build(cx, move |cx| {
            for band in 0..BANDS {
                Element::new(cx)
                    .class("spectrum-bar")
                    .height(Memo::new(move |_| {
                        Percentage((bands.get()[band] * 100.0).max(2.0))
                    }));
            }
        })
        .class("spectrum")
}

// ------------------------------------------------------------------ fader

/// Interactive track of a [`fader`]: drag, click to set, double-click to
/// reset to 0 dB, arrow keys to nudge.
pub struct FaderTrack {
    id: u32,
    lens: ParamLens<RelayParams>,
    value: Signal<f32>,
    dragging: bool,
}

impl FaderTrack {
    /// Cursor x within the track, normalized.
    fn at_cursor(cx: &EventContext) -> f32 {
        let bounds = cx.bounds();
        if bounds.w <= 0.0 {
            return 0.0;
        }
        ((cx.mouse().cursor_x - bounds.x) / bounds.w).clamp(0.0, 1.0)
    }

    fn nudge(&self, step: f32) {
        let next = (self.value.get() + step).clamp(0.0, 1.0);
        self.value.set(next);
        self.lens.automate(self.id, f64::from(next));
    }
}

impl View for FaderTrack {
    fn element(&self) -> Option<&'static str> {
        Some("fader-track")
    }

    fn event(&mut self, cx: &mut EventContext, event: &mut Event) {
        event.map(|window_event, meta| match window_event {
            WindowEvent::MouseDown(MouseButton::Left) => {
                cx.focus();
                cx.capture();
                self.dragging = true;
                let value = Self::at_cursor(cx);
                self.value.set(value);
                self.lens.begin_edit(self.id);
                self.lens.set(self.id, f64::from(value));
                meta.consume();
            }
            WindowEvent::MouseMove(_, _) if self.dragging => {
                let value = Self::at_cursor(cx);
                self.value.set(value);
                self.lens.set(self.id, f64::from(value));
            }
            WindowEvent::MouseUp(MouseButton::Left) if self.dragging => {
                self.dragging = false;
                cx.release();
                self.lens.end_edit(self.id);
                meta.consume();
            }
            WindowEvent::MouseDoubleClick(MouseButton::Left) => {
                self.value.set(GAIN_DEFAULT);
                self.lens.automate(self.id, f64::from(GAIN_DEFAULT));
                meta.consume();
            }
            // One dB per press on the -24..+12 dB range.
            WindowEvent::KeyDown(Code::ArrowRight | Code::ArrowUp, _) => self.nudge(1.0 / 36.0),
            WindowEvent::KeyDown(Code::ArrowLeft | Code::ArrowDown, _) => self.nudge(-1.0 / 36.0),
            _ => {}
        });
    }
}

/// Horizontal gain fader with a right-aligned dB readout.
pub fn fader(
    cx: &mut Context,
    lens: ParamLens<RelayParams>,
    param: impl Into<u32> + Copy,
) -> Handle<'_, impl View> {
    let id: u32 = param.into();
    let value = lens.value_signal(id);
    let readout_lens = lens.clone();
    let readout = Memo::new(move |_| {
        let _ = value.get();
        readout_lens.format(id)
    });

    HStack::new(cx, move |cx| {
        FaderTrack {
            id,
            lens,
            value,
            dragging: false,
        }
        .build(cx, move |cx| {
            Element::new(cx)
                .class("fader-fill")
                .width(Memo::new(move |_| Percentage(value.get() * 100.0)));
            Element::new(cx)
                .class("fader-cap")
                .position_type(PositionType::Absolute)
                .left(Memo::new(move |_| Percentage(value.get() * 100.0)));
        })
        .class("fader-track")
        .focusable(true);
        Label::new(cx, readout).class("fader-readout");
    })
    .class("fader")
}

// -------------------------------------------------------------- segmented

/// Segmented switch driven by an arbitrary selection signal. Used for the
/// theme picker, which is not a plugin parameter.
pub fn segmented_signal<'a, S: SignalGet<usize> + Copy + 'static>(
    cx: &'a mut Context,
    selected: S,
    labels: &'static [&'static str],
    pick: impl Fn(&mut EventContext, usize) + Send + Sync + 'static,
) -> Handle<'a, impl View> {
    let count = labels.len().max(1);
    let pick = Arc::new(pick);
    HStack::new(cx, move |cx| {
        #[allow(clippy::cast_precision_loss)]
        let step = 100.0 / count as f32;
        Element::new(cx)
            .class("segmented-indicator")
            .position_type(PositionType::Absolute)
            .width(Percentage(step))
            .left(Memo::new(move |_| {
                #[allow(clippy::cast_precision_loss)]
                Percentage(selected.get().min(count - 1) as f32 * step)
            }));
        for (index, label) in labels.iter().enumerate() {
            let pick = Arc::clone(&pick);
            Label::new(cx, *label)
                .class("segmented-cell")
                .toggle_class("selected", Memo::new(move |_| selected.get() == index))
                .on_press(move |cx| pick(cx, index));
        }
    })
    .class("segmented")
}

/// Segmented switch bound to a discrete plugin parameter.
pub fn segmented<'a>(
    cx: &'a mut Context,
    lens: ParamLens<RelayParams>,
    param: impl Into<u32> + Copy,
    labels: &'static [&'static str],
) -> Handle<'a, impl View> {
    let id: u32 = param.into();
    let value = lens.value_signal(id);
    let count = labels.len().max(1);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let selected = Memo::new(move |_| {
        #[allow(clippy::cast_precision_loss)]
        let last = (count - 1).max(1) as f32;
        (value.get() * last).round().clamp(0.0, last) as usize
    });
    segmented_signal(cx, selected, labels, move |_cx, index| {
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

// ------------------------------------------------------------------- lamp

/// Status lamp. Colour comes from the state class; the shape is a circle.
pub fn lamp(cx: &mut Context, health: Signal<Health>) -> Handle<'_, impl View> {
    let state = |want: Health| Memo::new(move |_| health.get() == want);
    Element::new(cx)
        .class("lamp")
        .toggle_class("off", state(Health::Off))
        .toggle_class("failed", state(Health::Failed))
        .toggle_class("asleep", state(Health::Asleep))
        .toggle_class("ready", state(Health::Ready))
        .toggle_class("pending", state(Health::Pending))
        .toggle_class("live", state(Health::Live))
}

#[cfg(test)]
mod tests {
    use super::normalized_index;

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
