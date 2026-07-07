//! Param-binding widget helpers - the truce-vizia counterpart of
//! `truce_egui::widgets` / `truce_iced::widgets`.
//!
//! Widgets render against vizia's default theme: no colors are
//! prescribed here, and the `BASE_CSS` constant carries only the
//! minimum CSS needed to work around vizia bugs (the collapsing
//! knob head, the popup overflow, and the popup arrow we don't
//! want). Plugin authors who want a particular palette layer their
//! own stylesheet via `ViziaEditor::with_stylesheet`; widgets stay
//! palette-agnostic so they slot into any look.
//!
//! Each widget tags its outer container with a `truce-*` class
//! (`truce-knob`, `truce-slider`, `truce-toggle`, ...). The class is
//! a styling hook only - widgets work without any CSS targeting them.

#![allow(clippy::needless_pass_by_value)]

use truce_params::Params;
use vizia::prelude::*;

use crate::param_lens::ParamLens;

/// Minimum CSS the widgets need to render correctly.
///
/// Carries only vizia compatibility shims: the knob-head explicit
/// size (vizia's default `width: 1s` collapses to zero under the
/// absolute-position + stretch combo we end up with), and the
/// dropdown popup workarounds (the arrow vizia builds even when
/// `show_arrow(false)` is the initial state, and the trigger-width
/// clamp that clips option grids wider than the trigger).
///
/// Opt in via `ViziaEditor::with_stylesheet(BASE_CSS)`. Without it
/// the knob renders with its needle outside the round body and the
/// dropdown popup overflows / shows an arrow.
pub const BASE_CSS: &str = include_str!("base.css");

/// Single labelled knob cell: knob on top, current formatted value
/// in the middle, name label at the bottom. Matches the layout the
/// built-in / egui / iced / slint backends use - the value sits
/// close to the knob arc that drove it.
///
/// Fixed at 48×48px. The inner head is pinned to 44×44 in
/// [`BASE_CSS`] to work around a vizia layout bug (default
/// `.knob-head` styling collapses to zero in our layout path), and
/// neither percentage nor stretch sizing of the head produced a
/// visible knob. A `size` arg is reserved for a future revision
/// that builds the knob via `vizia::Knob::custom` so the head can
/// scale with the outer.
pub fn param_knob<P: Params + 'static>(
    cx: &mut Context,
    lens: ParamLens<P>,
    id: impl Into<u32> + Copy,
    label: &str,
) {
    let id_u32: u32 = id.into();
    let steps = lens.step_count(id);
    let initial = snap_normalized(lens.get(id), steps);
    // Shared per-param Signal: every widget for `id` reads/writes the
    // same handle, so an XY pad / selector / other knob bound to the
    // same param updates this knob's arc + tick in lockstep.
    let value_signal = lens.value_signal(id);
    let label_text = label.to_string();
    // Memo'd display string: re-evaluates whenever `value_signal`
    // changes, calling back through the truce lens to pick up the
    // freshly-`automate`d plain-formatted value. The on_change handler
    // updates the signal before the lens write, so by the time the
    // Memo re-runs the atomic store already holds the new value.
    let lens_for_display = lens.clone();
    let display = Memo::new(move |_| {
        let _ = value_signal.get();
        lens_for_display.format(id_u32)
    });
    // Lock the value-label slot to fit the *widest* string this
    // param can ever format to. Without this the cell width tracks
    // the live value, so a continuous knob shrinks (e.g. "0.0 dB" ->
    // "-60.0 dB" adds two chars) and pushes every cell to its right.
    // Estimated in pixels from the char count at the BASE_CSS 11px
    // monospace size; the small +pad keeps neighbour-touching cells
    // from kissing when the longest string is rendered.
    #[allow(clippy::cast_precision_loss)]
    let value_slot_w = lens.widest_format_chars(id) as f32 * VALUE_CHAR_W_PX + VALUE_PAD_PX;

    VStack::new(cx, move |cx| {
        Knob::new(cx, initial, value_signal, false)
            .width(Pixels(48.0))
            .height(Pixels(48.0))
            .on_change(move |_cx, val| {
                // Discrete params snap to the nearest grid step so
                // the host sees integer / enum positions, not the
                // raw continuous value baseview hands us. Continuous
                // params pass through unchanged.
                let snapped = snap_normalized(val, steps);
                // Write to the store *before* nudging the signal so
                // the formatted-value Memo (which keys off
                // `value_signal` and re-reads `lens.format(id)`)
                // sees the freshly-automated value, not a stale one.
                lens.automate(id_u32, f64::from(snapped));
                // vizia's Knob only repaints its track when the
                // external `value` signal changes; the internal
                // `continuous_normal` field drives the head but not
                // the arc. Echoing the new value back into the
                // signal keeps both halves of the widget in sync
                // during drag.
                value_signal.set(snapped);
            });
        Label::new(cx, display)
            .width(Pixels(value_slot_w))
            .class("truce-knob-value");
        Label::new(cx, label_text);
    })
    .class("truce-knob")
    .width(Auto)
    .height(Auto)
    .vertical_gap(Pixels(2.0))
    .alignment(Alignment::Center);
}

/// Per-character advance estimate for the [`BASE_CSS`] value-label
/// font (`JetBrains` Mono / monospace fallback at 11px). 6.6px is
/// the real advance; we use a hair more so a string that just fits
/// at the estimate doesn't collide with a one-pixel rounding error
/// and trigger the cell to grow.
const VALUE_CHAR_W_PX: f32 = 7.0;
/// Extra breathing room added on top of the char-count estimate so
/// neighbouring cells don't visually kiss at the widest value.
const VALUE_PAD_PX: f32 = 2.0;

/// Single labelled slider cell: label on top, horizontal slider
/// below, current formatted value underneath. `width` controls the
/// cell's horizontal size (and therefore the slider track length) -
/// useful when one param's range is much wider than another's and
/// you want the visual width to reflect that.
pub fn param_slider<P: Params + 'static>(
    cx: &mut Context,
    lens: ParamLens<P>,
    id: impl Into<u32> + Copy,
    label: &str,
    width: f32,
) {
    let id_u32: u32 = id.into();
    let steps = lens.step_count(id);
    let value_signal = lens.value_signal(id);
    let label_text = label.to_string();
    let lens_for_display = lens.clone();
    let display = Memo::new(move |_| {
        let _ = value_signal.get();
        lens_for_display.format(id_u32)
    });
    // `Slider::step` controls the grid the visual thumb snaps to;
    // give it 1/steps for discrete params so the thumb itself jumps
    // between integer positions instead of free-running. Defaults to
    // the vizia continuous step (0.01) for non-discrete params.
    let step = step_size(steps);

    VStack::new(cx, move |cx| {
        Label::new(cx, label_text);
        Slider::new(cx, value_signal)
            .range(0.0..1.0)
            .step(step)
            .on_change(move |_cx, val| {
                let snapped = snap_normalized(val, steps);
                // Same ordering rationale as `param_knob`: store
                // first, signal second, so the value-label Memo
                // reads the new automated value.
                lens.automate(id_u32, f64::from(snapped));
                value_signal.set(snapped);
            });
        Label::new(cx, display);
    })
    .class("truce-slider")
    .width(Pixels(width))
    .height(Auto)
    .vertical_gap(Pixels(2.0));
}

/// Single labelled toggle: a vizia `Switch` next to a label.
pub fn param_toggle<P: Params + 'static>(
    cx: &mut Context,
    lens: ParamLens<P>,
    id: impl Into<u32> + Copy,
    label: &str,
) {
    let id_u32: u32 = id.into();
    // Boolean params still live in the shared `f32` Signal map; the
    // bool view derives from `> 0.5` on the wire so anything writing
    // through the param store (XY pad, automation, another toggle
    // bound to the same id) flips the switch automatically.
    let value_signal = lens.value_signal(id);
    let checked_signal = Memo::new(move |_| value_signal.get() > 0.5);
    let label_text = label.to_string();

    HStack::new(cx, move |cx| {
        Switch::new(cx, checked_signal).on_toggle(move |_cx| {
            let now = !checked_signal.get();
            lens.automate(id_u32, if now { 1.0 } else { 0.0 });
            value_signal.set(if now { 1.0 } else { 0.0 });
        });
        Label::new(cx, label_text);
    })
    .class("truce-toggle")
    .width(Auto)
    .height(Auto)
    .horizontal_gap(Pixels(6.0))
    .alignment(Alignment::Center);
}

/// Dropdown trigger that shows the current formatted value; popup
/// shows `count` options arranged into `cols` columns. `option_width`
/// is the per-option cell width in pixels and is also used for the
/// trigger button - sizing them together keeps the popup grid in
/// proportion with whatever the trigger shows, and stops the
/// trigger from collapsing when its parent uses `width: Auto`.
pub fn param_dropdown<P: Params + 'static>(
    cx: &mut Context,
    lens: ParamLens<P>,
    id: impl Into<u32> + Copy,
    label: &str,
    count: usize,
    cols: usize,
    option_width: f32,
) {
    let id_u32: u32 = id.into();
    let label_text = label.to_string();
    let value_signal = lens.value_signal(id);
    // Memo'd trigger label: re-evaluates whenever the shared value
    // signal flips, calling back through the truce lens to read the
    // freshly-`automate`d plain-formatted value. Without the Memo
    // the trigger text would be frozen at the on-build snapshot.
    let lens_for_trigger = lens.clone();
    let trigger_text = Memo::new(move |_| {
        let _ = value_signal.get();
        lens_for_trigger.format(id_u32)
    });

    VStack::new(cx, move |cx| {
        Label::new(cx, label_text);
        Dropdown::new(
            cx,
            // Trigger button: emit `PopupEvent::Switch` to toggle the
            // popup open / closed. Without this, the button is just a
            // labelled square that does nothing on click. Pattern
            // follows vizia's `examples/views/dropdown.rs`.
            move |cx| {
                Button::new(cx, |cx| Label::new(cx, trigger_text))
                    .width(Pixels(option_width))
                    .on_press(|cx| cx.emit(PopupEvent::Switch));
            },
            move |cx| {
                // `cols` lays the options out in a coarse grid;
                // count is small (<= 8 for the zoo's `Mode` enum),
                // so a manual chunked HStack is cheaper than wiring
                // vizia's `Grid` for so few cells.
                let rows = count.div_ceil(cols.max(1));
                let lens_for_popup = lens.clone();
                VStack::new(cx, move |cx| {
                    for row in 0..rows {
                        let lens_row = lens_for_popup.clone();
                        HStack::new(cx, move |cx| {
                            for col in 0..cols.max(1) {
                                let i = row * cols.max(1) + col;
                                if i >= count {
                                    break;
                                }
                                let lens_for_pick = lens_row.clone();
                                let option_label = lens_for_pick.step_label(id_u32, i);
                                Button::new(cx, move |cx| {
                                    Label::new(cx, option_label.clone()).hoverable(false)
                                })
                                .width(Pixels(option_width))
                                .on_press(move |cx| {
                                    // Order matters: write the store
                                    // *before* nudging the signal so
                                    // the trigger-text Memo reads the
                                    // freshly-automated value rather
                                    // than the previous click's.
                                    let new_norm = normalized_for_step(i, count);
                                    lens_for_pick.automate(id_u32, new_norm);
                                    #[allow(clippy::cast_possible_truncation)]
                                    value_signal.set(new_norm as f32);
                                    cx.emit(PopupEvent::Close);
                                });
                            }
                        })
                        // HStack defaults to `width: 1s` which can
                        // disagree with the popup's content-sized
                        // width and leave the right column
                        // overflowing the popup body. Pin both axes
                        // to auto so the rows size to their buttons.
                        .width(Auto)
                        .height(Auto);
                    }
                })
                .width(Auto)
                .height(Auto);
            },
        )
        // Drop vizia's default speech-bubble arrow on the popup -
        // it was rendering a small triangle artifact inside our
        // popup. Belt-and-braces: `popup arrow { display: none }`
        // in `BASE_CSS` also suppresses it.
        .show_arrow(false);
    })
    .class("truce-dropdown")
    .width(Auto)
    .height(Auto)
    .vertical_gap(Pixels(2.0));
}

/// Vertical level meter with one bar per supplied meter id.
///
/// Each bar binds its `padding-top` to a Memo over the shared meter
/// signal from [`ParamLens::meter_signal`]. The editor's root timer
/// (registered in `ViziaEditor::open`) calls
/// [`ParamLens::refresh_meters`] ~30Hz, which fans the latest store
/// values into every registered meter signal. vizia's reactive graph
/// then re-evaluates the per-bar Memos and the fill height tracks.
pub fn level_meter<P: Params + 'static>(
    cx: &mut Context,
    lens: ParamLens<P>,
    meter_ids: &[impl Into<u32> + Copy],
    height: impl Into<Units>,
) {
    let signals: Vec<Signal<f32>> = meter_ids.iter().map(|id| lens.meter_signal(*id)).collect();
    let channels = signals.len().max(1);
    // 4px bar + 2px gap per egui's level_meter constants; minimum
    // width matches `MIN_METER_W = 16` so a mono bar still has
    // sensible horizontal weight.
    let width = (small_count_as_f32(channels) * 4.0
        + small_count_as_f32(channels.saturating_sub(1)) * 2.0)
        .max(16.0);

    HStack::new(cx, move |cx| {
        for sig in signals {
            let pad = Memo::new(move |_| Percentage(100.0 - sig.get().clamp(0.0, 1.0) * 100.0));
            let is_clip = Memo::new(move |_| sig.get() > 0.95);
            VStack::new(cx, move |cx| {
                Element::new(cx)
                    .class("truce-meter-fill")
                    .toggle_class("truce-meter-fill-clip", is_clip)
                    .width(Stretch(1.0));
            })
            .class("truce-meter-bar")
            .width(Stretch(1.0))
            .height(Percentage(100.0))
            .padding_top(pad)
            .alignment(Alignment::BottomCenter);
        }
    })
    .class("truce-meter")
    // `height` is `Units`: `Pixels(240.0)` for a fixed band,
    // `Stretch(1.0)` to fill whatever vertical space the parent
    // layout hands the meter. Bar geometry is already percentage-
    // and stretch-based internally so the meter resizes cleanly.
    .height(height.into())
    .width(Pixels(width))
    .horizontal_gap(Pixels(2.0))
    .alignment(Alignment::BottomCenter);
}

/// Radius of the XY pad's draggable dot (the dot is `2 * radius` px).
const XY_DOT_RADIUS: f32 = 4.0;

/// XY pad: two-axis pad whose dot position tracks two truce params.
/// Click/drag inside the pad to set both x and y simultaneously; the
/// dot follows the cursor and the params are written through the
/// lens (`begin_edit` / `set` / `end_edit` via `automate`-style
/// gestures the host honours as user automation).
///
/// `w` / `h` are `Units`: `Pixels(130.0)` for a fixed square,
/// `Stretch(1.0)` to fill whatever the parent layout hands the pad.
/// The dot is 8x8 and tracks the runtime pad bounds via percentage
/// positioning + a pixel-level translate that centres the dot on
/// the value point; the y-axis is inverted from vizia's screen
/// coordinates so higher param values sit at the *top* of the pad -
/// the convention every other backend follows.
#[allow(clippy::cast_possible_truncation, clippy::similar_names)]
pub fn param_xy_pad<P: Params + 'static>(
    cx: &mut Context,
    lens: ParamLens<P>,
    x_id: impl Into<u32> + Copy,
    y_id: impl Into<u32> + Copy,
    label: &str,
    w: impl Into<Units>,
    h: impl Into<Units>,
) {
    let x_u32: u32 = x_id.into();
    let y_u32: u32 = y_id.into();
    let label_text = label.to_string();
    let pad_w = w.into();
    let pad_h = h.into();
    // Outer cell mirrors `Stretch(_)` so the inner ZStack has
    // stretch space to fill; for `Pixels(_)` we keep `Auto` so the
    // cell auto-fits the pad + label like the pre-stretch API did
    // (caller still gets an `N x (pad_h + label_h)` cell).
    let outer_w = if matches!(pad_w, Units::Stretch(_)) {
        pad_w
    } else {
        Units::Auto
    };
    let outer_h = if matches!(pad_h, Units::Stretch(_)) {
        pad_h
    } else {
        Units::Auto
    };
    // Shared per-param signals: dragging the pad updates these and
    // every other widget bound to `x_id` / `y_id` (knobs, sliders,
    // dropdowns, other pads) follows automatically through the
    // vizia reactive graph.
    let x_norm = lens.value_signal(x_id);
    let y_norm = lens.value_signal(y_id);
    let is_dragging = Signal::new(false);

    // Dot position via percentage of the pad's runtime bounds so the
    // dot follows live `Stretch(_)` resizing. The `.left` / `.top`
    // modifiers anchor the dot's top-left corner at the value
    // percentage; a constant `.translate` of minus one radius re-centers
    // the dot on that point. So the dot's *center* sits on the true pad
    // edge at a value extreme and its outer half spills past the border,
    // drawn unclipped (vizia doesn't clip the dot to the pad).
    let dot_left = Memo::new(move |_| Percentage(x_norm.get() * 100.0));
    let dot_top = Memo::new(move |_| Percentage((1.0 - y_norm.get()) * 100.0));
    let dot_translate = Translate {
        x: LengthOrPercentage::Length(Length::px(-XY_DOT_RADIUS)),
        y: LengthOrPercentage::Length(Length::px(-XY_DOT_RADIUS)),
    };

    let lens_for_down = lens.clone();
    let lens_for_move = lens.clone();
    let lens_for_up = lens.clone();

    VStack::new(cx, move |cx| {
        ZStack::new(cx, move |cx| {
            Element::new(cx)
                .class("truce-xy-pad-dot")
                .width(Pixels(XY_DOT_RADIUS * 2.0))
                .height(Pixels(XY_DOT_RADIUS * 2.0))
                .corner_radius(Percentage(50.0))
                .position_type(PositionType::Absolute)
                .left(dot_left)
                .top(dot_top)
                .translate(dot_translate);
        })
        .class("truce-xy-pad")
        .width(pad_w)
        .height(pad_h)
        .on_mouse_down(move |cx, button| {
            if button != MouseButton::Left {
                return;
            }
            // `cx.capture()` routes every subsequent mouse event to
            // this entity regardless of where the cursor is - without
            // it, dragging outside the pad bounds stops firing
            // `on_mouse_move` so the dot freezes at the boundary
            // while the user's mouse keeps moving.
            cx.capture();
            is_dragging.set(true);
            let (nx, ny) = cursor_to_normalized(cx);
            x_norm.set(nx);
            y_norm.set(ny);
            lens_for_down.begin_edit(x_u32);
            lens_for_down.set(x_u32, f64::from(nx));
            lens_for_down.begin_edit(y_u32);
            lens_for_down.set(y_u32, f64::from(ny));
        })
        .on_mouse_move(move |cx, _x, _y| {
            if !is_dragging.get() {
                return;
            }
            let (nx, ny) = cursor_to_normalized(cx);
            x_norm.set(nx);
            y_norm.set(ny);
            lens_for_move.set(x_u32, f64::from(nx));
            lens_for_move.set(y_u32, f64::from(ny));
        })
        .on_mouse_up(move |cx, button| {
            if button != MouseButton::Left || !is_dragging.get() {
                return;
            }
            is_dragging.set(false);
            cx.release();
            lens_for_up.end_edit(x_u32);
            lens_for_up.end_edit(y_u32);
        });
        Label::new(cx, label_text);
    })
    .class("truce-xy-pad-cell")
    .width(outer_w)
    .height(outer_h)
    .vertical_gap(Pixels(2.0))
    .alignment(Alignment::TopCenter);
}

/// Convert the current cursor position to normalised `(x, y)` pad
/// coordinates using vizia's own coordinate system (`bounds` and
/// `MouseState::cursor_x/y` share the same physical-pixel space, so
/// dividing the cursor offset by `bounds.w` / `bounds.h` cancels the
/// host DPI factor automatically - mixing in the *logical* pad size
/// `w` / `h` would double-count it on Retina, leaving clicks off by
/// the device's content scale). y is flipped so the audio convention
/// "up = high" matches the visual dot position.
fn cursor_to_normalized(cx: &EventContext) -> (f32, f32) {
    let bounds = cx.bounds();
    let mouse = cx.mouse();
    if bounds.w <= 0.0 || bounds.h <= 0.0 {
        return (0.0, 0.0);
    }
    let lx = (mouse.cursor_x - bounds.x).clamp(0.0, bounds.w);
    let ly = (mouse.cursor_y - bounds.y).clamp(0.0, bounds.h);
    let nx = (lx / bounds.w).clamp(0.0, 1.0);
    let ny = 1.0 - (ly / bounds.h).clamp(0.0, 1.0);
    (nx, ny)
}

fn normalized_for_step(step: usize, count: usize) -> f64 {
    if count <= 1 {
        return 0.0;
    }
    small_count_as_f64(step) / small_count_as_f64(count - 1)
}

// Widget cell counts (channel counts, step counts, dropdown option
// counts) are bounded by what fits on screen - a handful, not the
// `usize::MAX` clippy worries about. Keep the `as f32`/`as f64`
// confined to these helpers with a one-line WHY.
#[allow(clippy::cast_precision_loss)]
fn small_count_as_f32(n: usize) -> f32 {
    n as f32
}

#[allow(clippy::cast_precision_loss)]
fn small_count_as_f64(n: usize) -> f64 {
    n as f64
}

/// Snap a normalized `[0, 1]` value to the nearest grid step for a
/// discrete range with `steps` *intervals* (so `steps = 10` means 11
/// distinct positions). `None` (continuous) passes through unchanged.
fn snap_normalized(value: f32, steps: Option<u32>) -> f32 {
    let Some(steps) = steps else {
        return value;
    };
    if steps == 0 {
        return value;
    }
    let v = value.clamp(0.0, 1.0);
    let grid = small_count_as_f32(steps as usize);
    (v * grid).round() / grid
}

/// Per-step distance in normalized space for `Slider::step`. Discrete
/// ranges get `1 / steps`; continuous ranges keep vizia's default
/// `0.01` so wheel / arrow nudges still feel smooth.
fn step_size(steps: Option<u32>) -> f32 {
    match steps {
        Some(n) if n > 0 => 1.0 / small_count_as_f32(n as usize),
        _ => 0.01,
    }
}
