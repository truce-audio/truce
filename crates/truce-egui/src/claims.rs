//! Key claims: which keys and modifier chords the editor's UI handles.
//!
//! The desktop editor (`editor.rs`) returns every key the UI hasn't claimed
//! to the host, so DAW shortcuts keep working while the plug-in window is
//! focused. Kept out of `editor.rs` so the API exists on every platform: a
//! UI shared with iOS can call [`claim_keys`] / [`claim_shortcuts`]
//! unconditionally. On iOS they have no effect - that editor doesn't
//! forward keys to a host.

/// The keys the editor's UI handles on the next key event. Keyed in
/// egui's temp store by this private type (not a string), so only
/// [`claim_keys`] can write the slot and no foreign value can collide.
#[derive(Clone, Default)]
struct ClaimedKeys(Vec<egui::Key>);

/// Declare keys this editor handles this frame. Call it each frame from
/// your egui UI with the keys you use (a step sequencer's arrows, preset
/// number keys, `egui::Key::Space` for a transport toggle, ...). Calls
/// within a frame add up, so each panel can claim its own keys.
/// Every *unclaimed* key is returned to the host so its own commands keep
/// running while the plug-in window is focused. Two cases bypass this
/// list: a key held with a Cmd / Ctrl / Alt modifier always goes to the
/// host unless you claim it via [`claim_shortcuts`]), and a
/// focused text field keeps every key. Pass [`egui::Key::ALL`] to claim
/// everything.
///
/// A claim covers only the frame it was made in: stop calling this and the
/// keys go back to the host on the next rendered frame. So claim
/// unconditionally for keys you always handle, and conditionally (inside
/// the `if` that draws the panel owning them) for keys you only sometimes
/// handle - there's no "unclaim" to pair with it.
///
/// ```ignore
/// truce_egui::claim_keys(ui.ctx(), &[egui::Key::ArrowLeft, egui::Key::ArrowRight]);
/// ```
pub fn claim_keys(ctx: &egui::Context, keys: &[egui::Key]) {
    ctx.data_mut(|data| {
        let claimed = &mut data
            .get_temp_mut_or_default::<ClaimedKeys>(egui::Id::NULL)
            .0;
        add_unique(claimed, keys);
    });
}

/// Append the items of `new` not already in `claimed`. Deduped so repeated
/// claims can't grow the list - on iOS nothing clears it between frames.
fn add_unique<T: Copy + PartialEq>(claimed: &mut Vec<T>, new: &[T]) {
    for &item in new {
        if !claimed.contains(&item) {
            claimed.push(item);
        }
    }
}

// Read only by the desktop editor; iOS has no host to return keys to.
#[cfg_attr(target_os = "ios", allow(dead_code))]
pub(crate) fn claimed_keys(ctx: &egui::Context) -> Vec<egui::Key> {
    ctx.data(|data| {
        data.get_temp::<ClaimedKeys>(egui::Id::NULL)
            .map_or_default(|claimed| claimed.0)
    })
}

/// The modifier chords the editor's UI handles on the next key event.
/// A separate temp slot from [`ClaimedKeys`] (distinct type at the same
/// id), so [`claim_keys`] and [`claim_shortcuts`] don't clobber each other.
#[derive(Clone, Default)]
struct ClaimedShortcuts(Vec<egui::KeyboardShortcut>);

/// Like [`claim_keys`], but for modifier chords (Cmd+Z, Ctrl+Shift+A, ...).
/// Call it each frame with the chords your editor handles (calls within a
/// frame add up); an unclaimed chord returns to the host so its own
/// shortcuts keep working. Modifier
/// matching is egui's [`egui::Modifiers::matches_exact`], so
/// `Modifiers::COMMAND` is Cmd on macOS and Ctrl elsewhere, and `alt` /
/// `shift` must match exactly.
///
/// Per-frame like [`claim_keys`]: a chord stays claimed only while the UI
/// keeps declaring it.
///
/// ```ignore
/// use egui::{Key, KeyboardShortcut, Modifiers};
/// truce_egui::claim_shortcuts(ui.ctx(), &[KeyboardShortcut::new(Modifiers::COMMAND, Key::Z)]);
/// ```
pub fn claim_shortcuts(ctx: &egui::Context, shortcuts: &[egui::KeyboardShortcut]) {
    ctx.data_mut(|data| {
        let claimed = &mut data
            .get_temp_mut_or_default::<ClaimedShortcuts>(egui::Id::NULL)
            .0;
        add_unique(claimed, shortcuts);
    });
}

// Read only by the desktop editor; iOS has no host to return keys to.
#[cfg_attr(target_os = "ios", allow(dead_code))]
pub(crate) fn claimed_shortcuts(ctx: &egui::Context) -> Vec<egui::KeyboardShortcut> {
    ctx.data(|data| {
        data.get_temp::<ClaimedShortcuts>(egui::Id::NULL)
            .map_or_default(|claimed| claimed.0)
    })
}

/// Drop both claim slots. Called once per rendered frame, just before the
/// UI runs, so a claim covers only the frame that declared it - see the
/// call site in `editor.rs`'s `run_frame`.
// Read only by the desktop editor; iOS has no host to return keys to.
#[cfg_attr(target_os = "ios", allow(dead_code))]
pub(crate) fn clear_claims(ctx: &egui::Context) {
    ctx.data_mut(|data| {
        data.remove::<ClaimedKeys>(egui::Id::NULL);
        data.remove::<ClaimedShortcuts>(egui::Id::NULL);
    });
}
