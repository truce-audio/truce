//! Compile-time LV2 TTL renderer.
//!
//! `truce-derive` calls into this from inside `derive(Params)` to write
//! `manifest.ttl` + `plugin.ttl` next to the cargo target tree, before
//! `cargo truce package` even runs. cargo-truce then just copies those
//! files into the produced `.lv2` bundle alongside the cross-built
//! `.so`. No dlopen is involved at any point — that's what makes
//! cross-arch LV2 tarballs viable.
//!
//! The TTL string-building here mirrors the runtime path in
//! `truce-lv2/src/ttl.rs`. Inputs are plain `String`s / enums /
//! `Vec<Lv2Param>`s rather than the `truce-core` / `truce-params`
//! types so this module stays in `truce-build` (a tiny dep that
//! `truce-derive` already pulls in).

use std::collections::HashSet;
use std::fmt::Write as _;

/// Top-level inputs to the TTL renderer.
#[derive(Debug, Clone)]
pub struct Lv2Bundle {
    pub plugin_name: String,
    pub vendor: String,
    pub url: String,
    /// Plugin URI. Caller computes (typically `<vendor.url>/lv2/<clap_id>`
    /// or `urn:truce:<clap_id>` when `url` is empty) — kept explicit so
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

/// Layout indices used to lay out LV2 ports in the TTL.
struct Layout {
    audio_in: u32,
    audio_out: u32,
    num_params: u32,
    num_meters: u32,
    accepts_midi_in: bool,
    has_midi_out: bool,
}

impl Layout {
    fn audio_in_start(&self) -> u32 {
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
        accepts_midi_in: bundle.accepts_midi_in,
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
    f
}

fn emit_port(f: &mut String, index: u32, b: &Lv2Bundle, layout: &Layout, param_symbols: &[String]) {
    let _ = writeln!(f);
    if index < layout.audio_out_start() {
        let ch = index - layout.audio_in_start();
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
        // `midi:MidiEvent` support too — Reaper (and others) only route
        // transport to ports that also accept MIDI. Effects ignore any
        // arriving MIDI bytes.
        let _ = writeln!(f, "        a lv2:InputPort, atom:AtomPort ;");
        let _ = writeln!(f, "        atom:bufferType atom:Sequence ;");
        let _ = writeln!(f, "        atom:supports midi:MidiEvent, time:Position ;");
        if !layout.accepts_midi_in {
            let _ = writeln!(f, "        lv2:designation lv2:control ;");
        }
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
    let _ = writeln!(f, "        lv2:default {} ;", p.default_plain);
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
            "        rdfs:comment \"truce IS_BYPASS — `1` is bypassed (inverse of lv2:enabled)\" ;"
        );
    }
    if p.flags.readonly {
        let _ = writeln!(f, "        lv2:portProperty pprop:notAutomatic ;");
    }
    if p.flags.hidden {
        let _ = writeln!(f, "        lv2:portProperty pprop:notOnGUI ;");
    }
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
