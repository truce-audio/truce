//! Sample-accurate parameter-dependent chunking.
//!
//! Splits a host audio block into sub-blocks at the
//! `sample_offset` of every `ParamChange` (for chunkable parameters)
//! and every `Transport` event, calling `plugin.process()` once per
//! sub-block. `set_plain` for parameter events is deferred to the
//! sub-block boundary where the event actually sits, so smoothers
//! see `set_target` at the right sample instead of at sample 0 of
//! the whole audio block.
//!
//! Every format wrapper routes its `process()` call through
//! [`process_chunked`]. On formats whose host events all carry
//! `sample_offset = 0` (VST2, AAX, LV2 in v1, AU until ramp decoding
//! lands) the loop runs once per block and the splitting machinery
//! is inert.

use truce_params::{ParamFlags, ParamInfo, Params};

use crate::buffer::AudioBuffer;
use crate::config::ProcessMode;
use crate::events::{Event, EventBody, EventList, TransportInfo};
use crate::plugin::PluginRuntime;
use crate::process::{ProcessContext, ProcessStatus};
use crate::sample::Sample;

/// Inputs to [`process_chunked`].
///
/// Bundled into a struct because the call has eight load-bearing
/// references plus a couple of value fields and a positional argument
/// list at that width is unreadable at the call site (every wrapper
/// would invent its own helper). Construct one per `process()` call.
pub struct ChunkedProcess<'a> {
    /// Sorted, block-rate event stream from the host (param changes,
    /// transport changes, MIDI). The chunker walks this once forward;
    /// it does not mutate the list.
    pub events: &'a EventList,
    /// Per-instance scratch list pre-allocated to the same capacity
    /// as `events`. Used to hold the per-sub-block rebased view of
    /// `events`; `clear()`-ed at the start of every sub-block so the
    /// backing `Vec` capacity is preserved across blocks. Wrappers
    /// hold this alongside their input / output event lists.
    pub sub_event_scratch: &'a mut EventList,
    /// Initial transport snapshot for the block. Mutated in place
    /// as the chunker walks past `EventBody::Transport` events; the
    /// per-sub-block `ProcessContext` reads from this so the plugin
    /// sees the right tempo / position for the sub-block it's in.
    pub transport: &'a mut TransportInfo,
    /// Host sample rate, plumbed through to each per-sub-block
    /// `ProcessContext`.
    pub sample_rate: f64,
    /// Live processing mode for this block, stamped onto every
    /// per-sub-block `ProcessContext`. Wrappers read it from the host
    /// each block (VST3 `processMode`, LV2 freewheel port) or cache it
    /// from a set-once callback (CLAP / AU).
    pub process_mode: ProcessMode,
    /// Plugin's outbound event queue. The chunker re-bases outbound
    /// events back to block-relative coordinates before the wrapper
    /// hands them to the host: the plugin pushes events with
    /// sub-block-relative offsets, the chunker shifts them by the
    /// sub-block's start sample.
    pub output_events: &'a mut EventList,
    /// Optional read-side params closure plumbed through to each
    /// per-sub-block `ProcessContext`. Same shape as
    /// `ProcessContext::with_params`.
    pub params_fn: Option<&'a dyn Fn(u32) -> f64>,
    /// Optional meter-write closure plumbed through likewise.
    pub meters_fn: Option<&'a dyn Fn(u32, f32)>,
    /// Static param metadata - the chunker keys `is_chunked(id)`
    /// off `ParamFlags::CHUNKED` here. Wrappers cache this once
    /// when the plugin instantiates (via
    /// [`Params::param_infos_static`]) and pass the same slice on
    /// every block.
    pub param_infos: &'a [ParamInfo],
    /// Minimum sub-block size in samples. From
    /// [`crate::info::AutomationConfig::min_subblock_samples`].
    /// Events whose `sample_offset` falls within
    /// `min_subblock_samples` of the current sub-block start are
    /// coalesced into that sub-block's leading `apply_pending_events`
    /// batch instead of triggering a split.
    pub min_subblock_samples: u32,
}

/// Walk the audio block in sub-block chunks, calling
/// `plugin.process()` once per chunk with the events that land in
/// `[block_start, block_end)` rebased to sub-block-relative offsets.
///
/// Returns the `ProcessStatus` returned by the *last* sub-block; for
/// `Tail(N)` the plugin's own clock is the authority, so propagating
/// the last call's value is the cheapest correct rule.
///
/// Allocation-free: the rebased event list lives in
/// `sub_event_scratch` (capacity preserved across calls) and the
/// audio buffer sub-views are zero-copy via
/// [`AudioBuffer::slice`].
pub fn process_chunked<S, P>(
    plugin: &mut P,
    params: &dyn Params,
    buffer: &mut AudioBuffer<S>,
    args: ChunkedProcess<'_>,
) -> ProcessStatus
where
    S: Sample,
    P: PluginRuntime<Sample = S>,
{
    let ChunkedProcess {
        events,
        sub_event_scratch,
        transport,
        sample_rate,
        process_mode,
        output_events,
        params_fn,
        meters_fn,
        param_infos,
        min_subblock_samples,
    } = args;

    let total = buffer.num_samples();
    let mut block_start = 0usize;
    let mut event_idx = 0usize;
    let mut last_status = ProcessStatus::Normal;
    // Sample offset the current `transport` snapshot's position is anchored
    // to. Starts at the block top; a `Transport` event re-anchors it to
    // that split (the host-provided snapshot already carries the position
    // at that offset), so a sub-block advances only from the last anchor.
    let mut transport_anchor = 0usize;
    // Floor at 1: a sub-block can't be shorter than one sample, so 0
    // and 1 both mean "split at every event". Without the floor, an
    // event sitting exactly on `block_start` (e.g. a Transport at
    // offset 0, which every wrapper pushes) yields `block_end ==
    // block_start`, a zero-length sub-block, and a `while block_start
    // < total` loop that never advances - hanging the audio thread.
    let min_sub = (min_subblock_samples as usize).max(1);

    // Paranoid allocation check (the `rt-paranoid` feature): one section
    // spanning the whole per-host-block run, so it covers not just the
    // plugin's `process` but the framework glue around it - event
    // dispatch / rebasing, sub-block slicing, output re-basing - which
    // all run on the audio thread and must be allocation-free too. No-op
    // and zero-sized when the feature is off. This is the single site
    // every format wrapper and the test driver route through.
    let _rt = crate::rt::RtSection::enter();

    while block_start < total {
        // Find the next split-eligible event at or past
        // `block_start + min_sub`. Anything before that coalesces
        // into this sub-block's leading apply batch.
        let coalesce_until = block_start.saturating_add(min_sub).min(total);
        let next_split = find_next_split(events, param_infos, event_idx, coalesce_until);
        let block_end = next_split.map_or(total, |(s, _)| s.min(total));

        // Apply every event with sample_offset < block_end that's
        // still pending. This is the deferred `set_plain` call that
        // wrappers used to make eagerly at block start, plus
        // transport-snapshot updates for `EventBody::Transport`.
        // Advances `event_idx` past everything consumed.
        if apply_pending_events(events, params, transport, &mut event_idx, block_end) {
            // A transport event refreshed the snapshot at this boundary,
            // so its position already reflects `block_start`.
            transport_anchor = block_start;
        }

        // Rebase the in-window events into the scratch list with
        // sub-block-relative `sample_offset`s. ParamChange entries
        // get included so plugins that key off them (synths reading
        // ParamMod, plugins logging) see them at the right time
        // even though the wrapper has already applied them. Note
        // events / SysEx get included with rebased offsets.
        rebase_events_into(events, sub_event_scratch, block_start, block_end);

        let mut sub_buffer = buffer.slice(block_start, block_end - block_start);
        let sub_output_start = output_events.len();

        // Advance the playhead to this sub-block's start so a plugin that
        // re-derives phase from the transport sees the right position.
        let sub_transport =
            advance_transport(transport, block_start - transport_anchor, sample_rate);
        let mut ctx = ProcessContext::new(
            &sub_transport,
            sample_rate,
            block_end - block_start,
            output_events,
        )
        .with_process_mode(process_mode);
        if let Some(f) = params_fn {
            ctx = ctx.with_params(f);
        }
        if let Some(f) = meters_fn {
            ctx = ctx.with_meters(f);
        }

        last_status = plugin.process(&mut sub_buffer, sub_event_scratch, &mut ctx);

        // Re-base any events the plugin pushed during this sub-block
        // back into block-relative coordinates so the wrapper's
        // per-event encode loop sees host-block-rate timings.
        rebase_output_events(output_events, sub_output_start, block_start);

        block_start = block_end;
    }

    last_status
}

/// Return the index of the next split-eligible event at sample
/// `offset >= min_offset`, along with that sample offset.
///
/// "Split-eligible" = a `ParamChange` or mono `ParamMod` targeting a
/// `ParamFlags::CHUNKED` parameter, or any `Transport` event. Note
/// events (`NoteOn` / `NoteOff` / CC / etc.) don't split; they ride
/// inside whichever sub-block they fall into via `rebase_events_into`.
/// Polyphonic mod (`note_id != -1`) doesn't split either - it's a
/// per-voice offset and subdividing the audio block doesn't help.
fn find_next_split(
    events: &EventList,
    param_infos: &[ParamInfo],
    from: usize,
    min_offset: usize,
) -> Option<(usize, usize)> {
    for (i, ev) in events.iter().enumerate().skip(from) {
        let offset = ev.sample_offset as usize;
        if offset < min_offset {
            continue;
        }
        if is_split_event(&ev.body, param_infos) {
            return Some((offset, i));
        }
    }
    None
}

fn is_split_event(body: &EventBody, param_infos: &[ParamInfo]) -> bool {
    match body {
        EventBody::ParamChange { id, .. }
        | EventBody::ParamMod {
            id, note_id: -1, ..
        } => is_chunked(*id, param_infos),
        EventBody::Transport(_) => true,
        _ => false,
    }
}

fn is_chunked(id: u32, param_infos: &[ParamInfo]) -> bool {
    param_infos
        .iter()
        .find(|info| info.id == id)
        .is_some_and(|info| info.flags.contains(ParamFlags::CHUNKED))
}

/// Advance a transport snapshot forward by `delta_samples` for a
/// sub-block that starts that far into the host block. Only the playhead
/// fields move (`position_samples` / `position_seconds` /
/// `position_beats`); tempo, time signature, and loop bounds are
/// block-rate. `bar_start_beats` is left as-is - it's the *last* bar
/// start, which a sub-block advance rarely crosses, and computing a bar
/// crossing would need the full meter grid the host owns.
///
/// A stopped playhead doesn't move, so nothing advances unless
/// `playing`. Without this, sub-blocks split by a `CHUNKED` param all see
/// the block-start position, giving tempo-synced plugins that re-derive
/// phase from the transport up to a block of timing jitter right where
/// automation lands.
///
/// `delta_samples` is a sub-block offset - a few thousand samples at most,
/// exact in both `i64` and `f64`.
#[allow(clippy::cast_possible_wrap, clippy::cast_precision_loss)]
fn advance_transport(t: &TransportInfo, delta_samples: usize, sample_rate: f64) -> TransportInfo {
    let mut t = *t;
    if delta_samples == 0 || !t.playing || sample_rate <= 0.0 {
        return t;
    }
    let delta_secs = delta_samples as f64 / sample_rate;
    t.position_samples += delta_samples as i64;
    t.position_seconds += delta_secs;
    t.position_beats += delta_secs * t.tempo / 60.0;
    t
}

/// Walk `events` from `*event_idx` forward, applying every event with
/// `sample_offset < block_end` to the param store / transport
/// snapshot and advancing `*event_idx` past the consumed range.
///
/// `ParamChange` writes through to `params.set_plain`; `Transport`
/// overwrites the per-block snapshot. Note events / `ParamMod` / `SysEx`
/// are not "applied" - they ride in the rebased sub-event list for
/// the plugin to process itself; this function just advances past
/// them so the next split scan starts in the right place.
///
/// Returns `true` when a `Transport` event was applied, so the caller can
/// re-anchor its sub-block position advance to this boundary (the fresh
/// snapshot's position already reflects the split offset).
fn apply_pending_events(
    events: &EventList,
    params: &dyn Params,
    transport: &mut TransportInfo,
    event_idx: &mut usize,
    block_end: usize,
) -> bool {
    let mut i = *event_idx;
    let mut transport_applied = false;
    for ev in events.iter().skip(i) {
        if (ev.sample_offset as usize) >= block_end {
            break;
        }
        match ev.body {
            EventBody::ParamChange { id, value } => {
                params.set_plain(id, value);
            }
            EventBody::Transport(t) => {
                *transport = t;
                transport_applied = true;
            }
            // Note events, ParamMod, SysEx: the plugin handles these
            // via the rebased sub-event list. The apply pass only
            // advances past them.
            _ => {}
        }
        i += 1;
    }
    *event_idx = i;
    transport_applied
}

/// Copy events in `[block_start, block_end)` into `scratch` with
/// `sample_offset` rebased to sub-block-relative coordinates.
///
/// `clear()`s `scratch` first; the backing `Vec` capacity is
/// preserved across calls so steady-state operation is
/// allocation-free as long as the wrapper sized the scratch list to
/// match its input list's capacity.
///
/// `SysEx` payloads are copied into the scratch's own byte pool (via
/// `push_sysex`), so the scratch is self-contained. The plugin only
/// ever receives the scratch, so `EventList::sysex_bytes` must resolve
/// against it - copying the body verbatim would leave the rebased entry
/// pointing at the empty scratch pool and panic on access. The scratch
/// pool is pre-sized to `SYSEX_POOL_PREALLOC` (it's built with
/// `EventList::with_capacity`), so the copy stays allocation-free.
fn rebase_events_into(
    events: &EventList,
    scratch: &mut EventList,
    block_start: usize,
    block_end: usize,
) {
    scratch.clear();
    for ev in events.iter() {
        let off = ev.sample_offset as usize;
        if off < block_start {
            continue;
        }
        if off >= block_end {
            break;
        }
        // Rebase the sample offset. The cast is bounded: `off -
        // block_start < block_end - block_start <= u32::MAX in
        // practice` (audio blocks cap at a few thousand samples).
        #[allow(clippy::cast_possible_truncation)]
        let rebased_offset = (off - block_start) as u32;
        match ev.body {
            // Re-copy the payload so the scratch carries its own pool
            // entry; a pool-full drop matches the documented `SysEx`
            // overflow behaviour and can't occur in practice (the
            // scratch pool matches the source pool's size).
            EventBody::SysEx { .. } => {
                let _ = scratch.push_sysex_on_port(
                    rebased_offset,
                    ev.port,
                    events.sysex_bytes(&ev.body),
                );
            }
            body => scratch.push(Event::on_port(rebased_offset, ev.port, body)),
        }
    }
}

/// Shift the `sample_offset` of every output event the plugin
/// pushed during the just-completed sub-block back into block-relative
/// coordinates by adding `sub_block_start`.
///
/// Output events live in `output_events`; the plugin pushes them
/// with sub-block-relative offsets (e.g. "MIDI out on sample 10 of
/// the sub-block"). The wrapper's per-event host-encode loop expects
/// host-block-rate timings, so shift here once per sub-block.
fn rebase_output_events(output_events: &mut EventList, from: usize, sub_block_start: usize) {
    #[allow(clippy::cast_possible_truncation)]
    let shift = sub_block_start as u32;
    if shift == 0 {
        return;
    }
    let slice = output_events.events_mut();
    for ev in slice.iter_mut().skip(from) {
        ev.sample_offset = ev.sample_offset.saturating_add(shift);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::EVENT_LIST_PREALLOC;
    use truce_params::{ParamFlags, ParamInfo, ParamRange, ParamUnit, ParamValueKind};

    fn info(id: u32, chunked: bool) -> ParamInfo {
        let flags = if chunked {
            ParamFlags::AUTOMATABLE | ParamFlags::CHUNKED
        } else {
            ParamFlags::AUTOMATABLE
        };
        ParamInfo {
            id,
            name: "p",
            short_name: "p",
            group: "",
            range: ParamRange::Linear { min: 0.0, max: 1.0 },
            default_plain: 0.0,
            flags,
            unit: ParamUnit::None,
            kind: ParamValueKind::Float,
            midi_map: None,
            midi_channel: None,
        }
    }

    #[test]
    fn split_only_on_chunked_params() {
        let infos = [info(0, true), info(1, false)];
        let mut events = EventList::with_capacity(EVENT_LIST_PREALLOC);
        events.push(Event::new(
            100,
            EventBody::ParamChange { id: 1, value: 0.5 },
        ));
        events.push(Event::new(
            200,
            EventBody::ParamChange { id: 0, value: 0.5 },
        ));
        // Non-chunked param at 100 doesn't split; chunked at 200 does.
        let next = find_next_split(&events, &infos, 0, 0);
        assert_eq!(next, Some((200, 1)));
    }

    #[test]
    fn min_offset_skips_close_events() {
        let infos = [info(0, true)];
        let mut events = EventList::with_capacity(EVENT_LIST_PREALLOC);
        events.push(Event::new(5, EventBody::ParamChange { id: 0, value: 0.5 }));
        events.push(Event::new(50, EventBody::ParamChange { id: 0, value: 0.6 }));
        // min_offset = 32: first event (offset 5) coalesces, second (50) splits.
        let next = find_next_split(&events, &infos, 0, 32);
        assert_eq!(next, Some((50, 1)));
    }

    #[test]
    fn poly_mod_never_splits() {
        let infos = [info(0, true)];
        let mut events = EventList::with_capacity(EVENT_LIST_PREALLOC);
        events.push(Event::new(
            100,
            EventBody::ParamMod {
                id: 0,
                note_id: 7,
                value: 0.1,
            },
        ));
        let next = find_next_split(&events, &infos, 0, 0);
        assert_eq!(next, None);
    }

    #[test]
    fn rebase_drops_out_of_window() {
        let mut events = EventList::with_capacity(EVENT_LIST_PREALLOC);
        events.push(Event::new(10, EventBody::ParamChange { id: 0, value: 0.1 }));
        events.push(Event::new(50, EventBody::ParamChange { id: 0, value: 0.2 }));
        events.push(Event::new(90, EventBody::ParamChange { id: 0, value: 0.3 }));
        let mut scratch = EventList::with_capacity(EVENT_LIST_PREALLOC);
        rebase_events_into(&events, &mut scratch, 40, 80);
        let collected: Vec<u32> = scratch.iter().map(|e| e.sample_offset).collect();
        // Only the offset-50 event is in [40, 80); rebased to 10.
        assert_eq!(collected, vec![10]);
    }

    #[test]
    fn rebase_preserves_midi_port() {
        // The chunker splits the input list into sub-blocks; a
        // multi-port plugin's per-event port must survive that copy.
        let mut events = EventList::with_capacity(EVENT_LIST_PREALLOC);
        events.push(Event::on_port(
            20,
            3,
            EventBody::NoteOn {
                group: 0,
                channel: 0,
                note: 60,
                velocity: 100,
            },
        ));
        let mut scratch = EventList::with_capacity(EVENT_LIST_PREALLOC);
        rebase_events_into(&events, &mut scratch, 0, 64);
        assert_eq!(scratch.iter().map(|e| e.port).collect::<Vec<_>>(), vec![3]);
    }

    #[test]
    fn transport_always_splits() {
        let infos: [ParamInfo; 0] = [];
        let mut events = EventList::with_capacity(EVENT_LIST_PREALLOC);
        events.push(Event::new(
            100,
            EventBody::Transport(TransportInfo::default()),
        ));
        let next = find_next_split(&events, &infos, 0, 0);
        assert_eq!(next, Some((100, 0)));
    }

    #[test]
    fn offset_zero_split_needs_min_offset_at_least_one() {
        // The `min_subblock_samples.max(1)` clamp in `process_chunked`
        // rests on this: with `min_offset == 0`, an event sitting
        // exactly on `block_start` (offset 0 - a Transport every
        // wrapper pushes) is returned as a split point, so `block_end
        // == block_start` yields a zero-length sub-block and the loop
        // never advances (hang). With `min_offset >= 1` that event
        // coalesces, so `block_end` moves forward and the loop
        // terminates.
        let infos: [ParamInfo; 0] = [];
        let mut events = EventList::with_capacity(EVENT_LIST_PREALLOC);
        events.push(Event::new(
            0,
            EventBody::Transport(TransportInfo::default()),
        ));
        // Unclamped (0): the offset-0 event splits at 0 - the hang.
        assert_eq!(find_next_split(&events, &infos, 0, 0), Some((0, 0)));
        // Clamped (>= 1): coalesced, no split at the block start.
        assert_eq!(find_next_split(&events, &infos, 0, 1), None);
    }

    /// A sub-block that starts `delta` samples into the host block sees a
    /// playhead advanced by `delta` - samples, seconds, and beats (from
    /// tempo). Without this a `CHUNKED` param split leaves every sub-block
    /// at the block-start position, jittering tempo-synced phase.
    #[test]
    #[allow(clippy::float_cmp)]
    fn advance_transport_moves_playhead_when_playing() {
        let base = TransportInfo {
            playing: true,
            tempo: 120.0,
            position_samples: 1_000,
            position_seconds: 1.0,
            position_beats: 2.0,
            ..TransportInfo::default()
        };
        // 480 samples at 48 kHz = 0.01 s = 0.02 beats at 120 BPM.
        let adv = advance_transport(&base, 480, 48_000.0);
        assert_eq!(adv.position_samples, 1_480);
        assert!((adv.position_seconds - 1.01).abs() < 1e-12);
        assert!((adv.position_beats - 2.02).abs() < 1e-12);
        // Block-rate fields are untouched.
        assert_eq!(adv.tempo, base.tempo);
        assert_eq!(adv.bar_start_beats, base.bar_start_beats);
    }

    /// A stopped playhead doesn't move, and a zero-offset sub-block (the
    /// first, or an un-split block) is left exactly as-is.
    #[test]
    #[allow(clippy::float_cmp)]
    fn advance_transport_noop_when_stopped_or_zero_delta() {
        let stopped = TransportInfo {
            playing: false,
            tempo: 120.0,
            position_samples: 1_000,
            ..TransportInfo::default()
        };
        assert_eq!(advance_transport(&stopped, 480, 48_000.0), stopped);

        let playing = TransportInfo {
            playing: true,
            position_samples: 1_000,
            ..TransportInfo::default()
        };
        assert_eq!(advance_transport(&playing, 0, 48_000.0), playing);
    }

    #[test]
    fn rebase_copies_sysex_payload_into_scratch() {
        let mut events = EventList::with_capacity(EVENT_LIST_PREALLOC);
        events.push_sysex(50, &[0x11, 0x22, 0x33, 0x44]).unwrap();
        let mut scratch = EventList::with_capacity(EVENT_LIST_PREALLOC);
        rebase_events_into(&events, &mut scratch, 40, 80);

        let ev = scratch.iter().next().expect("sysex rebased into scratch");
        assert_eq!(ev.sample_offset, 10); // 50 - 40
        // Regression: the scratch used to carry the parent's pool
        // indices against an empty pool, so this access panicked
        // out-of-bounds. It now resolves against the scratch's own pool.
        assert_eq!(scratch.sysex_bytes(&ev.body), &[0x11, 0x22, 0x33, 0x44]);
    }
}
