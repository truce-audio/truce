//! A MIDI inspector: an audio effect with MIDI in/out that passes the
//! track's audio through untouched, forwards MIDI through, and shows a
//! live, scrolling log of every event it receives in `process()` (newest
//! first), decoded as far as truce understands the message - with a raw
//! line for anything it doesn't, so adding interpretations later is easy.
//!
//! Notable bit: streaming *structured* realtime data (not just scalar
//! meters) from the audio thread to the editor. The plugin owns a
//! lock-free `EventRing`; `process()` pushes decoded events into it,
//! and the iced editor - handed the same `Arc` through
//! `IcedEditor::with_plugin_factory` - drains it each frame in `view()`.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::Arc;

use truce_iced::iced::widget::{Column, Row, button, checkbox, container, scrollable, text};
use truce_iced::iced::{Color, Element, Font, Length, Task, alignment};

use truce::prelude::*;
use truce_iced::{IcedEditor, IcedPlugin, Message, ParamCache, PluginContext};

mod interpret;
mod ring;

use interpret::{Category, interpret};
use ring::{EventRing, LogEntry};

const JETBRAINS_MONO: Font = Font {
    family: truce_iced::iced::font::Family::Name("JetBrains Mono"),
    ..Font::DEFAULT
};
const WINDOW_W: u32 = 760;
const WINDOW_H: u32 = 460;

const HEADER_BG: Color = Color::from_rgb(0.08, 0.08, 0.10);
const HEADER_TEXT: Color = Color::from_rgb(0.75, 0.75, 0.80);
const FG: Color = Color::from_rgb(0.90, 0.90, 0.94);
const DIM: Color = Color::from_rgb(0.45, 0.45, 0.52);

/// Most rows kept for display; older rows scroll off the top.
const MAX_DISPLAY: usize = 500;

// --- Parameters ---

#[derive(Params)]
pub struct InspectorParams {
    /// When on (default), forward every MIDI event to the output - an
    /// inline monitor in the middle of a chain. Off makes it a terminal
    /// monitor: events are still captured and displayed, just not passed
    /// on.
    #[param(name = "MIDI Thru", default = true)]
    pub thru: BoolParam,

    /// Audio-thread -> editor event channel. Not a parameter: a
    /// `#[skip]` field, so both the plugin (writer, on the audio
    /// thread) and the editor (reader, on the GUI thread) reach the
    /// same ring through the `Arc<InspectorParams>` they already
    /// share. The derive default-initializes it.
    #[skip]
    pub ring: Arc<EventRing>,
}

// --- iced UI ---

#[derive(Debug, Clone)]
pub enum Msg {
    /// A category filter checkbox toggled (index into `Category::ALL`).
    SetFilter(usize, bool),
    TogglePause,
    Clear,
}

pub struct InspectorUi {
    /// Shared with the audio thread; the writer pushes, we drain.
    ring: Arc<EventRing>,
    /// Display buffer. `RefCell` because `view()` (the only per-frame
    /// hook - `Tick` isn't routed to plugin `update`, and timer
    /// subscriptions don't fire without an executor) takes `&self`.
    log: RefCell<VecDeque<LogEntry>>,
    /// Per-category visibility, indexed like `Category::ALL`.
    filters: [bool; Category::ALL.len()],
    /// When paused, the display freezes (the ring keeps overwriting).
    paused: bool,
}

impl InspectorUi {
    fn shows(&self, category: Category) -> bool {
        Category::ALL
            .iter()
            .position(|(c, _)| *c == category)
            .is_none_or(|i| self.filters[i])
    }
}

impl IcedPlugin<InspectorParams> for InspectorUi {
    type Message = Msg;

    // Reaches the plugin's capture ring through the shared params Arc.
    // `editor()` passes the live `self.params`, so this is the same
    // ring the audio thread writes to.
    fn new(params: Arc<InspectorParams>) -> Self {
        Self {
            ring: params.ring.clone(),
            log: RefCell::new(VecDeque::with_capacity(MAX_DISPLAY)),
            filters: [true; Category::ALL.len()],
            paused: false,
        }
    }

    // Repaint (and drain the ring in `view()`) whenever the audio thread
    // has queued events, so a live MIDI stream appears promptly instead
    // of waiting for the next stray UI event. Frozen while paused.
    fn needs_redraw(&self) -> bool {
        !self.paused && self.ring.has_pending()
    }

    fn update(
        &mut self,
        message: Message<Msg>,
        _params: &ParamCache<InspectorParams>,
        _ctx: &PluginContext<InspectorParams>,
    ) -> Task<Message<Msg>> {
        if let Message::Plugin(msg) = message {
            match msg {
                Msg::SetFilter(i, on) => {
                    if let Some(f) = self.filters.get_mut(i) {
                        *f = on;
                    }
                }
                Msg::TogglePause => self.paused = !self.paused,
                Msg::Clear => {
                    self.log.borrow_mut().clear();
                    self.ring.clear();
                }
            }
        }
        Task::none()
    }

    fn view<'a>(&'a self, _params: &'a ParamCache<InspectorParams>) -> Element<'a, Message<Msg>> {
        // Pull whatever the audio thread has queued since the last
        // frame. Frozen while paused so the user can read a burst.
        if !self.paused {
            self.ring
                .drain_into(&mut self.log.borrow_mut(), MAX_DISPLAY);
        }

        let header = self.header_bar();
        let filters = self.filter_bar();
        let list = self.event_list();

        Column::new()
            .push(header)
            .push(filters)
            .push(scrollable(list).height(Length::Fill))
            .into()
    }
}

impl InspectorUi {
    fn header_bar(&self) -> Element<'_, Message<Msg>> {
        let dropped = self.ring.dropped();
        let shown = self.log.borrow().len();
        let stats = if dropped > 0 {
            format!("{shown} shown · {dropped} dropped")
        } else {
            format!("{shown} shown")
        };

        let pause = button(
            text(if self.paused { "Resume" } else { "Pause" })
                .size(12)
                .font(JETBRAINS_MONO)
                .color(FG),
        )
        .on_press(Message::Plugin(Msg::TogglePause));

        let clear = button(text("Clear").size(12).font(JETBRAINS_MONO).color(FG))
            .on_press(Message::Plugin(Msg::Clear));

        let bar = Row::new()
            .push(
                text("MIDI INSPECTOR")
                    .size(14)
                    .font(JETBRAINS_MONO)
                    .color(HEADER_TEXT)
                    .width(Length::Fill),
            )
            .push(text(stats).size(12).font(JETBRAINS_MONO).color(DIM))
            .push(pause)
            .push(clear)
            .spacing(10)
            .align_y(alignment::Vertical::Center);

        container(bar)
            .padding(truce_iced::iced::Padding::from([8.0, 10.0]))
            .width(Length::Fill)
            .style(|_t: &truce_iced::iced::Theme| container::Style {
                background: Some(HEADER_BG.into()),
                ..Default::default()
            })
            .into()
    }

    fn filter_bar(&self) -> Element<'_, Message<Msg>> {
        let mut row = Row::new()
            .spacing(12)
            .padding(truce_iced::iced::Padding::from([6.0, 10.0]));
        for (i, (_, label)) in Category::ALL.iter().enumerate() {
            row = row.push(
                checkbox(self.filters[i])
                    .label(*label)
                    .on_toggle(move |on| Message::Plugin(Msg::SetFilter(i, on))),
            );
        }
        row.into()
    }

    fn event_list(&self) -> Element<'_, Message<Msg>> {
        let log = self.log.borrow();
        if log.is_empty() {
            return container(
                text("Waiting for MIDI - route MIDI to this plugin.")
                    .size(13)
                    .font(JETBRAINS_MONO)
                    .color(DIM),
            )
            .padding(16)
            .into();
        }

        // Newest first: the back of the deque is the most recent
        // entry, so iterate in reverse to put it at the top.
        let mut col = Column::new()
            .spacing(1)
            .padding(truce_iced::iced::Padding::from([4.0, 10.0]));
        for entry in log.iter().rev() {
            let interp = interpret(entry);
            if !self.shows(interp.category) {
                continue;
            }

            let ch = interp
                .channel
                .map_or_else(|| "  -".to_string(), |c| format!("ch{}", c + 1));
            // Monospace + fixed-width columns keep the log aligned.
            let line = format!(
                "{:>6} @{:<5} {:<5} {:<26} {}",
                entry.seq, entry.sample_offset, ch, interp.kind, interp.summary
            );

            let row = Row::new()
                .push(text(line).size(12).font(JETBRAINS_MONO).color(FG))
                .push(text(interp.raw).size(11).font(JETBRAINS_MONO).color(DIM))
                .spacing(16)
                .align_y(alignment::Vertical::Center);
            col = col.push(row);
        }
        col.into()
    }
}

// --- Plugin ---

pub struct MidiInspector {
    params: Arc<InspectorParams>,
}

impl MidiInspector {
    #[must_use]
    pub fn new(params: Arc<InspectorParams>) -> Self {
        Self { params }
    }

    /// The shared capture ring (lives on the params) - used by tests to
    /// inspect what `process()` recorded.
    #[must_use]
    pub fn ring(&self) -> &EventRing {
        &self.params.ring
    }
}

impl PluginLogic for MidiInspector {
    type Params = InspectorParams;

    /// Audio effect with MIDI in/out: a stereo bus passes the track's
    /// audio through untouched while the plugin monitors and forwards
    /// MIDI. The audio I/O is what lets it load as an ordinary insert in
    /// audio-centric hosts (Ableton, Reaper) that don't host MIDI-only
    /// plug-ins - the trade-off is it's no longer an `aumi` MIDI
    /// processor for the hosts (Logic, AUM) that do.
    fn bus_layouts() -> Vec<BusLayout> {
        vec![BusLayout::stereo()]
    }

    fn reset(&mut self, sample_rate: f64, _max_block_size: usize) {
        self.params.set_sample_rate(sample_rate);
    }

    fn process(
        &mut self,
        buffer: &mut AudioBuffer,
        events: &EventList,
        context: &mut ProcessContext,
    ) -> ProcessStatus {
        let thru = self.params.thru.value();
        // Capture every event for the editor; forward it to our MIDI
        // output when Thru is on. `sysex_bytes` resolves the payload
        // (empty for non-SysEx); the ring inlines a fixed prefix so the
        // capture push never allocates.
        for ev in events.iter() {
            let sysex = events.sysex_bytes(&ev.body);
            self.params.ring.push(ev.sample_offset, ev.body, sysex);

            if !thru {
                continue;
            }
            // SysEx is re-pushed by bytes so it lands in the output
            // list's own pool - the input body's pool offset isn't valid
            // for the output list.
            match ev.body {
                EventBody::SysEx { .. } => {
                    let _ =
                        context
                            .output_events
                            .push_sysex_on_port(ev.sample_offset, ev.port, sysex);
                }
                // Preserve the port a thru'd event arrived on.
                body => context
                    .output_events
                    .push(Event::on_port(ev.sample_offset, ev.port, body)),
            }
        }

        // Pass the track's audio through untouched - a monitor must not
        // colour the signal.
        let n_in = buffer.num_input_channels();
        for ch in 0..buffer.channels() {
            let (inp, out) = buffer.io(ch);
            if ch < n_in {
                out.copy_from_slice(inp);
            } else {
                out.fill(0.0);
            }
        }

        ProcessStatus::Normal
    }

    fn editor(params: Arc<InspectorParams>) -> Box<dyn Editor> {
        // Hand the editor the live params (like the built-in and egui
        // editors do) so the UI reaches the same `#[skip]` ring the
        // audio thread writes to - no separate plumbing needed.
        IcedEditor::<InspectorParams, InspectorUi>::new(params.clone(), (WINDOW_W, WINDOW_H))
            .with_font(truce_font::JETBRAINS_MONO)
            .resizable(true)
            .min_size((520, 320))
            .max_size((1600, 1200))
            .into_editor()
    }
}

truce::plugin! {
    logic: MidiInspector,
    params: InspectorParams,
}

#[cfg(test)]
mod tests {
    use super::*;
    use truce_core::events::EventBody;

    // -- interpret() unit tests (pure, no GUI / driver) --

    fn entry(body: EventBody) -> LogEntry {
        LogEntry {
            seq: 0,
            sample_offset: 0,
            body,
            sysex: [0u8; ring::SYSEX_INLINE],
            sysex_len: 0,
        }
    }

    #[test]
    fn interprets_note_on() {
        let i = interpret(&entry(EventBody::NoteOn {
            group: 0,
            channel: 0,
            note: 60,
            velocity: 100,
        }));
        assert_eq!(i.category, Category::Note);
        assert_eq!(i.kind, "Note On");
        assert_eq!(i.channel, Some(0));
        assert!(i.summary.contains("C4"), "summary was {:?}", i.summary);
    }

    #[test]
    fn interprets_cc_with_name() {
        let i = interpret(&entry(EventBody::ControlChange {
            group: 0,
            channel: 3,
            cc: 1,
            value: 64,
        }));
        assert_eq!(i.category, Category::Cc);
        assert_eq!(i.channel, Some(3));
        assert!(
            i.summary.contains("Mod Wheel"),
            "summary was {:?}",
            i.summary
        );
    }

    #[test]
    fn interprets_pitch_bend_center() {
        let i = interpret(&entry(EventBody::PitchBend {
            group: 0,
            channel: 0,
            value: 8192,
        }));
        assert_eq!(i.category, Category::PitchBend);
        assert!(
            i.summary.contains("+0.00 st"),
            "summary was {:?}",
            i.summary
        );
    }

    #[test]
    fn sysex_shows_source_and_hex() {
        let mut e = entry(EventBody::SysEx {
            pool_offset: 0,
            len: 3,
        });
        e.sysex[..3].copy_from_slice(&[0x7E, 0x00, 0x06]);
        e.sysex_len = 3;
        let i = interpret(&e);
        assert_eq!(i.category, Category::SysEx);
        assert!(i.summary.contains("Universal Non-Realtime"));
        assert!(i.raw.contains("7E 00 06"), "raw was {:?}", i.raw);
    }

    #[test]
    fn unknown_cc_falls_back_to_number() {
        let i = interpret(&entry(EventBody::ControlChange {
            group: 0,
            channel: 0,
            cc: 119,
            value: 1,
        }));
        // No friendly name, but still legible + raw line present.
        assert!(i.summary.contains("CC119"));
        assert!(i.raw.contains("ControlChange"));
    }

    // -- capture test: process() records events into the ring --

    fn captured(plugin: &MidiInspector) -> Vec<LogEntry> {
        let mut out = VecDeque::new();
        plugin.ring().drain_into(&mut out, usize::MAX);
        out.into_iter().collect()
    }

    // Passthrough is an exact copy, so an exact float compare is right.
    #[allow(clippy::float_cmp)]
    #[test]
    fn captures_events_from_process() {
        use truce_core::buffer::AudioBuffer;
        use truce_core::events::{Event, EventList, TransportInfo};
        use truce_core::process::ProcessContext;

        let mut plugin = MidiInspector::new(Arc::new(InspectorParams::new()));

        let mut events = EventList::with_capacity(8);
        events.push(Event::new(
            0,
            EventBody::NoteOn {
                group: 0,
                channel: 0,
                note: 60,
                velocity: 100,
            },
        ));
        events.push(Event::new(
            0,
            EventBody::ControlChange {
                group: 0,
                channel: 0,
                cc: 1,
                value: 64,
            },
        ));

        // Stereo passthrough buffer of constant 0.5.
        let input = [[0.5f32; 16], [0.5f32; 16]];
        let in_refs: [&[f32]; 2] = [&input[0], &input[1]];
        let mut output = [[0.0f32; 16], [0.0f32; 16]];
        let (o0, o1) = output.split_at_mut(1);
        let mut out_refs: [&mut [f32]; 2] = [&mut o0[0], &mut o1[0]];
        let mut buffer = AudioBuffer::from_slices_checked(&in_refs, &mut out_refs, 16);

        let transport = TransportInfo::default();
        let mut out_events = EventList::with_capacity(0);
        let mut ctx = ProcessContext::new(&transport, 44_100.0, 16, &mut out_events);

        plugin.process(&mut buffer, &events, &mut ctx);
        // `ctx`'s borrow of `out_events` ends here (last use above).

        let kinds: Vec<_> = captured(&plugin)
            .iter()
            .map(|e| interpret(e).kind)
            .collect();
        assert!(kinds.contains(&"Note On"), "kinds: {kinds:?}");
        assert!(kinds.contains(&"Control Change"), "kinds: {kinds:?}");
        // Audio passed through untouched.
        assert_eq!(output[0][0], 0.5);
        // MIDI forwarded to the output unchanged.
        let out: Vec<_> = out_events.iter().map(|e| e.body).collect();
        assert!(
            out.iter().any(|b| matches!(b, EventBody::NoteOn { .. })),
            "out: {out:?}"
        );
        assert!(
            out.iter()
                .any(|b| matches!(b, EventBody::ControlChange { .. })),
            "out: {out:?}"
        );
    }

    // -- standard plugin / editor checks --

    #[test]
    fn skip_field_is_not_a_param() {
        use truce::params::Params;
        let p = InspectorParams::new();
        // `thru` is the only parameter; the `#[skip]` ring is plugin
        // state, excluded from the parameter set entirely.
        assert_eq!(Params::count(&p), 1);
        assert_eq!(p.param_infos().len(), 1);
        assert_eq!(p.param_infos()[0].name, "MIDI Thru");
    }

    #[test]
    fn info_is_valid() {
        truce_test::assert_valid_info::<Plugin>();
    }

    #[test]
    fn has_editor() {
        truce_test::assert_has_editor::<Plugin>();
    }

    #[test]
    fn state_round_trips() {
        truce_test::assert_state_round_trip::<Plugin>();
    }

    #[test]
    fn editor_lifecycle() {
        truce_test::assert_editor_lifecycle::<Plugin>();
    }

    #[test]
    fn editor_size_consistent() {
        truce_test::assert_editor_size_consistent::<Plugin>();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn gui_screenshot_macos() {
        truce_test::screenshot!(Plugin, "screenshots/midi_inspector_default_macos.png").run();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn gui_screenshot_linux() {
        truce_test::screenshot!(Plugin, "screenshots/midi_inspector_default_linux.png")
            .pixel_threshold(2)
            .run();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn gui_screenshot_windows() {
        truce_test::screenshot!(Plugin, "screenshots/midi_inspector_default_windows.png")
            .pixel_threshold(2)
            .run();
    }
}
