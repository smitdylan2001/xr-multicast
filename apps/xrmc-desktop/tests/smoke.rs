//! Smoke tests: the app logic running headless in demo mode (no window, no GPU).

use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui;
use xrmc_core::StreamStatus;
use xrmc_core::view_size::{ViewMode, ViewRequest};
use xrmc_desktop::bridge::{Action, Bridge, Mode, UiEvent};
use xrmc_desktop::model::Model;
use xrmc_desktop::ui::{App, AppOptions};

fn pump(bridge: &Bridge, model: &mut Model) {
    for ev in bridge.poll() {
        match ev {
            UiEvent::Devices(s) => model.on_devices(s),
            UiEvent::Stream { state, generation } => model.on_stream_state(state, generation),
            UiEvent::Settings(s) => model.settings = *s,
            UiEvent::Env(e) => model.env = *e,
            UiEvent::Done { result: Err(e), .. } => panic!("action failed: {e}"),
            _ => {}
        }
    }
    for t in &mut model.tiles {
        if let Some(out) = bridge.output(&t.id)
            && let Some(f) = out.take_frame()
        {
            t.on_frame(f.generation, f.frame.width, f.frame.height, 8);
        }
    }
}

fn wait(
    bridge: &Bridge,
    model: &mut Model,
    secs: u64,
    mut done: impl FnMut(&Model) -> bool,
) -> bool {
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(secs) {
        pump(bridge, model);
        if done(model) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

fn request(w: f64, h: f64) -> ViewRequest {
    ViewRequest {
        visible: true,
        tile_w: w,
        tile_h: h,
        mode: ViewMode::Corrected,
        zoom: 1.0,
        output_aspect: Some("16:9".into()),
        layout: xrmc_core::layout::Layout::Sbs,
        rotation: 0.0,
    }
}

#[test]
fn demo_hub_casts_decodes_resizes_and_stops() {
    let mut bridge = Bridge::start(Mode::Demo { headsets: 3 }, Arc::new(|| {}), 1);
    let mut model = Model::default();
    assert!(wait(&bridge, &mut model, 5, |m| m.headsets.len() == 5));
    assert!(model.env.demo);
    let ids = model.castable_not_streaming();
    assert_eq!(ids.len(), 3);

    // Report tile sizes first (like the UI does), then cast.
    bridge.set_view_sizes(
        ids.iter()
            .map(|id| (id.clone(), Some(request(640.0, 360.0))))
            .collect(),
    );
    for id in &ids {
        model.begin_cast(id);
        bridge.send(Action::Cast {
            id: id.clone(),
            transport: None,
            restart: false,
            decode_failed: false,
        });
    }
    let live = |m: &Model| {
        m.tiles.len() == 3
            && m.tiles.iter().all(|t| t.has_video)
            && m.streams.values().all(|s| s.state == StreamStatus::Live)
    };
    assert!(
        wait(&bridge, &mut model, 10, live),
        "streams did not go live: {:?}",
        model.streams
    );
    let first = model.tiles[0].frame_size.unwrap();
    assert!(first.0 <= 1280, "matched to a small tile: {first:?}");
    // Layout detection found the two eye views.
    let id0 = model.tiles[0].id.clone();
    let out = bridge.output(&id0).unwrap();
    let t = Instant::now();
    while out.grid().is_none() && t.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(out.grid().map(|g| (g.cols, g.rows)), Some((2, 1)));
    assert!(out.frames.load(std::sync::atomic::Ordering::Relaxed) > 5);

    // A much bigger tile: the stream restarts at a higher resolution ("Changing resolution").
    bridge.set_view_sizes(vec![(id0.clone(), Some(request(1900.0, 1070.0)))]);
    let mut saw_resizing = false;
    let grew = wait(&bridge, &mut model, 15, |m| {
        if m.streams
            .get(&id0)
            .is_some_and(|s| s.state == StreamStatus::Resizing)
            || m.tile(&id0).is_some_and(|t| t.resizing)
        {
            saw_resizing = true;
        }
        m.tile(&id0)
            .and_then(|t| t.frame_size)
            .is_some_and(|(w, _)| w > first.0)
            && m.tile(&id0).is_some_and(|t| t.has_video && !t.resizing)
    });
    assert!(
        grew,
        "no resize: {:?}",
        model.tile(&id0).map(|t| t.frame_size)
    );
    assert!(saw_resizing);

    bridge.send(Action::StopAll);
    assert!(wait(&bridge, &mut model, 10, |m| m.tiles.is_empty()));
    bridge.shutdown();
}

#[test]
fn the_whole_app_runs_headless_in_demo_mode() {
    let ctx = egui::Context::default();
    let opts = AppOptions {
        mode: Mode::Demo { headsets: 4 },
        fullscreen: false,
        kiosk: false,
        ui_scale: None,
        screenshot: None,
        screenshot_after: None,
        exit_after: None,
        show: Some("settings".into()),
        decoder_threads: 1,
        offscreen: true,
        cast_on_start: false,
        platform: xrmc_desktop::platform::default_platform(),
    };
    let mut app = App::create(&ctx, None, opts);
    let start = Instant::now();
    let mut frames = 0;
    // Narrow portrait screen first, then a large landscape one.
    while start.elapsed() < Duration::from_secs(6) {
        let size = if start.elapsed() < Duration::from_secs(3) {
            egui::vec2(800.0, 1280.0)
        } else {
            egui::vec2(1920.0, 1080.0)
        };
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
            time: Some(start.elapsed().as_secs_f64()),
            ..Default::default()
        };
        let mut out = ctx.run_ui(input, |ui| app.frame(ui));
        assert!(!out.shapes.is_empty());
        out.textures_delta.clear();
        frames += 1;
        std::thread::sleep(Duration::from_millis(30));
    }
    let m = app.model();
    assert!(frames > 50);
    assert_eq!(m.tiles.len(), 4, "demo auto-casts every verified headset");
    assert!(
        m.tiles.iter().all(|t| t.has_frame),
        "frames decoded for every tile"
    );
    assert!(m.streams.values().all(|s| s.state.is_running()));
    app.shutdown();
}
