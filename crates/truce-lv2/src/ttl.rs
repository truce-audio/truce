//! Emit an LV2 bundle: `manifest.ttl`, `{name}.ttl`, and the plugin `.so`.
//!
//! `cargo truce install --lv2` calls `emit_bundle` after the shared library
//! has been copied into place. The TTL files describe the plugin's ports
//! and parameters in RDF/Turtle — LV2 hosts read them to know what the
//! plugin is before loading the binary.
//!
//! We stay conservative: generate only the lv2 core vocabulary, plus
//! `atom`/`midi` when the plugin has MIDI, plus `state` always. No
//! `presets`, no `patch`, no `extension` chains until a host asks.

use std::fs;
use std::io::Write;
use std::path::Path;

use truce_core::info::{PluginCategory, PluginInfo};
use truce_params::{ParamInfo, ParamRange, ParamUnit};

use crate::{derive_port_layout, plugin_uri, ui_uri, PortLayout};
use truce_core::export::PluginExport;
use truce_params::Params;

/// Emit an LV2 bundle for the plugin. Assumes the `.so` has already been
/// placed at `bundle_dir.join(&so_name)`.
pub fn emit_bundle<P: PluginExport>(
    bundle_dir: &Path,
    so_name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    fs::create_dir_all(bundle_dir)?;
    let info = P::info();
    let layout = derive_port_layout::<P>();
    let plugin = P::create();
    let params = plugin.params().param_infos();
    let meter_ids = plugin.params().meter_ids();

    let uri = plugin_uri(&info);
    let ui_uri = ui_uri(&info);
    let ttl_basename = "plugin.ttl";

    write_manifest(bundle_dir, &uri, &ui_uri, &layout, ttl_basename, so_name)?;
    write_plugin_ttl(
        bundle_dir,
        ttl_basename,
        &uri,
        &ui_uri,
        &info,
        &layout,
        &params,
        &meter_ids,
        so_name,
    )?;
    Ok(())
}

fn write_manifest(
    bundle_dir: &Path,
    uri: &str,
    ui_uri: &str,
    layout: &PortLayout,
    ttl_basename: &str,
    so_name: &str,
) -> std::io::Result<()> {
    let mut f = fs::File::create(bundle_dir.join("manifest.ttl"))?;
    writeln!(f, "@prefix lv2:  <http://lv2plug.in/ns/lv2core#> .")?;
    writeln!(f, "@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .")?;
    writeln!(f, "@prefix ui:   <http://lv2plug.in/ns/extensions/ui#> .")?;
    writeln!(f, "@prefix atom: <http://lv2plug.in/ns/ext/atom#> .")?;
    writeln!(f)?;
    writeln!(f, "<{uri}>")?;
    writeln!(f, "    a lv2:Plugin ;")?;
    writeln!(f, "    lv2:binary <{so_name}> ;")?;
    writeln!(f, "    rdfs:seeAlso <{ttl_basename}> .")?;
    writeln!(f)?;
    writeln!(f, "<{ui_uri}>")?;
    // UI type is tied to the running platform: on macOS the host hands us
    // an `NSView*` via `ui:parent`, on X11 an `xcb_window_t`. We bake the
    // right `rdf:type` at install time so the same plugin binary can be
    // cross-platform (the plugin's shared library is otherwise identical
    // on both platforms).
    #[cfg(target_os = "macos")]
    writeln!(f, "    a ui:CocoaUI ;")?;
    #[cfg(not(target_os = "macos"))]
    writeln!(f, "    a ui:X11UI ;")?;
    writeln!(f, "    ui:binary <{so_name}> ;")?;
    // Subscribe the UI to the DSP's notify-out port so the host forwards
    // atom events (currently time:Position) to our port_event callback.
    writeln!(f, "    ui:portNotification [")?;
    writeln!(f, "        ui:plugin <{uri}> ;")?;
    writeln!(f, "        lv2:symbol \"notify_out\" ;")?;
    writeln!(f, "        ui:notifyType atom:Object")?;
    writeln!(f, "    ] ;")?;
    // Subscribe the UI to each meter output so the host forwards float
    // control updates each block (LV2_UI__floatProtocol is the default
    // for ControlPorts, so no explicit notifyType is needed).
    for slot in 0..layout.num_meters {
        writeln!(f, "    ui:portNotification [")?;
        writeln!(f, "        ui:plugin <{uri}> ;")?;
        writeln!(f, "        lv2:symbol \"meter_{slot}\" ;")?;
        writeln!(f, "        ui:protocol ui:floatProtocol")?;
        writeln!(f, "    ] ;")?;
    }
    writeln!(f, "    .")?;
    Ok(())
}

fn write_plugin_ttl(
    bundle_dir: &Path,
    ttl_basename: &str,
    uri: &str,
    ui_uri: &str,
    info: &PluginInfo,
    layout: &PortLayout,
    params: &[ParamInfo],
    meter_ids: &[u32],
    so_name: &str,
) -> std::io::Result<()> {
    let mut f = fs::File::create(bundle_dir.join(ttl_basename))?;

    writeln!(f, "@prefix lv2:   <http://lv2plug.in/ns/lv2core#> .")?;
    writeln!(f, "@prefix rdfs:  <http://www.w3.org/2000/01/rdf-schema#> .")?;
    writeln!(f, "@prefix doap:  <http://usefulinc.com/ns/doap#> .")?;
    writeln!(f, "@prefix foaf:  <http://xmlns.com/foaf/0.1/> .")?;
    writeln!(f, "@prefix units: <http://lv2plug.in/ns/extensions/units#> .")?;
    writeln!(f, "@prefix atom:  <http://lv2plug.in/ns/ext/atom#> .")?;
    writeln!(f, "@prefix midi:  <http://lv2plug.in/ns/ext/midi#> .")?;
    writeln!(f, "@prefix time:  <http://lv2plug.in/ns/ext/time#> .")?;
    writeln!(f, "@prefix rsz:   <http://lv2plug.in/ns/ext/resize-port#> .")?;
    writeln!(f, "@prefix state: <http://lv2plug.in/ns/ext/state#> .")?;
    writeln!(f, "@prefix ui:    <http://lv2plug.in/ns/extensions/ui#> .")?;
    writeln!(f, "@prefix pprop: <http://lv2plug.in/ns/ext/port-props#> .")?;
    writeln!(f)?;

    let category = category_as_lv2(info.category);
    writeln!(f, "<{uri}>")?;
    if category == "lv2:Plugin" {
        writeln!(f, "    a lv2:Plugin ;")?;
    } else {
        writeln!(f, "    a lv2:Plugin, {category} ;")?;
    }
    writeln!(f, "    doap:name \"{}\" ;", escape_turtle(info.name))?;
    writeln!(f, "    doap:maintainer [")?;
    writeln!(
        f,
        "        a foaf:Person ; foaf:name \"{}\"",
        escape_turtle(info.vendor)
    )?;
    if !info.url.is_empty() {
        writeln!(f, "        ; foaf:homepage <{}>", info.url)?;
    }
    writeln!(f, "    ] ;")?;
    writeln!(f, "    lv2:binary <{so_name}> ;")?;
    writeln!(f, "    lv2:extensionData state:interface ;")?;
    writeln!(f, "    ui:ui <{ui_uri}> ;")?;

    let total_ports = layout.total();
    if total_ports > 0 {
        write!(f, "    lv2:port")?;
        for i in 0..total_ports {
            let sep = if i == 0 { " " } else { ",\n        " };
            write!(f, "{sep}[")?;
            emit_port(&mut f, i, layout, params, meter_ids)?;
            write!(f, "    ]")?;
        }
        writeln!(f, " .")?;
    } else {
        writeln!(f, "    .")?;
    }

    Ok(())
}

fn emit_port(
    f: &mut fs::File,
    index: u32,
    layout: &PortLayout,
    params: &[ParamInfo],
    meter_ids: &[u32],
) -> std::io::Result<()> {
    writeln!(f)?;
    if index < layout.audio_out_start() {
        let ch = index - layout.audio_in_start();
        writeln!(f, "        a lv2:InputPort, lv2:AudioPort ;")?;
        writeln!(f, "        lv2:index {index} ;")?;
        writeln!(f, "        lv2:symbol \"in_{ch}\" ;")?;
        writeln!(f, "        lv2:name \"Audio In {}\" ;", ch + 1)?;
    } else if index < layout.control_start() {
        let ch = index - layout.audio_out_start();
        writeln!(f, "        a lv2:OutputPort, lv2:AudioPort ;")?;
        writeln!(f, "        lv2:index {index} ;")?;
        writeln!(f, "        lv2:symbol \"out_{ch}\" ;")?;
        writeln!(f, "        lv2:name \"Audio Out {}\" ;", ch + 1)?;
    } else if index < layout.meter_start() {
        let p = &params[(index - layout.control_start()) as usize];
        emit_control_port(f, index, p)?;
    } else if index < layout.meter_start() + layout.num_meters {
        let slot = (index - layout.meter_start()) as usize;
        let id = meter_ids[slot];
        emit_meter_port(f, index, slot, id)?;
    } else if Some(index) == layout.midi_in_port() {
        writeln!(f, "        a lv2:InputPort, atom:AtomPort ;")?;
        writeln!(f, "        atom:bufferType atom:Sequence ;")?;
        // Declare support for both MIDI and time:Position so hosts send
        // transport events through this port alongside MIDI.
        writeln!(f, "        atom:supports midi:MidiEvent, time:Position ;")?;
        writeln!(f, "        lv2:index {index} ;")?;
        writeln!(f, "        lv2:symbol \"midi_in\" ;")?;
        writeln!(f, "        lv2:name \"MIDI In\" ;")?;
    } else if Some(index) == layout.midi_out_port() {
        writeln!(f, "        a lv2:OutputPort, atom:AtomPort ;")?;
        writeln!(f, "        atom:bufferType atom:Sequence ;")?;
        writeln!(f, "        atom:supports midi:MidiEvent ;")?;
        writeln!(f, "        lv2:index {index} ;")?;
        writeln!(f, "        lv2:symbol \"midi_out\" ;")?;
        writeln!(f, "        lv2:name \"MIDI Out\" ;")?;
    } else if index == layout.notify_out_port() {
        // DSP→UI notification port. Carries time:Position objects so the
        // LV2 UI can surface host transport to plugin editors. Kept as
        // the last port so adding it is a backwards-compatible change
        // for existing TTL bundles.
        writeln!(f, "        a lv2:OutputPort, atom:AtomPort ;")?;
        writeln!(f, "        atom:bufferType atom:Sequence ;")?;
        writeln!(f, "        atom:supports time:Position ;")?;
        writeln!(f, "        lv2:designation lv2:control ;")?;
        writeln!(f, "        lv2:index {index} ;")?;
        writeln!(f, "        lv2:symbol \"notify_out\" ;")?;
        writeln!(f, "        lv2:name \"Notify Out\" ;")?;
        writeln!(f, "        rsz:minimumSize 4096 ;")?;
    }
    Ok(())
}

/// Output control port for a `#[meter]` slot. Hosts read these each
/// process block and forward updates to the UI (wired via
/// `ui:portNotification` in the manifest).
fn emit_meter_port(
    f: &mut fs::File,
    index: u32,
    slot: usize,
    id: u32,
) -> std::io::Result<()> {
    writeln!(f, "        a lv2:OutputPort, lv2:ControlPort ;")?;
    writeln!(f, "        lv2:index {index} ;")?;
    writeln!(f, "        lv2:symbol \"meter_{slot}\" ;")?;
    writeln!(f, "        lv2:name \"Meter {}\" ;", slot + 1)?;
    writeln!(f, "        lv2:minimum 0.0 ;")?;
    writeln!(f, "        lv2:maximum 1.0 ;")?;
    writeln!(f, "        lv2:default 0.0 ;")?;
    // Hint to hosts that this is a read-only display port rather than
    // something they should automate or draw on the panel strip.
    writeln!(f, "        lv2:portProperty pprop:notOnGUI ;")?;
    // Round-trip the truce meter ID so a future UI extension could map
    // it back to `P::ParamId`.
    writeln!(f, "        rdfs:comment \"truce meter id {id}\" ;")?;
    Ok(())
}

fn emit_control_port(f: &mut fs::File, index: u32, p: &ParamInfo) -> std::io::Result<()> {
    writeln!(f, "        a lv2:InputPort, lv2:ControlPort ;")?;
    writeln!(f, "        lv2:index {index} ;")?;
    writeln!(
        f,
        "        lv2:symbol \"{}\" ;",
        param_symbol(p.id, p.name)
    )?;
    writeln!(f, "        lv2:name \"{}\" ;", escape_turtle(p.name))?;
    writeln!(f, "        lv2:minimum {} ;", p.range.min())?;
    writeln!(f, "        lv2:maximum {} ;", p.range.max())?;
    writeln!(f, "        lv2:default {} ;", p.default_plain)?;
    if let Some(unit) = lv2_unit(&p.unit) {
        writeln!(f, "        units:unit units:{unit} ;")?;
    }
    match p.range {
        ParamRange::Discrete { .. } => {
            writeln!(f, "        lv2:portProperty lv2:integer ;")?;
        }
        ParamRange::Enum { .. } => {
            writeln!(f, "        lv2:portProperty lv2:integer, lv2:enumeration ;")?;
        }
        _ => {}
    }
    Ok(())
}

fn param_symbol(id: u32, name: &str) -> String {
    // LV2 symbols must be [A-Za-z_][A-Za-z0-9_]*. Sanitize from the display
    // name; fall back to p_<id> if nothing usable remains.
    let mut s = String::with_capacity(name.len() + 2);
    for (i, c) in name.chars().enumerate() {
        if c.is_ascii_alphanumeric() || c == '_' {
            s.push(c);
        } else if c == ' ' || c == '-' {
            s.push('_');
        }
        if i == 0 && !c.is_ascii_alphabetic() && c != '_' {
            s.insert(0, 'p');
        }
    }
    if s.is_empty() || !s.chars().next().map_or(false, |c| c.is_ascii_alphabetic() || c == '_') {
        return format!("p_{id}");
    }
    s
}

fn category_as_lv2(cat: PluginCategory) -> &'static str {
    match cat {
        PluginCategory::Instrument => "lv2:InstrumentPlugin",
        PluginCategory::Effect => "lv2:Plugin",
        PluginCategory::NoteEffect => "lv2:MIDIPlugin",
        PluginCategory::Analyzer => "lv2:AnalyserPlugin",
        PluginCategory::Tool => "lv2:UtilityPlugin",
    }
}

fn lv2_unit(u: &ParamUnit) -> Option<&'static str> {
    Some(match u {
        ParamUnit::Db => "db",
        ParamUnit::Hz => "hz",
        ParamUnit::Milliseconds => "ms",
        ParamUnit::Seconds => "s",
        ParamUnit::Percent => "pc",
        ParamUnit::Semitones => "semitone12TET",
        ParamUnit::Pan | ParamUnit::None => return None,
    })
}

fn escape_turtle(s: &str) -> String {
    // Minimal escape for string literals (no newlines in names expected).
    s.replace('\\', "\\\\").replace('"', "\\\"")
}
