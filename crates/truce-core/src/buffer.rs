/// Non-interleaved audio buffer. Zero-copy — borrows host memory
/// through the format wrapper.
pub struct AudioBuffer<'a> {
    inputs: &'a [&'a [f32]],
    outputs: &'a mut [&'a mut [f32]],
    offset: usize,
    num_samples: usize,
}

impl<'a> AudioBuffer<'a> {
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
                let i_end = i_start + inp.len() * std::mem::size_of::<f32>();
                for (o, out) in outputs.iter().enumerate() {
                    let o_start = out.as_ptr() as usize;
                    let o_end = o_start + out.len() * std::mem::size_of::<f32>();
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
    pub fn output_peak(&self, ch: usize) -> f32 {
        let end = self.offset + self.num_samples;
        self.outputs[ch][self.offset..end]
            .iter()
            .fold(0.0f32, |a, &b| a.max(b.abs()))
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
    /// wrappers. It:
    /// 1. Converts raw `*const f32` / `*mut f32` channel pointers to slices
    /// 2. Copies input channels to output channels (in-place effect processing)
    /// 3. Returns an `AudioBuffer` borrowing the scratch slices
    ///
    /// # Safety
    /// - `inputs` must point to `num_in` valid `*const f32` pointers,
    ///   each pointing to `num_frames` samples.
    /// - `outputs` must point to `num_out` valid `*mut f32` pointers,
    ///   each pointing to `num_frames` samples.
    /// - The pointed-to memory must remain valid for the lifetime of
    ///   the returned `AudioBuffer`.
    pub unsafe fn build<'a>(
        &'a mut self,
        inputs: *const *const f32,
        outputs: *mut *mut f32,
        num_in: u32,
        num_out: u32,
        num_frames: u32,
    ) -> AudioBuffer<'a> {
        let nf = num_frames as usize;

        self.input_slices.clear();
        for ch in 0..num_in as usize {
            let ptr = *inputs.add(ch);
            if !ptr.is_null() {
                self.input_slices.push(std::slice::from_raw_parts(ptr, nf));
            }
        }

        self.output_slices.clear();
        for ch in 0..num_out as usize {
            let ptr = *outputs.add(ch);
            if !ptr.is_null() {
                self.output_slices
                    .push(std::slice::from_raw_parts_mut(ptr, nf));
            }
        }

        // Copy input to output for in-place effect processing.
        let copy_ch = self.input_slices.len().min(self.output_slices.len());
        for ch in 0..copy_ch {
            self.output_slices[ch][..nf].copy_from_slice(&self.input_slices[ch][..nf]);
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

impl Default for RawBufferScratch {
    fn default() -> Self {
        Self {
            input_slices: Vec::with_capacity(2),
            output_slices: Vec::with_capacity(2),
        }
    }
}
