//! Host audio access + analysis-progress reporting (phase 3).
//!
//! During analysis the plug-in reads a source's samples at random
//! positions through the host's [`ARAAudioAccessControllerInterface`],
//! and reports progress back through the
//! [`ARAModelUpdateControllerInterface`]. Both run on the analysis
//! (task-pool) thread, never the audio thread. ARA permits calling these
//! host callbacks from any non-realtime thread (one thread per reader),
//! which is why the surrounding job can carry the raw host pointers to a
//! pool worker.

use core::ffi::c_void;
use core::marker::PhantomData;

use crate::abi::{
    ARAAudioAccessControllerHostRef, ARAAudioAccessControllerInterface, ARAAudioReaderHostRef,
    ARAAudioSourceHostRef, ARAModelUpdateControllerHostRef, ARAModelUpdateControllerInterface,
    kARAAnalysisProgressCompleted, kARAAnalysisProgressStarted, kARAAnalysisProgressUpdated,
    kARAFalse,
};

/// A read failed at the host I/O layer (e.g. a network drop mid-file).
/// Out-of-range reads are *not* errors - the host fills them with
/// silence per the ARA contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AraReadError;

/// A safe, bounded reader over one ARA audio source. Holds the host's
/// audio-reader handle for its lifetime (created on construction,
/// destroyed on drop) and reads non-interleaved `f32` channels.
pub struct AraAudioSource<'a> {
    access_host_ref: ARAAudioAccessControllerHostRef,
    access: *const ARAAudioAccessControllerInterface,
    reader: ARAAudioReaderHostRef,
    channel_count: u32,
    sample_count: i64,
    sample_rate: f64,
    _marker: PhantomData<&'a ()>,
}

impl AraAudioSource<'_> {
    /// Create a 32-bit reader for `source_host_ref`. Returns `None` if
    /// the host can't create a reader.
    ///
    /// # Safety
    /// `access` must be a valid `ARAAudioAccessControllerInterface` and
    /// `access_host_ref` / `source_host_ref` the host refs it was handed,
    /// all valid for the returned reader's lifetime.
    pub(crate) unsafe fn new(
        access_host_ref: ARAAudioAccessControllerHostRef,
        access: *const ARAAudioAccessControllerInterface,
        source_host_ref: ARAAudioSourceHostRef,
        channel_count: u32,
        sample_count: i64,
        sample_rate: f64,
    ) -> Option<Self> {
        if access.is_null() {
            return None;
        }
        // SAFETY: caller guarantees `access` is a valid interface.
        let create = unsafe { (*access).create_audio_reader_for_source }?;
        // SAFETY: valid host refs per the safety contract; `kARAFalse`
        // selects 32-bit samples.
        let reader = unsafe { create(access_host_ref, source_host_ref, kARAFalse) };
        if reader.is_null() {
            return None;
        }
        Some(Self {
            access_host_ref,
            access,
            reader,
            channel_count,
            sample_count,
            sample_rate,
            _marker: PhantomData,
        })
    }

    #[must_use]
    pub fn channel_count(&self) -> u32 {
        self.channel_count
    }

    #[must_use]
    pub fn sample_count(&self) -> i64 {
        self.sample_count
    }

    #[must_use]
    pub fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    /// Read `channels.len()` non-interleaved channels starting at
    /// `start_frame`. Every channel slice must be the same length; that
    /// length is the per-channel sample count requested. Frames outside
    /// `[0, sample_count)` are filled with silence by the host (not an
    /// error). `channels.len()` must equal [`Self::channel_count`].
    ///
    /// # Errors
    /// [`AraReadError`] on a host-side I/O failure; the buffers are then
    /// left as the host set them (silence).
    pub fn read(&self, start_frame: i64, channels: &mut [&mut [f32]]) -> Result<(), AraReadError> {
        if channels.len() != self.channel_count as usize {
            return Err(AraReadError);
        }
        let per_channel = channels.first().map_or(0, |c| c.len());
        if per_channel == 0 {
            return Ok(());
        }
        if channels.iter().any(|c| c.len() != per_channel) {
            return Err(AraReadError);
        }
        // Non-realtime analysis thread: the small per-read pointer-array
        // allocation is fine here.
        let ptrs: Vec<*mut c_void> = channels.iter_mut().map(|c| c.as_mut_ptr().cast()).collect();
        // SAFETY: `access` is valid; `reader` is our live reader; `ptrs`
        // holds `channel_count` buffers each `per_channel` long.
        let read = unsafe { (*self.access).read_audio_samples }.ok_or(AraReadError)?;
        #[allow(clippy::cast_possible_wrap)]
        let ok = unsafe {
            read(
                self.access_host_ref,
                self.reader,
                start_frame,
                per_channel as i64,
                ptrs.as_ptr(),
            )
        };
        // Keep `ptrs` alive across the call.
        drop(ptrs);
        if ok == kARAFalse {
            Err(AraReadError)
        } else {
            Ok(())
        }
    }
}

impl Drop for AraAudioSource<'_> {
    fn drop(&mut self) {
        if self.access.is_null() {
            return;
        }
        // SAFETY: `access` is valid and `reader` is the live reader we
        // created; the ARA contract requires it not be in use elsewhere,
        // which holds since analysis is single-threaded per reader.
        if let Some(destroy) = unsafe { (*self.access).destroy_audio_reader } {
            unsafe { destroy(self.access_host_ref, self.reader) };
        }
    }
}

/// Reports analysis progress back to the host for one audio source.
/// A null model-update interface (the host didn't provide one) makes
/// every call a no-op.
pub struct AnalysisProgress {
    model_host_ref: ARAModelUpdateControllerHostRef,
    model: *const ARAModelUpdateControllerInterface,
    source_host_ref: ARAAudioSourceHostRef,
}

impl AnalysisProgress {
    pub(crate) fn new(
        model_host_ref: ARAModelUpdateControllerHostRef,
        model: *const ARAModelUpdateControllerInterface,
        source_host_ref: ARAAudioSourceHostRef,
    ) -> Self {
        Self {
            model_host_ref,
            model,
            source_host_ref,
        }
    }

    fn notify(&self, state: i32, value: f32) {
        if self.model.is_null() {
            return;
        }
        // SAFETY: non-null model interface; `notify_audio_source_analysis_progress`
        // is the first slot after `struct_size`, always present.
        if let Some(notify) = unsafe { (*self.model).notify_audio_source_analysis_progress } {
            unsafe { notify(self.model_host_ref, self.source_host_ref, state, value) };
        }
    }

    /// Signal that analysis has begun.
    pub fn start(&self) {
        self.notify(kARAAnalysisProgressStarted, 0.0);
    }

    /// Report intermediate progress in `0.0..=1.0`.
    pub fn update(&self, value: f32) {
        self.notify(kARAAnalysisProgressUpdated, value.clamp(0.0, 1.0));
    }

    /// Signal that analysis is complete.
    pub fn complete(&self) {
        self.notify(kARAAnalysisProgressCompleted, 1.0);
    }
}

#[cfg(test)]
mod tests {
    use core::mem::size_of;
    use core::ptr;

    use super::*;
    use crate::abi::{
        ARAAudioAccessControllerHostRef, ARAAudioReaderHostRef, ARAAudioSourceHostRef, ARABool,
        ARASampleCount, kARATrue,
    };

    // A dummy object whose address is a valid, non-null reader ref.
    static READER: u8 = 0;

    unsafe extern "C" fn mock_create_reader(
        _h: ARAAudioAccessControllerHostRef,
        _s: ARAAudioSourceHostRef,
        _use64: ARABool,
    ) -> ARAAudioReaderHostRef {
        ptr::from_ref(&READER).cast_mut().cast()
    }

    /// Fill two channels with a deterministic ramp:
    /// `sample(ch, i) = (position + i) * 10 + ch`.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss
    )]
    unsafe extern "C" fn mock_read(
        _h: ARAAudioAccessControllerHostRef,
        _r: ARAAudioReaderHostRef,
        position: ARASampleCount,
        count: ARASampleCount,
        buffers: *const *mut c_void,
    ) -> ARABool {
        for ch in 0..2usize {
            let buf = unsafe { *buffers.add(ch) }.cast::<f32>();
            for i in 0..count as usize {
                let v = (position + i as i64) * 10 + ch as i64;
                unsafe { *buf.add(i) = v as f32 };
            }
        }
        kARATrue
    }

    unsafe extern "C" fn mock_destroy(
        _h: ARAAudioAccessControllerHostRef,
        _r: ARAAudioReaderHostRef,
    ) {
    }

    #[test]
    #[allow(clippy::float_cmp)]
    fn reads_non_interleaved_samples() {
        let iface = ARAAudioAccessControllerInterface {
            struct_size: size_of::<ARAAudioAccessControllerInterface>(),
            create_audio_reader_for_source: Some(mock_create_reader),
            read_audio_samples: Some(mock_read),
            destroy_audio_reader: Some(mock_destroy),
        };
        // SAFETY: `iface` is a valid interface for this call's duration.
        let source = unsafe {
            AraAudioSource::new(
                ptr::null_mut(),
                ptr::from_ref(&iface),
                ptr::null_mut(),
                2,
                8,
                48_000.0,
            )
        }
        .expect("reader");
        assert_eq!(source.channel_count(), 2);
        assert_eq!(source.sample_count(), 8);
        assert!((source.sample_rate() - 48_000.0).abs() < f64::EPSILON);

        let mut left = [0f32; 4];
        let mut right = [0f32; 4];
        let mut channels: [&mut [f32]; 2] = [&mut left, &mut right];
        source.read(2, &mut channels).expect("read");
        // position 2: left = (2+i)*10, right = that + 1.
        assert_eq!(left, [20.0, 30.0, 40.0, 50.0]);
        assert_eq!(right, [21.0, 31.0, 41.0, 51.0]);

        // Channel-count mismatch is rejected.
        let mut only = [0f32; 4];
        let mut one: [&mut [f32]; 1] = [&mut only];
        assert_eq!(source.read(0, &mut one), Err(AraReadError));
    }
}
