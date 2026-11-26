//! Read the bus-layout table `truce::plugin!` embeds in a built plugin.
//!
//! The table is an exported data symbol (see [`truce_utils::bus_table`]
//! for the wire format), so it is found and read the same way for every
//! object format a plugin ships in - Mach-O (thin or universal), ELF and
//! PE - without loading the binary or running any plugin code. That keeps
//! it usable for a cross build whose binary can't run on the packaging
//! machine.

use object::read::macho::{FatArch, MachOFatFile32, MachOFatFile64};
use object::{Export, ExportTarget, FileKind, NameOrOrdinal, Object, ObjectSection, ObjectSymbol};
use std::path::Path;

pub use truce_utils::bus_table::{BusRecord, LayoutRecord};

/// Read and decode the layouts `crate_name`'s `truce::plugin!` embedded in
/// `binary`.
///
/// # Errors
///
/// The file can't be read or parsed, it doesn't export the table (built
/// by a truce older than the table, or not a truce plugin of that crate),
/// or the table doesn't decode.
pub fn read_from_binary(binary: &Path, crate_name: &str) -> Result<Vec<LayoutRecord>, String> {
    let data = std::fs::read(binary).map_err(|e| format!("reading {}: {e}", binary.display()))?;
    let symbol = truce_utils::bus_table::symbol_name(crate_name);
    let bytes = table_bytes(thin_image(&data)?, &symbol).map_err(|e| {
        format!(
            "{}: {e} (rebuild it with this truce; the table is emitted by `truce::plugin!`)",
            binary.display()
        )
    })?;
    truce_utils::bus_table::decode(bytes)
        .map_err(|e| format!("{}: bus-layout table `{symbol}`: {e}", binary.display()))
}

/// The image to read: `data` itself, or a universal Mach-O's first slice.
/// Every slice is the same crate built for another architecture, so they
/// all carry the same table.
fn thin_image(data: &[u8]) -> Result<&[u8], String> {
    match FileKind::parse(data).map_err(|e| format!("unrecognized binary: {e}"))? {
        FileKind::MachOFat32 => first_slice(
            data,
            MachOFatFile32::parse(data)
                .map_err(|e| e.to_string())?
                .arches(),
        ),
        FileKind::MachOFat64 => first_slice(
            data,
            MachOFatFile64::parse(data)
                .map_err(|e| e.to_string())?
                .arches(),
        ),
        _ => Ok(data),
    }
}

fn first_slice<'d, A: FatArch>(data: &'d [u8], arches: &[A]) -> Result<&'d [u8], String> {
    arches
        .first()
        .ok_or("universal binary has no slices")?
        .data(data)
        .map_err(|e| format!("universal binary slice: {e}"))
}

/// The bytes from `symbol`'s address to the end of its section. The
/// decoder stops at the table's own end, so the slack is harmless and no
/// symbol size is needed (Mach-O doesn't record one).
fn table_bytes<'d>(image: &'d [u8], symbol: &str) -> Result<&'d [u8], String> {
    let file = object::File::parse(image).map_err(|e| format!("unrecognized binary: {e}"))?;
    let address = symbol_address(&file, symbol)
        .ok_or_else(|| format!("no exported bus-layout table `{symbol}`"))?;
    let section = file
        .sections()
        .find(|s| (s.address()..s.address() + s.size()).contains(&address))
        .ok_or_else(|| format!("`{symbol}` points outside every section"))?;
    let data = section
        .data()
        .map_err(|e| format!("reading the section holding `{symbol}`: {e}"))?;
    usize::try_from(address - section.address())
        .ok()
        .and_then(|offset| data.get(offset..))
        .filter(|bytes| !bytes.is_empty())
        .ok_or_else(|| format!("`{symbol}` lies past its section's file data"))
}

/// `symbol`'s address, with or without the leading `_` Mach-O (and 32-bit
/// Windows) put on C symbols. Looks at the export table first - it
/// survives stripping, and is the only table a PE plugin has - then the
/// symbol tables.
fn symbol_address(file: &object::File<'_>, symbol: &str) -> Option<u64> {
    let matches = |name: &[u8]| {
        name == symbol.as_bytes() || name.strip_prefix(b"_") == Some(symbol.as_bytes())
    };
    let exported = |export: &Export<'_>| match (export.name(), export.target()) {
        (NameOrOrdinal::Name(name), ExportTarget::Address { address }) if matches(name) => {
            Some(address)
        }
        _ => None,
    };
    file.exports()
        .ok()
        .and_then(|exports| exports.filter_map(Result::ok).find_map(|e| exported(&e)))
        .or_else(|| {
            file.symbols()
                .chain(file.dynamic_symbols())
                .find(|s| s.is_definition() && s.name_bytes().is_ok_and(matches))
                .map(|s| s.address())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_binary_without_the_table_is_a_named_error() {
        let exe = std::env::current_exe().unwrap();
        let err = read_from_binary(&exe, "no-such-plugin").unwrap_err();
        assert!(err.contains("truce_bus_layouts_no_such_plugin"), "{err}");
    }

    #[test]
    fn a_non_binary_is_an_error() {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        assert!(read_from_binary(&manifest, "truce-build").is_err());
    }
}
