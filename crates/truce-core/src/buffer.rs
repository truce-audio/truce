use truce_params::sample::Sample;

/// Non-interleaved audio buffer. Borrows host memory through the
/// format wrapper.
///
/// Generic over the sample type `S` (the plugin's chosen precision,
/// `f32` or `f64`). The format wrapper bridges between host-buffer
/// precision and `S` at the block boundary - see
/// [`RawBufferScratch::build`]. Plugin code under
/// `use truce::prelude::*;` (f32) or `use truce::prelude64::*;` (f64)
/// sees `AudioBuffer<S>` with `S` already picked.
///
/// **In-place I/O.** Some hosts (Reaper, pluginval) pass the same
/// buffer for both input and output of a given channel. By default
/// the wrapper copies the aliased inputs into per-channel scratch so
/// `input(ch)` and `output(ch)` are disjoint `&[S]` / `&mut [S]` -
/// no plugin code change required. Plugins that opt into
/// `Plugin::supports_in_place() = true` skip the copy and must use
/// [`Self::in_out_mut`] for channels where [`Self::is_in_place`]
/// returns `true`.
pub struct AudioBuffer<'a, S: Sample = f32> {
    inputs: &'a [&'a [S]],
    outputs: &'a mut [&'a mut [S]],
    /// Bit `ch` is set when `inputs[ch]` and `outputs[ch]` point to
    /// the same host memory. Channels ≥ 64 are always reported as
    /// non-aliased - formats with that many channels are exotic
    /// enough to be a follow-up.
    in_place_mask: u64,
    offset: usize,
    num_samples: usize,
}

impl<'a, S: Sample> AudioBuffer<'a, S> {
    /// Safe wrapper around [`Self::from_slices`] for callers that hold their
    /// own owned `Vec<Vec<S>>` (e.g. `truce-driver`'s test harness).
    /// Forwards to the unsafe constructor - the borrow checker proves
    /// the lifetime invariants the `unsafe fn` requires when both
    /// slice arrays and the buffer itself live in the same scope.
    /// `num_samples > slice length` still asserts in debug builds.
    pub fn from_slices_checked(
        inputs: &'a [&'a [S]],
        outputs: &'a mut [&'a mut [S]],
        num_samples: usize,
    ) -> Self {
        // SAFETY: caller hands us references that the borrow checker
        // already proved valid for `'a`; the debug-mode assertions
        // inside `from_slices` cover the `num_samples` bound.
        unsafe { Self::from_slices(inputs, outputs, num_samples) }
    }

    /// Create a buffer from pre-split channel slices.
    /// Used by format wrappers after converting from host-specific buffer types.
    ///
    /// # Safety
    /// The caller must ensure the slices are valid for the lifetime `'a`
    /// and that `num_samples` does not exceed any slice's length.
    ///
    /// # Panics
    ///
    /// In debug builds only, panics if any input channel aliases an
    /// output channel or `num_samples` exceeds the length of any
    /// input/output slice. Release builds skip these checks (they're
    /// safety preconditions, not runtime invariants).
    pub unsafe fn from_slices(
        inputs: &'a [&'a [S]],
        outputs: &'a mut [&'a mut [S]],
        num_samples: usize,
    ) -> Self {
        #[cfg(debug_assertions)]
        {
            // Verify no input channel aliases any output channel.
            for (i, inp) in inputs.iter().enumerate() {
                let i_start = inp.as_ptr() as usize;
                let i_end = i_start + std::mem::size_of_val(*inp);
                for (o, out) in outputs.iter().enumerate() {
                    let o_start = out.as_ptr() as usize;
                    let o_end = o_start + std::mem::size_of_val(*out);
                    assert!(
                        i_end <= o_start || o_end <= i_start,
                        "AudioBuffer: input channel {i} and output channel {o} alias \
                         - pass disjoint slices or use RawBufferScratch::build which \
                         handles aliasing automatically",
                    );
                }
            }
            // Verify num_samples doesn't exceed any slice length.
            for (i, inp) in inputs.iter().enumerate() {
                assert!(
                    num_samples <= inp.len(),
                    "AudioBuffer: num_samples ({num_samples}) exceeds input channel {i} length ({})",
                    inp.len(),
                );
            }
            for (o, out) in outputs.iter().enumerate() {
                assert!(
                    num_samples <= out.len(),
                    "AudioBuffer: num_samples ({num_samples}) exceeds output channel {o} length ({})",
                    out.len(),
                );
            }
        }
        AudioBuffer {
            inputs,
            outputs,
            in_place_mask: 0,
            offset: 0,
            num_samples,
        }
    }

    /// Set the in-place mask. Called by format wrappers (or
    /// `RawBufferScratch::build`) after construction once they've
    /// determined which channels alias on the host side.
    #[inline]
    pub fn set_in_place_mask(&mut self, mask: u64) {
        self.in_place_mask = mask;
    }

    /// `true` when the host passes a single buffer for both input and
    /// output of `ch` (in-place I/O). Use [`Self::in_out_mut`] to read
    /// and write that buffer directly when this returns `true`.
    #[must_use]
    pub fn is_in_place(&self, ch: usize) -> bool {
        ch < 64 && (self.in_place_mask >> ch) & 1 == 1
    }

    /// Read+write slice for an in-place channel - the same memory the
    /// host gave us for both input and output. Each sample reads as
    /// the input value before the plugin overwrites it.
    ///
    /// Only meaningful when [`Self::is_in_place`] returns `true`. On a
    /// non-in-place channel this returns the output slice with no
    /// input data in it; reading is allowed but produces uninitialized
    /// host-buffer contents.
    pub fn in_out_mut(&mut self, ch: usize) -> &mut [S] {
        let end = self.offset + self.num_samples;
        &mut self.outputs[ch][self.offset..end]
    }

    #[must_use]
    pub fn num_samples(&self) -> usize {
        self.num_samples
    }

    #[must_use]
    pub fn num_input_channels(&self) -> usize {
        self.inputs.len()
    }

    #[must_use]
    pub fn num_output_channels(&self) -> usize {
        self.outputs.len()
    }

    #[must_use]
    pub fn input(&self, channel: usize) -> &[S] {
        let end = self.offset + self.num_samples;
        &self.inputs[channel][self.offset..end]
    }

    pub fn output(&mut self, channel: usize) -> &mut [S] {
        let end = self.offset + self.num_samples;
        &mut self.outputs[channel][self.offset..end]
    }

    /// Number of channels (min of input and output).
    #[must_use]
    pub fn channels(&self) -> usize {
        self.inputs.len().min(self.outputs.len())
    }

    /// Get an input/output pair for a channel. Useful for in-place processing.
    pub fn io_pair(&mut self, in_ch: usize, out_ch: usize) -> (&[S], &mut [S]) {
        let end = self.offset + self.num_samples;
        let input = &self.inputs[in_ch][self.offset..end];
        let output = &mut self.outputs[out_ch][self.offset..end];
        (input, output)
    }

    /// Get an input/output pair for the same channel index. Shorthand for `io_pair(ch, ch)`.
    pub fn io(&mut self, ch: usize) -> (&[S], &mut [S]) {
        self.io_pair(ch, ch)
    }

    /// Iterate per-frame and hand a fixed-size `(input, output)`
    /// stack-array pair to `tick`. Sized at the type level by const
    /// generic `N`, which must equal [`Self::channels`].
    ///
    /// `io()` / `io_pair()` give a per-channel slice view, which is
    /// the right shape for "process channel `ch` in isolation"
    /// loops. But libraries that expect a per-frame `(in: &[S],
    /// out: &mut [S])` callback - `fundsp::AudioUnit::tick`,
    /// `nih_plug`'s frame iterators, custom per-sample DSP nodes -
    /// can't take that shape directly without either copying inputs
    /// into a scratch first (heap allocation on the audio thread)
    /// or fighting the borrow checker over two simultaneous `&mut`
    /// borrows of the buffer.
    ///
    /// This helper does the per-frame transpose in-place against a
    /// stack-allocated `[S; N]` pair, calls `tick` `num_samples()`
    /// times, and writes back. No heap, no borrow gymnastics at the
    /// call site:
    ///
    /// ```ignore
    /// // Stereo plugin delegating per-frame DSP to fundsp:
    /// buffer.for_each_frame::<2, _>(|frame_in, frame_out| {
    ///     self.graph.tick(frame_in, frame_out);
    /// });
    /// ```
    ///
    /// `&[S; N]` deref-coerces to `&[S]` at the call site, so
    /// callers can pass the arrays straight to slice-taking APIs
    /// like fundsp's `tick`.
    ///
    /// # Panics
    ///
    /// Debug builds panic if `N != self.channels()`. Release builds
    /// rely on the same precondition without checking; reading past
    /// the actual channel count would index out of bounds anyway.
    pub fn for_each_frame<const N: usize, F>(&mut self, mut tick: F)
    where
        F: FnMut(&[S; N], &mut [S; N]),
    {
        debug_assert_eq!(
            N,
            self.channels(),
            "for_each_frame::<{N}> requires the buffer to have exactly {N} channels"
        );
        let mut frame_in = [S::default(); N];
        let mut frame_out = [S::default(); N];
        let end = self.offset + self.num_samples;
        for i in self.offset..end {
            for (ch, slot) in frame_in.iter_mut().enumerate() {
                *slot = self.inputs[ch][i];
            }
            tick(&frame_in, &mut frame_out);
            for (ch, sample) in frame_out.iter().enumerate() {
                self.outputs[ch][i] = *sample;
            }
        }
    }

    /// Peak absolute value across an output channel, returned as `f32`
    /// because meters / UI display always work in `f32` regardless of
    /// the plugin's internal precision.
    ///
    /// Short-circuits and returns `f32::NAN` on the **first** NaN
    /// sample seen, so meters can flag runaway plugins instead of
    /// silently reporting "peaks within range" while NaN poison
    /// spreads downstream.
    #[must_use]
    pub fn output_peak(&self, ch: usize) -> f32 {
        let end = self.offset + self.num_samples;
        let mut peak = 0.0f32;
        for &b in &self.outputs[ch][self.offset..end] {
            let v = b.to_f32();
            if v.is_nan() {
                return f32::NAN;
            }
            let abs = v.abs();
            if abs > peak {
                peak = abs;
            }
        }
        peak
    }

    /// Return a sub-block view covering samples `start..start+len`.
    ///
    /// The returned buffer borrows `self` exclusively - you cannot use
    /// the original buffer while the slice is alive.
    ///
    /// # Panics
    /// Panics if `start + len > self.num_samples()`.
    pub fn slice(&mut self, start: usize, len: usize) -> AudioBuffer<'_, S> {
        assert!(
            start + len <= self.num_samples,
            "slice({start}, {len}) out of bounds for buffer of {} samples",
            self.num_samples,
        );
        let new_offset = self.offset + start;
        // SAFETY: We construct an AudioBuffer<'a, S> and transmute to AudioBuffer<'_, S>.
        // These have identical memory layout (lifetimes are erased at runtime).
        // This is sound because:
        // 1. &mut self prevents the caller from using self while the slice exists
        // 2. The underlying channel memory lives for 'a which outlives '_
        // 3. Bounds are checked by the assert above
        let self_ptr: *mut Self = self;
        unsafe {
            let s = &mut *self_ptr;
            std::mem::transmute::<AudioBuffer<'a, S>, AudioBuffer<'_, S>>(AudioBuffer {
                inputs: s.inputs,
                outputs: &mut *s.outputs,
                in_place_mask: s.in_place_mask,
                offset: new_offset,
                num_samples: len,
            })
        }
    }
}

/// Scratch space for `RawBufferScratch::build` / `build_widening`.
///
/// Callers allocate this on the stack and pass it to a `build*`
/// method. The buffer borrows the slices stored here, so this struct
/// must outlive the returned `AudioBuffer`.
///
/// Generic over the plugin's sample type `S`. When the host buffer
/// matches `S`, slices point into host memory (zero-copy). When the
/// host buffer is a different precision, the input is widened/narrowed
/// into per-channel scratch; the output is rendered into scratch and
/// the wrapper copies + casts it back to the host buffer at the end
/// of the block (`finish_widening`).
pub struct RawBufferScratch<S: Sample = f32> {
    pub input_slices: Vec<&'static [S]>,
    pub output_slices: Vec<&'static mut [S]>,
    /// Per-channel input copies. Used (a) when the host passes the
    /// same buffer for input and output (in-place processing - VST3
    /// spec allows this and several real DAWs use it for effects),
    /// or (b) when the host buffer precision differs from `S` and
    /// we widen/narrow on the way in. In either case the slice the
    /// plugin sees points into the matching slot here.
    input_copies: Vec<Vec<S>>,
    /// Per-channel output scratch. Only populated by
    /// [`Self::build_widening`] when the host buffer precision differs
    /// from `S`; the wrapper copies + casts these back to the host
    /// buffer at the end of the block via [`Self::finish_widening`].
    output_buffers: Vec<Vec<S>>,
}

impl<S: Sample> RawBufferScratch<S> {
    /// Build an `AudioBuffer<S>` from raw `f32` host pointers - the
    /// common case (CLAP, LV2, AAX always; VST3/VST2/AU 32-bit mode).
    ///
    /// When `S = f32`, slices point directly into host memory (modulo
    /// in-place input copying). When `S = f64`, every channel is
    /// widened into per-channel scratch and the wrapper must call
    /// [`Self::finish_widening_f32`] at the end of the block to copy
    /// the rendered samples back to the host's `f32` output pointers.
    ///
    /// # Safety
    /// - `inputs` must point to `num_in` valid `*const f32` pointers
    ///   (each non-null pointer must address at least `num_frames`
    ///   readable samples; null is allowed and yields an empty slice).
    /// - `outputs` must point to `num_out` valid `*mut f32` pointers
    ///   (each non-null pointer must address at least `num_frames`
    ///   writable samples; null is allowed and yields an empty slice).
    /// - The pointed-to memory must remain valid for the lifetime of
    ///   the returned `AudioBuffer`.
    pub unsafe fn build(
        &mut self,
        inputs: *const *const f32,
        outputs: *mut *mut f32,
        num_in: u32,
        num_out: u32,
        num_frames: u32,
        supports_in_place: bool,
    ) -> AudioBuffer<'_, S> {
        // SAFETY: forwarded - caller's contract is the same.
        unsafe {
            self.build_inner(
                inputs,
                outputs,
                num_in,
                num_out,
                num_frames,
                supports_in_place,
            )
        }
    }

    /// Copy + narrow the rendered `S` output back to the host's
    /// `f32` output pointers. No-op when `S = f32` (the slices the
    /// plugin wrote already point directly at host memory).
    ///
    /// # Safety
    /// `outputs` and `num_out` / `num_frames` must match the values
    /// passed to the prior [`Self::build`] call on this scratch.
    pub unsafe fn finish_widening_f32(
        &self,
        outputs: *mut *mut f32,
        num_out: u32,
        num_frames: u32,
    ) {
        // When the plugin is `f32` we wrote straight into host memory.
        if std::any::TypeId::of::<S>() == std::any::TypeId::of::<f32>() {
            return;
        }
        unsafe {
            let nf = num_frames as usize;
            for ch in 0..(num_out as usize) {
                let ptr = *outputs.add(ch);
                if ptr.is_null() {
                    continue;
                }
                let host = std::slice::from_raw_parts_mut(ptr, nf);
                let plugin_out = &self.output_buffers[ch];
                for (h, &p) in host.iter_mut().zip(plugin_out.iter()) {
                    *h = p.to_f32();
                }
            }
        }
    }

    unsafe fn build_inner<'a>(
        &'a mut self,
        inputs: *const *const f32,
        outputs: *mut *mut f32,
        num_in: u32,
        num_out: u32,
        num_frames: u32,
        supports_in_place: bool,
    ) -> AudioBuffer<'a, S> {
        const MAX_CHANNELS_TRACKED: usize = 64;
        // Whether the plugin's chosen precision matches the host's.
        // When matched, we zero-copy host pointers into the slice
        // arrays; when not, we widen/narrow through input_copies and
        // output_buffers.
        let same_precision = std::any::TypeId::of::<S>() == std::any::TypeId::of::<f32>();

        unsafe {
            let nf = num_frames as usize;
            let num_out_u = num_out as usize;
            let num_in_u = num_in as usize;
            debug_assert!(
                num_out_u <= MAX_CHANNELS_TRACKED,
                "RawBufferScratch::build: alias detection only covers up to {MAX_CHANNELS_TRACKED} \
                 output channels; got {num_out_u}. Channels beyond the cap won't be \
                 detected as aliased.",
            );
            let out_ptrs: [Option<*mut f32>; MAX_CHANNELS_TRACKED] = std::array::from_fn(|ch| {
                if ch < num_out_u {
                    let p = *outputs.add(ch);
                    if p.is_null() { None } else { Some(p) }
                } else {
                    None
                }
            });
            let aliases_any_output = |in_ptr: *const f32| -> bool {
                let in_start = in_ptr as usize;
                let in_end = in_start + nf * std::mem::size_of::<f32>();
                out_ptrs
                    .iter()
                    .take(num_out_u.min(MAX_CHANNELS_TRACKED))
                    .any(|o| {
                        o.is_some_and(|op| {
                            let o_start = op as usize;
                            let o_end = o_start + nf * std::mem::size_of::<f32>();
                            !(in_end <= o_start || o_end <= in_start)
                        })
                    })
            };

            // Grow per-channel scratch slots if the bus widened or
            // we're widening precision and need every channel copied.
            while self.input_copies.len() < num_in_u {
                self.input_copies.push(Vec::new());
            }
            if !same_precision {
                while self.output_buffers.len() < num_out_u {
                    self.output_buffers.push(Vec::new());
                }
            }

            self.input_slices.clear();
            self.input_slices.reserve(num_in_u);
            let mut in_place_mask: u64 = 0;
            for ch in 0..num_in_u {
                let ptr = *inputs.add(ch);
                let slice: &[S] = if ptr.is_null() {
                    &[]
                } else if aliases_any_output(ptr) {
                    if ch < 64 {
                        in_place_mask |= 1 << ch;
                    }
                    if supports_in_place && same_precision {
                        // Plugin opted in: hand it nothing through
                        // input(ch); it must read+write via in_out_mut.
                        // Only supported in the same-precision case;
                        // the cross-precision path always copies.
                        &[]
                    } else {
                        // Snapshot the input (and widen if needed)
                        // before the plugin overwrites the shared
                        // buffer.
                        let host = std::slice::from_raw_parts(ptr, nf);
                        let copy = &mut self.input_copies[ch];
                        copy.clear();
                        copy.reserve(nf);
                        for &h in host {
                            copy.push(S::from_f32(h));
                        }
                        let p = copy.as_ptr();
                        let l = copy.len();
                        // SAFETY: `copy` lives as long as `self`, which
                        // outlives the returned `AudioBuffer<'a>`.
                        std::slice::from_raw_parts(p, l)
                    }
                } else if same_precision {
                    // SAFETY: the in-precision case is `&[f32]`. We
                    // transmute via raw parts because the function
                    // signature is generic over S but the runtime
                    // branch knows S == f32.
                    let raw = ptr.cast::<S>();
                    std::slice::from_raw_parts(raw, nf)
                } else {
                    // Different precision, no aliasing: widen into scratch.
                    let host = std::slice::from_raw_parts(ptr, nf);
                    let copy = &mut self.input_copies[ch];
                    copy.clear();
                    copy.reserve(nf);
                    for &h in host {
                        copy.push(S::from_f32(h));
                    }
                    let p = copy.as_ptr();
                    let l = copy.len();
                    std::slice::from_raw_parts(p, l)
                };
                self.input_slices.push(slice);
            }

            self.output_slices.clear();
            self.output_slices.reserve(num_out_u);
            for ch in 0..num_out_u {
                let ptr = *outputs.add(ch);
                let slice: &mut [S] = if ptr.is_null() {
                    &mut []
                } else if same_precision {
                    // SAFETY: same-precision branch - host pointer is
                    // already `*mut S` modulo runtime type identity.
                    let raw = ptr.cast::<S>();
                    std::slice::from_raw_parts_mut(raw, nf)
                } else {
                    // Different precision: render into per-channel
                    // scratch; finish_widening_f32 copies+narrows back.
                    let buf = &mut self.output_buffers[ch];
                    buf.clear();
                    buf.resize(nf, S::default());
                    let p = buf.as_mut_ptr();
                    let l = buf.len();
                    std::slice::from_raw_parts_mut(p, l)
                };
                self.output_slices.push(slice);
            }

            // SAFETY: Same transmute pattern as AudioBuffer::slice().
            // RawBufferScratch stores 'static slices but we return AudioBuffer<'a>.
            let self_ptr: *mut Self = self;
            let s = &mut *self_ptr;
            let mut buf = std::mem::transmute::<AudioBuffer<'static, S>, AudioBuffer<'a, S>>(
                AudioBuffer::from_slices(&s.input_slices, &mut s.output_slices, nf),
            );
            buf.set_in_place_mask(in_place_mask);
            buf
        }
    }

    /// Pre-allocate the per-channel scratch vectors so `build` runs
    /// allocation-free for buses up to `num_in` × `num_out` channels
    /// and blocks up to `max_frames`. Idempotent and growth-only.
    pub fn ensure_capacity(&mut self, num_in: usize, num_out: usize, max_frames: usize) {
        if self.input_slices.capacity() < num_in {
            self.input_slices
                .reserve_exact(num_in - self.input_slices.capacity());
        }
        if self.output_slices.capacity() < num_out {
            self.output_slices
                .reserve_exact(num_out - self.output_slices.capacity());
        }
        while self.input_copies.len() < num_in {
            self.input_copies.push(Vec::with_capacity(max_frames));
        }
        for buf in &mut self.input_copies {
            if buf.capacity() < max_frames {
                buf.reserve_exact(max_frames - buf.capacity());
            }
        }
        while self.output_buffers.len() < num_out {
            self.output_buffers.push(Vec::with_capacity(max_frames));
        }
        for buf in &mut self.output_buffers {
            if buf.capacity() < max_frames {
                buf.reserve_exact(max_frames - buf.capacity());
            }
        }
    }
}

impl<S: Sample> Default for RawBufferScratch<S> {
    fn default() -> Self {
        Self {
            input_slices: Vec::with_capacity(2),
            output_slices: Vec::with_capacity(2),
            input_copies: Vec::with_capacity(2),
            output_buffers: Vec::with_capacity(2),
        }
    }
}
