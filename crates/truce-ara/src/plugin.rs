//! The author-facing ARA trait (`AraPlugin`) and the wrapper-facing
//! export seam (`AraExport`).

use std::sync::Arc;

use truce_core::export::PluginExport;
use truce_core::{AudioBuffer, ProcessStatus, Sample, TransportInfo};
use truce_params::Params;

use crate::abi::ARAFactory;
use crate::audio_source::{AnalysisProgress, AraAudioSource};

/// One playback region assigned to the renderer, paired with its audio
/// source's analysis (`None` until analysis completes). Times are in
/// seconds on the playback and modification timelines. Holds the
/// analysis as an `Arc` so the renderer can keep a reusable region list
/// across blocks; read it with [`PlaybackRegion::analysis`].
pub struct PlaybackRegion<A> {
    analysis: Option<Arc<A>>,
    pub start_in_playback_time: f64,
    pub duration_in_playback_time: f64,
    pub start_in_modification_time: f64,
    pub duration_in_modification_time: f64,
}

impl<A> PlaybackRegion<A> {
    #[doc(hidden)]
    #[must_use]
    pub fn new(
        analysis: Option<Arc<A>>,
        start_in_playback_time: f64,
        duration_in_playback_time: f64,
        start_in_modification_time: f64,
        duration_in_modification_time: f64,
    ) -> Self {
        Self {
            analysis,
            start_in_playback_time,
            duration_in_playback_time,
            start_in_modification_time,
            duration_in_modification_time,
        }
    }

    /// The source analysis for this region, or `None` while it is still
    /// analyzing.
    #[must_use]
    pub fn analysis(&self) -> Option<&A> {
        self.analysis.as_deref()
    }
}

/// Per-block context for ARA rendering - the ARA analogue of
/// `ProcessContext`, minus the MIDI event list (ARA regions, not events,
/// drive playback timing).
pub struct AraRenderContext<'a> {
    pub transport: &'a TransportInfo,
    pub sample_rate: f64,
    pub block_size: usize,
    /// The instance's assigned ARA roles
    /// (`ARAPlugInInstanceRoleFlags`): a bitmask of
    /// `kARAPlaybackRendererRole` (this block feeds the host's timeline
    /// playback) and/or `kARAEditorRendererRole` (this block feeds the
    /// editor's edit preview). A plug-in can render differently for each,
    /// or ignore it.
    pub roles: crate::abi::ARAPlugInInstanceRoleFlags,
}

/// A plug-in's ARA capability. A truce plug-in implements this alongside
/// its normal `PluginLogic` to opt into ARA; the format wrappers detect
/// it (via the `ara` feature) and expose the ARA factory + plug-in
/// extension so an ARA host binds the instance to a document controller.
///
/// The associated types mirror the plug-in's own (`Sample`, `DspState`,
/// `Params`) so [`AraPlugin::render_regions`] can reuse the audio buffer
/// and DSP state; a plug-in sets them to the same types its `PluginLogic`
/// uses.
pub trait AraPlugin: Send + 'static {
    /// How this plug-in transforms region playback, advertised to the host
    /// in the ARA factory (`ARAPlaybackTransformationFlags`, e.g.
    /// [`kARAPlaybackTransformationTimestretch`]). A host only engages a
    /// plug-in as a playback renderer when it declares it changes playback
    /// somehow; the default [`kARAPlaybackTransformationNoChanges`] means
    /// the host is free to play the source directly and never route
    /// rendering through the plug-in (so `render_regions` never fires).
    ///
    /// [`kARAPlaybackTransformationTimestretch`]: crate::abi::kARAPlaybackTransformationTimestretch
    /// [`kARAPlaybackTransformationNoChanges`]: crate::abi::kARAPlaybackTransformationNoChanges
    const PLAYBACK_TRANSFORMATION_FLAGS: i32 = crate::abi::kARAPlaybackTransformationNoChanges;

    /// Immutable per-audio-source analysis result, produced off the audio
    /// thread and published to the renderer through a lock-free slot.
    /// `Sync` because the render thread reads it by shared reference.
    type Analysis: Default + Send + Sync + 'static;

    /// The editable layer (pitch edits, time warps, ...) persisted and
    /// restored across sessions.
    type Edits: Default + Send + 'static;

    /// Audio sample precision - the same `f32` / `f64` the plug-in's
    /// `PluginLogic` uses.
    type Sample: Sample;

    /// The plug-in's DSP state (the same type its `PluginLogic` names).
    type DspState: Send + 'static;

    /// The plug-in's parameter struct.
    type Params: Params;

    /// Analyze one audio source off the audio thread (on truce's task
    /// pool). Read the source's samples through `source`, report progress
    /// through `progress`, and return the analysis result the renderer
    /// will use. The framework brackets the call with progress
    /// start/complete, so a plug-in that reports no intermediate progress
    /// can ignore `progress`.
    ///
    /// The default performs no analysis (right for a plug-in that only
    /// renders, or the phase-1 passthrough example).
    #[must_use]
    fn analyze(source: &AraAudioSource<'_>, progress: &AnalysisProgress) -> Self::Analysis {
        let _ = (source, progress);
        Self::Analysis::default()
    }

    /// Render the assigned playback regions into `buffer` on the audio
    /// thread. An ARA playback renderer *generates* its output from the
    /// regions' analyses and ignores the input signal, so the same
    /// audio-thread discipline as `process` applies (no allocation, no
    /// locks). The default writes silence.
    fn render_regions(
        state: &mut Self::DspState,
        params: &Self::Params,
        regions: &[PlaybackRegion<Self::Analysis>],
        buffer: &mut AudioBuffer<'_, Self::Sample>,
        context: &AraRenderContext<'_>,
    ) -> ProcessStatus {
        let _ = (state, params, regions, context);
        for ch in 0..buffer.num_output_channels() {
            for s in buffer.output(ch) {
                *s = Self::Sample::default();
            }
        }
        ProcessStatus::Normal
    }

    /// Serialize one audio source's analysis into `out` for the host's
    /// ARA archive, so a re-opened session restores it instead of
    /// re-analyzing. Runs on the host archiving thread (not the audio
    /// thread). The default writes nothing - right for a plug-in with no
    /// persistent analysis. Pair with [`AraPlugin::restore`].
    ///
    /// (Editable state - `Edits` - persists once the editor phase adds
    /// per-modification edit state.)
    fn store(analysis: &Self::Analysis, out: &mut Vec<u8>) {
        let _ = (analysis, out);
    }

    /// Deserialize an analysis from a blob produced by
    /// [`AraPlugin::store`]. `None` if the bytes aren't recognized (a
    /// stale or foreign archive), in which case the source re-analyzes.
    #[must_use]
    fn restore(bytes: &[u8]) -> Option<Self::Analysis> {
        let _ = bytes;
        None
    }
}

/// The ARA analogue of [`PluginExport`], implemented for the generated
/// wrapper type by `truce::plugin!` when ARA is enabled. Format wrappers
/// query it to retrieve the plug-in's static [`ARAFactory`].
pub trait AraExport: PluginExport {
    /// The author's [`AraPlugin`] type (usually the same type that
    /// implements `PluginLogic`).
    type Ara: AraPlugin;

    /// The ARA factory the host asks for after discovery - one per
    /// plug-in *type*, not per instance. The default builds it from this
    /// plug-in's [`PluginExport::info`] metadata and monomorphized ARA
    /// thunks; there is no reason to override it.
    #[must_use]
    fn ara_factory() -> &'static ARAFactory {
        crate::factory::ara_factory::<Self>()
    }
}
