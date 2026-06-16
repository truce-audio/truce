# Gain (egui, aspect-locked)

The [egui](https://github.com/emilk/egui) gain example with a 2:3
aspect-ratio lock: the editor calls `.aspect_ratio(Some((2, 3)))`, so
the host keeps width and height proportional on every resize edge. The
window size and min/max bounds all sit on 2:3 so the lock holds across
the whole range.

Honored by CLAP, VST3, AU v3, and the standalone host; VST2, LV2, and
AAX ignore aspect ratios.
