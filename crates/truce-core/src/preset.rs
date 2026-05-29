/// Host-visible factory preset metadata.
///
/// Factory presets are static, plugin-supplied presets that hosts may
/// show in their native preset menus. The `number` is the host-facing
/// stable identifier. AUv3 conventionally uses non-negative numbers
/// for factory presets; plugins that do not need a custom numbering
/// scheme can use the preset's zero-based index.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FactoryPresetInfo {
    pub number: i32,
    pub name: &'static str,
}

impl FactoryPresetInfo {
    #[must_use]
    pub const fn new(number: i32, name: &'static str) -> Self {
        Self { number, name }
    }
}
