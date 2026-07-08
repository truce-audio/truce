//! Audible proof that MIDI port selects the lane, with no DAW in the
//! loop. Renders the same phrase twice - once on port 0, once on
//! port 1 - and writes a stereo WAV with port 0 in the left ear and
//! port 1 in the right. Left is the PORT 0 lane's patch, right is
//! PORT 1's; same notes, obviously different timbre = the wrapper's
//! `event.port` dispatch working end to end through real `process()`.
//!
//! Ignored by default (it writes a file and takes a moment). Run it
//! on demand:
//!
//! ```sh
//! cargo test -p truce-example-multiport --test audible_demo -- --ignored --nocapture
//! ```
//!
//! It prints the output path; open that WAV and listen on headphones.

// Fixture arithmetic on small, bounded values (block offsets, a
// 44.1 kHz rate, a handful of note counts) - the pedantic cast lints
// guard against real truncation, which none of these can hit.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]

use truce::prelude::*;
use truce_example_multiport::{Multiport, MultiportParams};

const SR: f64 = 44_100.0;
const BLOCK: usize = 128;
const CHANNELS: usize = 2;

/// One scripted note: absolute sample offset, note number, and how
/// long to hold it (in samples).
struct Note {
    at: usize,
    key: u8,
    len: usize,
}

/// Render `notes` on `port` across `total` samples, returning the
/// left output channel. Drives the real `PluginLogic::process` in
/// `BLOCK`-sized chunks, injecting each note's on/off at its
/// sample-accurate offset within the block it falls in.
fn render_port(port: u8, notes: &[Note], total: usize) -> Vec<f32> {
    let params = MultiportParams::new();
    let mut state = Multiport::init(&params);
    Multiport::reset(&mut state, &params, &AudioConfig::new(SR, BLOCK));

    let mut out = Vec::with_capacity(total);
    let transport = TransportInfo::default();

    let mut pos = 0;
    while pos < total {
        let this = BLOCK.min(total - pos);

        let mut events = EventList::default();
        for n in notes {
            if n.at >= pos && n.at < pos + this {
                events.push(note(port, 0x90, n.key, (n.at - pos) as u32));
            }
            let off = n.at + n.len;
            if off >= pos && off < pos + this {
                events.push(note(port, 0x80, n.key, (off - pos) as u32));
            }
        }
        events.ensure_sorted_by_offset();

        let no_inputs: Vec<&[f32]> = Vec::new();
        let mut l = vec![0.0f32; this];
        let mut r = vec![0.0f32; this];
        {
            let (a, b) = (&mut l[..], &mut r[..]);
            let mut outs: Vec<&mut [f32]> = vec![a, b];
            let mut buffer = AudioBuffer::from_slices_checked(&no_inputs, &mut outs, this);
            let mut out_ev = EventList::default();
            let mut ctx = ProcessContext::new(&transport, SR, this, &mut out_ev);
            Multiport::process(&mut state, &params, &mut buffer, &events, &mut ctx);
        }
        out.extend_from_slice(&l);
        pos += this;
    }
    out
}

fn note(port: u8, status: u8, key: u8, offset: u32) -> Event {
    let body = if status == 0x90 {
        EventBody::NoteOn {
            group: 0,
            channel: 0,
            note: key,
            velocity: 100,
        }
    } else {
        EventBody::NoteOff {
            group: 0,
            channel: 0,
            note: key,
            velocity: 0,
        }
    };
    Event::on_port(offset, port, body)
}

#[test]
#[ignore = "writes a WAV; run explicitly with --ignored"]
fn render_two_ports_to_stereo_wav() {
    // A little four-note arpeggio, one note every half second.
    let step = (SR * 0.5) as usize;
    let hold = (SR * 0.45) as usize;
    let keys = [60u8, 64, 67, 72];
    let phrase: Vec<Note> = keys
        .iter()
        .enumerate()
        .map(|(i, &key)| Note {
            at: i * step,
            key,
            len: hold,
        })
        .collect();
    let total = keys.len() * step;

    let left = render_port(0, &phrase, total);
    let right = render_port(1, &phrase, total);

    // The two lanes must actually differ - if this is silence-vs-
    // silence or identical, the demo proves nothing.
    let differ = left.iter().zip(&right).any(|(a, b)| (a - b).abs() > 1e-3);
    assert!(differ, "port 0 and port 1 rendered identically");

    let spec = hound::WavSpec {
        channels: CHANNELS as u16,
        sample_rate: SR as u32,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let path = std::env::temp_dir().join("truce-multiport-demo.wav");
    let mut wav = hound::WavWriter::create(&path, spec).expect("create wav");
    for (l, r) in left.iter().zip(&right) {
        wav.write_sample(*l).expect("write l");
        wav.write_sample(*r).expect("write r");
    }
    wav.finalize().expect("finalize wav");

    println!(
        "multiport demo written to {}\n  left ear  = PORT 0 lane (default sine)\n  right ear = PORT 1 lane (default saw)\nOpen it and listen on headphones.",
        path.display()
    );
}
