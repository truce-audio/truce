use std::f64::consts::TAU;

pub struct Voice {
    pub note: u8,
    pub releasing: bool,
    velocity: f64,
    phase: f64,
    phase_inc: f64,
    envelope: Envelope,
    filter: OnePoleFilter,
}

impl Voice {
    pub fn new(
        note: u8,
        freq: f64,
        velocity: f32,
        sample_rate: f64,
        attack: f64,
        decay: f64,
        sustain: f64,
        release: f64,
    ) -> Self {
        Self {
            note,
            releasing: false,
            velocity: velocity as f64,
            phase: 0.0,
            phase_inc: freq / sample_rate,
            envelope: Envelope::new(attack, decay, sustain, release, sample_rate),
            filter: OnePoleFilter::new(),
        }
    }

    pub fn release(&mut self) {
        self.releasing = true;
        self.envelope.trigger_release();
    }

    pub fn is_done(&self) -> bool {
        self.envelope.is_done()
    }

    pub fn render(&mut self, waveform: u32, cutoff: f64, resonance: f64, sample_rate: f64) -> f64 {
        let osc = match waveform {
            0 => self.osc_sine(),
            1 => self.osc_saw(),
            2 => self.osc_square(),
            3 => self.osc_triangle(),
            _ => self.osc_saw(),
        };

        self.phase += self.phase_inc;
        if self.phase >= 1.0 {
            self.phase -= 1.0;
        }

        let env = self.envelope.next();
        let filtered = self.filter.process(osc, cutoff, resonance, sample_rate);

        filtered * env * self.velocity
    }

    fn osc_sine(&self) -> f64 {
        (self.phase * TAU).sin()
    }

    fn osc_saw(&self) -> f64 {
        2.0 * self.phase - 1.0
    }

    fn osc_square(&self) -> f64 {
        if self.phase < 0.5 {
            1.0
        } else {
            -1.0
        }
    }

    fn osc_triangle(&self) -> f64 {
        4.0 * (self.phase - (self.phase + 0.5).floor()).abs() - 1.0
    }
}

/// Simple ADSR envelope.
pub struct Envelope {
    stage: EnvStage,
    level: f64,
    attack_rate: f64,
    decay_rate: f64,
    sustain: f64,
    release_rate: f64,
}

#[derive(Clone, Copy, PartialEq)]
enum EnvStage {
    Attack,
    Decay,
    Sustain,
    Release,
    Done,
}

impl Envelope {
    pub fn new(attack: f64, decay: f64, sustain: f64, release: f64, sample_rate: f64) -> Self {
        let attack_samples = (attack * sample_rate).max(1.0);
        let decay_samples = (decay * sample_rate).max(1.0);
        let release_samples = (release * sample_rate).max(1.0);

        Self {
            stage: EnvStage::Attack,
            level: 0.0,
            attack_rate: 1.0 / attack_samples,
            decay_rate: 1.0 / decay_samples,
            sustain: sustain.clamp(0.0, 1.0),
            release_rate: 1.0 / release_samples,
        }
    }

    pub fn trigger_release(&mut self) {
        if self.stage != EnvStage::Done {
            self.stage = EnvStage::Release;
        }
    }

    pub fn is_done(&self) -> bool {
        self.stage == EnvStage::Done
    }

    pub fn next(&mut self) -> f64 {
        match self.stage {
            EnvStage::Attack => {
                self.level += self.attack_rate;
                if self.level >= 1.0 {
                    self.level = 1.0;
                    self.stage = EnvStage::Decay;
                }
            }
            EnvStage::Decay => {
                self.level -= self.decay_rate * (self.level - self.sustain);
                if self.level <= self.sustain + 0.0001 {
                    self.level = self.sustain;
                    self.stage = EnvStage::Sustain;
                }
            }
            EnvStage::Sustain => {
                self.level = self.sustain;
            }
            EnvStage::Release => {
                self.level -= self.release_rate * self.level;
                if self.level < 0.0001 {
                    self.level = 0.0;
                    self.stage = EnvStage::Done;
                }
            }
            EnvStage::Done => {
                self.level = 0.0;
            }
        }
        self.level
    }
}

/// Simple one-pole low-pass filter.
pub struct OnePoleFilter {
    prev: f64,
}

impl OnePoleFilter {
    pub fn new() -> Self {
        Self { prev: 0.0 }
    }

    pub fn process(&mut self, input: f64, cutoff: f64, _resonance: f64, sample_rate: f64) -> f64 {
        let freq = cutoff.clamp(20.0, sample_rate * 0.49);
        let rc = 1.0 / (TAU * freq);
        let dt = 1.0 / sample_rate;
        let alpha = dt / (rc + dt);

        self.prev += alpha * (input - self.prev);
        self.prev
    }
}
