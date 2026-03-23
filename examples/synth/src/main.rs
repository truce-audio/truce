use truce_example_synth::{gui_layout, Synth};

fn main() {
    truce_standalone::run_with_gui::<Synth>(gui_layout());
}
