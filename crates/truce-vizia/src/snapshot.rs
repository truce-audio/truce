//! Standalone vizia snapshot rendering.
//!
//! Opens a vizia window, captures the first rendered frame via
//! Skia's `Canvas::read_pixels`, saves to PNG, and exits.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use vizia::prelude::*;
use vizia::vg;

use crate::param_model::ParamModel;
use crate::theme;

/// Capture view — added to the end of the view tree to read back
/// the rendered frame from the Skia canvas.
struct FrameCapture {
    path: String,
    width: u32,
    height: u32,
    done: Arc<AtomicBool>,
}

impl View for FrameCapture {
    fn draw(&self, _cx: &mut DrawContext, canvas: &Canvas) {
        if self.done.load(Ordering::Relaxed) {
            return;
        }
        self.done.store(true, Ordering::Relaxed);

        let w = self.width as i32;
        let h = self.height as i32;

        let info = vg::ImageInfo::new(
            vg::ISize::new(w, h),
            vg::ColorType::RGBA8888,
            vg::AlphaType::Premul,
            None,
        );

        let row_bytes = self.width as usize * 4;
        let mut pixels = vec![0u8; row_bytes * self.height as usize];

        // read_pixels ignores canvas clip/transform — reads the full surface.
        let ok = canvas.read_pixels(
            &info,
            &mut pixels,
            row_bytes,
            vg::IPoint::new(0, 0),
        );

        if !ok {
            eprintln!("[truce-vizia] snapshot: read_pixels failed");
            return;
        }

        // Skia returns premultiplied RGBA. Un-premultiply for PNG.
        for chunk in pixels.chunks_exact_mut(4) {
            let a = chunk[3] as u16;
            if a > 0 && a < 255 {
                chunk[0] = ((chunk[0] as u16 * 255) / a).min(255) as u8;
                chunk[1] = ((chunk[1] as u16 * 255) / a).min(255) as u8;
                chunk[2] = ((chunk[2] as u16 * 255) / a).min(255) as u8;
            }
        }

        save_png(&self.path, &pixels, self.width, self.height);
        eprintln!(
            "[truce-vizia] snapshot saved: {} ({}x{})",
            self.path, self.width, self.height
        );

        // Exit immediately. vizia's event loop is event-driven and may
        // not fire on_idle again if there are no new events, so we can't
        // rely on a deferred exit.
        std::process::exit(0);
    }
}

/// Open a vizia window, render one frame, save a PNG snapshot, and exit.
///
/// `size` is (width, height) in logical pixels for the snapshot.
/// `path` is the output PNG path.
/// `app` is the UI builder closure (same as `ViziaEditor::new`).
///
/// This blocks until the snapshot is captured (typically one frame).
pub fn capture_snapshot(
    size: (u32, u32),
    path: &str,
    app: impl Fn(&mut Context) + Send + 'static,
) {
    let (w, h) = size;
    let path_owned = path.to_string();
    let done = Arc::new(AtomicBool::new(false));

    let noop_context = truce_core::editor::EditorContext {
        begin_edit: Arc::new(|_| {}),
        set_param: Arc::new(|_, _| {}),
        end_edit: Arc::new(|_| {}),
        request_resize: Arc::new(|_, _| false),
        get_param: Arc::new(|_| 0.0),
        get_param_plain: Arc::new(|_| 0.0),
        format_param: Arc::new(|_| String::from("0.0")),
        get_meter: Arc::new(|_| 0.0),
    };

    Application::new(move |cx| {
        theme::apply_default_theme(cx);
        ParamModel::new(noop_context.clone()).build(cx);


        app(cx);

        // Capture view — positioned over the full window, drawn last.
        FrameCapture {
            path: path_owned.clone(),
            width: w,
            height: h,
            done: done.clone(),
        }
        .build(cx, |_| {})
        .position_type(PositionType::Absolute)
        .left(Pixels(0.0))
        .top(Pixels(0.0))
        .width(Pixels(w as f32))
        .height(Pixels(h as f32));
    })
    .inner_size((w, h))
    .run();
}

/// Render and compare against a reference PNG snapshot.
///
/// On first run (no reference exists), saves the reference and returns.
/// On subsequent runs, compares pixel-by-pixel and panics if the diff
/// exceeds `max_diff_pixels`.
pub fn assert_snapshot(
    snapshot_dir: &str,
    name: &str,
    size: (u32, u32),
    max_diff_pixels: usize,
    app: impl Fn(&mut Context) + Send + 'static,
) {
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let root = manifest_dir.parent().unwrap().parent().unwrap();
    let dir = root.join(snapshot_dir);
    std::fs::create_dir_all(&dir).ok();

    let ref_path = dir.join(format!("{name}.png"));
    let tmp_path = dir.join(format!("{name}_TMP.png"));

    capture_snapshot(size, tmp_path.to_str().unwrap(), app);

    let (current_pixels, cur_w, cur_h) = load_png(&tmp_path);

    if !ref_path.exists() {
        std::fs::rename(&tmp_path, &ref_path).unwrap();
        eprintln!(
            "[truce-vizia] Reference created: {}",
            ref_path.display()
        );
        return;
    }

    let (ref_pixels, ref_w, ref_h) = load_png(&ref_path);
    std::fs::remove_file(&tmp_path).ok();

    assert_eq!(
        (cur_w, cur_h),
        (ref_w, ref_h),
        "Snapshot size changed: current {cur_w}x{cur_h}, reference {ref_w}x{ref_h}. \
         Delete {} to regenerate.",
        ref_path.display()
    );

    let mut diff_count = 0usize;
    for (&current, &reference) in current_pixels.iter().zip(ref_pixels.iter()) {
        if current != reference {
            diff_count += 1;
        }
    }

    if diff_count > max_diff_pixels {
        let fail_path = dir.join(format!("{name}_FAILED.png"));
        save_png(
            fail_path.to_str().unwrap(),
            &current_pixels,
            cur_w,
            cur_h,
        );
        panic!(
            "Snapshot mismatch: {diff_count} pixels differ (max: {max_diff_pixels}).\n\
             Reference: {}\n\
             Current:   {}",
            ref_path.display(),
            fail_path.display(),
        );
    }
}

fn save_png(path: &str, pixels: &[u8], w: u32, h: u32) {
    let file = std::fs::File::create(path)
        .unwrap_or_else(|e| panic!("Failed to create {path}: {e}"));
    let mut encoder = png::Encoder::new(file, w, h);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().unwrap();
    writer.write_image_data(pixels).unwrap();
}

fn load_png(path: &std::path::Path) -> (Vec<u8>, u32, u32) {
    let file = std::fs::File::open(path)
        .unwrap_or_else(|e| panic!("Failed to open {}: {e}", path.display()));
    let decoder = png::Decoder::new(std::io::BufReader::new(file));
    let mut reader = decoder.read_info().unwrap();
    let mut buf = vec![0u8; reader.output_buffer_size().unwrap()];
    let info = reader.next_frame(&mut buf).unwrap();
    buf.truncate(info.buffer_size());
    (buf, info.width, info.height)
}
