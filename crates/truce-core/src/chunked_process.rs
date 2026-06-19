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
    let min_sub = min_subblock_samples as usize;

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
        apply_pending_events(events, params, transport, &mut event_idx, block_end);

        // Rebase the in-window events into the scratch list with
        // sub-block-relative `sample_offset`s. ParamChange entries
        // get included so plugins that key off them (synths reading
        // ParamMod, plugins logging) see them at the right time
        // even though the wrapper has already applied them. Note
        // events / SysEx get included with rebased offsets.
        rebase_events_into(events, sub_event_scratch, block_start, block_end);

        let mut sub_buffer = buffer.slice(block_start, block_end - block_start);
        let sub_output_start = output_events.len();

        let mut ctx = ProcessContext::new(
            transport,
            sample_rate,
            block_end - block_start,
            output_events,
        );
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

/// Walk `events` from `*event_idx` forward, applying every event with
/// `sample_offset < block_end` to the param store / transport
/// snapshot and advancing `*event_idx` past the consumed range.
///
/// `ParamChange` writes through to `params.set_plain`; `Transport`
/// overwrites the per-block snapshot. Note events / `ParamMod` / `SysEx`
/// are not "applied" - they ride in the rebased sub-event list for
/// the plugin to process itself; this function just advances past
/// them so the next split scan starts in the right place.
fn apply_pending_events(
    events: &EventList,
    params: &dyn Params,
    transport: &mut TransportInfo,
    event_idx: &mut usize,
    block_end: usize,
) {
    let mut i = *event_idx;
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
            }
            // Note events, ParamMod, SysEx: the plugin handles these
            // via the rebased sub-event list. The apply pass only
            // advances past them.
            _ => {}
        }
        i += 1;
    }
    *event_idx = i;
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
                let _ = scratch.push_sysex(rebased_offset, events.sysex_bytes(&ev.body));
            }
            body => scratch.push(Event {
                sample_offset: rebased_offset,
                body,
            }),
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
        }
    }

    #[test]
    fn split_only_on_chunked_params() {
        let infos = [info(0, true), info(1, false)];
        let mut events = EventList::with_capacity(EVENT_LIST_PREALLOC);
        events.push(Event {
            sample_offset: 100,
            body: EventBody::ParamChange { id: 1, value: 0.5 },
        });
        events.push(Event {
            sample_offset: 200,
            body: EventBody::ParamChange { id: 0, value: 0.5 },
        });
        // Non-chunked param at 100 doesn't split; chunked at 200 does.
        let next = find_next_split(&events, &infos, 0, 0);
        assert_eq!(next, Some((200, 1)));
    }

    #[test]
    fn min_offset_skips_close_events() {
        let infos = [info(0, true)];
        let mut events = EventList::with_capacity(EVENT_LIST_PREALLOC);
        events.push(Event {
            sample_offset: 5,
            body: EventBody::ParamChange { id: 0, value: 0.5 },
        });
        events.push(Event {
            sample_offset: 50,
            body: EventBody::ParamChange { id: 0, value: 0.6 },
        });
        // min_offset = 32: first event (offset 5) coalesces, second (50) splits.
        let next = find_next_split(&events, &infos, 0, 32);
        assert_eq!(next, Some((50, 1)));
    }

    #[test]
    fn poly_mod_never_splits() {
        let infos = [info(0, true)];
        let mut events = EventList::with_capacity(EVENT_LIST_PREALLOC);
        events.push(Event {
            sample_offset: 100,
            body: EventBody::ParamMod {
                id: 0,
                note_id: 7,
                value: 0.1,
            },
        });
        let next = find_next_split(&events, &infos, 0, 0);
        assert_eq!(next, None);
    }

    #[test]
    fn rebase_drops_out_of_window() {
        let mut events = EventList::with_capacity(EVENT_LIST_PREALLOC);
        events.push(Event {
            sample_offset: 10,
            body: EventBody::ParamChange { id: 0, value: 0.1 },
        });
        events.push(Event {
            sample_offset: 50,
            body: EventBody::ParamChange { id: 0, value: 0.2 },
        });
        events.push(Event {
            sample_offset: 90,
            body: EventBody::ParamChange { id: 0, value: 0.3 },
        });
        let mut scratch = EventList::with_capacity(EVENT_LIST_PREALLOC);
        rebase_events_into(&events, &mut scratch, 40, 80);
        let collected: Vec<u32> = scratch.iter().map(|e| e.sample_offset).collect();
        // Only the offset-50 event is in [40, 80); rebased to 10.
        assert_eq!(collected, vec![10]);
    }

    #[test]
    fn transport_always_splits() {
        let infos: [ParamInfo; 0] = [];
        let mut events = EventList::with_capacity(EVENT_LIST_PREALLOC);
        events.push(Event {
            sample_offset: 100,
            body: EventBody::Transport(TransportInfo::default()),
        });
        let next = find_next_split(&events, &infos, 0, 0);
        assert_eq!(next, Some((100, 0)));
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
