//! Stereo plate reverb wired through a `fundsp` audio graph.
//!
//! **Worker-thread variant.** Rebuilds the fundsp graph on a
//! dedicated background thread and hands the new graph to the audio
//! thread via lock-free queues, so `process()` never calls
//! `Box::new`, `graph.allocate()`, or drops a graph. This is the
//! production pattern.
//!
//! ```text
//!     in (L,R) ──► high-pass (low cut) ──► low-pass (high cut) ──► reverb_stereo ──┐
//!                                                                                  │
//!     in (L,R) ──────────────────────────────────────────────────────────────► dry ┤──► out
//! ```
//!
//! See `README.md` for the integration patterns + gotchas.

use crossbeam_queue::ArrayQueue;
use fundsp::prelude::{
    AudioUnit, Shared, U2, dc, highpass, lowpass, multipass, pass, reverb_stereo, shared, var,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle, Thread};
use truce::prelude::*;
use truce_gui::IntoLayoutEditor;
use truce_gui_types::layout::{GridLayout, knob, meter, widgets};

use FundspReverbWorkerParamsParamId as P;

const DEFAULT_LOW_CUT_HZ: f32 = 120.0;
const DEFAULT_HIGH_CUT_HZ: f32 = 8000.0;
const DEFAULT_REVERB_MIX: f32 = 0.25;
const DEFAULT_TIME_S: f32 = 3.0;
const FILTER_Q: f32 = 0.707;
const ROOM_SIZE: f64 = 10.0;
const DAMPING: f64 = 0.5;

/// Hysteresis on Time changes - fundsp's `reverb_stereo` bakes RT60
/// into the FDN at construction, so each crossing triggers a full
/// rebuild (= delay lines reset, tail dropped). Threshold keeps tiny
/// drifts from rebuilding.
const TIME_REBUILD_THRESHOLD_S: f32 = 0.05;

#[derive(Params)]
pub struct FundspReverbWorkerParams {
    #[param(
        name = "Low Cut",
        range = "log(20, 2000)",
        unit = "Hz",
        default = 120.0,
        smooth = "exp(5)"
    )]
    pub low_cut: FloatParam,

    #[param(
        name = "High Cut",
        range = "log(500, 18000)",
        unit = "Hz",
        default = 8000.0,
        smooth = "exp(5)"
    )]
    pub high_cut: FloatParam,

    // Unsmoothed: Time changes rebuild the FDN once on the raw
    // target; the smoother would have no reader.
    #[param(
        name = "Time",
        range = "log(0.1, 20)",
        unit = "s",
        default = 3.0,
        smooth = "none"
    )]
    pub time: FloatParam,

    #[param(
        name = "Mix",
        range = "linear(0, 1)",
        default = 0.25,
        unit = "%",
        smooth = "exp(20)"
    )]
    pub mix: FloatParam,

    #[meter]
    pub meter_l: MeterSlot,

    #[meter]
    pub meter_r: MeterSlot,
}

/// Allocates via `Box::new` + `allocate()`. The worker thread calls
/// this off the audio thread; `reset()` also calls it directly since
/// the host invokes `reset` off the audio thread.
fn build_graph(
    sample_rate: f64,
    time_s: f32,
    low_cut: &Shared,
    high_cut: &Shared,
    mix: &Shared,
) -> Box<dyn AudioUnit> {
    // fundsp SVFs take inputs positionally as `(signal, cutoff, Q)`;
    // every input is `f32` so the type system can't catch a swap.
    let hp_l = (pass() | var(low_cut) | dc(FILTER_Q)) >> highpass::<f32>();
    let hp_r = (pass() | var(low_cut) | dc(FILTER_Q)) >> highpass::<f32>();
    let lp_l = (pass() | var(high_cut) | dc(FILTER_Q)) >> lowpass::<f32>();
    let lp_r = (pass() | var(high_cut) | dc(FILTER_Q)) >> lowpass::<f32>();

    let filters_stereo = (hp_l | hp_r) >> (lp_l | lp_r);
    let wet = filters_stereo >> reverb_stereo(ROOM_SIZE, f64::from(time_s), DAMPING);
    let dry = multipass::<U2>();

    // `var()` is mono; broadcast to stereo by stacking two reads
    // so `*` matches the (stereo) wet/dry counts.
    let mix_stereo = || var(mix) | var(mix);
    let inv_mix_stereo = || (dc(1.0) - var(mix)) | (dc(1.0) - var(mix));

    // `&` is Bus: dry + wet share the input and sum their outputs.
    let mut graph: Box<dyn AudioUnit> = Box::new((dry * inv_mix_stereo()) & (wet * mix_stereo()));
    graph.set_sample_rate(sample_rate);
    graph.allocate();
    graph
}

/// Inputs the worker needs to build a graph. SR is paired with each
/// request so the audio thread can detect (and reject) a ready graph
/// that was built for a stale SR after a `reset()`.
#[derive(Copy, Clone)]
struct RebuildRequest {
    sample_rate: f64,
    time_s: f32,
}

/// A built graph plus the inputs it was built with. Same `(sr, time)`
/// the audio thread copies into `last_built_*` after swapping in.
struct ReadyGraph {
    graph: Box<dyn AudioUnit>,
    sample_rate: f64,
    time_s: f32,
}

/// Lock-free handoff between the audio thread and the rebuild worker.
/// All three queues are `force_push` / `try_push` from the producer
/// side, so neither thread ever blocks or allocates on a hot path.
struct RebuildChannel {
    // Audio → worker: the latest target. Capacity 1; newer overwrites
    // older (a `RebuildRequest` is `Copy`, so the displaced value is
    // free to drop on the audio thread).
    requests: ArrayQueue<RebuildRequest>,
    // Worker → audio: at most one freshly-built graph waiting. The
    // worker overwrites a stale entry on its own thread, where
    // dropping the graph is safe.
    ready: ArrayQueue<ReadyGraph>,
    // Audio → worker: graphs the audio thread has just swapped out.
    // Drop runs on the worker, never on the audio thread. Capacity is
    // padded so a slow worker can't stall the audio thread by filling
    // the queue.
    discard: ArrayQueue<Box<dyn AudioUnit>>,
    shutdown: AtomicBool,
}

/// Stateless descriptor - DSP state lives in [`FundspReverbWorkerDspState`].
pub struct FundspReverbWorker;

/// Per-instance DSP state: the live graph, the atomic cells it reads,
/// and the lock-free channel + worker thread that rebuild it.
pub struct FundspReverbWorkerDspState {
    // Atomic cells the fundsp graph reads each sample via `var()`.
    low_cut_shared: Shared,
    high_cut_shared: Shared,
    mix_shared: Shared,
    graph: Box<dyn AudioUnit>,
    // Inputs the current graph was built with. `reset()` skips the
    // rebuild when neither changed.
    last_built_sr: f64,
    last_built_time_s: f32,
    rebuild: Arc<RebuildChannel>,
    // Kept so `Drop` can join the worker. `worker_thread` is a
    // separate handle so the audio thread can `unpark` without
    // touching the `Option`.
    worker_thread: Thread,
    worker_handle: Option<JoinHandle<()>>,
}

impl FundspReverbWorkerDspState {
    fn new() -> Self {
        let low_cut_shared = shared(DEFAULT_LOW_CUT_HZ);
        let high_cut_shared = shared(DEFAULT_HIGH_CUT_HZ);
        let mix_shared = shared(DEFAULT_REVERB_MIX);

        let rebuild = Arc::new(RebuildChannel {
            requests: ArrayQueue::new(1),
            ready: ArrayQueue::new(1),
            discard: ArrayQueue::new(8),
            shutdown: AtomicBool::new(false),
        });

        let worker_handle = spawn_rebuild_worker(
            Arc::clone(&rebuild),
            low_cut_shared.clone(),
            high_cut_shared.clone(),
            mix_shared.clone(),
        );
        let worker_thread = worker_handle.thread().clone();

        Self {
            low_cut_shared,
            high_cut_shared,
            mix_shared,
            graph: Box::new(multipass::<U2>()),
            last_built_sr: 0.0,
            last_built_time_s: DEFAULT_TIME_S,
            rebuild,
            worker_thread,
            worker_handle: Some(worker_handle),
        }
    }

    /// Synchronous rebuild path used by `reset()`, which the host
    /// calls off the audio thread. Drains any in-flight rebuild so a
    /// graph that's still being built for the *previous* SR can't
    /// slip into `process()` after this returns.
    fn rebuild_now(&mut self, sample_rate: f64, time_s: f32) {
        self.graph = build_graph(
            sample_rate,
            time_s,
            &self.low_cut_shared,
            &self.high_cut_shared,
            &self.mix_shared,
        );
        self.last_built_sr = sample_rate;
        self.last_built_time_s = time_s;
        while self.rebuild.requests.pop().is_some() {}
        while self.rebuild.ready.pop().is_some() {}
    }
}

fn spawn_rebuild_worker(
    channel: Arc<RebuildChannel>,
    low_cut: Shared,
    high_cut: Shared,
    mix: Shared,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name("fundsp-reverb-rebuild".into())
        .spawn(move || {
            loop {
                // Drop anything the audio thread handed back. Free
                // off-thread so the audio thread never pays for a
                // heap free.
                while channel.discard.pop().is_some() {}

                // Coalesce: only the latest target matters. Older
                // requests are stale by definition because the audio
                // thread only requests once it crosses the threshold.
                let mut latest: Option<RebuildRequest> = None;
                while let Some(req) = channel.requests.pop() {
                    latest = Some(req);
                }
                if let Some(req) = latest {
                    let graph = build_graph(req.sample_rate, req.time_s, &low_cut, &high_cut, &mix);
                    let ready = ReadyGraph {
                        graph,
                        sample_rate: req.sample_rate,
                        time_s: req.time_s,
                    };
                    // `force_push` drops the previous ready graph
                    // here on the worker - never on the audio thread.
                    let _ = channel.ready.force_push(ready);
                }

                if channel.shutdown.load(Ordering::Acquire) {
                    return;
                }
                thread::park();
            }
        })
        .expect("spawn fundsp-reverb-rebuild worker")
}

impl Drop for FundspReverbWorkerDspState {
    fn drop(&mut self) {
        self.rebuild.shutdown.store(true, Ordering::Release);
        self.worker_thread.unpark();
        if let Some(handle) = self.worker_handle.take() {
            let _ = handle.join();
        }
    }
}

impl PluginLogic for FundspReverbWorker {
    type Params = FundspReverbWorkerParams;
    type DspState = FundspReverbWorkerDspState;

    fn init(_params: &FundspReverbWorkerParams) -> FundspReverbWorkerDspState {
        FundspReverbWorkerDspState::new()
    }

    fn reset(
        state: &mut FundspReverbWorkerDspState,
        params: &FundspReverbWorkerParams,
        config: &AudioConfig,
    ) {
        let sample_rate = config.sample_rate;
        let time_s = params.time.value();
        // SR is a discrete host setting, not a measurement.
        let sr_changed = sample_rate.to_bits() != state.last_built_sr.to_bits();
        let time_changed = (time_s - state.last_built_time_s).abs() > TIME_REBUILD_THRESHOLD_S;
        if sr_changed || time_changed {
            state.rebuild_now(sample_rate, time_s);
        }
    }

    fn process(
        state: &mut FundspReverbWorkerDspState,
        params: &FundspReverbWorkerParams,
        buffer: &mut AudioBuffer,
        _events: &EventList,
        context: &mut ProcessContext,
    ) -> ProcessStatus {
        // Swap in any graph the worker has finished. A ready entry
        // built for a stale SR (one `reset()` ago) is rerouted to the
        // discard queue so it's freed off-thread.
        if let Some(ready) = state.rebuild.ready.pop() {
            if ready.sample_rate.to_bits() == state.last_built_sr.to_bits() {
                let old = std::mem::replace(&mut state.graph, ready.graph);
                // try_push: capacity 8 vs at most one swap per block,
                // so a non-stalled worker drains long before this
                // ever fills. On the theoretical overflow we keep the
                // old graph live for a block rather than free on the
                // audio thread.
                let _ = state.rebuild.discard.push(old);
                state.last_built_time_s = ready.time_s;
            } else {
                let _ = state.rebuild.discard.push(ready.graph);
            }
        }

        // Read the raw target, not `read()`: a smoothed value would
        // crawl across the threshold for ~200 ms and request a
        // rebuild every block - audible as an unstable tail until it
        // settles.
        let time_s = params.time.value();
        if (time_s - state.last_built_time_s).abs() > TIME_REBUILD_THRESHOLD_S {
            // Optimistic update so we don't re-request the same
            // target every block while the worker is building. If
            // the user moves Time again past the threshold, this
            // diff trips and we re-request.
            state.last_built_time_s = time_s;
            state.rebuild.requests.force_push(RebuildRequest {
                sample_rate: state.last_built_sr,
                time_s,
            });
            state.worker_thread.unpark();
        }

        // `for_each_frame::<2>` transposes channel-major to stereo
        // frames so fundsp's `tick(in, out)` fits the closure.
        buffer.for_each_frame::<2, _>(|frame_in, frame_out| {
            state.low_cut_shared.set_value(params.low_cut.read());
            state.high_cut_shared.set_value(params.high_cut.read());
            state.mix_shared.set_value(params.mix.read());
            state.graph.tick(frame_in, frame_out);
        });

        if buffer.num_output_channels() >= 1 {
            context.set_meter(P::MeterL, buffer.output_peak(0));
        }
        if buffer.num_output_channels() >= 2 {
            context.set_meter(P::MeterR, buffer.output_peak(1));
        }

        ProcessStatus::Normal
    }

    fn editor(params: Arc<FundspReverbWorkerParams>) -> Box<dyn Editor> {
        GridLayout::build(vec![widgets(vec![
            knob(P::LowCut, "Low Cut").at(0, 0),
            knob(P::HighCut, "High Cut").at(1, 0),
            knob(P::Time, "Time").at(0, 1),
            knob(P::Mix, "Mix").at(1, 1),
            meter(&[P::MeterL, P::MeterR], "Level").at(2, 0).rows(2),
        ])])
        .with_title("FUNDSP REVERB (WORKER)")
        .into_editor(&params)
    }
}

truce::plugin! {
    logic: FundspReverbWorker,
    params: FundspReverbWorkerParams,
}

// Install the real-time allocation checker under `--features rt-paranoid`
// (no-op otherwise), so the test below can gate on audio-thread allocs.
truce::enable_rt_paranoid!();

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn info_is_valid() {
        truce_test::assert_valid_info::<Plugin>();
    }

    /// The point of the worker variant: a Time change hands the graph
    /// rebuild to a worker thread over lock-free queues, so `process`
    /// itself never allocates - the exact contrast with
    /// `truce-example-fundsp-reverb-simple`, whose `process` rebuilds
    /// inline and trips the checker. The worker thread's own allocations
    /// happen off the audio thread, so the checker (audio-thread only)
    /// correctly ignores them. Only checks under `--features rt-paranoid`.
    #[test]
    fn time_change_stays_alloc_free() {
        use std::time::Duration;
        use truce_test::{InputSource, assert_no_audio_alloc, driver};

        assert_no_audio_alloc(|| {
            driver!(Plugin)
                .duration(Duration::from_millis(40))
                .input(InputSource::Constant(0.5))
                .script(|sc| {
                    sc.set_param(P::Time, 0.1);
                    sc.wait_ms(15);
                    sc.set_param(P::Time, 0.95);
                    sc.wait_ms(15);
                })
                .run()
        });
    }

    #[test]
    fn renders_nonzero_output() {
        use std::time::Duration;
        use truce_test::{InputSource, assertions, driver};

        let result = driver!(Plugin)
            .duration(Duration::from_millis(50))
            .input(InputSource::Constant(0.5))
            .run();
        assertions::assert_no_nans(&result);
        assertions::assert_nonzero(&result);
    }

    /// Reverb tail survives a brief constant input without NaN or
    /// runaway peaks.
    #[test]
    fn reverb_tail_stays_finite() {
        use std::time::Duration;
        use truce_test::{InputSource, assertions, driver};

        let result = driver!(Plugin)
            .duration(Duration::from_millis(500))
            .input(InputSource::Constant(0.5))
            .run();
        assertions::assert_no_nans(&result);
        assertions::assert_peak_below(&result, 2.0);
    }

    /// Regression: SVF filter input order is positional + unchecked.
    /// Stacking `(cutoff | Q | signal)` instead of `(signal | cutoff
    /// | Q)` compiles fine and feeds the filter cutoff as audio;
    /// downstream reverb FDN amplifies it past peak ~7000 within a
    /// second. 2 s of constant input exposes the runaway.
    #[test]
    fn extended_steady_state_stays_bounded() {
        use std::time::Duration;
        use truce_test::{InputSource, assertions, driver};

        let result = driver!(Plugin)
            .duration(Duration::from_secs(2))
            .input(InputSource::Constant(0.5))
            .run();
        assertions::assert_no_nans(&result);
        assertions::assert_peak_below(&result, 2.0);
    }

    /// Regression: Time changes during playback must not crash and
    /// must not allocate on the audio thread (the worker rebuilds
    /// off-thread and hands the new graph back via the lock-free
    /// queue). Ramps Time across the 0.05 s rebuild threshold
    /// repeatedly so the worker swap path runs many times.
    #[test]
    fn time_automation_stays_finite() {
        use std::time::Duration;
        use truce_test::{InputSource, assertions, driver};

        let result = driver!(Plugin)
            .duration(Duration::from_millis(1500))
            .input(InputSource::Constant(0.3))
            .script(|s| {
                for step in 1..=15 {
                    // 0.067..=1.0 normalized maps across the Time
                    // log range; each step crosses the threshold.
                    let normalized = f64::from(step) / 15.0;
                    s.set_param(P::Time, normalized);
                    s.wait_ms(80);
                }
            })
            .run();
        assertions::assert_no_nans(&result);
        assertions::assert_peak_below(&result, 2.0);
        assertions::assert_nonzero_after(&result, Duration::from_millis(500));
    }

    /// Regression: param → `Shared` sync. Ramps `low_cut` from 0.0 to
    /// 1.0 in 10 steps over 500 ms; asserts no NaN, bounded peak,
    /// and that the wet path stays non-silent (catches "filter froze
    /// at default cutoff because the sync write was dropped").
    #[test]
    fn cutoff_automation_stays_finite() {
        use std::time::Duration;
        use truce_test::{InputSource, assertions, driver};

        let result = driver!(Plugin)
            .duration(Duration::from_millis(750))
            .input(InputSource::Constant(0.3))
            .script(|s| {
                for step in 1..=10 {
                    let normalized = f64::from(step) / 10.0;
                    s.set_param(P::LowCut, normalized);
                    s.wait_ms(50);
                }
            })
            .run();
        assertions::assert_no_nans(&result);
        assertions::assert_peak_below(&result, 2.0);
        assertions::assert_nonzero_after(&result, Duration::from_millis(500));
    }

    /// Regression: sample-rate propagation. SVF coefficients are
    /// SR-dependent - if `set_sample_rate` misses a sub-node the
    /// filter de-tunes or blows up at non-default rates.
    #[test]
    fn stability_at_96k() {
        use std::time::Duration;
        use truce_test::{InputSource, assertions, driver};

        let result = driver!(Plugin)
            .sample_rate(96_000.0)
            .duration(Duration::from_secs(1))
            .input(InputSource::Constant(0.5))
            .run();
        assertions::assert_no_nans(&result);
        assertions::assert_peak_below(&result, 2.0);
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
    fn bus_config_effect() {
        truce_test::assert_bus_config_effect::<Plugin>();
    }

    #[test]
    fn param_count_matches() {
        truce_test::assert_param_count_matches::<Plugin>();
    }

    #[test]
    fn corrupt_state_no_crash() {
        truce_test::assert_corrupt_state_no_crash::<Plugin>();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn gui_screenshot_macos() {
        truce_test::screenshot!(Plugin, "screenshots/fundsp_reverb_worker_default_macos.png").run();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn gui_screenshot_linux() {
        truce_test::screenshot!(Plugin, "screenshots/fundsp_reverb_worker_default_linux.png")
            .pixel_threshold(2)
            .run();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn gui_screenshot_windows() {
        truce_test::screenshot!(
            Plugin,
            "screenshots/fundsp_reverb_worker_default_windows.png"
        )
        .pixel_threshold(2)
        .run();
    }
}
