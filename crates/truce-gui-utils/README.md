# truce-gui-utils

Cross-backend host-side platform helpers for truce GUI backends.

## Overview

Small helper crate shared by the GUI backends that embed a child view
(wgpu- or CALayer-backed) into a DAW-provided parent window. It exists so
each backend (`truce-gui`, `truce-egui`, `truce-iced`, `truce-vizia`) does not
re-implement the same host-window quirks. No rendering, no widgets.

Currently macOS-only in effect. On Linux and Windows the host manages
child-window positioning natively, so the helpers compile to no-ops there.

## Key functions

- **`reanchor_to_superview_top`** -- pin an embedded `NSView`'s top edge to
  its superview's top across host-driven resizes (AppKit's autoresize math
  only runs on *parent* resize, so resizing the child alone drifts it
  off-anchor and clips the header row)
- **`reanchor_all_children_to_top`** -- the same, applied to every child of a
  given parent `NSView`
- **`should_skip_frame`** -- host-resize stability check; lets a backend drop
  a frame mid-resize rather than render against a transient surface size

All take a `raw_window_handle::RawWindowHandle` (or raw parent pointer) and
are no-ops on non-macOS targets, so callers can invoke them unconditionally.

Part of [truce](https://github.com/truce-audio/truce). [Docs](https://truce.audio/docs/).
