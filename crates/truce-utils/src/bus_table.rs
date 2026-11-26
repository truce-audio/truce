//! The bus-layout table a plugin embeds in its built binary.
//!
//! A plugin's audio layouts are a `const` (`PluginLogic::BUS_LAYOUTS`), so
//! `truce::plugin!` serializes them at compile time into an exported data
//! symbol, [`symbol_name`]. A package-time tool - a format helper rendering
//! port declarations, say - then reads the layouts straight out of the
//! built `.dylib` / `.so` / `.dll` without running any plugin code, which is
//! also what makes it work for a cross build whose binary can't run here.
//!
//! The encoder lives with `BusLayout` in `truce-core` (it has to be a
//! `const fn`); this module owns the wire format both sides agree on and
//! the decoder, dependency-free so any reader can use it.
//!
//! ```text
//! "TBUS"        magic            4 bytes
//! u8            format version   (currently 1)
//! u8            layout count
//! per layout:
//!   u8          input bus count
//!   u8          output bus count
//!   per bus, inputs then outputs:
//!     u8        kind             0 = main, 1 = sidechain
//!     u32 LE    channel count
//!     u8        name length
//!     [u8]      name             UTF-8
//! ```
//!
//! The layout changes only with [`VERSION`]: a reader that finds a version
//! it doesn't know must refuse rather than guess.

/// The table's first four bytes.
pub const MAGIC: [u8; 4] = *b"TBUS";

/// Format version this build writes and reads.
pub const VERSION: u8 = 1;

/// Bus kind byte for a main bus.
pub const KIND_MAIN: u8 = 0;
/// Bus kind byte for a sidechain bus.
pub const KIND_SIDECHAIN: u8 = 1;

/// The exported symbol a plugin crate's table lives under. Named per crate
/// (`-` becoming `_`, as in the crate's own identifier) so two plugin
/// crates linked into one binary - a test harness, say - don't collide.
/// Platforms that decorate C symbols (a leading `_` on macOS) add it on
/// top; readers look for both spellings.
#[must_use]
pub fn symbol_name(crate_name: &str) -> String {
    format!("truce_bus_layouts_{}", crate_name.replace('-', "_"))
}

/// One bus of a decoded layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BusRecord {
    pub name: String,
    pub channels: u32,
    pub sidechain: bool,
}

/// One decoded layout: input buses, then output buses, in declaration
/// order (the first input is the main input, as in `BusLayout`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LayoutRecord {
    pub inputs: Vec<BusRecord>,
    pub outputs: Vec<BusRecord>,
}

impl LayoutRecord {
    /// Total input channels across every input bus.
    #[must_use]
    pub fn input_channels(&self) -> u32 {
        self.inputs.iter().map(|b| b.channels).sum()
    }

    /// Total output channels across every output bus.
    #[must_use]
    pub fn output_channels(&self) -> u32 {
        self.outputs.iter().map(|b| b.channels).sum()
    }
}

/// Decode a table. Trailing bytes after the last layout are ignored, so a
/// reader can hand over a symbol's bytes padded to its section alignment.
///
/// # Errors
///
/// A bad magic, an unknown version, a truncated table, a bad kind byte, or
/// a name that isn't UTF-8.
pub fn decode(bytes: &[u8]) -> Result<Vec<LayoutRecord>, String> {
    let mut r = Reader { bytes, at: 0 };
    if r.take(4)? != MAGIC {
        return Err("not a truce bus-layout table (bad magic)".into());
    }
    let version = r.u8()?;
    if version != VERSION {
        return Err(format!(
            "bus-layout table version {version}; this build reads version {VERSION}"
        ));
    }
    let count = r.u8()?;
    let mut layouts = Vec::with_capacity(usize::from(count));
    for _ in 0..count {
        let (n_in, n_out) = (r.u8()?, r.u8()?);
        let inputs = (0..n_in).map(|_| r.bus()).collect::<Result<_, _>>()?;
        let outputs = (0..n_out).map(|_| r.bus()).collect::<Result<_, _>>()?;
        layouts.push(LayoutRecord { inputs, outputs });
    }
    Ok(layouts)
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], String> {
        let end = self.at.checked_add(n).filter(|&e| e <= self.bytes.len());
        let end = end.ok_or("bus-layout table is truncated")?;
        let out = &self.bytes[self.at..end];
        self.at = end;
        Ok(out)
    }

    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }

    fn bus(&mut self) -> Result<BusRecord, String> {
        let sidechain = match self.u8()? {
            KIND_MAIN => false,
            KIND_SIDECHAIN => true,
            other => return Err(format!("bus-layout table has an unknown bus kind {other}")),
        };
        let channels = u32::from_le_bytes(
            self.take(4)?
                .try_into()
                .map_err(|_| "bus-layout table is truncated")?,
        );
        let len = usize::from(self.u8()?);
        let name = std::str::from_utf8(self.take(len)?)
            .map_err(|_| "bus-layout table has a bus name that isn't UTF-8")?
            .to_string();
        Ok(BusRecord {
            name,
            channels,
            sidechain,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{MAGIC, VERSION, decode, symbol_name};

    #[test]
    fn the_symbol_is_named_per_crate() {
        assert_eq!(
            symbol_name("truce-example-gain"),
            "truce_bus_layouts_truce_example_gain"
        );
    }

    #[test]
    fn decode_refuses_what_it_does_not_understand() {
        assert!(decode(b"NOPE").is_err());
        let mut future = MAGIC.to_vec();
        future.extend([VERSION + 1, 0]);
        assert!(decode(&future).is_err(), "an unknown version");
        let mut truncated = MAGIC.to_vec();
        truncated.extend([VERSION, 1, 1]);
        assert!(decode(&truncated).is_err(), "a layout cut short");
    }

    #[test]
    fn an_empty_table_decodes_to_no_layouts() {
        let mut empty = MAGIC.to_vec();
        empty.extend([VERSION, 0, 0xAA, 0xAA]);
        assert_eq!(
            decode(&empty).unwrap(),
            Vec::new(),
            "trailing padding is ignored"
        );
    }
}
