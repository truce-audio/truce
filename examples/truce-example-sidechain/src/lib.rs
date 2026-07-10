//! Sidechain metering - the reference for wiring a separate sidechain
//! input bus.
//!
//! `bus_layouts()` declares a stereo **Main** input, a stereo
//! **Sidechain** input, and a stereo output. The host surfaces the
//! sidechain as its own routable bus (a `kBusType_Aux` bus on VST3), so
//! you can feed a different source into it. Inside `process`, channels
//! are flat-indexed across buses: `input(0)`/`(1)` is main L/R and
//! `input(2)`/`(3)` is the sidechain L/R.
//!
//! The plugin meters both the main and the sidechain input level, and a
//! **Mix** knob blends the two into the output - at 0 you hear the main
//! input, at 1 the sidechain - so the routing is easy to verify by both
//! eye (the meters) and ear (the blend).

use truce::prelude::*;
use truce_core::bus::{BusLayout, ChannelConfig};
use truce_gui::IntoLayoutEditor;
use truce_gui_types::layout::{GridLayout, knob, meter, widgets};

use SidechainParamsParamId as P;
use std::sync::Arc;

#[derive(Params)]
pub struct SidechainParams {
    #[param(
        name = "Mix",
        range = "linear(0, 1)",
        default = 0.0,
        unit = "%",
        smooth = "exp(10)"
    )]
    pub mix: FloatParam,

    #[meter]
    pub in_l: MeterSlot,
    #[meter]
    pub in_r: MeterSlot,
    #[meter]
    pub sc_l: MeterSlot,
    #[meter]
    pub sc_r: MeterSlot,
}

/// Stateless descriptor - metering + a block-constant blend keep nothing
/// between blocks.
pub struct Sidechain;

const IN_METERS: [P; 2] = [P::InL, P::InR];
const SC_METERS: [P; 2] = [P::ScL, P::ScR];

impl PurePluginLogic for Sidechain {
    type Params = SidechainParams;

    fn bus_layouts() -> Vec<BusLayout> {
        vec![
            BusLayout::new()
                .with_input("Main", ChannelConfig::Stereo)
                .with_sidechain_input("Sidechain", ChannelConfig::Stereo)
                .with_output("Main", ChannelConfig::Stereo),
        ]
    }

    fn process(
        params: &SidechainParams,
        buffer: &mut AudioBuffer,
        _events: &EventList,
        context: &mut ProcessContext,
    ) -> ProcessStatus {
        let n_in = buffer.num_input_channels();
        // The sidechain bus may be left unconnected; the wrapper then
        // hands its channels silence, so `has_sc` gates only the reads.
        let has_sc = n_in >= 4;

        // Peak-meter each input channel. Main is channels 0/1, sidechain
        // is 2/3 - the flat indices the wrapper maps the separate
        // sidechain bus into.
        let peak = |b: &[f32]| b.iter().fold(0.0f32, |m, &s| m.max(s.abs())).min(1.0);
        let in_l = if n_in > 0 { peak(buffer.input(0)) } else { 0.0 };
        let in_r = if n_in > 1 { peak(buffer.input(1)) } else { 0.0 };
        let sc_l = if has_sc { peak(buffer.input(2)) } else { 0.0 };
        let sc_r = if has_sc { peak(buffer.input(3)) } else { 0.0 };

        // Blend main and sidechain into the stereo output. Block-constant:
        // `read_after` advances the smoother by the whole block so the
        // exp(10) settling completes in ~10 ms rather than ~10 blocks.
        let wet = params.mix.read_after(buffer.num_samples());
        let dry = 1.0 - wet;
        for ch in 0..buffer.num_output_channels().min(2) {
            {
                let (main, out) = buffer.io_pair(ch, ch);
                for (o, &m) in out.iter_mut().zip(main) {
                    *o = m * dry;
                }
            }
            if has_sc {
                let (sc, out) = buffer.io_pair(ch + 2, ch);
                for (o, &s) in out.iter_mut().zip(sc) {
                    *o += s * wet;
                }
            }
        }

        context.set_meter(P::InL, in_l);
        context.set_meter(P::InR, in_r);
        context.set_meter(P::ScL, sc_l);
        context.set_meter(P::ScR, sc_r);
        ProcessStatus::Normal
    }

    fn editor(params: Arc<SidechainParams>) -> Box<dyn Editor> {
        // Side by side in separate columns so nothing overlaps: the
        // input meter, the sidechain meter, then the Mix knob. The meters
        // span two rows for height; the knob matches so it sits centered
        // alongside them.
        GridLayout::build(vec![widgets(vec![
            meter(&IN_METERS, "IN").at(0, 0).rows(2),
            meter(&SC_METERS, "SC").at(1, 0).rows(2),
            knob(P::Mix, "Mix").at(2, 0).rows(2),
        ])])
        .with_title("SIDECHAIN")
        .into_editor(&params)
    }
}

truce::plugin! {
    logic: Sidechain,
    params: SidechainParams,
}

truce::enable_rt_paranoid!();

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn info_is_valid() {
        truce_test::assert_valid_info::<Plugin>();
    }

    #[test]
    fn has_editor() {
        truce_test::assert_has_editor::<Plugin>();
    }

    #[test]
    fn state_round_trips() {
        truce_test::assert_state_round_trip::<Plugin>();
    }

    #[test]
    fn declares_a_separate_sidechain_bus() {
        let layout = <Sidechain as PurePluginLogic>::bus_layouts()
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(layout.inputs.len(), 2, "main + sidechain input buses");
        assert_eq!(layout.inputs[0].kind, truce_core::bus::BusKind::Main);
        assert_eq!(layout.inputs[1].kind, truce_core::bus::BusKind::Sidechain);
        assert_eq!(layout.outputs.len(), 1);
    }

    #[test]
    fn meters_are_per_channel() {
        use std::time::Duration;
        use truce_test::{InputSource, MeterCapture, MeterReadings, driver};
        // main L hot / main R silent; sidechain L silent / sidechain R hot.
        // Main and sidechain are independent driver sources - the sidechain
        // width auto-detects from the plugin's bus layout, so the run drives
        // all four input channels without a manual channel override.
        let frames = 4096;
        let main = vec![vec![1.0f32; frames], vec![0.0f32; frames]];
        let side = vec![vec![0.0f32; frames], vec![1.0f32; frames]];
        let result = driver!(Plugin)
            .duration(Duration::from_millis(20))
            .input(InputSource::Buffer(main))
            .sidechain(InputSource::Buffer(side))
            .capture_meters(MeterCapture::Final)
            .run();
        let MeterReadings::Final(meters) = result.meters else {
            panic!("expected final meters");
        };
        let get = |p: P| {
            let id: u32 = p.into();
            meters
                .iter()
                .find(|(mid, _)| *mid == id)
                .map_or(0.0, |(_, v)| *v)
        };
        assert!(get(P::InL) > 0.9, "in L should be hot: {}", get(P::InL));
        assert!(get(P::InR) < 0.1, "in R should be silent: {}", get(P::InR));
        assert!(get(P::ScL) < 0.1, "sc L should be silent: {}", get(P::ScL));
        assert!(get(P::ScR) > 0.9, "sc R should be hot: {}", get(P::ScR));
    }

    #[test]
    fn mix_knob_blends_sidechain_into_output() {
        use std::time::Duration;
        use truce_test::{InputSource, driver};
        // Main = DC 0.25, sidechain = DC 0.75 on both channels. At Mix=1 the
        // output is the sidechain; at Mix=0 it's the main. Proves the
        // sidechain bus reaches `process` and the blend routes it out.
        let frames = 2048;
        let main = || InputSource::Buffer(vec![vec![0.25f32; frames]; 2]);
        let side = || InputSource::Buffer(vec![vec![0.75f32; frames]; 2]);
        let tail = |mix: f64| {
            let out = driver!(Plugin)
                .duration(Duration::from_millis(40))
                .input(main())
                .sidechain(side())
                .set_param(P::Mix, mix)
                .run()
                .output;
            // Last settled sample of the left output channel.
            *out[0].last().unwrap()
        };
        let wet = tail(1.0);
        let dry = tail(0.0);
        assert!(
            (wet - 0.75).abs() < 0.01,
            "Mix=1 should pass sidechain: {wet}"
        );
        assert!((dry - 0.25).abs() < 0.01, "Mix=0 should pass main: {dry}");
    }

    #[test]
    fn process_is_allocation_free() {
        use std::time::Duration;
        use truce_test::{InputSource, assert_no_audio_alloc, driver};
        assert_no_audio_alloc(|| {
            driver!(Plugin)
                .duration(Duration::from_millis(30))
                .input(InputSource::Constant(0.3))
                .script(|s| {
                    s.set_param(P::Mix, 0.5);
                    s.wait_ms(10);
                })
                .run()
        });
    }
}
