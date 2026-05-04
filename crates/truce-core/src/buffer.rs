/// Non-interleaved audio buffer. Zero-copy — borrows host memory
/// through the format wrapper.
pub struct AudioBuffer<'a> {
    inputs: &'a [&'a [f32]],
    outputs: &'a mut [&'a mut [f32]],
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
            offset: 0,
            num_samples,
        }
    }

    pub fn num_samples(&self) -> usize {
        self.num_samples
    }

    pub fn num_input_channels(&self) -> usize {
        self.inputs.len()
    }

    pub fn num_output_channels(&self) -> usize {
        self.outputs.len()
    }

    pub fn input(&self, channel: usize) -> &[f32] {
        let end = self.offset + self.num_samples;
        &self.inputs[channel][self.offset..end]
    }

    pub fn output(&mut self, channel: usize) -> &mut [f32] {
        let end = self.offset + self.num_samples;
        &mut self.outputs[channel][self.offset..end]
    }

    /// Number of channels (min of input and output).
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
}

impl RawBufferScratch {
    /// Build an `AudioBuffer` from raw C pointers.
    ///
    /// This is the common FFI pattern used by VST3, VST2, AU, and AAX
    /// wrappers. It converts raw `*const f32` / `*mut f32` channel
    /// pointers into slices and returns an `AudioBuffer` that borrows
    /// the scratch storage.
    ///
    /// **No input-to-output copy is performed.** Earlier revisions
    /// silently copied each input channel into the matching output
    /// channel as a "convenience for in-place effects"; that
    /// clobbered the previous-block tail of any plugin that reads its
    /// own output (delay/reverb feedback) and turned out to be hidden
    /// silent corruption. Plugins that genuinely want pass-through
    /// must do `output.copy_from_slice(input)` themselves; almost
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
    /// - No input pointer may alias any output pointer.
    pub unsafe fn build<'a>(
        &'a mut self,
        inputs: *const *const f32,
        outputs: *mut *mut f32,
        num_in: u32,
        num_out: u32,
        num_frames: u32,
    ) -> AudioBuffer<'a> {
        unsafe {
            let nf = num_frames as usize;

            self.input_slices.clear();
            self.input_slices.reserve(num_in as usize);
            for ch in 0..num_in as usize {
                let ptr = *inputs.add(ch);
                let slice: &[f32] = if ptr.is_null() {
                    &[]
                } else {
                    std::slice::from_raw_parts(ptr, nf)
                };
                self.input_slices.push(slice);
            }

            self.output_slices.clear();
            self.output_slices.reserve(num_out as usize);
            for ch in 0..num_out as usize {
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
            std::mem::transmute::<AudioBuffer<'static>, AudioBuffer<'a>>(AudioBuffer::from_slices(
                &s.input_slices,
                &mut s.output_slices,
                nf,
            ))
        }
    }
}

impl Default for RawBufferScratch {
    fn default() -> Self {
        Self {
            input_slices: Vec::with_capacity(2),
            output_slices: Vec::with_capacity(2),
        }
    }
}
