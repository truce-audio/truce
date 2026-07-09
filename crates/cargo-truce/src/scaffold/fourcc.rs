//! Plugin fourcc generation + collision resolution.
//!
//! Every plugin gets a unique 4-character ASCII code used as its AU
//! `kAudioUnitSubType_*` identifier and elsewhere in the host
//! ecosystem. The default code is derived from the plugin's
//! `--name` argument; collisions in a `--workspace` scaffold get
//! mutated until each plugin has a unique code.

use std::collections::{HashMap, HashSet};

use super::PluginSpec;

/// Generate a 4-character code from a plugin name using segment initials.
///
/// 1. Split on any non-alphanumeric (`-`, `_`, `.`, etc.), take the
///    first character (uppercased) of each segment.
/// 2. If fewer than 4 initials, backfill from the last segment's remaining
///    characters first (the differentiator), then earlier segments.
/// 3. Pad with 'X' if still short.
///
/// Always returns a 4-character ASCII string; on inputs with no
/// alphanumeric content (`""`, `"---"`) it falls through to all-`'X'`.
#[must_use]
pub fn to_fourcc(s: &str) -> String {
    // `filter_map` keeps the iterator chain total - no `unwrap` after
    // the `!seg.is_empty()` guard, no defensive panic on empty input.
    // `chars().next()` returns `None` only when the segment was empty,
    // which the split filter has already excluded.
    let segments: Vec<&str> = s
        .split(|c: char| !c.is_alphanumeric())
        .filter(|seg| !seg.is_empty())
        .collect();

    let mut code: Vec<char> = segments
        .iter()
        .filter_map(|seg| seg.chars().next())
        .flat_map(char::to_uppercase)
        .collect();

    if code.len() >= 4 {
        code.truncate(4);
        return code.into_iter().collect();
    }

    // Backfill from segments in reverse order (last segment = differentiator)
    let needed = 4 - code.len();
    let mut fill: Vec<char> = Vec::new();
    for seg in segments.iter().rev() {
        fill.extend(seg.chars().skip(1));
        if fill.len() >= needed {
            break;
        }
    }
    code.extend(fill.into_iter().take(needed));

    while code.len() < 4 {
        code.push('X');
    }

    code.into_iter().collect()
}

/// Assign collision-free fourcc codes to all plugins. When two plugins produce
/// the same code, the later one gets its last character replaced with '2'–'9',
/// then 'A'–'Z' until a unique code is found.
///
/// Returns `Err` only when 35+ plugins share the same 3-character
/// prefix and the suffix slots ('2'–'9', 'A'–'Z' = 34 distinct
/// characters) are exhausted. The caller (`scaffold`) should surface
/// this as a clean error - the user can rename one plugin to break the
/// prefix collision instead of getting a process panic mid-scaffold.
///
/// # Errors
///
/// Returns `Err(String)` naming the colliding plugin when the suffix
/// pool is exhausted (35+ plugins sharing one 3-char prefix).
pub fn resolve_fourccs(plugins: &[PluginSpec]) -> Result<HashMap<String, String>, String> {
    let mut assignments: HashMap<String, String> = HashMap::new();
    let mut used: HashSet<String> = HashSet::new();

    for p in plugins {
        let mut fc = to_fourcc(&p.name);
        if !used.contains(&fc) {
            used.insert(fc.clone());
            assignments.insert(p.name.clone(), fc);
            continue;
        }
        // Collision - mutate last character
        let base: String = fc.chars().take(3).collect();
        let mut resolved = false;
        for suffix in ('2'..='9').chain('A'..='Z') {
            let candidate = format!("{base}{suffix}");
            if !used.contains(&candidate) {
                fc = candidate;
                resolved = true;
                break;
            }
        }
        if !resolved {
            return Err(format!(
                "cannot resolve fourcc collision for '{}': 34+ plugins share \
                 the prefix '{}'. Rename one to break the collision.",
                p.name, base,
            ));
        }
        used.insert(fc.clone());
        assignments.insert(p.name.clone(), fc);
    }

    Ok(assignments)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scaffold::{PluginKind, Statefulness};

    // `resolve_fourccs` keys only off the plugin name, so these tests
    // don't care about kind or statefulness - one constructor keeps
    // the fixtures terse.
    fn spec(name: &str, kind: PluginKind) -> PluginSpec {
        PluginSpec {
            name: name.into(),
            kind,
            statefulness: Statefulness::Pure,
        }
    }

    // --- to_fourcc: segment-initials algorithm ---

    #[test]
    fn single_short_word() {
        assert_eq!(to_fourcc("gain"), "Gain");
    }

    #[test]
    fn snake_case_separator() {
        // Regression: only `-` was treated as a segment separator,
        // so `demo_effect` collapsed to a single 11-char run instead
        // of two segments. Now it produces "DE" + backfill.
        assert_eq!(to_fourcc("demo_effect"), "DEff");
    }

    #[test]
    fn single_long_word() {
        assert_eq!(to_fourcc("synth"), "Synt");
    }

    #[test]
    fn single_short_word_padded() {
        assert_eq!(to_fourcc("eq"), "EqXX");
    }

    #[test]
    fn multi_segment_uses_initials() {
        let fc = to_fourcc("delay-mono");
        // D from delay, M from mono, then backfill from "mono"
        assert_eq!(fc, "DMon");
    }

    #[test]
    fn multi_segment_differentiates_suffixes() {
        // These collided before the fix (both produced "Dela")
        assert_ne!(to_fourcc("delay-mono"), to_fourcc("delay-stereo"));
    }

    #[test]
    fn multi_segment_backfills_from_last() {
        assert_eq!(to_fourcc("delay-stereo"), "DSte");
    }

    #[test]
    fn four_plus_segments_truncated() {
        let fc = to_fourcc("a-b-c-d-e");
        assert_eq!(fc.len(), 4);
        assert_eq!(fc, "ABCD");
    }

    #[test]
    fn always_four_chars() {
        for name in ["a", "ab", "abc-d", "very-long-plugin-name"] {
            assert_eq!(to_fourcc(name).len(), 4, "failed for {name}");
        }
    }

    // --- resolve_fourccs: collision handling ---

    #[test]
    fn no_collision() {
        let plugins = vec![
            spec("gain", PluginKind::Effect),
            spec("synth", PluginKind::Instrument),
        ];
        let map = resolve_fourccs(&plugins).unwrap();
        assert_eq!(map["gain"], to_fourcc("gain"));
        assert_eq!(map["synth"], to_fourcc("synth"));
    }

    #[test]
    fn collision_produces_unique_codes() {
        // Two names that produce the same initials + backfill
        let plugins = vec![
            spec("aa", PluginKind::Effect),
            spec("ab", PluginKind::Effect),
        ];
        let map = resolve_fourccs(&plugins).unwrap();
        assert_ne!(map["aa"], map["ab"]);
        assert_eq!(map["aa"].len(), 4);
        assert_eq!(map["ab"].len(), 4);
    }

    #[test]
    fn three_way_collision_all_unique() {
        let plugins = vec![
            spec("soft-clip", PluginKind::Effect),
            spec("soft-comp", PluginKind::Effect),
            spec("soft-crush", PluginKind::Effect),
        ];
        let map = resolve_fourccs(&plugins).unwrap();
        let mut codes: Vec<&String> = map.values().collect();
        codes.sort();
        codes.dedup();
        assert_eq!(codes.len(), 3);
    }

    #[test]
    fn first_plugin_keeps_natural_code() {
        let plugins = vec![
            spec("soft-clip", PluginKind::Effect),
            spec("soft-comp", PluginKind::Effect),
        ];
        let map = resolve_fourccs(&plugins).unwrap();
        // First plugin should keep its natural code
        assert_eq!(map["soft-clip"], to_fourcc("soft-clip"));
    }
}
