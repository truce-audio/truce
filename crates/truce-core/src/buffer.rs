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
            // Verify num_samples doesn't exceed any slice length. An empty
            // input slice is an in-place channel (`supports_in_place`): the
            // plugin reads+writes via `in_out_mut`, so there's no separate
            // input to bound-check. Any non-empty slice must cover the block.
            for (i, inp) in inputs.iter().enumerate() {
                assert!(
                    inp.is_empty() || num_samples <= inp.len(),
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

    /// Debug guard for the accessors that hand out a disjoint `(&[S], &mut
    /// [S])` for a channel (`io` / `io_pair` / `for_each_frame` /
    /// `for_each_frame_io` / `chunks_mut`). Those shapes can't represent a
    /// zero-copy in-place channel: its input and output are the same memory,
    /// so a live `&` input would alias the `&mut` output - which is why such
    /// a channel's input slice is the empty sentinel. Use [`Self::in_out_mut`]
    /// there instead.
    ///
    /// Keys on the empty input slice, **not** [`Self::is_in_place`]: the copy
    /// path also reports `is_in_place` (the host aliased, but the wrapper
    /// snapshotted the input), yet its input is a full, readable slice these
    /// accessors handle fine. Compiled out in release, where indexing the
    /// empty slice then panics out of range (caught by the process firewall).
    #[inline]
    fn debug_assert_not_in_place(&self, ch: usize) {
        debug_assert!(
            self.num_samples == 0
                || ch >= self.inputs.len()
                || self.inputs[ch].len() >= self.offset + self.num_samples,
            "AudioBuffer: channel {ch} is a zero-copy in-place channel (host \
             aliases its input and output, so its input slice is empty); a \
             disjoint (input, output) accessor can't represent it - use \
             in_out_mut({ch})"
        );
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
        let s = self.inputs[channel];
        // An empty backing slice marks an in-place channel: the host aliases
        // it and the plugin opted into `supports_in_place`, so it reads and
        // writes the shared buffer through `in_out_mut`. A real input slice
        // would alias the output. Return the empty slice as-is; slicing
        // `[offset..end]` would be out of range.
        if s.is_empty() {
            return s;
        }
        let end = self.offset + self.num_samples;
        &s[self.offset..end]
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

    /// Get a disjoint `(input, output)` pair for a channel. NOT for
    /// in-place (host-aliased) channels: their input and output are the
    /// same memory, which this shape can't represent - use
    /// [`Self::in_out_mut`] there.
    pub fn io_pair(&mut self, in_ch: usize, out_ch: usize) -> (&[S], &mut [S]) {
        self.debug_assert_not_in_place(in_ch);
        let end = self.offset + self.num_samples;
        let input = &self.inputs[in_ch][self.offset..end];
        let output = &mut self.outputs[out_ch][self.offset..end];
        (input, output)
    }

    /// Get an input/output pair for the same channel index. Shorthand for `io_pair(ch, ch)`.
    pub fn io(&mut self, ch: usize) -> (&[S], &mut [S]) {
        self.io_pair(ch, ch)
    }

    /// Iterate per-channel, in fixed-size `N`-sample chunks. The
    /// last chunk of each channel may be shorter than `N`; it's
    /// yielded as a [`ChunkItem::Tail`] with the actual remaining
    /// length, and the caller falls back to scalar for it. Full
    /// `N`-sample chunks arrive as [`ChunkItem::Full`] carrying
    /// `&[S; N]` / `&mut [S; N]` stack arrays - exactly the shape
    /// the per-op SIMD primitives in `truce-simd` expect.
    ///
    /// Iteration order is channel-major: all chunks of channel 0,
    /// then all chunks of channel 1, etc. Matches the natural
    /// orientation for per-channel state (biquad coefficients,
    /// per-channel meters) and lets the caller read its smoothed
    /// params once per chunk instead of once per sample.
    ///
    /// The returned object is a "lending iterator" - it doesn't
    /// implement [`Iterator`] because each yielded item borrows
    /// from the iterator itself. Use `while let Some(chunk) = …
    /// .next()`:
    ///
    /// ```ignore
    /// let mut chunks = buffer.chunks_mut::<32>();
    /// while let Some(chunk) = chunks.next() {
    ///     match chunk {
    ///         ChunkItem::Full { ch, inp, out } => {
    ///             // SIMD-friendly path, inp / out are &[f32; 32]
    ///         }
    ///         ChunkItem::Tail { ch, inp, out } => {
    ///             // scalar fallback for the trailing samples
    ///         }
    ///     }
    /// }
    /// ```
    ///
    /// Const-generic `N` is the chunk size; pick it to match the
    /// SIMD width × unroll factor for your inner op (32 / 64 are
    /// good defaults for current Apple Silicon + `x86_64`).
    pub fn chunks_mut<const N: usize>(&mut self) -> ChunksMut<'_, 'a, S, N> {
        ChunksMut {
            buffer: self,
            ch: 0,
            pos: 0,
        }
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
        for ch in 0..N {
            self.debug_assert_not_in_place(ch);
        }
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

    /// Like [`Self::for_each_frame`] but for a DSP whose frame shape is a
    /// fixed `(IN, OUT)` that need not match the bus width. Input slot `k`
    /// reads bus input channel `k`, repeating the last available channel
    /// when the bus has fewer than `IN` inputs - so a mono source fans into
    /// both inputs of a stereo graph. Output slot `k` writes bus output
    /// channel `k` while `k < num_output_channels`; frame outputs past the
    /// bus width are dropped.
    ///
    /// This lets a plugin built around a fixed-shape DSP (a fundsp
    /// `reverb_stereo`, a dasp graph) run on any declared bus layout -
    /// `(2, 2)` stereo and `(1, 2)` mono-in/stereo-out alike - through one
    /// `for_each_frame_io::<2, 2>` call, with no per-width branch. A bus
    /// with no inputs (an instrument) feeds silence.
    pub fn for_each_frame_io<const IN: usize, const OUT: usize, F>(&mut self, mut tick: F)
    where
        F: FnMut(&[S; IN], &mut [S; OUT]),
    {
        let num_in = self.inputs.len();
        let num_out = self.outputs.len();
        for ch in 0..num_in.min(IN) {
            self.debug_assert_not_in_place(ch);
        }
        let mut frame_in = [S::default(); IN];
        let mut frame_out = [S::default(); OUT];
        let end = self.offset + self.num_samples;
        for i in self.offset..end {
            if num_in > 0 {
                for (k, slot) in frame_in.iter_mut().enumerate() {
                    *slot = self.inputs[k.min(num_in - 1)][i];
                }
            }
            tick(&frame_in, &mut frame_out);
            for (k, sample) in frame_out.iter().enumerate().take(num_out) {
                self.outputs[k][i] = *sample;
            }
        }
    }

    /// [`Self::for_each_frame_io`] specialized to a stereo `(2, 2)` DSP -
    /// the common case (a `reverb_stereo`, a stereo filter block). Runs the
    /// 2-in/2-out `tick` over any declared bus: a mono source fans into
    /// both inputs, a stereo bus maps 1:1, so a stereo effect needs no
    /// per-width branch and no turbofish.
    pub fn for_each_stereo_frame<F>(&mut self, tick: F)
    where
        F: FnMut(&[S; 2], &mut [S; 2]),
    {
        self.for_each_frame_io::<2, 2, F>(tick);
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

/// One yielded chunk from [`AudioBuffer::chunks_mut`].
///
/// `Full` is the SIMD-friendly path: `inp` and `out` are stack
/// arrays of exactly `N` elements, ready to feed `truce-simd`'s
/// block ops. `Tail` is the trailing fragment when `num_samples()`
/// isn't a multiple of `N`; fall back to a scalar loop.
pub enum ChunkItem<'b, S: Sample, const N: usize> {
    /// Full N-sample chunk. The `&[S; N]` / `&mut [S; N]` are the
    /// shape `truce-simd` ops are written against - no slice
    /// length check at the call site.
    Full {
        /// Channel index this chunk belongs to.
        ch: usize,
        /// Sample offset within the audio block this chunk starts
        /// at. Use this when indexing into a precomputed envelope
        /// array - `chunks_mut` iterates channel-major, so the
        /// envelope (typically read once per audio block via
        /// `read_into(&mut env[..num_samples])`) is shared across all
        /// channel passes.
        sample: usize,
        /// Read-only N-sample input slice.
        inp: &'b [S; N],
        /// Mutable N-sample output slice.
        out: &'b mut [S; N],
    },
    /// Trailing chunk when `num_samples()` isn't a multiple of `N`.
    /// Length is in `(0, N)`. Fall back to scalar processing.
    Tail {
        /// Channel index this chunk belongs to.
        ch: usize,
        /// Sample offset within the audio block this chunk starts at.
        sample: usize,
        /// Read-only tail input slice; length < N.
        inp: &'b [S],
        /// Mutable tail output slice; length < N.
        out: &'b mut [S],
    },
}

/// Lending iterator returned by [`AudioBuffer::chunks_mut`].
///
/// Does not implement [`Iterator`] because each yielded
/// [`ChunkItem`] borrows from the iterator itself - the standard
/// "GATs would help here" pattern. Drive it with `while let
/// Some(chunk) = chunks.next()` instead. See
/// [`AudioBuffer::chunks_mut`] for a worked example.
pub struct ChunksMut<'b, 'a, S: Sample, const N: usize> {
    buffer: &'b mut AudioBuffer<'a, S>,
    /// Current channel being walked.
    ch: usize,
    /// Position within the current channel, relative to
    /// `buffer.offset`. Advances by N each Full chunk, then jumps
    /// to `num_samples` for the Tail (or directly past it when
    /// `num_samples` is a multiple of N).
    pos: usize,
}

impl<S: Sample, const N: usize> ChunksMut<'_, '_, S, N> {
    /// Yield the next chunk, or `None` when every channel has been
    /// fully walked.
    ///
    /// Method-on-self rather than `Iterator::next` because each
    /// yielded [`ChunkItem`] borrows from `self`; GATs would be
    /// needed to express that through the `Iterator` trait.
    #[allow(clippy::should_implement_trait, clippy::missing_panics_doc)]
    pub fn next(&mut self) -> Option<ChunkItem<'_, S, N>> {
        loop {
            if self.ch >= self.buffer.outputs.len() {
                return None;
            }
            let ns = self.buffer.num_samples;
            if self.pos >= ns {
                self.ch += 1;
                self.pos = 0;
                continue;
            }
            let abs_start = self.buffer.offset + self.pos;
            let remaining = ns - self.pos;
            let take = remaining.min(N);
            let abs_end = abs_start + take;
            let ch = self.ch;
            let sample = self.pos;

            self.buffer.debug_assert_not_in_place(ch);
            let inp_slice = &self.buffer.inputs[ch][abs_start..abs_end];
            let out_slice: &mut [S] = &mut self.buffer.outputs[ch][abs_start..abs_end];

            self.pos += take;

            // Full vs Tail by length: full chunks convert to `&[S;
            // N]` / `&mut [S; N]` for the SIMD-friendly path; tails
            // fall back to slice form.
            return Some(if take == N {
                ChunkItem::Full {
                    ch,
                    sample,
                    // Length-checked above; `try_into` here is a
                    // free reinterpret.
                    inp: inp_slice.try_into().expect("len == N by construction"),
                    out: out_slice.try_into().expect("len == N by construction"),
                }
            } else {
                ChunkItem::Tail {
                    ch,
                    sample,
                    inp: inp_slice,
                    out: out_slice,
                }
            });
        }
    }
}

/// Scratch space for [`RawBufferScratch::build`].
///
/// Callers allocate this on the stack and pass it to `build`. The
/// buffer borrows the slices stored here, so this struct must outlive
/// the returned `AudioBuffer`.
///
/// Generic over the plugin's sample type `S`. When the host buffer
/// matches `S`, slices point into host memory (zero-copy). When the
/// host buffer is a different precision, the input is widened/narrowed
/// into per-channel scratch; the output is rendered into scratch and
/// the wrapper copies + casts it back to the host buffer at the end
/// of the block via [`Self::finish_widening`].
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
    /// Per-channel output scratch. Populated by [`Self::build`] when
    /// the host buffer precision differs from `S` (the wrapper copies +
    /// casts these back via [`Self::finish_widening`]), and reused as
    /// write-discard scratch for an unconnected (null) output channel.
    output_buffers: Vec<Vec<S>>,
    /// Shared read-only silence handed to the plugin for an unconnected
    /// (null) input channel - an unrouted sidechain, or an LV2 port the
    /// host never connected. The plugin negotiated the channel, so it
    /// must read block-length silence, never the out-of-range empty
    /// slice a raw null would otherwise produce. Never written.
    silence: Vec<S>,
}

impl<S: Sample> RawBufferScratch<S> {
    /// Build an `AudioBuffer<S>` from raw host pointers of wire
    /// precision `H` - `f32` in the common case (CLAP, LV2, AAX
    /// always; VST3/VST2/AU 32-bit mode), `f64` when the host
    /// negotiated a double-precision wire (VST3 `kSample64`, VST2
    /// `processDoubleReplacing`).
    ///
    /// When `S = H`, slices point directly into host memory (modulo
    /// in-place input copying). Otherwise every channel is converted
    /// into per-channel scratch and the wrapper must call
    /// [`Self::finish_widening`] at the end of the block to copy the
    /// rendered samples back to the host's output pointers.
    ///
    /// # Safety
    /// - `inputs` must point to `num_in` valid `*const H` pointers
    ///   (each non-null pointer must address at least `num_frames`
    ///   readable samples; a null pointer marks an unconnected channel
    ///   and reads back as block-length silence).
    /// - `outputs` must point to `num_out` valid `*mut H` pointers
    ///   (each non-null pointer must address at least `num_frames`
    ///   writable samples; a null pointer marks an unconnected channel
    ///   whose writes are discarded).
    /// - The pointed-to memory must remain valid for the lifetime of
    ///   the returned `AudioBuffer`.
    pub unsafe fn build<H: Sample>(
        &mut self,
        inputs: *const *const H,
        outputs: *mut *mut H,
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

    /// Copy + convert the rendered `S` output back to the host's `H`
    /// output pointers. No-op when `S = H` (the slices the plugin
    /// wrote already point directly at host memory).
    ///
    /// # Safety
    /// `outputs` and `num_out` / `num_frames` must match the values
    /// passed to the prior [`Self::build`] call on this scratch.
    pub unsafe fn finish_widening<H: Sample>(
        &self,
        outputs: *mut *mut H,
        num_out: u32,
        num_frames: u32,
    ) {
        // Same precision: the plugin wrote straight into host memory.
        if S::IS_F64 == H::IS_F64 {
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
                    *h = H::from_f64(p.to_f64());
                }
            }
        }
    }

    unsafe fn build_inner<'a, H: Sample>(
        &'a mut self,
        inputs: *const *const H,
        outputs: *mut *mut H,
        num_in: u32,
        num_out: u32,
        num_frames: u32,
        supports_in_place: bool,
    ) -> AudioBuffer<'a, S> {
        const MAX_CHANNELS_TRACKED: usize = 64;
        // Whether the plugin's chosen precision matches the host's.
        // When matched, we zero-copy host pointers into the slice
        // arrays; when not, we convert through input_copies and
        // output_buffers. The traits are sealed at f32/f64, so equal
        // IS_F64 flags mean S and H are the same type.
        let same_precision = S::IS_F64 == H::IS_F64;

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
            let out_ptrs: [Option<*mut H>; MAX_CHANNELS_TRACKED] = std::array::from_fn(|ch| {
                if ch < num_out_u {
                    let p = *outputs.add(ch);
                    if p.is_null() { None } else { Some(p) }
                } else {
                    None
                }
            });
            let aliases_any_output = |in_ptr: *const H| -> bool {
                let in_start = in_ptr as usize;
                let in_end = in_start + nf * std::mem::size_of::<H>();
                out_ptrs
                    .iter()
                    .take(num_out_u.min(MAX_CHANNELS_TRACKED))
                    .any(|o| {
                        o.is_some_and(|op| {
                            let o_start = op as usize;
                            let o_end = o_start + nf * std::mem::size_of::<H>();
                            !(in_end <= o_start || o_end <= in_start)
                        })
                    })
            };

            // Grow per-channel scratch slots if the bus widened or
            // we're widening precision and need every channel copied.
            // `output_buffers` grows unconditionally now: an unconnected
            // output channel discards its writes into this scratch even in
            // the same-precision path.
            while self.input_copies.len() < num_in_u {
                self.input_copies.push(Vec::new());
            }
            while self.output_buffers.len() < num_out_u {
                self.output_buffers.push(Vec::new());
            }
            // Block-length silence for any unconnected input channel.
            if self.silence.len() < nf {
                self.silence.resize(nf, S::default());
            }
            let silence_ptr = self.silence.as_ptr();

            self.input_slices.clear();
            self.input_slices.reserve(num_in_u);
            let mut in_place_mask: u64 = 0;
            for ch in 0..num_in_u {
                let ptr = *inputs.add(ch);
                let slice: &[S] = if ptr.is_null() {
                    // Unconnected channel (unrouted sidechain, unbound LV2
                    // port). The plugin negotiated it, so hand it
                    // block-length silence, not an out-of-range empty slice.
                    std::slice::from_raw_parts(silence_ptr, nf)
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
                        // Snapshot the input (converting precision if
                        // needed) before the plugin overwrites the
                        // shared buffer. Routing through f64 is
                        // lossless in the widening direction.
                        let host = std::slice::from_raw_parts(ptr, nf);
                        let copy = &mut self.input_copies[ch];
                        copy.clear();
                        copy.reserve(nf);
                        for &h in host {
                            copy.push(S::from_f64(h.to_f64()));
                        }
                        let p = copy.as_ptr();
                        let l = copy.len();
                        // SAFETY: `copy` lives as long as `self`, which
                        // outlives the returned `AudioBuffer<'a>`.
                        std::slice::from_raw_parts(p, l)
                    }
                } else if same_precision {
                    // SAFETY: same-precision branch - host pointer is
                    // already `*const S` modulo runtime type identity;
                    // the cast reinterprets `*const H` as `*const S`.
                    let raw = ptr.cast::<S>();
                    std::slice::from_raw_parts(raw, nf)
                } else {
                    // Different precision, no aliasing: convert into
                    // scratch (f64 round-trip, lossless when widening).
                    let host = std::slice::from_raw_parts(ptr, nf);
                    let copy = &mut self.input_copies[ch];
                    copy.clear();
                    copy.reserve(nf);
                    for &h in host {
                        copy.push(S::from_f64(h.to_f64()));
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
                    // Unconnected output channel: give the plugin a
                    // block-length discard buffer to write into rather than
                    // an empty slice it would index out of range.
                    // `finish_widening` skips it (null host pointer), so
                    // nothing is copied back.
                    let buf = &mut self.output_buffers[ch];
                    buf.clear();
                    buf.resize(nf, S::default());
                    let p = buf.as_mut_ptr();
                    let l = buf.len();
                    std::slice::from_raw_parts_mut(p, l)
                } else if same_precision {
                    // SAFETY: same-precision branch - host pointer is
                    // already `*mut S` modulo runtime type identity.
                    let raw = ptr.cast::<S>();
                    std::slice::from_raw_parts_mut(raw, nf)
                } else {
                    // Different precision: render into per-channel
                    // scratch; finish_widening copies+converts back.
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
        // Shared silence for unconnected input channels, kept block-sized
        // and zeroed so `build` never allocates it on the audio thread.
        if self.silence.len() < max_frames {
            self.silence.resize(max_frames, S::default());
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
            silence: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive one block through `build` / `finish_widening` with
    /// plugin precision `S` on host wire `H`: the plugin doubles a
    /// `[1, 2, 3, 4]` input ramp into the output.
    fn double_one_block<S: Sample, H: Sample>() -> Vec<H> {
        let input: Vec<H> = (1..=4).map(|v| H::from_f64(f64::from(v))).collect();
        let mut output: Vec<H> = vec![H::default(); 4];
        let in_ptrs = [input.as_ptr()];
        let mut out_ptrs = [output.as_mut_ptr()];
        let mut scratch = RawBufferScratch::<S>::default();
        // SAFETY: both pointers address 4 valid samples that outlive
        // the buffer; the finish call reuses the same layout.
        unsafe {
            let mut buf = scratch.build(in_ptrs.as_ptr(), out_ptrs.as_mut_ptr(), 1, 1, 4, false);
            for i in 0..4 {
                let v = buf.input(0)[i];
                buf.output(0)[i] = v + v;
            }
            scratch.finish_widening(out_ptrs.as_mut_ptr(), 1, 4);
        }
        output
    }

    fn assert_doubled<H: Sample>(output: &[H]) {
        let got: Vec<f64> = output.iter().map(|v| v.to_f64()).collect();
        assert_eq!(got, vec![2.0, 4.0, 6.0, 8.0]);
    }

    // Passthrough, so the outputs are bit-identical to the input - exact
    // float equality is the contract being checked.
    #[allow(clippy::float_cmp)]
    #[test]
    fn for_each_frame_io_fans_mono_input_to_a_stereo_graph() {
        // Mono-in (1) / stereo-out (2) bus fed through a 2-in/2-out identity
        // "graph": the single input must fan into both frame slots, so both
        // outputs receive the mono signal, with no per-width branch.
        let input: [f32; 3] = [0.1, 0.2, 0.3];
        let mut out_l = [0.0f32; 3];
        let mut out_r = [0.0f32; 3];
        let inputs: [&[f32]; 1] = [&input];
        let mut outputs: [&mut [f32]; 2] = [&mut out_l, &mut out_r];
        let mut buf = AudioBuffer::<f32>::from_slices_checked(&inputs, &mut outputs, 3);

        buf.for_each_frame_io::<2, 2, _>(|frame_in, frame_out| {
            // Identity graph: both channels pass through.
            frame_out[0] = frame_in[0];
            frame_out[1] = frame_in[1];
        });

        // frame_in[1] repeated the last (only) input channel, so both
        // outputs equal the mono input.
        assert_eq!(out_l, input);
        assert_eq!(out_r, input);
    }

    #[test]
    fn f32_wire_f32_plugin_zero_copy() {
        assert_doubled(&double_one_block::<f32, f32>());
    }

    #[test]
    fn f32_wire_f64_plugin_widens() {
        assert_doubled(&double_one_block::<f64, f32>());
    }

    #[test]
    fn f64_wire_f64_plugin_zero_copy() {
        assert_doubled(&double_one_block::<f64, f64>());
    }

    #[test]
    fn f64_wire_f32_plugin_narrows() {
        assert_doubled(&double_one_block::<f32, f64>());
    }

    #[test]
    #[allow(clippy::float_cmp)]
    fn f64_wire_in_place_snapshots_input() {
        // Host hands the same f64 buffer for input and output; the
        // input reads must see the pre-write values.
        let mut io: Vec<f64> = vec![1.0, 2.0, 3.0, 4.0];
        let in_ptrs = [io.as_ptr()];
        let mut out_ptrs = [io.as_mut_ptr()];
        let mut scratch = RawBufferScratch::<f64>::default();
        // SAFETY: the aliased pointer addresses 4 valid samples that
        // outlive the buffer.
        unsafe {
            let mut buf = scratch.build(in_ptrs.as_ptr(), out_ptrs.as_mut_ptr(), 1, 1, 4, false);
            assert!(buf.is_in_place(0));
            for i in 0..4 {
                let v = buf.input(0)[i];
                buf.output(0)[i] = v * 10.0;
            }
        }
        assert_eq!(io, vec![10.0, 20.0, 30.0, 40.0]);
    }

    #[test]
    #[allow(clippy::float_cmp)]
    fn in_place_true_path_hands_shared_buffer() {
        // `supports_in_place = true`: the host aliases in/out, so the wrapper
        // skips the copy. `input(ch)` is empty and the plugin reads+writes
        // the shared buffer through `in_out_mut`. This is the zero-copy path
        // the `f64_wire_in_place_snapshots_input` test (opting out) never
        // exercises - and the one that used to panic at construction (debug)
        // or in `input()` (release).
        let mut io: Vec<f32> = vec![1.0, 2.0, 3.0, 4.0];
        let in_ptrs = [io.as_ptr()];
        let mut out_ptrs = [io.as_mut_ptr()];
        let mut scratch = RawBufferScratch::<f32>::default();
        // SAFETY: the aliased pointer addresses 4 valid samples that outlive
        // the buffer.
        unsafe {
            let mut buf = scratch.build(in_ptrs.as_ptr(), out_ptrs.as_mut_ptr(), 1, 1, 4, true);
            assert!(buf.is_in_place(0));
            assert!(buf.input(0).is_empty(), "in-place input(ch) is empty");
            let io_ch = buf.in_out_mut(0);
            assert_eq!(io_ch.len(), 4);
            for s in io_ch.iter_mut() {
                *s *= 10.0; // read the current (input) value, write in place
            }
        }
        assert_eq!(io, vec![10.0, 20.0, 30.0, 40.0]);
    }

    /// The disjoint `(input, output)` accessors can't represent an in-place
    /// channel, so they debug-assert with a clear message instead of the
    /// opaque out-of-range panic the empty input slice would otherwise
    /// produce. Gated on `debug_assertions`: the guard is compiled out in
    /// release, so this only runs (and only should panic) in debug.
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "in-place")]
    fn io_on_zero_copy_in_place_channel_debug_asserts() {
        let mut io: Vec<f32> = vec![1.0, 2.0, 3.0, 4.0];
        let in_ptrs = [io.as_ptr()];
        let mut out_ptrs = [io.as_mut_ptr()];
        let mut scratch = RawBufferScratch::<f32>::default();
        // SAFETY: the aliased pointer addresses 4 valid samples that outlive
        // the buffer.
        unsafe {
            // supports_in_place = true -> zero-copy, empty input slice.
            let mut buf = scratch.build(in_ptrs.as_ptr(), out_ptrs.as_mut_ptr(), 1, 1, 4, true);
            assert!(buf.is_in_place(0));
            // Should fire the guard, not index the empty input slice.
            let _ = buf.io(0);
        }
    }

    /// The guard must NOT fire on the copy path: a host-aliased channel with
    /// `supports_in_place = false` reports `is_in_place`, but the wrapper
    /// snapshotted its input into a full slice, so `io()` works. A guard
    /// keyed on `is_in_place` would false-positive here and break every
    /// normal plugin that uses `io()` in an aliasing host (e.g. AU, which
    /// advertises in-place unconditionally).
    #[test]
    #[allow(clippy::float_cmp)]
    fn io_on_copy_path_aliased_channel_is_fine() {
        let mut io: Vec<f32> = vec![1.0, 2.0, 3.0, 4.0];
        let in_ptrs = [io.as_ptr()];
        let mut out_ptrs = [io.as_mut_ptr()];
        let mut scratch = RawBufferScratch::<f32>::default();
        // SAFETY: the aliased pointer addresses 4 valid samples that outlive
        // the buffer.
        unsafe {
            // supports_in_place = false -> copy path, full readable input.
            let mut buf = scratch.build(in_ptrs.as_ptr(), out_ptrs.as_mut_ptr(), 1, 1, 4, false);
            assert!(buf.is_in_place(0), "still reports host aliasing");
            let (inp, out) = buf.io(0); // must not panic
            assert_eq!(inp, &[1.0, 2.0, 3.0, 4.0]);
            for (o, &i) in out.iter_mut().zip(inp) {
                *o = i * 2.0;
            }
        }
        assert_eq!(io, vec![2.0, 4.0, 6.0, 8.0]);
    }

    /// A disconnected sidechain reaches the plugin as block-sized silence,
    /// not a null pointer - the uniform "declared width always; missing
    /// buses read as silence" contract every format upholds. The VST3 shim
    /// substitutes silence for any missing or null input channel (a
    /// deactivated bus arrives with null channel buffers, or a trailing bus
    /// is dropped from `ProcessData`), and the AAX shim feeds silence for an
    /// unpatched sidechain, so the flat channel array always carries the
    /// negotiated width. This pins the downstream contract: such a channel
    /// is a readable, zeroed, block-length slice, not the out-of-range
    /// empty slice a raw null would produce.
    #[test]
    fn silence_substituted_sidechain_channels_are_full_length_zeros() {
        let nf = 512usize;
        let main_l = vec![0.5f32; nf];
        let main_r = vec![0.5f32; nf];
        // What the shim now hands us for a disconnected stereo sidechain:
        // a block-sized zeroed buffer per channel (shared read-only).
        let silence = vec![0.0f32; nf];
        let mut out_l = vec![0.0f32; nf];
        let mut out_r = vec![0.0f32; nf];

        let in_ptrs = [
            main_l.as_ptr(),
            main_r.as_ptr(),
            silence.as_ptr(),
            silence.as_ptr(),
        ];
        let mut out_ptrs = [out_l.as_mut_ptr(), out_r.as_mut_ptr()];
        let mut scratch = RawBufferScratch::<f32>::default();
        // SAFETY: every pointer addresses `nf` valid samples that outlive
        // the buffer; `silence` backs both deactivated-bus channels.
        unsafe {
            #[allow(clippy::cast_possible_truncation)]
            let buf = scratch.build(
                in_ptrs.as_ptr(),
                out_ptrs.as_mut_ptr(),
                4,
                2,
                nf as u32,
                false,
            );
            assert_eq!(buf.num_input_channels(), 4);
            // The reads that panicked when the sidechain arrived as a
            // null/empty slice now return block-length silence.
            assert_eq!(buf.input(2).len(), nf);
            assert_eq!(buf.input(3).len(), nf);
            assert!(buf.input(2).iter().all(|&s| s == 0.0));
            assert!(buf.input(3).iter().all(|&s| s == 0.0));
        }
    }

    /// A raw null channel pointer is handled at the `build` layer itself:
    /// a null input reads as block-length silence and a null output absorbs
    /// the plugin's writes into discard scratch - so a wrapper that hands
    /// `build` a null (a CLAP/VST2/LV2 port the host left unconnected) can
    /// never produce the out-of-range empty slice that used to panic.
    #[test]
    fn raw_null_channels_read_silence_and_discard_writes() {
        let nf = 512usize;
        let main_l = vec![0.5f32; nf];
        let main_r = vec![0.5f32; nf];
        let mut out_l = vec![0.0f32; nf];
        // Input channels 2/3 and output channel 1 arrive unconnected.
        let in_ptrs = [
            main_l.as_ptr(),
            main_r.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
        ];
        let mut out_ptrs = [out_l.as_mut_ptr(), std::ptr::null_mut()];
        let mut scratch = RawBufferScratch::<f32>::default();
        // SAFETY: the non-null pointers address `nf` valid samples; the
        // null channels are the unconnected-port shape under test.
        unsafe {
            #[allow(clippy::cast_possible_truncation)]
            let mut buf = scratch.build(
                in_ptrs.as_ptr(),
                out_ptrs.as_mut_ptr(),
                4,
                2,
                nf as u32,
                false,
            );
            assert_eq!(buf.num_input_channels(), 4);
            assert_eq!(buf.num_output_channels(), 2);
            // Null input channels read as full-length silence.
            assert_eq!(buf.input(2).len(), nf);
            assert!(buf.input(3).iter().all(|&s| s == 0.0));
            // The null output channel is a full-length discard buffer: the
            // plugin can write it without an out-of-range panic.
            assert_eq!(buf.output(1).len(), nf);
            for s in buf.output(1) {
                *s = 1.0;
            }
        }
    }
}
