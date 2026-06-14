//! Compile-time LV2 TTL renderer.
//!
//! `truce-derive` calls into this from inside `derive(Params)` to write
//! `manifest.ttl` + `plugin.ttl` next to the cargo target tree, before
//! `cargo truce package` even runs. cargo-truce then just copies those
//! files into the produced `.lv2` bundle alongside the cross-built
//! `.so`. No dlopen is involved at any point, which is what makes
//! cross-arch LV2 tarballs viable.
//!
//! Inputs are plain `String`s / enums / `Vec<Lv2Param>`s rather than
//! the `truce-core` / `truce-params` types so this module stays in
//! `truce-build` (a tiny dep that `truce-derive` already pulls in).

use base64::Engine as _;
use std::collections::HashSet;
use std::fmt::Write as _;

/// The plugin's LV2 URI: the identity LV2 hosts persist in saved
/// sessions. Both the compile-time manifest writer and the runtime
/// descriptor lookup call this so a host can rediscover the plugin
/// under the same URI it persisted.
///
/// Format: `<vendor_url>/lv2/<bundle_id>`, or `urn:truce:<bundle_id>`
/// when `vendor_url` is empty (lilv's reference loader prefers an
/// `http://` URI but accepts the URN fallback for projects that
/// haven't picked a public URL yet).
#[must_use]
pub fn plugin_uri(vendor_url: &str, bundle_id: &str) -> String {
    if vendor_url.is_empty() {
        return format!("urn:truce:{bundle_id}");
    }
    format!("{}/lv2/{}", vendor_url.trim_end_matches('/'), bundle_id)
}

/// Convention: `<plugin_uri>#ui`. Same single-source-of-truth posture
/// as [`plugin_uri`].
#[must_use]
pub fn ui_uri(vendor_url: &str, bundle_id: &str) -> String {
    format!("{}#ui", plugin_uri(vendor_url, bundle_id))
}

/// Top-level inputs to the TTL renderer.
#[derive(Debug, Clone)]
pub struct Lv2Bundle {
    pub plugin_name: String,
    pub vendor: String,
    pub url: String,
    /// Plugin URI. Caller computes (typically `<vendor.url>/lv2/<clap_id>`
    /// or `urn:truce:<clap_id>` when `url` is empty) - kept explicit so
    /// the URI scheme stays in one place at the call site.
    pub uri: String,
    /// UI URI. Convention is `<plugin_uri>#ui`.
    pub ui_uri: String,
    pub category: Lv2Category,
    /// Audio inputs / outputs in the default bus layout. The TTL
    /// renderer only writes the first bus layout's channel counts; the
    /// rest of the configuration matrix is for the runtime to map.
    pub audio_in: u32,
    pub audio_out: u32,
    pub accepts_midi_in: bool,
    pub has_midi_out: bool,
    pub params: Vec<Lv2Param>,
    pub meter_ids: Vec<u32>,
    /// Whether the plugin ships a UI. Drives the `ui:ui <…>` line and
    /// the manifest's `<ui_uri>` block.
    pub has_ui: bool,
}

#[derive(Debug, Clone, Copy)]
pub enum Lv2Category {
    Instrument,
    Effect,
    NoteEffect,
    Analyzer,
    Tool,
}

#[derive(Debug, Clone)]
pub struct Lv2Param {
    pub id: u32,
    pub name: String,
    pub default_plain: f64,
    pub range: Lv2Range,
    pub unit: Lv2Unit,
    pub flags: Lv2Flags,
}

#[derive(Debug, Clone, Copy)]
pub enum Lv2Range {
    Linear { min: f64, max: f64 },
    Logarithmic { min: f64, max: f64 },
    Discrete { min: f64, max: f64 },
    Enum { count: u32 },
}

#[derive(Debug, Clone, Copy)]
pub enum Lv2Unit {
    None,
    Db,
    Hz,
    Milliseconds,
    Seconds,
    Percent,
    Semitones,
    Pan,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Lv2Flags {
    pub is_bypass: bool,
    pub readonly: bool,
    pub hidden: bool,
}

impl Lv2Range {
    fn min(&self) -> f64 {
        match *self {
            Lv2Range::Linear { min, .. }
            | Lv2Range::Logarithmic { min, .. }
            | Lv2Range::Discrete { min, .. } => min,
            Lv2Range::Enum { .. } => 0.0,
        }
    }
    fn max(&self) -> f64 {
        match *self {
            Lv2Range::Linear { max, .. }
            | Lv2Range::Logarithmic { max, .. }
            | Lv2Range::Discrete { max, .. } => max,
            Lv2Range::Enum { count } => f64::from(count.saturating_sub(1)),
        }
    }
}

impl Lv2Param {
    /// The `lv2:default` to emit, clamped into `[min, max]`. LV2 hosts
    /// (REAPER's lilv loader in particular) reject a port whose default
    /// falls outside its declared range, so an unresolved enum count or
    /// a stray `#[param(default = ...)]` can never produce an
    /// unloadable plugin - it just opens at the clamped value.
    fn clamped_default(&self) -> f64 {
        self.default_plain.clamp(self.range.min(), self.range.max())
    }
}

/// Layout indices used to lay out LV2 ports in the TTL.
struct Layout {
    audio_in: u32,
    audio_out: u32,
    num_params: u32,
    num_meters: u32,
    has_midi_out: bool,
}

impl Layout {
    fn audio_in_start() -> u32 {
        0
    }
    fn audio_out_start(&self) -> u32 {
        self.audio_in
    }
    fn control_start(&self) -> u32 {
        self.audio_in + self.audio_out
    }
    fn meter_start(&self) -> u32 {
        self.control_start() + self.num_params
    }
    fn atom_in_port(&self) -> u32 {
        self.meter_start() + self.num_meters
    }
    fn midi_out_port(&self) -> Option<u32> {
        if self.has_midi_out {
            Some(self.atom_in_port() + 1)
        } else {
            None
        }
    }
    fn notify_out_port(&self) -> u32 {
        self.midi_out_port().unwrap_or(self.atom_in_port()) + 1
    }
    fn total(&self) -> u32 {
        self.notify_out_port() + 1
    }
}

/// Returns `(manifest.ttl, plugin.ttl)` rendered for `bundle`.
/// `so_name` is the bundle-relative filename (e.g. `truce-gain.so`)
/// that ends up in `lv2:binary <…>`.
#[must_use]
pub fn render_ttls(bundle: &Lv2Bundle, so_name: &str) -> (String, String) {
    let layout = Layout {
        audio_in: bundle.audio_in,
        audio_out: bundle.audio_out,
        num_params: u32::try_from(bundle.params.len()).unwrap_or(u32::MAX),
        num_meters: u32::try_from(bundle.meter_ids.len()).unwrap_or(u32::MAX),
        has_midi_out: bundle.has_midi_out,
    };
    let manifest = render_manifest(bundle, &layout, "plugin.ttl", so_name);
    let plugin_ttl = render_plugin_ttl(bundle, &layout, so_name);
    (manifest, plugin_ttl)
}

fn render_manifest(b: &Lv2Bundle, layout: &Layout, ttl_basename: &str, so_name: &str) -> String {
    let mut f = String::new();
    let _ = writeln!(f, "@prefix lv2:  <http://lv2plug.in/ns/lv2core#> .");
    let _ = writeln!(f, "@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .");
    let _ = writeln!(f, "@prefix ui:   <http://lv2plug.in/ns/extensions/ui#> .");
    let _ = writeln!(f, "@prefix atom: <http://lv2plug.in/ns/ext/atom#> .");
    let _ = writeln!(f);
    let _ = writeln!(f, "<{}>", b.uri);
    let _ = writeln!(f, "    a lv2:Plugin ;");
    let _ = writeln!(f, "    lv2:binary <{so_name}> ;");
    let _ = writeln!(f, "    rdfs:seeAlso <{ttl_basename}> .");

    if b.has_ui {
        let _ = writeln!(f);
        let _ = writeln!(f, "<{}>", b.ui_uri);
        // UI type matches the build host's windowing layer. Hosts on
        // other OSes still discover the plugin (they just can't open the
        // editor); embedding the right `a ui:*UI` is what tells them the
        // editor type is supported on this platform.
        #[cfg(target_os = "macos")]
        let _ = writeln!(f, "    a ui:CocoaUI ;");
        #[cfg(target_os = "windows")]
        let _ = writeln!(f, "    a ui:WindowsUI ;");
        #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
        let _ = writeln!(f, "    a ui:X11UI ;");
        let _ = writeln!(f, "    ui:binary <{so_name}> ;");

        // Subscribe the UI to the DSP's notify-out port.
        let _ = writeln!(f, "    ui:portNotification [");
        let _ = writeln!(f, "        ui:plugin <{}> ;", b.uri);
        let _ = writeln!(f, "        lv2:symbol \"notify_out\" ;");
        let _ = writeln!(f, "        ui:protocol atom:eventTransfer ;");
        let _ = writeln!(f, "        ui:notifyType atom:Object");
        let _ = writeln!(f, "    ] ;");
        for slot in 0..layout.num_meters {
            let _ = writeln!(f, "    ui:portNotification [");
            let _ = writeln!(f, "        ui:plugin <{}> ;", b.uri);
            let _ = writeln!(f, "        lv2:symbol \"meter_{slot}\" ;");
            let _ = writeln!(f, "        ui:protocol ui:floatProtocol");
            let _ = writeln!(f, "    ] ;");
        }
        let _ = writeln!(f, "    .");
    }
    f
}

fn render_plugin_ttl(b: &Lv2Bundle, layout: &Layout, so_name: &str) -> String {
    let symbols = resolve_param_symbols(&b.params);
    let mut f = String::new();
    let _ = writeln!(f, "@prefix lv2:   <http://lv2plug.in/ns/lv2core#> .");
    let _ = writeln!(
        f,
        "@prefix rdfs:  <http://www.w3.org/2000/01/rdf-schema#> ."
    );
    let _ = writeln!(f, "@prefix doap:  <http://usefulinc.com/ns/doap#> .");
    let _ = writeln!(f, "@prefix foaf:  <http://xmlns.com/foaf/0.1/> .");
    let _ = writeln!(
        f,
        "@prefix units: <http://lv2plug.in/ns/extensions/units#> ."
    );
    let _ = writeln!(f, "@prefix atom:  <http://lv2plug.in/ns/ext/atom#> .");
    let _ = writeln!(f, "@prefix midi:  <http://lv2plug.in/ns/ext/midi#> .");
    let _ = writeln!(f, "@prefix time:  <http://lv2plug.in/ns/ext/time#> .");
    let _ = writeln!(
        f,
        "@prefix rsz:   <http://lv2plug.in/ns/ext/resize-port#> ."
    );
    let _ = writeln!(f, "@prefix state: <http://lv2plug.in/ns/ext/state#> .");
    let _ = writeln!(f, "@prefix ui:    <http://lv2plug.in/ns/extensions/ui#> .");
    let _ = writeln!(f, "@prefix pprop: <http://lv2plug.in/ns/ext/port-props#> .");
    // LV2 1.18+ parameter API: each plugin parameter becomes an
    // `lv2:Parameter` declared as `patch:writable` from the input
    // atom port, and hosts deliver per-sample updates as
    // `patch:Set` messages in the atom sequence (each event's
    // `time_frames` is the within-block sample offset). This sits
    // alongside the legacy `lv2:ControlPort` path for hosts that
    // haven't migrated.
    let _ = writeln!(f, "@prefix patch: <http://lv2plug.in/ns/ext/patch#> .");
    let _ = writeln!(f);

    let category = category_as_lv2(b.category);
    let _ = writeln!(f, "<{}>", b.uri);
    if category == "lv2:Plugin" {
        let _ = writeln!(f, "    a lv2:Plugin ;");
    } else {
        let _ = writeln!(f, "    a lv2:Plugin, {category} ;");
    }
    let _ = writeln!(f, "    doap:name \"{}\" ;", escape_turtle(&b.plugin_name));
    let _ = writeln!(f, "    doap:maintainer [");
    let _ = writeln!(
        f,
        "        a foaf:Person ; foaf:name \"{}\"",
        escape_turtle(&b.vendor)
    );
    if !b.url.is_empty() {
        let _ = writeln!(f, "        ; foaf:homepage <{}>", b.url);
    }
    let _ = writeln!(f, "    ] ;");
    let _ = writeln!(f, "    lv2:binary <{so_name}> ;");
    let _ = writeln!(f, "    lv2:extensionData state:interface ;");
    if b.has_ui {
        let _ = writeln!(f, "    ui:ui <{}> ;", b.ui_uri);
    }

    // Advertise every parameter as `patch:writable` so hosts that
    // speak the LV2 1.18+ patch API deliver updates as `patch:Set`
    // messages on the input atom port (with sample-accurate timing
    // via the atom event's `time_frames`). The legacy control-port
    // path remains for hosts that haven't migrated.
    for p in &b.params {
        let _ = writeln!(f, "    patch:writable <{}#p_{}> ;", b.uri, p.id);
    }

    let total_ports = layout.total();
    if total_ports > 0 {
        let _ = write!(f, "    lv2:port");
        for i in 0..total_ports {
            let sep = if i == 0 { " " } else { ",\n        " };
            let _ = write!(f, "{sep}[");
            emit_port(&mut f, i, b, layout, &symbols);
            let _ = write!(f, "    ]");
        }
        let _ = writeln!(f, " .");
    } else {
        let _ = writeln!(f, "    .");
    }

    // One `lv2:Parameter` block per truce param. Each carries the
    // human-readable label, an `rdfs:range` of `atom:Float` so hosts
    // know to send `patch:Set` payloads as f32 atoms, and the
    // min/max/default in plain units. The Parameter URI is
    // `<plugin_uri>#p_<id>`; the runtime interns the same string at
    // instantiate time to build its property-URID → param-id table.
    for p in &b.params {
        let _ = writeln!(f);
        let _ = writeln!(f, "<{}#p_{}>", b.uri, p.id);
        let _ = writeln!(f, "    a lv2:Parameter ;");
        let _ = writeln!(f, "    rdfs:label \"{}\" ;", escape_turtle(&p.name));
        let _ = writeln!(f, "    rdfs:range atom:Float ;");
        let _ = writeln!(f, "    lv2:minimum {} ;", p.range.min());
        let _ = writeln!(f, "    lv2:maximum {} ;", p.range.max());
        let _ = writeln!(f, "    lv2:default {} ;", p.clamped_default());
        if let Some(unit) = lv2_unit(p.unit) {
            let _ = writeln!(f, "    units:unit units:{unit} ;");
        }
        match p.range {
            Lv2Range::Discrete { .. } => {
                let _ = writeln!(f, "    lv2:portProperty lv2:integer ;");
            }
            Lv2Range::Enum { .. } => {
                let _ = writeln!(f, "    lv2:portProperty lv2:integer, lv2:enumeration ;");
            }
            _ => {}
        }
        let _ = writeln!(f, "    .");
    }

    f
}

fn emit_port(f: &mut String, index: u32, b: &Lv2Bundle, layout: &Layout, param_symbols: &[String]) {
    let _ = writeln!(f);
    if index < layout.audio_out_start() {
        let ch = index - Layout::audio_in_start();
        let _ = writeln!(f, "        a lv2:InputPort, lv2:AudioPort ;");
        let _ = writeln!(f, "        lv2:index {index} ;");
        let _ = writeln!(f, "        lv2:symbol \"in_{ch}\" ;");
        let _ = writeln!(f, "        lv2:name \"Audio In {}\" ;", ch + 1);
    } else if index < layout.control_start() {
        let ch = index - layout.audio_out_start();
        let _ = writeln!(f, "        a lv2:OutputPort, lv2:AudioPort ;");
        let _ = writeln!(f, "        lv2:index {index} ;");
        let _ = writeln!(f, "        lv2:symbol \"out_{ch}\" ;");
        let _ = writeln!(f, "        lv2:name \"Audio Out {}\" ;", ch + 1);
    } else if index < layout.meter_start() {
        let slot = (index - layout.control_start()) as usize;
        let p = &b.params[slot];
        emit_control_port(f, index, p, &param_symbols[slot]);
    } else if index < layout.meter_start() + layout.num_meters {
        let slot = (index - layout.meter_start()) as usize;
        let id = b.meter_ids[slot];
        emit_meter_port(f, index, slot, id);
    } else if index == layout.atom_in_port() {
        // Input atom port. Always declared so hosts deliver
        // `time:Position` to every plugin type. We always advertise
        // `midi:MidiEvent` support too - Reaper (and others) only route
        // transport to ports that also accept MIDI. Effects ignore any
        // arriving MIDI bytes.
        let _ = writeln!(f, "        a lv2:InputPort, atom:AtomPort ;");
        let _ = writeln!(f, "        atom:bufferType atom:Sequence ;");
        // `patch:Message` lets hosts deliver `patch:Set` parameter
        // updates here with sample-accurate timing - see the
        // `patch:writable` block above for the per-param URI list.
        let _ = writeln!(
            f,
            "        atom:supports midi:MidiEvent, time:Position, patch:Message ;"
        );
        // The single atom input is the plugin's control/event input, so
        // designate it `lv2:control` for EVERY plugin type. The LV2 atom
        // spec defines this designation as "which port MIDI should be
        // sent to" - omitting it on instruments (the previous
        // `!accepts_midi_in` guard) left synths with no designated MIDI
        // input, which REAPER rejects when scanning an `lv2:InstrumentPlugin`.
        // Effects already carried it; this just extends it to instruments
        // and note effects.
        let _ = writeln!(f, "        lv2:designation lv2:control ;");
        let _ = writeln!(f, "        lv2:index {index} ;");
        let _ = writeln!(f, "        lv2:symbol \"midi_in\" ;");
        let _ = writeln!(f, "        lv2:name \"MIDI In\" ;");
        let _ = writeln!(f, "        rsz:minimumSize 4096 ;");
    } else if Some(index) == layout.midi_out_port() {
        let _ = writeln!(f, "        a lv2:OutputPort, atom:AtomPort ;");
        let _ = writeln!(f, "        atom:bufferType atom:Sequence ;");
        let _ = writeln!(f, "        atom:supports midi:MidiEvent ;");
        let _ = writeln!(f, "        lv2:index {index} ;");
        let _ = writeln!(f, "        lv2:symbol \"midi_out\" ;");
        let _ = writeln!(f, "        lv2:name \"MIDI Out\" ;");
    } else if index == layout.notify_out_port() {
        let _ = writeln!(f, "        a lv2:OutputPort, atom:AtomPort ;");
        let _ = writeln!(f, "        atom:bufferType atom:Sequence ;");
        let _ = writeln!(f, "        atom:supports time:Position ;");
        let _ = writeln!(f, "        lv2:designation lv2:control ;");
        let _ = writeln!(f, "        lv2:index {index} ;");
        let _ = writeln!(f, "        lv2:symbol \"notify_out\" ;");
        let _ = writeln!(f, "        lv2:name \"Notify Out\" ;");
        let _ = writeln!(f, "        rsz:minimumSize 4096 ;");
    }
}

fn emit_meter_port(f: &mut String, index: u32, slot: usize, id: u32) {
    let _ = writeln!(f, "        a lv2:OutputPort, lv2:ControlPort ;");
    let _ = writeln!(f, "        lv2:index {index} ;");
    let _ = writeln!(f, "        lv2:symbol \"meter_{slot}\" ;");
    let _ = writeln!(f, "        lv2:name \"Meter {}\" ;", slot + 1);
    let _ = writeln!(f, "        lv2:minimum 0.0 ;");
    let _ = writeln!(f, "        lv2:maximum 1.0 ;");
    let _ = writeln!(f, "        lv2:default 0.0 ;");
    let _ = writeln!(f, "        lv2:portProperty pprop:notOnGUI ;");
    let _ = writeln!(f, "        rdfs:comment \"truce meter id {id}\" ;");
}

fn emit_control_port(f: &mut String, index: u32, p: &Lv2Param, symbol: &str) {
    let _ = writeln!(f, "        a lv2:InputPort, lv2:ControlPort ;");
    let _ = writeln!(f, "        lv2:index {index} ;");
    let _ = writeln!(f, "        lv2:symbol \"{symbol}\" ;");
    let _ = writeln!(f, "        lv2:name \"{}\" ;", escape_turtle(&p.name));
    let _ = writeln!(f, "        lv2:minimum {} ;", p.range.min());
    let _ = writeln!(f, "        lv2:maximum {} ;", p.range.max());
    let _ = writeln!(f, "        lv2:default {} ;", p.clamped_default());
    if let Some(unit) = lv2_unit(p.unit) {
        let _ = writeln!(f, "        units:unit units:{unit} ;");
    }
    match p.range {
        Lv2Range::Discrete { .. } => {
            let _ = writeln!(f, "        lv2:portProperty lv2:integer ;");
        }
        Lv2Range::Enum { .. } => {
            let _ = writeln!(f, "        lv2:portProperty lv2:integer, lv2:enumeration ;");
        }
        Lv2Range::Logarithmic { .. } => {
            let _ = writeln!(f, "        lv2:portProperty pprop:logarithmic ;");
        }
        Lv2Range::Linear { .. } => {}
    }
    if p.flags.is_bypass {
        // LV2's `lv2:enabled` designation has inverted semantics:
        // `1` = active, `0` = bypassed. truce's IS_BYPASS is `1` =
        // bypassed; the comment warns hosts wiring host bypass.
        let _ = writeln!(f, "        lv2:designation lv2:enabled ;");
        let _ = writeln!(
            f,
            "        rdfs:comment \"truce IS_BYPASS - `1` is bypassed (inverse of lv2:enabled)\" ;"
        );
    }
    if p.flags.readonly {
        let _ = writeln!(f, "        lv2:portProperty pprop:notAutomatic ;");
    }
    if p.flags.hidden {
        let _ = writeln!(f, "        lv2:portProperty pprop:notOnGUI ;");
    }
}

/// Public, id-paired view of `resolve_param_symbols`: the exact
/// `lv2:symbol` the manifest assigns each control port, paired with its
/// truce param id, in declaration order. The derive macro persists this
/// to `lv2-meta/<pkg>/symbols.toml` (via
/// [`crate::presets::render_param_symbols`]) so the install-time preset
/// emitter can turn a preset's param ids into `lv2:port` / `pset:value`
/// entries that port-based hosts (REAPER) apply through the control
/// ports - which also refreshes the host's param display and notifies
/// the UI, unlike a `state:state`-only preset.
#[must_use]
pub fn resolved_param_symbols(params: &[Lv2Param]) -> Vec<(u32, String)> {
    params
        .iter()
        .map(|p| p.id)
        .zip(resolve_param_symbols(params))
        .collect()
}

fn resolve_param_symbols(params: &[Lv2Param]) -> Vec<String> {
    let mut out = Vec::with_capacity(params.len());
    let mut seen: HashSet<String> = HashSet::with_capacity(params.len());
    for p in params {
        let candidate = param_symbol_candidate(p.id, &p.name);
        let resolved = if seen.insert(candidate.clone()) {
            candidate
        } else {
            let fallback = format!("p_{}", p.id);
            seen.insert(fallback.clone());
            fallback
        };
        out.push(resolved);
    }
    out
}

fn param_symbol_candidate(id: u32, name: &str) -> String {
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
    if s.is_empty()
        || !s
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
    {
        return format!("p_{id}");
    }
    s
}

fn category_as_lv2(c: Lv2Category) -> &'static str {
    match c {
        Lv2Category::Instrument => "lv2:InstrumentPlugin",
        Lv2Category::Effect => "lv2:Plugin",
        Lv2Category::NoteEffect => "lv2:MIDIPlugin",
        Lv2Category::Analyzer => "lv2:AnalyserPlugin",
        Lv2Category::Tool => "lv2:UtilityPlugin",
    }
}

fn lv2_unit(u: Lv2Unit) -> Option<&'static str> {
    Some(match u {
        Lv2Unit::Db => "db",
        Lv2Unit::Hz => "hz",
        Lv2Unit::Milliseconds => "ms",
        Lv2Unit::Seconds => "s",
        Lv2Unit::Percent => "pc",
        Lv2Unit::Semitones => "semitone12TET",
        Lv2Unit::Pan | Lv2Unit::None => return None,
    })
}

fn escape_turtle(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// The URI of one factory preset: `<plugin_uri>#preset-<uuid>`.
/// Fragment-anchored on the plugin URI (rather than path-derived
/// like lilv's user presets) so the identity tracks the preset's
/// uuid, not its on-disk location.
#[must_use]
pub fn preset_uri(plugin_uri: &str, uuid: &str) -> String {
    format!("{plugin_uri}#preset-{uuid}")
}

/// Render the `manifest.ttl` entry for one factory preset. Appended
/// to the derive-emitted manifest at install time (the proc macro
/// renders the manifest without knowledge of the `presets/`
/// directory).
#[must_use]
pub fn render_preset_manifest_entry(plugin_uri: &str, uuid: &str, ttl_rel_path: &str) -> String {
    let mut f = String::new();
    let _ = writeln!(f);
    let _ = writeln!(f, "<{}>", preset_uri(plugin_uri, uuid));
    let _ = writeln!(f, "    a pset:Preset ;");
    let _ = writeln!(f, "    lv2:appliesTo <{plugin_uri}> ;");
    let _ = writeln!(f, "    rdfs:seeAlso <{ttl_rel_path}> .");
    f
}

/// Prefix block the preset manifest entries need. Appended once
/// before the first [`render_preset_manifest_entry`] (the derive-
/// emitted manifest doesn't declare `pset:`).
pub const PRESET_MANIFEST_PREFIXES: &str =
    "\n@prefix pset: <http://lv2plug.in/ns/ext/presets#> .\n";

/// Render one factory preset's TTL file.
///
/// The preset carries the same param values two ways, because hosts
/// apply LV2 presets differently:
/// - `state:state` - the canonical state envelope (the
///   `urn:truce:state-blob` chunk `save()` stores) as an
///   `xsd:base64Binary` literal. lilv hosts (Ardour, Carla, jalv) map
///   it back to an `atom:Chunk` and hand it to `restore()`.
/// - `lv2:port` / `pset:value` - the per-control-port plain values in
///   `port_values` (`(symbol, value)`, symbols from
///   [`resolved_param_symbols`]). Port-based hosts (REAPER) apply
///   presets through the control ports, which is the *only* path that
///   also refreshes the host's own param display and pushes
///   `port_event`s to the (separate-instance) UI. A `state:state`-only
///   preset left REAPER's editor showing stale values.
///
/// `port_values` may be empty (e.g. when symbols aren't available);
/// the preset then degrades to `state:state` only, which still works
/// on lilv hosts.
#[must_use]
pub fn render_preset_ttl(
    plugin_uri: &str,
    uuid: &str,
    label: &str,
    state_blob: &[u8],
    port_values: &[(String, f64)],
) -> String {
    let mut f = String::new();
    let _ = writeln!(f, "@prefix lv2:   <http://lv2plug.in/ns/lv2core#> .");
    let _ = writeln!(f, "@prefix pset:  <http://lv2plug.in/ns/ext/presets#> .");
    let _ = writeln!(
        f,
        "@prefix rdfs:  <http://www.w3.org/2000/01/rdf-schema#> ."
    );
    let _ = writeln!(f, "@prefix state: <http://lv2plug.in/ns/ext/state#> .");
    let _ = writeln!(f, "@prefix xsd:   <http://www.w3.org/2001/XMLSchema#> .");
    let _ = writeln!(f);
    let _ = writeln!(f, "<{}>", preset_uri(plugin_uri, uuid));
    let _ = writeln!(f, "    a pset:Preset ;");
    let _ = writeln!(f, "    lv2:appliesTo <{plugin_uri}> ;");
    let _ = writeln!(f, "    rdfs:label \"{}\" ;", escape_turtle(label));
    // Repeating the `lv2:port` predicate (one blank node per port) is
    // valid Turtle and how lilv itself serialises port presets.
    for (symbol, value) in port_values {
        let _ = writeln!(f, "    lv2:port [");
        let _ = writeln!(f, "        lv2:symbol \"{}\" ;", escape_turtle(symbol));
        let _ = writeln!(f, "        pset:value {}", fmt_pset_value(*value));
        let _ = writeln!(f, "    ] ;");
    }
    let _ = writeln!(f, "    state:state [");
    let _ = writeln!(
        f,
        "        <urn:truce:state-blob> \"{}\"^^xsd:base64Binary",
        base64::engine::general_purpose::STANDARD.encode(state_blob)
    );
    let _ = writeln!(f, "    ] .");
    f
}

/// Format a plain param value for a `pset:value` literal: always with
/// a decimal point so it reads as a number (not an `xsd:integer`),
/// matching how the control ports' `lv2:default` would be parsed.
fn fmt_pset_value(v: f64) -> String {
    if v.is_finite() && v.fract() == 0.0 {
        format!("{v:.1}")
    } else {
        format!("{v}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_uri_empty_url_falls_back_to_urn() {
        assert_eq!(plugin_uri("", "my-gain"), "urn:truce:my-gain");
    }

    #[test]
    fn plugin_uri_uses_vendor_url() {
        assert_eq!(
            plugin_uri("https://example.com", "my-gain"),
            "https://example.com/lv2/my-gain"
        );
    }

    #[test]
    fn plugin_uri_strips_trailing_slash() {
        assert_eq!(
            plugin_uri("https://example.com/", "my-gain"),
            "https://example.com/lv2/my-gain"
        );
        assert_eq!(
            plugin_uri("https://example.com///", "my-gain"),
            "https://example.com/lv2/my-gain"
        );
    }

    #[test]
    fn ui_uri_appends_ui_fragment() {
        assert_eq!(
            ui_uri("https://example.com", "my-gain"),
            "https://example.com/lv2/my-gain#ui"
        );
        assert_eq!(ui_uri("", "my-gain"), "urn:truce:my-gain#ui");
    }

    fn bundle(category: Lv2Category, accepts_midi_in: bool, params: Vec<Lv2Param>) -> Lv2Bundle {
        let (audio_in, audio_out) = match category {
            Lv2Category::Instrument | Lv2Category::NoteEffect => (0, 2),
            _ => (2, 2),
        };
        Lv2Bundle {
            plugin_name: "Test".into(),
            vendor: "Vendor".into(),
            url: "https://example.com".into(),
            uri: plugin_uri("https://example.com", "test"),
            ui_uri: ui_uri("https://example.com", "test"),
            category,
            audio_in,
            audio_out,
            accepts_midi_in,
            has_midi_out: false,
            params,
            meter_ids: vec![],
            has_ui: false,
        }
    }

    fn enum_param() -> Lv2Param {
        Lv2Param {
            id: 0,
            name: "Waveform".into(),
            default_plain: 1.0,
            range: Lv2Range::Enum { count: 4 },
            unit: Lv2Unit::None,
            flags: Lv2Flags::default(),
        }
    }

    /// The atom input port is the control/event input for EVERY plugin
    /// type, so it must carry `lv2:designation lv2:control` - including
    /// instruments, whose MIDI input is exactly the port the spec says
    /// this designation names. A regression here is what stopped REAPER
    /// from loading the LV2 synth.
    #[test]
    fn instrument_midi_input_is_designated_control() {
        let (_manifest, ttl) = render_ttls(&bundle(Lv2Category::Instrument, true, vec![]), "x.so");
        // Isolate the `midi_in` input atom port block and assert the
        // designation rides on it.
        let block = ttl
            .split("lv2:symbol \"midi_in\"")
            .next()
            .and_then(|head| head.rsplit("a lv2:InputPort, atom:AtomPort").next())
            .expect("midi_in input port present");
        assert!(
            block.contains("lv2:designation lv2:control"),
            "instrument midi_in must be designated lv2:control:\n{ttl}"
        );
    }

    #[test]
    fn effect_midi_input_is_designated_control() {
        let (_manifest, ttl) = render_ttls(&bundle(Lv2Category::Effect, false, vec![]), "x.so");
        assert!(ttl.contains("lv2:designation lv2:control"));
    }

    /// A range-less enum resolves to a concrete count upstream; here we
    /// pin that the rendered port is valid (`minimum <= default <= maximum`)
    /// so no host rejects it (the `enum(0)` bug rendered `maximum 0` with
    /// `default 1`).
    #[test]
    fn enum_port_default_within_range() {
        let (_m, ttl) = render_ttls(
            &bundle(Lv2Category::Effect, false, vec![enum_param()]),
            "x.so",
        );
        assert!(ttl.contains("lv2:maximum 3"), "enum(4) -> max 3:\n{ttl}");
        assert!(ttl.contains("lv2:default 1"));
        // And the clamp guards an out-of-range default.
        let mut p = enum_param();
        p.default_plain = 99.0;
        assert!((p.clamped_default() - 3.0).abs() < f64::EPSILON);
    }
}
