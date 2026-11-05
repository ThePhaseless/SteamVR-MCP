mod common;

use alvr_common::glam::{Quat, Vec3};
use common::Snap;
use alvr_common::parking_lot::Mutex;
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    panic,
    path::Path,
    process::{Child, Command, Stdio},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use steamvr_mcp::{
    checks::startup_checks,
    config::{Config, Paths},
    headset::{Eye, Source},
    openvr::TrackedState,
    hand::Gesture,
    rig::{
        AnimateArgs, AxisArgs, Button, ButtonArgs, FingerCurlArgs, GripArgs, Hand, HandGestureArgs, HandMode, HandModeArgs,
        PoseArgs, RigAction, SequenceStep, TouchArgs, TrackpadArgs, TriggerArgs,
    },
    setup::{prepare, restore, session_values},
    stack::{LaunchArgs, LogQuery, SettingValue, Stack},
    steamvr,
};

/// Not a check. Stack re-executes the running binary as its `alvr_dashboard` child; in this test
/// binary that runs libtest, which runs only the non-ignored tests, so this one plays the child.
#[test]
fn dashboard_child_role() {
    let argv0 = std::env::args_os().next().unwrap_or_default();
    if Path::new(&argv0).file_name() == Some("alvr_dashboard".as_ref()) {
        steamvr::run_dashboard_child();
    }
}

/// Live tests that skipped in this process.
static SKIPPED: AtomicUsize = AtomicUsize::new(0);

/// Reports a live test that cannot run on this machine as it is, which libtest still counts as ok.
/// With VR_TEST_REQUIRE_ALL=1 it fails instead, so a run that exercised nothing is not green.
fn skip(why: &str) {
    skip_or_fail(why, std::env::var_os("VR_TEST_REQUIRE_ALL").is_some_and(|v| v == "1"), &SKIPPED);
}

/// A skip VR_TEST_REQUIRE_ALL=1 lets pass: the two live_start_streams tests need Steam closed and
/// running, so every run skips one of them (the first runs when Steam was closed, else the second).
fn skip_steam_state(why: &str) {
    skip_or_fail(why, false, &SKIPPED);
}

fn skip_or_fail(why: &str, require_all: bool, skipped: &AtomicUsize) {
    assert!(!require_all, "not run, and VR_TEST_REQUIRE_ALL=1: {why}");
    let n = skipped.fetch_add(1, Ordering::SeqCst) + 1;
    // straight to stderr: libtest hides eprintln! of a passing test
    let _ = writeln!(std::io::stderr(), "skipped ({n} live tests so far): {why}");
}

#[test]
fn skips_count_and_fail_when_all_are_required() {
    // its own counter: live_zz_stop_and_restore prints SKIPPED, which counts live tests only
    let skipped = AtomicUsize::new(0);
    skip_or_fail("a reason", false, &skipped);
    assert_eq!(skipped.load(Ordering::SeqCst), 1);
    let failed = panic::catch_unwind(|| skip_or_fail("a reason", true, &skipped)).unwrap_err();
    assert!(failed.downcast_ref::<String>().is_some_and(|m| m.contains("VR_TEST_REQUIRE_ALL=1: a reason")));
}

/// prepare switches SteamVR to the configured ALVR driver, and restore puts every file back as it was.
/// Touches the real SteamVR and ALVR files; needs `streamer_dir` in config.toml (or a registered ALVR driver).
#[test]
#[ignore]
fn live_prepare_restore_round_trip() {
    if let Err(e) = stop_and_restore() {
        return skip(&e);
    }
    if steamvr::is_steamvr_running() {
        return skip("vrserver is running; close SteamVR (or run live_zz_stop_and_restore) first");
    }

    let cfg = Config::load().unwrap();
    let paths = Paths::detect(&cfg);
    println!("{paths:#?}\nsession file: {:?}", paths.session_file());
    let before = Snap::take(&paths);
    let values = session_values(&cfg).unwrap();

    let prepared = prepare(&paths, &values, "1234.client.local.");
    let mut problems = vec![];
    match &prepared {
        Ok(_) => {
            let drivers = Snap::take(&paths).openvrpaths["external_drivers"].clone();
            if drivers != serde_json::json!([paths.driver_dir().unwrap()]) {
                problems.push(format!("external_drivers is {drivers}, expected only the configured driver dir"));
            }
            if !paths.vrcompositor().unwrap().is_file() {
                problems.push("vrcompositor is not a regular file".into());
            }
        }
        Err(e) => problems.push(format!("prepare failed: {e:#}")),
    }

    // restore even when a check above failed, so the machine is never left switched
    let restored = restore(&paths, false);
    assert!(problems.is_empty(), "{problems:?}");
    restored.unwrap();
    assert_eq!(Snap::take(&paths), before);
    assert!(!paths.restore_file().exists());
}

/// vr_start streams and vr_stop cleans up. The `alvr_dashboard` child gives early HMD init: the
/// first connect restarts SteamVR exactly once, then the client streams with no further restart and
/// ALVR never misses the dashboard. The manual 127.0.0.1 entry connects with client discovery off.
/// Limitation: a headset reconnect without a SteamVR restart is not proven; on loopback TCP the
/// ALVR EOF spin (sockets/src/stream_socket/tcp.rs:137-147) leaves the server stuck Disconnecting,
/// so the MCP restarts SteamVR on every reconnect.
/// Needs Steam closed and `streamer_dir` in config.toml (or a registered ALVR driver).
#[test]
#[ignore]
fn live_start_streams_and_stop_cleans() {
    if let Err(e) = stop_and_restore() {
        return skip(&e);
    }
    if steamvr::is_steam_running() {
        return skip_steam_state("Steam is running; close it first");
    }
    start_streams_and_stop_cleans(false);
}

/// The same checks with the user's Steam already running: the MCP must neither start nor close it.
/// Needs Steam running (on a GPU that ALVR 21's direct mode supports) and SteamVR closed.
#[test]
#[ignore]
fn live_start_streams_and_stop_cleans_with_steam_running() {
    if let Err(e) = stop_and_restore() {
        return skip(&e);
    }
    if !steamvr::is_steam_running() {
        return skip_steam_state("needs Steam running");
    }
    if steamvr::is_steamvr_running() {
        return skip("needs SteamVR closed");
    }
    start_streams_and_stop_cleans(true);
    assert!(steamvr::is_steam_running(), "the user's Steam was closed");
}

fn start_streams_and_stop_cleans(steam_was_running: bool) {
    let cfg = Config::load().unwrap();
    let paths = Paths::detect(&cfg);
    let before = Snap::take(&paths);
    // ALVR logs errors to crash_log.txt (always on, appended); the session log is off in release
    // builds and truncated at each SteamVR start
    let crash_log = alvr_filesystem::filesystem_layout_from_openvr_driver_root_dir(&paths.driver_dir().unwrap())
        .unwrap()
        .crash_log();
    let log_start = fs::metadata(&crash_log).map_or(0, |m| m.len() as usize);
    let new_errors = || {
        let bytes = fs::read(&crash_log).unwrap_or_default();
        String::from_utf8_lossy(bytes.get(log_start..).unwrap_or(&bytes)).into_owned()
    };
    let s = Stack::new(cfg, paths.clone());

    let checks = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        let r = s.start().unwrap();
        println!("start: {r:?}");
        assert!(r.started_steam != steam_was_running && !r.already_streaming && !r.adopted, "{r:?}");
        assert_eq!(r.restarts, 1, "a fresh session (hash 0) restarts SteamVR exactly once: {r:?}");
        assert_eq!(r.codec.as_deref(), Some("H264"));
        assert!(!steamvr::pids_named("alvr_dashboard").is_empty(), "no alvr_dashboard child");
        // the HUD trigger restarts SteamVR even when ALVR missed the dashboard, so check ALVR's own view
        let errors = new_errors();
        assert!(!errors.contains("No dashboard process found"), "ALVR saw no dashboard:\n{errors}");

        let frames = || s.status().headset.unwrap().frames_received;
        let f0 = frames();
        thread::sleep(Duration::from_secs(2));
        assert!(frames() > f0, "no video frames in 2 s");
        assert!(s.start().unwrap().already_streaming);

        // no further restart while streaming: same vrserver, frames still growing
        let vrserver = steamvr::pids_named("vrserver");
        let f1 = frames();
        thread::sleep(Duration::from_secs(10));
        let h = s.status().headset.unwrap();
        assert!(h.streaming && h.frames_received > f1, "stream stalled: {h:?}");
        assert_eq!(steamvr::pids_named("vrserver"), vrserver, "vrserver restarted while streaming");
        println!("status: {:?}", s.status());
    }));

    // stop and restore even when a check failed, so the machine is never left switched
    let stopped = s.stop();
    let restored = restore(&paths, false);
    if let Err(p) = checks {
        panic::resume_unwind(p);
    }
    let stop = stopped.unwrap();
    println!("stop: {stop:?}");
    assert!(stop.remaining_processes.is_empty() && stop.closed_steam != steam_was_running, "{stop:?}");
    restored.unwrap();
    assert_eq!(Snap::take(&paths), before);
    assert!(!paths.restore_file().exists());
}

/// With no config file, every startup check passes or says what to set; with the machine's
/// config.toml they all pass. Read-only.
#[test]
#[ignore]
fn live_checks_without_config() {
    let started = std::time::Instant::now();
    let bare = startup_checks(&Config::default(), &Paths::detect(&Config::default()));
    println!("without config ({:?}): {bare:#?}", started.elapsed());
    assert!(bare.iter().all(|c| c.ok || !c.fix.is_empty()), "{bare:#?}");
    let streamer = bare.iter().find(|c| c.name == "alvr_streamer").unwrap();
    assert!(streamer.ok || streamer.fix.contains("streamer_dir"), "{streamer:?}");

    let cfg = Config::load().unwrap();
    let configured = startup_checks(&cfg, &Paths::detect(&cfg));
    println!("with config: {configured:#?}");
    assert!(configured.iter().all(|c| c.ok), "{configured:#?}");
}

/// The shared stack of the live window: starts it when the headset is not streaming and leaves it running.
/// None, after printing why, when SteamVR runs without ALVR: vr_start refuses that one.
fn ensure_streaming() -> Option<Arc<Stack>> {
    let stack = shared_stack();
    if !stack.status().headset.is_some_and(|h| h.streaming) {
        if steamvr::is_steamvr_running() && !steamvr::vrserver_has_alvr() {
            skip("SteamVR is running without ALVR; close it first");
            return None;
        }
        stack.start().unwrap();
    }
    Some(stack)
}

/// VR_TEST_GAME_ID, or None after printing that it is unset.
fn game_id() -> Option<String> {
    let id = std::env::var("VR_TEST_GAME_ID").ok().filter(|id| !id.is_empty());
    if id.is_none() {
        skip("set VR_TEST_GAME_ID to a VR game's Steam app id or non-Steam shortcut game id");
    }
    id
}

/// The one Stack of this test process.
fn shared_stack() -> Arc<Stack> {
    static STACK: OnceLock<Arc<Stack>> = OnceLock::new();
    let stack = STACK.get_or_init(|| {
        let cfg = Config::load().unwrap();
        Stack::new(cfg.clone(), Paths::detect(&cfg))
    });
    Arc::clone(stack)
}

/// The headset screenshot path decodes a real ALVR 21 VA-API stream. Needs Steam closed or its GPU supported, and SteamVR closed or streaming.
#[test]
#[ignore]
fn live_screenshot_png() {
    let Some(s) = ensure_streaming() else { return };
    let [view_w, view_h] = s.status().headset.unwrap().view_resolution;
    let decode = |png: &[u8]| {
        let mut reader = png::Decoder::new(std::io::Cursor::new(png)).read_info().unwrap();
        let mut rgb = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut rgb).unwrap();
        (info.width, info.height, rgb)
    };
    for (eye, width) in [(Eye::Both, 2 * view_w), (Eye::Left, view_w)] {
        let started = Instant::now();
        let shot = s.screenshot(Source::Headset, eye).unwrap();
        let took = started.elapsed();
        println!("{eye:?}: {}x{} in {took:?}, frame at {:?} ms", shot.width, shot.height, shot.frame_timestamp_ms);
        // the shot waits for about 18 frames after the keyframe and decodes them all; 3.07 s measured in a debug build
        assert!(took < Duration::from_secs(5), "{eye:?} took {took:?}");
        let (w, h, rgb) = decode(&shot.png);
        assert_eq!((w, h, shot.width, shot.height), (width, view_h, width, view_h), "{eye:?}");
        let mean = rgb.iter().map(|&b| b as f64).sum::<f64>() / rgb.len() as f64;
        let stddev = (rgb.iter().map(|&b| (b as f64 - mean).powi(2)).sum::<f64>() / rgb.len() as f64).sqrt();
        println!("{eye:?}: pixel stddev {stddev:.2}");
        assert!(stddev > 0.0, "{eye:?} image is uniform");
    }
}

/// The host-side OpenVR client reaches vrserver in its container, and get_tracked_state shows
/// the default head pose. Needs Steam closed or its GPU supported, and SteamVR closed or streaming.
#[test]
#[ignore]
fn live_tracked_head_matches() {
    let Some(s) = ensure_streaming() else { return };
    let state = s.tracked_state(false).unwrap();
    println!("{state:#?}\nscene app: {:?}", s.scene_app());
    let hmds: Vec<_> = state.devices.iter().filter(|d| d.class == "hmd").collect();
    assert_eq!(hmds.len(), 1, "{hmds:?}");
    let [x, y, z] = hmds[0].position;
    assert!(hmds[0].pose_valid, "{:?}", hmds[0]);
    assert!(x.abs() < 0.01 && (y - 1.6).abs() < 0.01 && z.abs() < 0.01, "{:?}", hmds[0]);
    let mut roles: Vec<_> = state.devices.iter().filter(|d| d.class == "controller").map(|d| d.role.as_str()).collect();
    roles.sort();
    // pose_valid may stay false until a controller pose is sent
    assert_eq!(roles, ["left", "right"], "{:?}", state.devices);
}

/// Head moves show in get_tracked_state and in the screenshot, which changes between two head
/// poses. Needs Steam closed
/// or its GPU supported, and SteamVR closed or streaming.
#[test]
#[ignore]
fn live_head_move_tracked() {
    let Some(s) = ensure_streaming() else { return };
    // later tests in this process expect the default head pose, also after a failed assertion
    struct Reset(Arc<Stack>);
    impl Drop for Reset {
        fn drop(&mut self) {
            let default = PoseArgs { position: [0.0, 1.6, 0.0], ..Default::default() };
            self.0.apply(RigAction::SetHeadPose(default)).ok(); // also stops an animation
        }
    }
    let _reset = Reset(Arc::clone(&s));
    let set = |position, yaw| s.apply(RigAction::SetHeadPose(PoseArgs { position, yaw, ..Default::default() })).unwrap();
    let hmd = || s.tracked_state(false).unwrap().devices.into_iter().find(|d| d.class == "hmd").expect("no hmd");
    let pixels = |eye| {
        let png = s.screenshot(Source::Headset, eye).unwrap().png;
        let mut reader = png::Decoder::new(std::io::Cursor::new(png)).read_info().unwrap();
        let mut rgb = vec![0; reader.output_buffer_size().unwrap()];
        reader.next_frame(&mut rgb).unwrap();
        rgb
    };

    set([0.0, 1.6, 0.0], None);
    thread::sleep(Duration::from_millis(300));
    let a = pixels(Eye::Left);
    set([0.3, 1.7, -0.2], Some(90.0));
    thread::sleep(Duration::from_millis(300));
    let d = hmd();
    let forward = Quat::from_array(d.quat) * Vec3::NEG_Z;
    let yaw = (-forward.x).atan2(-forward.z).to_degrees();
    println!("hmd {:?} yaw {yaw:.2}", d.position);
    assert!(Vec3::from_array(d.position).distance(Vec3::new(0.3, 1.7, -0.2)) < 0.01, "{d:?}");
    assert!((yaw - 90.0).abs() < 1.0, "yaw {yaw}: {d:?}");
    let b = pixels(Eye::Left);
    assert_eq!(a.len(), b.len());
    let diff = a.iter().zip(&b).map(|(&x, &y)| (x as f64 - y as f64).abs()).sum::<f64>() / a.len() as f64;
    println!("mean absolute pixel difference {diff:.2}");
    // 7.1 measured on the dark SteamVR Home between yaw 0 and yaw 90
    assert!(diff > 4.0, "the screenshot did not change with the head pose: mean abs diff {diff:.2}, want > 4");

    let waypoints = vec![
        PoseArgs { position: [0.0, 1.6, -0.5], ..Default::default() },
        PoseArgs { position: [0.3, 1.6, 0.0], yaw: Some(-45.0), ..Default::default() },
    ];
    let started = Instant::now();
    let done_after = s.apply(RigAction::AnimateHead(AnimateArgs { waypoints, duration_s: 2.0 })).unwrap();
    assert_eq!(done_after, Duration::from_secs(2));
    for at in [0.5, 1.5] {
        thread::sleep(Duration::from_secs_f32(at).saturating_sub(started.elapsed()));
        let d = hmd();
        println!("at {at} s: {d:?}");
        assert!(Vec3::from_array(d.linear_velocity).length() > 0.05, "no velocity mid-animation: {d:?}");
    }
    thread::sleep(done_after.saturating_sub(started.elapsed()) + Duration::from_millis(300));
    let d = hmd();
    assert!(Vec3::from_array(d.position).distance(Vec3::new(0.3, 1.6, 0.0)) < 0.01, "{d:?}");
}

/// Index finger curls, sent as a hand-model skeleton with the
/// controller pose, show in SteamVR's skeleton per finger independently, for both hands; index to
/// pinky also as ALVR's /input/finger/* components (device_curls). Needs Steam closed or its GPU
/// supported, and SteamVR closed or streaming.
#[test]
#[ignore]
fn live_finger_curls_per_finger() {
    let Some(s) = ensure_streaming() else { return };
    /// One finger at 1, the rest at 0; None opens the hand.
    fn curl(s: &Stack, hand: Hand, f: Option<usize>) -> Result<Duration, impl std::fmt::Debug> {
        let v = |k| Some(if f == Some(k) { 1.0 } else { 0.0 });
        let args = FingerCurlArgs { hand, thumb: v(0), index: v(1), middle: v(2), ring: v(3), pinky: v(4), splay: Some(0.0) };
        s.apply(RigAction::SetFingerCurl(args))
    }
    // later tests in this process expect open hands, also after a failed assertion
    struct Reset(Arc<Stack>);
    impl Drop for Reset {
        fn drop(&mut self) {
            curl(&self.0, Hand::Left, None).ok();
            curl(&self.0, Hand::Right, None).ok();
        }
    }
    let _reset = Reset(Arc::clone(&s));

    // SteamVR gives the reader no input while the scene app starts (IsInputAvailable false, scene
    // state "waiting"), so right after vr_start every skeleton action reads inactive
    let active = |st: &TrackedState| st.skeletons.values().all(|k| k.active);
    let ready = steamvr::wait_for(Duration::from_secs(60), || s.tracked_state(false).is_ok_and(|st| active(&st)));
    assert!(ready, "skeleton actions still inactive after 60 s; scene app {:?}", s.scene_app());

    let mut failures = vec![];
    for (hand, name) in [(Hand::Left, "left"), (Hand::Right, "right")] {
        for f in 0..5 {
            curl(&s, hand, Some(f)).unwrap();
            let follows = |k: usize, v: f32| if k == f { v >= 0.7 } else { v <= 0.3 };
            let settled = |st: &TrackedState| {
                let sk = &st.skeletons[name];
                sk.active && (0..5).all(|k| follows(k, sk.curls[k])) && (1..5).all(|k| follows(k, sk.device_curls[k]))
            };
            steamvr::wait_for(Duration::from_secs(2), || s.tracked_state(false).is_ok_and(|st| settled(&st)));
            let state = s.tracked_state(false).unwrap();
            let sk = &state.skeletons[name];
            println!("{name} finger {f} at 1: active {} curls {:?} device_curls {:?}", sk.active, sk.curls, sk.device_curls);
            if !sk.active {
                failures.push(format!("{name} finger {f}: skeleton action inactive"));
            }
            if let Some(k) = (0..5).find(|&k| !follows(k, sk.curls[k])) {
                failures.push(format!("{name} finger {f}: SteamVR curl {k} is {}: {:?}", sk.curls[k], sk.curls));
            }
            // ALVR sends index to pinky only, no thumb component (Controller.cpp:329-340)
            if let Some(k) = (1..5).find(|&k| !follows(k, sk.device_curls[k])) {
                failures.push(format!("{name} finger {f}: device curl {k} is {}: {:?}", sk.device_curls[k], sk.device_curls));
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// The game holds its frame rate at the MCP's default resolution: under 10% of frames dropped or
/// reprojected. Needs env VR_TEST_GAME_ID: a VR game's Steam app id or non-Steam shortcut game id
/// (the menu tests were written against Beat Saber).
#[test]
#[ignore]
fn live_game_menu_fps() {
    let Some(id) = game_id() else { return };
    let Some(s) = ensure_streaming() else { return };
    steamvr::launch(&Config::load().unwrap(), &format!("steam://rungameid/{id}")).unwrap();
    let running = steamvr::wait_for(Duration::from_secs(180), || s.scene_app().is_some_and(|a| a.state == "running"));
    let app = s.scene_app();
    assert!(running, "scene app not running after 180 s: {app:?}");
    thread::sleep(Duration::from_secs(20));
    let f0 = s.frame_stats().expect("frame stats");
    thread::sleep(Duration::from_secs(30));
    let f1 = s.frame_stats().expect("frame stats");
    steamvr::terminate(app.unwrap().pid, libc::SIGTERM);
    let presents = f1.presents - f0.presents;
    let bad = (f1.dropped - f0.dropped) + (f1.reprojected - f0.reprojected);
    let ratio = bad as f64 / presents.max(1) as f64;
    println!("{f0:?} -> {f1:?}: {bad} dropped or reprojected of {presents} presents ({ratio:.3})");
    assert!(presents > 0 && ratio < 0.10, "ratio {ratio:.3}");
}

/// launch_app reaches the game's menu and a screenshot shows it. Needs env VR_TEST_GAME_ID: a
/// Windows VR game's Steam app id or non-Steam shortcut (game) id, such as Beat Saber's.
#[test]
#[ignore]
fn live_launch_app_reaches_menu() {
    let Some(id) = game_id() else { return };
    let Some(s) = ensure_streaming() else { return };
    let report = s.launch_app(LaunchArgs { app_id: id.clone(), ..Default::default() }).unwrap();
    println!("{report:?}");
    assert!(report.scene_pid > 0);
    let comm = steamvr::comm(report.scene_pid);
    assert!(comm.as_deref().is_some_and(|c| !steamvr::VR_PROCESS_NAMES.contains(&c)), "scene pid is SteamVR's own: {comm:?}");
    // a Windows game runs through Proton; this needs an explicit CompatToolMapping entry, as a shortcut has
    assert!(report.app.compat_tool.is_some(), "no compatibility tool reported: {report:?}");
    thread::sleep(Duration::from_secs(15));
    let shot = s.screenshot(Source::Headset, Eye::Left).unwrap();
    let mut reader = png::Decoder::new(std::io::Cursor::new(&shot.png)).read_info().unwrap();
    let mut rgb = vec![0; reader.output_buffer_size().unwrap()];
    reader.next_frame(&mut rgb).unwrap();
    let mean = rgb.iter().map(|&b| b as f64).sum::<f64>() / rgb.len() as f64;
    let stddev = (rgb.iter().map(|&b| (b as f64 - mean).powi(2)).sum::<f64>() / rgb.len() as f64).sqrt();
    println!("menu pixel stddev {stddev:.2}");
    assert!(stddev > 10.0, "menu screenshot looks blank: stddev {stddev:.2}");
    let stopped = s.stop_app().unwrap();
    let pid = stopped["stopped_pid"].as_u64().unwrap() as u32;
    assert_eq!(pid, report.scene_pid);
    // SteamVR may bring up its Home scene afterwards; the stopped game's pid must be gone from the scene
    let gone = steamvr::wait_for(Duration::from_secs(20), || s.scene_app().is_some_and(|a| a.state == "none" || a.pid != pid));
    assert!(gone, "scene app after stop_app: {:?}", s.scene_app());
}

/// An animated controller swing with the trigger held is visible in game
/// (a screenshot taken mid-swing differs from one before), and the playspace and recenter tools
/// leave the stream running. Needs env VR_TEST_GAME_ID, as live_launch_app_reaches_menu.
#[test]
#[ignore]
fn live_swing_with_trigger() {
    let rgb = |png: &[u8]| {
        let mut reader = png::Decoder::new(std::io::Cursor::new(png)).read_info().unwrap();
        let mut rgb = vec![0; reader.output_buffer_size().unwrap()];
        reader.next_frame(&mut rgb).unwrap();
        rgb
    };
    let Some(id) = game_id() else { return };
    let Some(s) = ensure_streaming() else { return };
    let app = s.launch_app(LaunchArgs { app_id: id.clone(), ..Default::default() }).unwrap();
    thread::sleep(Duration::from_secs(15)); // the menu
    let a = rgb(&s.screenshot(Source::Headset, Eye::Left).unwrap().png);
    thread::sleep(Duration::from_secs(1));
    let a2 = rgb(&s.screenshot(Source::Headset, Eye::Left).unwrap().png); // the menu at rest, to tell menu motion from the swing
    let steps: Vec<SequenceStep> = serde_json::from_value(json!([
        { "at_s": 0.0, "action": "set_trigger", "args": { "hand": "right", "value": 1.0 } },
        { "at_s": 0.0, "action": "animate_controller", "args": {
            "hand": "right", "relative_to": "head", "duration_s": 1.5,
            "waypoints": [{ "position": [-0.3, -0.1, -0.5] }, { "position": [0.0, 0.1, -0.6] }, { "position": [0.3, -0.1, -0.5] }] } },
        { "at_s": 1.6, "action": "set_trigger", "args": { "hand": "right", "value": 0.0 } },
    ]))
    .unwrap();
    let shooter = {
        let s = Arc::clone(&s);
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(750));
            s.screenshot(Source::Headset, Eye::Left).unwrap()
        })
    };
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let report = runtime.block_on(s.run_sequence(steps)).unwrap();
    println!("{report:?}");
    let b = rgb(&shooter.join().unwrap().png);
    assert!(a.len() == b.len() && a.len() == a2.len());
    let mean_diff = |x: &[u8], y: &[u8]| x.iter().zip(y).map(|(p, q)| p.abs_diff(*q) as f64).sum::<f64>() / x.len() as f64;
    let (swing, rest) = (mean_diff(&a, &b), mean_diff(&a, &a2));
    println!("mean abs diff: swing {swing:.2}, at rest {rest:.2}");
    assert!(swing > 8.0 && swing > rest + 4.0, "the swing is not visible: swing {swing:.2}, at rest {rest:.2}");
    assert!(report.steps.len() == 3 && report.steps.iter().all(|st| st.ok), "{report:?}");
    assert!(report.total_s >= 1.6, "{report:?}");

    let set = s.set_playspace(3.0, 2.0).unwrap();
    assert!(set["warning"].is_null(), "{set}");
    println!("{}", s.recenter());
    thread::sleep(Duration::from_secs(2));
    // ALVR logs the area it received (server_core connection.rs:1274): the half extents of 3 x 2
    let logs = s.logs(LogQuery { source: Some("alvr".into()), contains: Some("Received new playspace with size".into()), min_level: None, limit: Some(10) }).unwrap();
    assert!(logs.iter().any(|l| l.text.contains("size: [1.5, 1]")), "{logs:?}");
    assert!(s.status().headset.unwrap().streaming);
    assert!(s.tracked_state(false).is_ok());
    let stopped = s.stop_app().unwrap();
    assert_eq!(stopped["stopped_pid"].as_u64().unwrap() as u32, app.scene_pid);
}

/// examples/xr_probe as a child, with its newest stdout line. SIGTERM on drop.
struct Probe {
    child: Child,
    line: Arc<Mutex<Value>>,
}

impl Probe {
    /// Builds and starts the probe, and waits up to 60 s for its session to be FOCUSED.
    fn start() -> Probe {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let built = Command::new(cargo).args(["build", "--example", "xr_probe"]).status().unwrap();
        assert!(built.success(), "cargo build --example xr_probe failed");
        let target = std::env::var("CARGO_TARGET_DIR").unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/target").into());
        let mut child = Command::new(format!("{target}/debug/examples/xr_probe")).stdout(Stdio::piped()).spawn().unwrap();
        let line = Arc::new(Mutex::new(Value::Null));
        let stdout = child.stdout.take().unwrap();
        let sink = Arc::clone(&line);
        thread::spawn(move || {
            for l in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Ok(v) = serde_json::from_str(&l) {
                    *sink.lock() = v;
                }
            }
        });
        let probe = Probe { child, line };
        let focused = steamvr::wait_for(Duration::from_secs(60), || probe.state() == "FOCUSED");
        assert!(focused, "probe not FOCUSED after 60 s: {}", probe.line());
        probe
    }

    fn line(&self) -> Value {
        self.line.lock().clone()
    }

    fn state(&self) -> String {
        self.line()["state"].as_str().unwrap_or_default().to_owned()
    }

    /// Waits up to `timeout` for `ok` on the newest line; returns the last line seen and whether it held.
    fn wait(&self, timeout: Duration, ok: impl Fn(&Value) -> bool) -> (Value, bool) {
        let held = steamvr::wait_for(timeout, || ok(&self.line()));
        (self.line(), held)
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        // SAFETY: plain kill(2) on our own child's pid
        unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
        if !steamvr::wait_for(Duration::from_secs(10), || self.child.try_wait().is_ok_and(|s| s.is_some())) {
            self.child.kill().ok();
        }
        self.child.wait().ok();
    }
}

/// Whether probe hand `h` shows `want` for each key: numbers within 0.05, [x, y] per axis, booleans exact.
fn shows(h: &Value, want: &[(String, Value)]) -> bool {
    let close = |a: &Value, b: &Value| a.as_f64().zip(b.as_f64()).is_some_and(|(a, b)| (a - b).abs() <= 0.05);
    want.iter().all(|(k, w)| match (&h[k.as_str()], w) {
        (Value::Array(a), Value::Array(b)) => a.len() == b.len() && a.iter().zip(b).all(|(a, b)| close(a, b)),
        (Value::Bool(a), Value::Bool(b)) => a == b,
        (a, b) => close(a, b),
    })
}

/// One input table row: the call, what the probe must show, and the call that undoes it.
struct Row {
    name: String,
    apply: RigAction,
    want: Vec<(String, Value)>,
    undo: RigAction,
}

/// A probe value at rest: false, 0, or [0, 0].
fn zero(v: &Value) -> Value {
    match v {
        Value::Bool(_) => json!(false),
        Value::Array(a) => json!(vec![0.0; a.len()]),
        _ => json!(0.0),
    }
}

fn want(pairs: &[(&str, Value)]) -> Vec<(String, Value)> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
}

fn rows(hand: Hand) -> Vec<Row> {
    let button = |button| ButtonArgs { hand, button };
    let mut rows = vec![];
    for (b, key) in [(Button::A, "a"), (Button::B, "b"), (Button::Thumbstick, "thumbstick"), (Button::System, "system")] {
        let want = vec![(format!("{key}_click"), json!(true)), (format!("{key}_touch"), json!(true))];
        rows.push(Row { name: format!("press {key}"), apply: RigAction::PressButton(button(b)), want, undo: RigAction::ReleaseButton(button(b)) });
    }
    let presses = [
        (Button::Trigger, "trigger", want(&[("trigger_value", json!(1.0)), ("trigger_click", json!(true)), ("trigger_touch", json!(true))])),
        (Button::Grip, "grip", want(&[("squeeze_value", json!(1.0)), ("squeeze_force", json!(0.0))])),
        (Button::Trackpad, "trackpad", want(&[("trackpad_force", json!(1.0)), ("trackpad_touch", json!(true))])),
    ];
    for (b, key, want) in presses {
        rows.push(Row { name: format!("press {key}"), apply: RigAction::PressButton(button(b)), want, undo: RigAction::ReleaseButton(button(b)) });
    }
    let touchable = [(Button::A, "a"), (Button::B, "b"), (Button::Trigger, "trigger"), (Button::Thumbstick, "thumbstick"), (Button::Trackpad, "trackpad"), (Button::System, "system")];
    for (b, key) in touchable {
        let touch = |touched| RigAction::TouchButton(TouchArgs { hand, button: b, touched });
        rows.push(Row { name: format!("touch {key}"), apply: touch(true), want: vec![(format!("{key}_touch"), json!(true))], undo: touch(false) });
    }
    let trigger = |value| RigAction::SetTrigger(TriggerArgs { hand, value });
    rows.push(Row {
        name: "set_trigger 0.5".into(),
        apply: trigger(0.5),
        want: want(&[("trigger_value", json!(0.5)), ("trigger_click", json!(false)), ("trigger_touch", json!(true))]),
        undo: trigger(0.0),
    });
    rows.push(Row {
        name: "set_grip 0.6 force 0.4".into(),
        apply: RigAction::SetGrip(GripArgs { hand, value: 0.6, force: Some(0.4) }),
        want: want(&[("squeeze_value", json!(0.6)), ("squeeze_force", json!(0.4))]),
        undo: RigAction::SetGrip(GripArgs { hand, value: 0.0, force: None }),
    });
    let stick = |x, y| RigAction::SetThumbstick(AxisArgs { hand, x, y, click: None, touch: None });
    rows.push(Row {
        name: "set_thumbstick 0.5 -0.5".into(),
        apply: stick(0.5, -0.5),
        want: want(&[("thumbstick", json!([0.5, -0.5])), ("thumbstick_click", json!(false)), ("thumbstick_touch", json!(true))]),
        undo: stick(0.0, 0.0),
    });
    rows.push(Row {
        name: "set_trackpad -0.4 0.3 force 0.2".into(),
        apply: RigAction::SetTrackpad(TrackpadArgs { hand, x: -0.4, y: 0.3, force: Some(0.2), touch: None }),
        want: want(&[("trackpad", json!([-0.4, 0.3])), ("trackpad_force", json!(0.2)), ("trackpad_touch", json!(true))]),
        undo: RigAction::SetTrackpad(TrackpadArgs { hand, x: 0.0, y: 0.0, force: None, touch: Some(false) }),
    });
    // system click last: SteamVR keeps it for its dashboard, which takes the probe's input away
    rows.sort_by_key(|r| r.name == "press system");
    rows
}

/// Every Index input, including the trackpad and left A/B, reaches an OpenXR
/// app through ALVR's manual button mappings and SteamVR's Index bindings, for both hands.
/// SteamVR keeps the system click for its dashboard (seen live: the probe's actions go inactive,
/// then its session drops to VISIBLE); that counts as arrival, and a second press closes it. Needs Steam closed or its GPU supported,
/// and SteamVR closed or streaming. Builds and runs examples/xr_probe.
#[test]
#[ignore]
fn live_index_inputs_reach_openxr() {
    let Some(s) = ensure_streaming() else { return };
    let probe = Probe::start();
    let mut failures = vec![];
    for hand in [Hand::Left, Hand::Right] {
        let name = if hand == Hand::Left { "left" } else { "right" };
        for row in rows(hand) {
            s.apply(row.apply).unwrap();
            let (line, ok) = probe.wait(Duration::from_secs(1), |l| shows(&l[name], &row.want));
            // released: every key of the row back to false, 0 or [0, 0]
            let released: Vec<_> = row.want.iter().map(|(k, v)| (k.clone(), zero(v))).collect();
            s.apply(row.undo).unwrap();
            let (after, let_go) = probe.wait(Duration::from_secs(1), |l| shows(&l[name], &released));
            // SteamVR may open its dashboard on the press or on the release, so decide after both
            let lost = |l: &Value| l[name]["active"] == false || l["state"] != "FOCUSED";
            let dashboard = row.name == "press system" && probe.wait(Duration::from_secs(1), lost).1;
            let verdict = match (ok, dashboard) {
                (true, _) => "ok",
                (false, true) => "dashboard opened",
                (false, false) => "MISSING",
            };
            println!("{name} {}: {verdict}, released {let_go}; pressed {} released {}", row.name, line[name], after[name]);
            if !ok && !dashboard {
                failures.push(format!("{name} {}: want {:?}, probe {}", row.name, row.want, line[name]));
            }
            if !let_go && !dashboard {
                failures.push(format!("{name} {}: not released, probe {}", row.name, after[name]));
            }
            if dashboard {
                // the dashboard is fully up once the session leaves FOCUSED; a second press closes it
                probe.wait(Duration::from_secs(3), |l| l["state"] != "FOCUSED");
                s.apply(RigAction::PressButton(ButtonArgs { hand, button: Button::System })).unwrap();
                thread::sleep(Duration::from_millis(200));
                s.apply(RigAction::ReleaseButton(ButtonArgs { hand, button: Button::System })).unwrap();
                let back = probe.wait(Duration::from_secs(10), |l| l["state"] == "FOCUSED" && l[name]["active"] == true);
                assert!(back.1, "probe did not get its focus back: {}", back.0);
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// Haptics on the probe: the probe's pulse on an A press is recorded
/// with its hand and amplitude. Needs Steam closed or its GPU supported, and SteamVR closed or streaming.
#[test]
#[ignore]
fn live_haptics_recorded() {
    let Some(s) = ensure_streaming() else { return };
    let probe = Probe::start();
    s.haptics(f64::NEG_INFINITY, true);
    let a = ButtonArgs { hand: Hand::Left, button: Button::A };
    s.apply(RigAction::PressButton(a.clone())).unwrap();
    let pulse = |e: &steamvr_mcp::headset::HapticEvent| e.hand == "left" && (e.amplitude - 0.8).abs() <= 0.05;
    let recorded = steamvr::wait_for(Duration::from_secs(1), || s.haptics(f64::NEG_INFINITY, false).iter().any(pulse));
    s.apply(RigAction::ReleaseButton(a)).unwrap();
    let events = s.haptics(f64::NEG_INFINITY, false);
    println!("{events:#?}\nprobe {}", probe.line()["left"]);
    assert!(recorded, "no left pulse at amplitude 0.8: {events:?}");
}

/// Index flavour on the probe: a bare hand's pinch acts as the trigger in an
/// OpenXR app. Needs Steam closed or its GPU supported, and SteamVR closed or streaming.
#[test]
#[ignore]
fn live_bare_hand_pinch_is_trigger() {
    let Some(s) = ensure_streaming() else { return };
    // later tests in this process expect a right controller, also after a failed assertion
    struct Reset(Arc<Stack>);
    impl Drop for Reset {
        fn drop(&mut self) {
            let open = HandGestureArgs { hand: Hand::Right, gesture: Some(Gesture::Open), curls: None };
            self.0.apply(RigAction::SetHandGesture(open)).ok();
            self.0.apply(RigAction::SetHandMode(HandModeArgs { hand: Hand::Right, mode: HandMode::Controller })).ok();
        }
    }
    let _reset = Reset(Arc::clone(&s));
    let probe = Probe::start();
    let gesture = |g| RigAction::SetHandGesture(HandGestureArgs { hand: Hand::Right, gesture: Some(g), curls: None });
    s.apply(RigAction::SetHandMode(HandModeArgs { hand: Hand::Right, mode: HandMode::Hand })).unwrap();
    s.apply(gesture(Gesture::Pinch)).unwrap();
    let value = |l: &Value| l["right"]["trigger_value"].as_f64().unwrap_or(-1.0);
    let (line, pinched) = probe.wait(Duration::from_secs(1), |l| value(l) > 0.5);
    println!("pinch: {}", line["right"]);
    assert!(pinched, "pinch did not pull the trigger: {}", line["right"]);
    s.apply(gesture(Gesture::Open)).unwrap();
    let (line, opened) = probe.wait(Duration::from_secs(1), |l| (0.0..0.1).contains(&value(l)));
    println!("open: {}", line["right"]);
    assert!(opened, "open hand did not release the trigger: {}", line["right"]);
}

fn preferred_fps(fps: f64) -> Vec<SettingValue> {
    vec![SettingValue { path: "session_settings.video.preferred_fps".into(), value: json!(fps) }]
}

/// A restart-relevant change restarts SteamVR and returns to streaming on its
/// own. Also shows get_logs carrying ALVR's rejection of a bad path and SteamVR's own log lines.
/// Needs Steam closed or its GPU supported, and SteamVR closed or streaming.
#[test]
#[ignore]
fn live_restart_relevant_setting_restarts() {
    let Some(s) = ensure_streaming() else { return };
    // later tests expect the default 72 Hz, also after a failed assertion
    struct Reset(Arc<Stack>);
    impl Drop for Reset {
        fn drop(&mut self) {
            if thread::panicking() {
                println!("reset to 72 Hz: {:?}", self.0.set_alvr_settings(preferred_fps(72.0)));
            }
        }
    }
    let _reset = Reset(Arc::clone(&s));

    for fps in [90.0, 72.0] {
        let started = Instant::now();
        let report = s.set_alvr_settings(preferred_fps(fps)).unwrap();
        let headset = s.status().headset;
        println!("{fps} Hz after {:?}: {report:?}; headset {headset:?}", started.elapsed());
        assert!(report.restarted && report.streaming, "{report:?}");
        assert_eq!(headset.map(|h| h.fps), Some(fps as f32));
    }

    let bad = vec![SettingValue { path: "session_settings.video.nope".into(), value: json!(1) }];
    let e = format!("{:#}", s.set_alvr_settings(bad).unwrap_err());
    println!("bad path: {e}");
    assert!(e.contains("session_settings.video.nope"), "{e}");
    let query = |source: &str, contains: Option<&str>| LogQuery { source: Some(source.into()), contains: contains.map(Into::into), min_level: None, limit: Some(5) };
    let rejected = s.logs(query("alvr", Some("video.nope"))).unwrap();
    println!("alvr: {rejected:#?}");
    assert!(rejected.iter().any(|l| l.level == "error"), "{rejected:?}");
    let vrserver = s.logs(query("vrserver", None)).unwrap();
    println!("vrserver: {vrserver:#?}");
    assert!(!vrserver.is_empty());
}

/// Skeletal-tracker flavour: SteamVR reports hand-tracker skeletons for both
/// hands, and switching flavour restarts SteamVR on its own. Needs Steam closed or its GPU
/// supported, and SteamVR closed or streaming.
#[test]
#[ignore]
fn live_flavour_switch() {
    let started = Instant::now();
    let Some(s) = ensure_streaming() else { return };
    println!("streaming after {:?}", started.elapsed());
    // later tests expect controllers in the Index flavour, also after a failed assertion
    struct Reset(Arc<Stack>);
    impl Drop for Reset {
        fn drop(&mut self) {
            for hand in [Hand::Left, Hand::Right] {
                self.0.apply(RigAction::SetHandMode(HandModeArgs { hand, mode: HandMode::Controller })).ok();
            }
            if thread::panicking() {
                println!("reset to Index flavour: {:?}", self.0.set_hand_tracking_flavour(false));
            }
        }
    }
    let reset = Reset(Arc::clone(&s));

    let started = Instant::now();
    let report = s.set_hand_tracking_flavour(true).unwrap();
    println!("skeletal tracker after {:?}: {report:?}", started.elapsed());
    assert!(report.restarted && report.streaming, "{report:?}");
    for hand in [Hand::Left, Hand::Right] {
        s.apply(RigAction::SetHandMode(HandModeArgs { hand, mode: HandMode::Hand })).unwrap();
    }
    let serials = ["VRLINKQ_Hand_Left", "VRLINKQ_Hand_Right"];
    let trackers = |st: &TrackedState| {
        serials.iter().all(|serial| st.devices.iter().any(|d| d.serial == *serial && d.pose_valid))
            && ["left", "right"].iter().all(|h| st.skeletons.get(*h).is_some_and(|k| k.active))
    };
    // as in live_finger_curls_per_finger: skeleton actions read inactive while the scene app starts
    let found = steamvr::wait_for(Duration::from_secs(60), || s.tracked_state(false).is_ok_and(|st| trackers(&st)));
    let state = s.tracked_state(false);
    println!("{state:#?}");
    assert!(found, "no posed {serials:?} with active skeletons after 60 s; scene app {:?}", s.scene_app());

    drop(reset); // controllers back before the switch
    let started = Instant::now();
    let report = s.set_hand_tracking_flavour(false).unwrap();
    println!("index after {:?}: {report:?}", started.elapsed());
    assert!(report.restarted && report.streaming, "{report:?}");
}

/// Not a check: ends a live window. Stops the stack this process started (or a SteamVR left by an
/// earlier process) and restores SteamVR's files. Name sorts after the other live tests, so with
/// --test-threads=1 it also prints how many of them skipped.
#[test]
#[ignore]
fn live_zz_stop_and_restore() {
    let _ = writeln!(std::io::stderr(), "live run: {} live tests skipped (VR_TEST_REQUIRE_ALL=1 fails them instead)", SKIPPED.load(Ordering::SeqCst));
    stop_and_restore().unwrap();
}

/// Stops the shared stack (or a SteamVR left by an earlier process) and restores SteamVR's files, so the
/// tests that need SteamVR closed also run after the streaming ones. Err when vr_stop refuses: SteamVR runs without ALVR.
fn stop_and_restore() -> Result<(), String> {
    let paths = Paths::detect(&Config::load().unwrap());
    let stopped = shared_stack().stop().map_err(|e| format!("{e:#}"))?;
    println!("{stopped:?}");
    assert!(stopped.remaining_processes.is_empty(), "{stopped:?}");
    if paths.restore_file().exists() {
        restore(&paths, false).unwrap();
    }
    assert!(!paths.restore_file().exists());
    Ok(())
}
