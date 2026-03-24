//! Vizia lens implementations for reading parameter values from `ParamModel`.
//!
//! These lenses are `Copy + Debug + 'static` as required by vizia's `Lens` trait.
//! They read values from the `ParamModel` stored in the vizia context tree.

use vizia::binding::{Lens, LensValue};

use crate::param_model::ParamModel;

/// Lens reading a parameter's normalized value (0.0–1.0) as `f32`.
#[derive(Debug, Clone, Copy)]
pub struct ParamNormLens(pub u32);

impl ParamNormLens {
    pub fn new(id: impl Into<u32>) -> Self {
        Self(id.into())
    }
}

impl Lens for ParamNormLens {
    type Source = ParamModel;
    type Target = f32;

    fn view<'a>(&self, source: &'a Self::Source) -> Option<LensValue<'a, Self::Target>> {
        Some(LensValue::Owned(source.get(self.0) as f32))
    }
}

/// Lens reading a parameter's formatted display string.
#[derive(Debug, Clone, Copy)]
pub struct ParamFormatLens(pub u32);

impl ParamFormatLens {
    pub fn new(id: impl Into<u32>) -> Self {
        Self(id.into())
    }
}

impl Lens for ParamFormatLens {
    type Source = ParamModel;
    type Target = String;

    fn view<'a>(&self, source: &'a Self::Source) -> Option<LensValue<'a, Self::Target>> {
        Some(LensValue::Owned(source.format(self.0)))
    }
}

/// Lens reading a parameter as a bool (normalized > 0.5).
#[derive(Debug, Clone, Copy)]
pub struct ParamBoolLens(pub u32);

impl ParamBoolLens {
    pub fn new(id: impl Into<u32>) -> Self {
        Self(id.into())
    }
}

impl Lens for ParamBoolLens {
    type Source = ParamModel;
    type Target = bool;

    fn view<'a>(&self, source: &'a Self::Source) -> Option<LensValue<'a, Self::Target>> {
        Some(LensValue::Owned(source.get(self.0) > 0.5))
    }
}

/// Lens reading a meter value (0.0–1.0) as `f32`.
#[derive(Debug, Clone, Copy)]
pub struct MeterLens(pub u32);

impl MeterLens {
    pub fn new(id: impl Into<u32>) -> Self {
        Self(id.into())
    }
}

impl Lens for MeterLens {
    type Source = ParamModel;
    type Target = f32;

    fn view<'a>(&self, source: &'a Self::Source) -> Option<LensValue<'a, Self::Target>> {
        Some(LensValue::Owned(source.meter(self.0)))
    }
}
