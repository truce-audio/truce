# truce-gui-utils

Host-side platform helpers shared by truce GUI backends.

## Overview

Small helper crate shared by the GUI backends that embed a child view
(wgpu- or CALayer-backed) into a DAW-provided parent window. It exists so
each backend (`truce-gui`, `truce-egui`, `truce-iced`, `truce-vizia`) does not
re-implement the same host-window quirks. No rendering, no widgets.

The re-anchoring helpers are macOS-only: on Linux and Windows the host
manages child-window positioning natively, so they compile to no-ops
there. `should_skip_frame` is implemented natively on both macOS and
Windows.

## Key functions

- **`reanchor_to_superview_top`** -- pin an embedded `NSView`'s top edge to
  its superview's top across host-driven resizes (AppKit's autoresize math
  only runs on *parent* resize, so resizing the child alone drifts it
  off-anchor and clips the header row)
- **`reanchor_all_children_to_top`** -- the same, applied to every child of a
  given parent `NSView`
- **`should_skip_frame`** -- host-resize stability check; lets a backend drop
  a frame mid-resize rather than render against a transient surface size

The re-anchoring helpers take a `raw_window_handle::RawWindowHandle` (or
raw parent pointer) and are no-ops off macOS, so callers can invoke them
unconditionally; `should_skip_frame` returns a real answer on macOS and
Windows and `false` elsewhere.

Part of [truce](https://github.com/truce-audio/truce). [Docs](https://truce.audio/docs/).
