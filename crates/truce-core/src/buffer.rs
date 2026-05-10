/// Non-interleaved audio buffer. Borrows host memory through the
/// format wrapper.
///
/// **In-place I/O.** Some hosts (Reaper, pluginval) pass the same
/// buffer for both input and output of a given channel. By default
/// the wrapper copies the aliased inputs into per-channel scratch so
/// `input(ch)` and `output(ch)` are disjoint `&[f32]` / `&mut [f32]`
/// — no plugin code change required. Plugins that opt into
/// `Plugin::supports_in_place() = true` skip the copy and must use
/// [`Self::in_out_mut`] for channels where [`Self::is_in_place`]
/// returns `true`. See `docs/reference/processing.md` for the
/// full contract.
pub struct AudioBuffer<'a> {
    inputs: &'a [&'a [f32]],
    outputs: &'a mut [&'a mut [f32]],
    /// Bit `ch` is set when `inputs[ch]` and `outputs[ch]` point to
    /// the same host memory. Channels ≥ 64 are always reported as
    /// non-aliased — formats with that many channels are exotic
    /// enough to be a follow-up.
    in_place_mask: u64,
    offset: usize,
    num_samples: usize,
}

impl<'a> AudioBuffer<'a> {
    /// Safe wrapper around [`Self::from_slices`] for callers that hold their
    /// own owned `Vec<Vec<f32>>` (e.g. `truce-driver`'s test harness).
    /// Forwards to the unsafe constructor — the borrow checker proves
    /// the lifetime invariants the `unsafe fn` requires when both
    /// slice arrays and the buffer itself live in the same scope.
    /// `num_samples > slice length` still asserts in debug builds.
    pub fn from_slices_checked(
        inputs: &'a [&'a [f32]],
        outputs: &'a mut [&'a mut [f32]],
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
        inputs: &'a [&'a [f32]],
        outputs: &'a mut [&'a mut [f32]],
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
                         (input: {i_start:#x}..{i_end:#x}, output: {o_start:#x}..{o_end:#x})"
                    );
                }
            }
            // Verify num_samples doesn't exceed any slice.
            for (i, inp) in inputs.iter().enumerate() {
                assert!(
                    num_samples <= inp.len(),
                    "AudioBuffer: num_samples ({num_samples}) exceeds input channel {i} length ({})",
                    inp.len()
                );
            }
            for (o, out) in outputs.iter().enumerate() {
                assert!(
                    num_samples <= out.len(),
                    "AudioBuffer: num_samples ({num_samples}) exceeds output channel {o} length ({})",
                    out.len()
                );
            }
        }
        Self {
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

    /// Read+write slice for an in-place channel — the same memory the
    /// host gave us for both input and output. Each sample reads as
    /// the input value before the plugin overwrites it.
    ///
    /// Only meaningful when [`Self::is_in_place`] returns `true`. On a
    /// non-in-place channel this returns the output slice with no
    /// input data in it; reading is allowed but produces uninitialized
    /// host-buffer contents.
    pub fn in_out_mut(&mut self, ch: usize) -> &mut [f32] {
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
    pub fn input(&self, channel: usize) -> &[f32] {
        let end = self.offset + self.num_samples;
        &self.inputs[channel][self.offset..end]
    }

    pub fn output(&mut self, channel: usize) -> &mut [f32] {
        let end = self.offset + self.num_samples;
        &mut self.outputs[channel][self.offset..end]
    }

    /// Number of channels (min of input and output).
    #[must_use]
    pub fn channels(&self) -> usize {
        self.inputs.len().min(self.outputs.len())
    }

    /// Get an input/output pair for a channel. Useful for in-place processing.
    pub fn io_pair(&mut self, in_ch: usize, out_ch: usize) -> (&[f32], &mut [f32]) {
        let end = self.offset + self.num_samples;
        let input = &self.inputs[in_ch][self.offset..end];
        let output = &mut self.outputs[out_ch][self.offset..end];
        (input, output)
    }

    /// Get an input/output pair for the same channel index. Shorthand for `io_pair(ch, ch)`.
    pub fn io(&mut self, ch: usize) -> (&[f32], &mut [f32]) {
        self.io_pair(ch, ch)
    }

    /// Peak absolute value across an output channel.
    ///
    /// Short-circuits and returns `f32::NAN` on the **first** NaN
    /// sample seen, so meters can flag runaway plugins instead of
    /// silently reporting "peaks within range" while NaN poison
    /// spreads downstream. (`f32::max` treats NaN as smaller than
    /// every finite value, which used to make NaN samples disappear
    /// from the peak — that's why this walks manually instead of
    /// folding `.max()`.)
    #[must_use]
    pub fn output_peak(&self, ch: usize) -> f32 {
        let end = self.offset + self.num_samples;
        let mut peak = 0.0f32;
        for &b in &self.outputs[ch][self.offset..end] {
            if b.is_nan() {
                return f32::NAN;
            }
            let abs = b.abs();
            if abs > peak {
                peak = abs;
            }
        }
        peak
    }

    /// Return a sub-block view covering samples `start..start+len`.
    ///
    /// The returned buffer borrows `self` exclusively — you cannot use
    /// the original buffer while the slice is alive.
    ///
    /// # Panics
    /// Panics if `start + len > self.num_samples()`.
    ///
    /// # Example
    /// ```ignore
    /// let mut offset = 0;
    /// for event in events.iter() {
    ///     let at = event.sample_offset as usize;
    ///     if at > offset {
    ///         let mut sub = buffer.slice(offset, at - offset);
    ///         process_sub_block(&mut sub);
    ///     }
    ///     handle_event(&event.body);
    ///     offset = at;
    /// }
    /// if offset < buffer.num_samples() {
    ///     let mut sub = buffer.slice(offset, buffer.num_samples() - offset);
    ///     process_sub_block(&mut sub);
    /// }
    /// ```
    pub fn slice(&mut self, start: usize, len: usize) -> AudioBuffer<'_> {
        assert!(
            start + len <= self.num_samples,
            "slice({start}, {len}) out of bounds for buffer of {} samples",
            self.num_samples,
        );
        let new_offset = self.offset + start;
        // SAFETY: We construct an AudioBuffer<'a> and transmute to AudioBuffer<'_>.
        // These have identical memory layout (lifetimes are erased at runtime).
        // This is sound because:
        // 1. &mut self prevents the caller from using self while the slice exists
        // 2. The underlying channel memory lives for 'a which outlives '_
        // 3. Bounds are checked by the assert above
        let self_ptr: *mut Self = self;
        unsafe {
            let s = &mut *self_ptr;
            std::mem::transmute::<AudioBuffer<'a>, AudioBuffer<'_>>(AudioBuffer {
                inputs: s.inputs,
                outputs: &mut *s.outputs,
                in_place_mask: s.in_place_mask,
                offset: new_offset,
                num_samples: len,
            })
        }
    }
}

/// Scratch space for `AudioBuffer::from_raw_ptrs`.
///
/// Callers allocate this on the stack and pass it to `from_raw_ptrs`.
/// The buffer borrows the slices stored here, so this struct must
/// outlive the returned `AudioBuffer`.
pub struct RawBufferScratch {
    pub input_slices: Vec<&'static [f32]>,
    pub output_slices: Vec<&'static mut [f32]>,
    /// Per-channel copies of input data when the host passes the same
    /// buffer for input and output (in-place processing — VST3 spec
    /// allows this and several real DAWs use it for effects). We can't
    /// hand a `&[f32]` and `&mut [f32]` to overlapping memory without
    /// UB, so we copy the input through here so the slices the plugin
    /// sees are disjoint. Sized lazily; reused across blocks.
    input_copies: Vec<Vec<f32>>,
}

impl RawBufferScratch {
    /// Build an `AudioBuffer` from raw C pointers.
    ///
    /// This is the common FFI pattern used by VST3, VST2, AU, and AAX
    /// wrappers. It converts raw `*const f32` / `*mut f32` channel
    /// pointers into slices and returns an `AudioBuffer` that borrows
    /// the scratch storage.
    ///
    /// **No input-to-output copy is performed.** Plugins that want
    /// pass-through must do `output.copy_from_slice(input)` themselves
    /// — auto-copying clobbers the previous-block tail that delay /
    /// reverb feedback paths read back from the output, and almost
    /// every plugin overwrites outputs anyway.
    ///
    /// **Channel indexing is preserved.** A null channel pointer
    /// becomes an empty slice at that index rather than being skipped
    /// — preserving channel index avoids the silent re-mapping bug
    /// where (input null, output non-null) pairs would shift the
    /// output assignments.
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
    ///
    /// In-place I/O (an input pointer that aliases any output pointer)
    /// is handled per `supports_in_place`:
    /// - `false` (default): aliased inputs are copied into per-channel
    ///   scratch, so `input(ch)` and `output(ch)` are always disjoint.
    /// - `true`: no copy. Aliased input slices are exposed as empty
    ///   (`&[]`); the plugin must use [`AudioBuffer::in_out_mut`] for
    ///   channels where [`AudioBuffer::is_in_place`] returns `true`.
    pub unsafe fn build<'a>(
        &'a mut self,
        inputs: *const *const f32,
        outputs: *mut *mut f32,
        num_in: u32,
        num_out: u32,
        num_frames: u32,
        supports_in_place: bool,
    ) -> AudioBuffer<'a> {
        unsafe {
            let nf = num_frames as usize;

            // Snapshot output pointers up front so the input pass can
            // detect aliasing without holding `&mut` to output_slices.
            let num_out = num_out as usize;
            let num_in = num_in as usize;
            let out_ptrs: [Option<*mut f32>; 32] = std::array::from_fn(|ch| {
                if ch < num_out {
                    let p = *outputs.add(ch);
                    if p.is_null() { None } else { Some(p) }
                } else {
                    None
                }
            });
            let aliases_any_output = |in_ptr: *const f32| -> bool {
                let in_start = in_ptr as usize;
                let in_end = in_start + nf * std::mem::size_of::<f32>();
                out_ptrs.iter().take(num_out).any(|o| {
                    o.is_some_and(|op| {
                        let o_start = op as usize;
                        let o_end = o_start + nf * std::mem::size_of::<f32>();
                        !(in_end <= o_start || o_end <= in_start)
                    })
                })
            };

            self.input_slices.clear();
            self.input_slices.reserve(num_in);
            // Grow the per-channel copy slots if the bus widened.
            while self.input_copies.len() < num_in {
                self.input_copies.push(Vec::new());
            }
            let mut in_place_mask: u64 = 0;
            for ch in 0..num_in {
                let ptr = *inputs.add(ch);
                let slice: &[f32] = if ptr.is_null() {
                    &[]
                } else if aliases_any_output(ptr) {
                    if ch < 64 {
                        in_place_mask |= 1 << ch;
                    }
                    if supports_in_place {
                        // Plugin opted in: hand it nothing through
                        // input(ch); it must read+write via in_out_mut.
                        &[]
                    } else {
                        // Default: snapshot the input before the
                        // plugin overwrites the shared buffer.
                        let copy = &mut self.input_copies[ch];
                        copy.clear();
                        copy.extend_from_slice(std::slice::from_raw_parts(ptr, nf));
                        std::slice::from_raw_parts(copy.as_ptr(), nf)
                    }
                } else {
                    std::slice::from_raw_parts(ptr, nf)
                };
                self.input_slices.push(slice);
            }

            self.output_slices.clear();
            self.output_slices.reserve(num_out);
            for ch in 0..num_out {
                let ptr = *outputs.add(ch);
                let slice: &mut [f32] = if ptr.is_null() {
                    &mut []
                } else {
                    std::slice::from_raw_parts_mut(ptr, nf)
                };
                self.output_slices.push(slice);
            }

            // SAFETY: Same transmute pattern as AudioBuffer::slice().
            // RawBufferScratch stores 'static slices but we return AudioBuffer<'a>.
            // Sound because the caller's raw pointers must outlive 'a, and
            // &'a mut self prevents aliasing.
            let self_ptr: *mut Self = self;
            let s = &mut *self_ptr;
            let mut buf = std::mem::transmute::<AudioBuffer<'static>, AudioBuffer<'a>>(
                AudioBuffer::from_slices(&s.input_slices, &mut s.output_slices, nf),
            );
            buf.set_in_place_mask(in_place_mask);
            buf
        }
    }
}

impl RawBufferScratch {
    /// Pre-allocate the per-channel scratch vectors so `build` runs
    /// allocation-free for buses up to `num_in` × `num_out` channels
    /// and blocks up to `max_frames`. Idempotent and growth-only:
    /// safe to call from both `cb_create` (with the default layout's
    /// counts) and `cb_reset` (with the host's negotiated max). Without
    /// this hook, the first audio block at >2 channels would heap-
    /// allocate on the audio thread because the `Default` impl only
    /// sizes for stereo.
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
    }
}

impl Default for RawBufferScratch {
    fn default() -> Self {
        Self {
            input_slices: Vec::with_capacity(2),
            output_slices: Vec::with_capacity(2),
            input_copies: Vec::with_capacity(2),
        }
    }
}
