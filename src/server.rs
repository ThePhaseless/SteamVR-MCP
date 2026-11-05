use crate::{
    checks::{self, Readiness},
    headset::{Eye, Source},
    lease::Owner,
    rig::{
        AnimateArgs, AnimateControllerArgs, AxisArgs, ButtonArgs, ControllerPoseArgs, ControllerStateArgs, FingerCurlArgs, GripArgs,
        HandGestureArgs, HandModeArgs, HandSkeletonArgs, MoveArgs, MoveControllerArgs, PoseArgs, RigAction, SequenceStep, TouchArgs, TrackpadArgs,
        TriggerArgs,
    },
    stack::{LaunchArgs, LogQuery, ReadAppLogArgs, SettingValue, Stack, WaitForLogArgs},
};
use alvr_common::anyhow::{self, Result};
use base64::{Engine, engine::general_purpose::STANDARD};
use rmcp::{
    ErrorData, RoleServer, ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, tool::ToolCallContext, wrapper::Parameters},
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation, ListToolsResult,
        PaginatedRequestParams, ServerCapabilities, ServerConfig, SubscriptionFilter,
    },
    service::{NotificationContext, RequestContext, SubscriptionContext},
    tool, tool_router,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Client names of hosts that get every tool listed, where a gated one answers `not ready: <reason>`.
/// Claude Code acts on `notifications/tools/list_changed`, but its subagents keep the tool list from
/// their spawn and a mid-turn refresh drops tools the model planned to call.
pub const HOSTS_WITHOUT_LIST_CHANGED: &[&str] = &["claude-code"];

pub struct VrServer {
    stack: Arc<Stack>,
    router: ToolRouter<VrServer>,
}

/// The client name: from `initialize` on a legacy connection, from each request's `_meta` on a 2026-07-28 one.
fn client_name(context: &RequestContext<RoleServer>) -> String {
    context.client_info().map(|c| c.name).unwrap_or_default()
}

#[derive(Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct ScreenshotArgs {
    /// "headset" (default): the H264 stream the virtual headset receives. "compositor": SteamVR's uncompressed scene textures, which need a running scene app.
    #[serde(default)]
    source: Source,
    /// Which eye to show; "both" is the side-by-side frame ALVR streams.
    #[serde(default)]
    eye: Eye,
}

#[derive(Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct TrackedStateArgs {
    /// Also return the 31 OpenVR hand bones per hand (model space, without controller).
    #[serde(default)]
    include_bones: bool,
}

#[derive(Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct RestoreArgs {
    /// Drop, with a note, each recorded file whose directory is gone (an install removed on purpose), and the vrcompositor wrap when Valve's binary is gone, then delete restore.json. Never drops a file whose directory exists or cannot be checked; ignored for a restore.json that recorded no paths.
    #[serde(default)]
    forget_missing: bool,
}

#[derive(Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct ProximityArgs {
    /// true: the headset is worn; false: taken off.
    on_head: bool,
}

#[derive(Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct RecordAudioArgs {
    /// Seconds to record, 0.1 to 60; default 5.
    seconds: Option<f64>,
}

#[derive(Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct AudioLevelArgs {
    /// Seconds to measure, 0.05 to 10; default 1.
    window_s: Option<f64>,
}

#[derive(Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct FrameTimingArgs {
    /// Seconds to measure, 0.5 to 60; default 5.
    window_s: Option<f64>,
}

#[derive(Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct HapticsArgs {
    /// Only events with t_s after this; pass the last t_s seen to get new ones. Omit it after the system clock was stepped back.
    since_s: Option<f64>,
    /// Empty the log after reading.
    #[serde(default)]
    clear: bool,
}

#[derive(Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct SettingsArgs {
    values: Vec<SettingValue>,
}

#[derive(Deserialize, rmcp::schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(crate = "rmcp::schemars")]
enum Flavour {
    /// A bare hand drives the Valve Index controller device.
    Index,
    /// A bare hand is a separate full-skeleton hand-tracker device, as VRChat expects.
    SkeletalTracker,
}

#[derive(Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct FlavourArgs {
    flavour: Flavour,
}

#[derive(Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct PlayspaceArgs {
    /// Meters, above 0 and at most 1000.
    width: f32,
    /// Meters, above 0 and at most 1000.
    depth: f32,
}

#[derive(Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct SequenceArgs {
    /// 0 to 256 steps; run in at_s order, equal times in the order given.
    steps: Vec<SequenceStep>,
}

#[derive(Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct AcquireArgs {
    /// Who holds SteamVR, 1 to 64 characters; shown to an agent that finds the lease taken.
    owner: String,
    /// Seconds until the lease expires, 600 to 14400; default 1800. Any mutating tool renews it once half is left.
    ttl_s: Option<u64>,
}

/// A tool result with `v` as pretty JSON text.
fn json(v: impl Serialize) -> Result<CallToolResult> {
    Ok(CallToolResult::success(vec![ContentBlock::text(serde_json::to_string_pretty(&v)?)]))
}

/// Error text for a tool result the caller reads.
fn message(e: anyhow::Error) -> String {
    format!("{e:#}")
}

#[tool_router(vis = "pub(crate)")]
impl VrServer {
    pub fn new(stack: Arc<Stack>) -> VrServer {
        VrServer { stack, router: Self::tool_router() }
    }

    /// Runs blocking Stack work off the async runtime.
    async fn run(&self, f: impl FnOnce(&Stack) -> Result<CallToolResult> + Send + 'static) -> Result<CallToolResult> {
        let stack = Arc::clone(&self.stack);
        tokio::task::spawn_blocking(move || f(&stack)).await?
    }

    #[tool(description = "Startup checks (ok, reason, fix) as of MCP start or the last vr_start, re-run (and config.toml re-read) here when config.toml changed or a core check fails, with the ALVR dashboard check always re-run, and the current state of Steam, SteamVR, ALVR, the virtual headset (streaming, fps, resolution, codec), on_head (the proximity it reports at every stream start) and, while the OpenVR reader is connected, the scene app. Top-level dashboard_visible says whether SteamVR's dashboard is open (null when unknown: the OpenVR reader is not connected or lacks the interface). The state also carries blocked_drivers (driver_* sections SteamVR blocked in safe mode; vr_start, vr_stop and a SteamVR restart clear them), owner (who holds the SteamVR lease, and whether this MCP does; null when free) and the session clock: now_t_s (wall-clock seconds since MCP start), now_at and t0_at (RFC 3339 UTC, the instants of now and of t_s 0). t_s follows the system clock, so it jumps, or goes back, when the clock is stepped.")]
    async fn vr_status(&self) -> Result<CallToolResult, String> {
        self.run(|s| {
            let dashboard_visible = s.reader_connected().then(|| s.dashboard_visible()).flatten();
            json(serde_json::json!({ "checks": s.checks(), "state": s.status(), "dashboard_visible": dashboard_visible }))
        })
        .await
        .map_err(message)
    }

    #[tool(description = "Take or renew the lease on this machine's SteamVR under a name of your choice. Every tool that changes anything takes it by itself, so call this only to pick the name or the ttl, or to take over a lease that expired after its MCP stopped managing a session. Refuses while another live MCP holds it. The name and ttl hold until the lease is released. Returns the lease.")]
    async fn vr_acquire(&self, Parameters(args): Parameters<AcquireArgs>) -> Result<CallToolResult, String> {
        self.run(move |s| json(Owner { lease: s.acquire_lease(args.owner, args.ttl_s)?, mine: true })).await.map_err(message)
    }

    #[tool(description = "Give up this MCP's lease on SteamVR so another agent can take it at once. Returns released: false when this MCP did not hold it. Leaves SteamVR running. Refuses while this MCP still runs a session (its headset or ALVR dashboard): run vr_stop first.")]
    async fn vr_release(&self) -> Result<CallToolResult, String> {
        self.run(|s| json(serde_json::json!({ "released": s.release()? }))).await.map_err(message)
    }

    #[tool(description = "Start Steam if needed, SteamVR with ALVR and the virtual headset, and return once it streams, or with the reason it stopped. Can take minutes (up to timeouts.streaming_s, default 240 s). The result's dashboard_visible says whether SteamVR's dashboard is open at that moment (null when unknown); SteamVR opens it about 10 s after no scene app runs, so call close_dashboard once your game reaches its scene. A vr_start that gets past its preflight checks resets the rig to its defaults (head and controller poses, buttons, hand state, playspace, proximity, animations), so set poses after it. It resets also when the start then fails, for example when it cannot adopt a running SteamVR, and during a stream gap, where it builds a new session. A start refused by a preflight check (no ALVR streamer found, or a running Steam without steam.env) keeps the rig, and so does a vr_start while the headset streams, as SteamVR restarts and reconnects do. Fails at once if SteamVR's safe mode blocks ALVR's driver.")]
    async fn vr_start(&self) -> Result<CallToolResult, String> {
        self.run(|s| json(s.start()?)).await.map_err(message)
    }

    #[tool(description = "Shut SteamVR down through ALVR (with signals if ALVR hangs) and disconnect the headset. If vr_start started Steam, asks it to exit. When the MCP identified the Steam client it started, and that client still runs after 60 s, it sends that one process SIGTERM; while that client still runs afterwards, the next vr_stop tries again. Without that record it forwards one -shutdown at this vr_stop, sends no signal and keeps nothing for a later one. Once the recorded client is gone (Steam may restart it itself), the Steam running now is left alone. No other Steam process is ever signalled. A Steam you started is never asked to exit when the MCP recorded the client it started; without that record, vr_stop asks whatever Steam runs then to exit, once. -shutdown is forwarded only if a process named steam still runs. When vr_start started Steam or a client record was kept, closed_steam is true when no Steam process is left (also when Steam had already exited) and remaining_steam lists the names of the Steam processes still alive at the end; otherwise Steam is not touched and they stay false and empty. signalled_steam says whether the SIGTERM went out; steam_note says why Steam may still run (the -shutdown failed, the recorded client was gone, or no later vr_stop will close it). Ends a game that launch_app via proton started first (stopped_app lists its pids). Refuses a SteamVR that runs without ALVR. Can take minutes.")]
    async fn vr_stop(&self) -> Result<CallToolResult, String> {
        self.run(|s| json(s.stop()?)).await.map_err(message)
    }

    #[tool(description = "A PNG screenshot: both eyes side by side, or one eye. source headset (default): what the headset streams, about 18 frames after a fresh keyframe, so the image is sharper and a pose just set has usually reached the game; this is slower than taking the keyframe alone, and a slow game can still show the old aim. If the stream stalls, or a frame fails to decode (a lost frame usually does), it returns an earlier frame, at worst the keyframe; corrupt data that still decodes is not detected. source compositor: SteamVR's uncompressed scene textures; needs a scene app that has submitted a frame. Only VR frames: there is no desktop or window source.")]
    async fn screenshot(&self, Parameters(args): Parameters<ScreenshotArgs>) -> Result<CallToolResult, String> {
        self.run(move |s| {
            checks::source_ready(args.source.name(), &s.readiness()).map_err(|e| anyhow::anyhow!("not ready: {e}"))?;
            let shot = s.screenshot(args.source, args.eye)?;
            Ok(CallToolResult::success(vec![
                ContentBlock::image(STANDARD.encode(&shot.png), "image/png"),
                ContentBlock::text(serde_json::to_string(&shot)?),
            ]))
        })
        .await
        .map_err(message)
    }

    #[tool(description = "What SteamVR itself tracks: each device's class, role, serial, model, connection, pose (SteamVR standing space: meters, +Y up, -Z forward; quat x, y, z, w) and velocities; per hand the skeleton action's state, finger curls and splay as SteamVR animates them and as the driver reports them, and optionally the bones; plus the scene app's pid and state.")]
    async fn get_tracked_state(&self, Parameters(args): Parameters<TrackedStateArgs>) -> Result<CallToolResult, String> {
        self.run(move |s| {
            let state = s.tracked_state(args.include_bones)?;
            json(serde_json::json!({ "devices": state.devices, "skeletons": state.skeletons, "scene_app": s.scene_app() }))
        })
        .await
        .map_err(message)
    }

    /// Applies `a`; an animation returns once it is done.
    async fn rig(&self, a: RigAction) -> Result<CallToolResult, String> {
        let animated = matches!(a, RigAction::AnimateHead(_) | RigAction::AnimateController(_));
        let done_after = self.stack.apply(a).map_err(message)?; // a brief rig lock, no blocking call
        if !animated {
            return json(serde_json::json!({ "ok": true })).map_err(message);
        }
        tokio::time::sleep(done_after).await; // no Stack lock is held here
        json(serde_json::json!({ "done_after_s": done_after.as_secs_f32() })).map_err(message)
    }

    #[tool(description = "Set the head pose in SteamVR standing space (meters, +Y up, -Z forward): position plus yaw/pitch/roll in degrees, or a quaternion x, y, z, w. Stops an animation.")]
    async fn set_head_pose(&self, Parameters(args): Parameters<PoseArgs>) -> Result<CallToolResult, String> {
        self.rig(RigAction::SetHeadPose(args)).await
    }

    #[tool(description = "Move the head relative to where it is: offset in meters along the playspace axes or, with frame \"local\", the head's own axes; yaw/pitch/roll in degrees about the head's own axes.")]
    async fn move_head(&self, Parameters(args): Parameters<MoveArgs>) -> Result<CallToolResult, String> {
        self.rig(RigAction::MoveHead(args)).await
    }

    #[tool(description = "Move the head smoothly from its current pose through the waypoints, evenly spaced over duration_s, sending velocities as a real headset does. Returns when done.")]
    async fn animate_head(&self, Parameters(args): Parameters<AnimateArgs>) -> Result<CallToolResult, String> {
        self.rig(RigAction::AnimateHead(args)).await
    }

    #[tool(description = "Set a Valve Index controller's raw pose, in the playspace or relative to the head's pose now (relative_to \"head\"). ALVR moves it by its controller offset (11 cm along the controller's own -Z by default) and games aim and grip from poses SteamVR derives from it, so a level pose can aim tilted. Same pose fields as set_head_pose.")]
    async fn set_controller_pose(&self, Parameters(args): Parameters<ControllerPoseArgs>) -> Result<CallToolResult, String> {
        self.rig(RigAction::SetControllerPose(args)).await
    }

    #[tool(description = "Move a controller relative to where it is, as move_head does for the head.")]
    async fn move_controller(&self, Parameters(args): Parameters<MoveControllerArgs>) -> Result<CallToolResult, String> {
        self.rig(RigAction::MoveController(args)).await
    }

    #[tool(description = "Animate a controller through waypoints, as animate_head does; waypoints are in the playspace or relative to the head's pose at the call. Returns when done.")]
    async fn animate_controller(&self, Parameters(args): Parameters<AnimateControllerArgs>) -> Result<CallToolResult, String> {
        self.rig(RigAction::AnimateController(args)).await
    }

    #[tool(description = "Connect or disconnect a controller. ALVR 21 has one flag for both connected and tracked (Controller.cpp:187-205), so either false makes the controller disconnected and untracked. Input tools refuse for it.")]
    async fn set_controller_state(&self, Parameters(args): Parameters<ControllerStateArgs>) -> Result<CallToolResult, String> {
        let active = args.active();
        self.rig(RigAction::SetControllerState(args)).await?;
        json(serde_json::json!({
            "ok": true,
            "connected": active,
            "tracked": active,
            "note": "ALVR 21 sets connected and tracked from one flag per hand (Controller.cpp:187-205)",
        }))
        .map_err(message)
    }

    #[tool(description = "Curl the thumb, index, middle, ring and pinky fingers (0 straight to 1 curled) and set the splay (0 together to 1 spread); an unset value keeps its last value. The hand model turns them into a 26-joint skeleton sent with the controller pose. SteamVR gets index to pinky also as /input/finger/* and the thumb through the skeleton only.")]
    async fn set_finger_curl(&self, Parameters(args): Parameters<FingerCurlArgs>) -> Result<CallToolResult, String> {
        self.rig(RigAction::SetFingerCurl(args)).await
    }

    #[tool(description = "Per hand: hold a Valve Index controller (\"controller\") or be a bare tracked hand (\"hand\"), which sends only a skeleton, open unless set. With ALVR's default Index flavour a bare hand drives the Index controller device, and ALVR's gesture mapping turns pinches into buttons. Immediate.")]
    async fn set_hand_mode(&self, Parameters(args): Parameters<HandModeArgs>) -> Result<CallToolResult, String> {
        self.rig(RigAction::SetHandMode(args)).await
    }

    #[tool(description = "Send raw hand joints: 26 entries in OpenXR XR_EXT_hand_tracking order (palm, wrist, thumb metacarpal..tip, index..little metacarpal..tip), each px, py, pz in meters in the playspace and qx, qy, qz, qw (-Z along the bone toward the tip, +Y the back of the hand). Holds until the next curl or gesture call.")]
    async fn set_hand_skeleton(&self, Parameters(args): Parameters<HandSkeletonArgs>) -> Result<CallToolResult, String> {
        self.rig(RigAction::SetHandSkeleton(args)).await
    }

    #[tool(description = "Pose a hand through the hand model: give either gesture (open, fist, point, pinch, thumbs_up) or custom curls (thumb, index, middle, ring, pinky, splay, each 0 to 1, unset 0), not both. A bare hand's pinch meets ALVR's pinch distance, so it acts as the trigger.")]
    async fn set_hand_gesture(&self, Parameters(args): Parameters<HandGestureArgs>) -> Result<CallToolResult, String> {
        self.rig(RigAction::SetHandGesture(args)).await
    }

    #[tool(description = "Press a Valve Index button and hold it until release_button: a, b, system, thumbstick (click and touch), trigger (value 1, click, touch), grip (squeeze value 1, force 0) or trackpad (force 1, touch). Refuses for an untracked controller or a bare hand.")]
    async fn press_button(&self, Parameters(args): Parameters<ButtonArgs>) -> Result<CallToolResult, String> {
        self.rig(RigAction::PressButton(args)).await
    }

    #[tool(description = "Release a button: everything press_button set for it goes back to false or 0.")]
    async fn release_button(&self, Parameters(args): Parameters<ButtonArgs>) -> Result<CallToolResult, String> {
        self.rig(RigAction::ReleaseButton(args)).await
    }

    #[tool(description = "Touch or untouch a button's capacitive sensor without pressing it: a, b, system, trigger, thumbstick or trackpad. The grip has no touch in ALVR's Index profile.")]
    async fn touch_button(&self, Parameters(args): Parameters<TouchArgs>) -> Result<CallToolResult, String> {
        self.rig(RigAction::TouchButton(args)).await
    }

    #[tool(description = "Set the trigger value, 0 to 1; click follows at >= 0.95 and touch at > 0.")]
    async fn set_trigger(&self, Parameters(args): Parameters<TriggerArgs>) -> Result<CallToolResult, String> {
        self.rig(RigAction::SetTrigger(args)).await
    }

    #[tool(description = "Set the grip (squeeze) value and force, each 0 to 1; force defaults to 0.")]
    async fn set_grip(&self, Parameters(args): Parameters<GripArgs>) -> Result<CallToolResult, String> {
        self.rig(RigAction::SetGrip(args)).await
    }

    #[tool(description = "Set the thumbstick: x and y from -1 to 1 (+x right, +y up), click (default false), touch (default: deflected or clicked). Holds until changed; set 0, 0 to center it.")]
    async fn set_thumbstick(&self, Parameters(args): Parameters<AxisArgs>) -> Result<CallToolResult, String> {
        self.rig(RigAction::SetThumbstick(args)).await
    }

    #[tool(description = "Set the trackpad: x and y from -1 to 1 (+x right, +y up), force 0 to 1 (default 0), touch (default true). Holds until changed; send touch false to lift the finger.")]
    async fn set_trackpad(&self, Parameters(args): Parameters<TrackpadArgs>) -> Result<CallToolResult, String> {
        self.rig(RigAction::SetTrackpad(args)).await
    }

    #[tool(description = "Set the play area to width by depth meters (the chaperone). The size is sent to ALVR as half extents, because ALVR draws the chaperone at twice what it receives. Warns when ALVR's recentering_mode is not Stage, since ALVR then also re-origins the playspace around the head's last pose.")]
    async fn set_playspace(&self, Parameters(args): Parameters<PlayspaceArgs>) -> Result<CallToolResult, String> {
        self.run(move |s| json(s.set_playspace(args.width, args.depth)?)).await.map_err(message)
    }

    #[tool(description = "Send the current play area to ALVR again, which makes ALVR recenter. With the MCP's default Stage recentering this is the identity: nothing moves. Any other recentering_mode re-origins the playspace around the head's last pose.")]
    async fn recenter(&self) -> Result<CallToolResult, String> {
        json(self.stack.recenter()).map_err(message)
    }

    #[tool(description = "Tell SteamVR the headset is worn (on_head true) or taken off (false), as the proximity sensor does. get_tracked_state shows SteamVR's last worn or removed event as the hmd's head_proximity (null before any). The headset is worn until set_proximity says otherwise; the last value is resent at every stream start.")]
    async fn set_proximity(&self, Parameters(args): Parameters<ProximityArgs>) -> Result<CallToolResult, String> {
        self.run(move |s| {
            s.set_proximity(args.on_head)?; // connects the OpenVR reader: up to OPENVR_TIMEOUT
            json(serde_json::json!({ "ok": true, "on_head": args.on_head }))
        })
        .await
        .map_err(message)
    }

    #[tool(description = "Run timed steps: each has at_s (seconds from the start, 0 to 600), action (a rig tool's name such as set_head_pose, animate_controller, set_trigger, press_button, set_finger_curl or set_hand_gesture) and args (that tool's arguments). Animations start and do not block later steps. Every step is checked first, including against the rig's state (a trigger of 7, an untracked controller, inputs to a bare hand): a refused one fails the call, naming the step, before any runs and with no state changed. Returns once the last step and the latest animation are done: started_t_s and started_at (the session clock when it began), per step at_s, action, applied_s, ok and error, plus total_s. Can take minutes: it lasts as long as its steps.")]
    async fn run_sequence(&self, Parameters(args): Parameters<SequenceArgs>) -> Result<CallToolResult, String> {
        json(self.stack.run_sequence(args.steps).await.map_err(message)?).map_err(message)
    }

    #[tool(description = "Haptic events the app sent, oldest first: t_s (wall-clock seconds since MCP start; it jumps or goes back if the system clock is stepped, and after a backward step omit since_s), at (the same instant as RFC 3339 UTC), hand (left, right or other), duration_s, frequency (Hz) and amplitude (0 to 1). Keeps the newest 1024.")]
    async fn get_haptics(&self, Parameters(args): Parameters<HapticsArgs>) -> Result<CallToolResult, String> {
        self.run(move |s| json(s.haptics(args.since_s.unwrap_or(f64::NEG_INFINITY), args.clear))).await.map_err(message)
    }

    #[tool(description = "Wait window_s (0.5 to 60, default 5), then report frame timing over it: compositor (SteamVR's GetFrameTimings for frames within window_s of the newest: frames, fps, interval_ms and gpu_ms p50/p95/p99 by nearest rank, dropped, mispresented, reprojected frames; nominal_hz is the headset's display frequency and throttled is true when fps is under 90% of it, which tells SteamVR throttling to 36/24/18 Hz from a real refresh change; both null when unknown), counts (compositor presents, dropped and reprojected frames during the window) and alvr (mean of ALVR's statistics summaries: samples, latencies in ms, client_fps, server_fps, video_mbits_per_sec), t_s and at of the window's end. A part that could not be read is null and its error is in notes. Limit: the MCP asks SteamVR for up to 1024 frames, and SteamVR has been seen to return only about the newest 128, so at a high compositor rate the compositor block covers less than window_s; counts covers the whole window.")]
    async fn get_frame_timing(&self, Parameters(args): Parameters<FrameTimingArgs>) -> Result<CallToolResult, String> {
        self.run(move |s| json(s.frame_timing(args.window_s)?)).await.map_err(message)
    }

    #[tool(description = "Record seconds (0.1 to 60, default 5) of what the default audio output plays, from its monitor through PipeWire, into a WAV file (16-bit stereo, 48 kHz) in the MCP's private runtime dir. Returns path, seconds, sample_rate, channels, left and right ({rms_dbfs, peak_dbfs}, silence is -120), t_s and at of the end. Limit: it holds everything the desktop plays, other apps included, and the files stay until logout. Needs the game's audio to reach the default output.")]
    async fn record_audio(&self, Parameters(args): Parameters<RecordAudioArgs>) -> Result<CallToolResult, String> {
        self.run(move |s| json(s.record_audio(args.seconds)?)).await.map_err(message)
    }

    #[tool(description = "Wait window_s (0.05 to 10, default 1), then report the level of what the default audio output plays over it, from its monitor through PipeWire: left and right ({rms_dbfs, peak_dbfs}, silence is -120), window_s, t_s and at of the end. Limit: it hears everything the desktop plays, other apps included.")]
    async fn audio_level(&self, Parameters(args): Parameters<AudioLevelArgs>) -> Result<CallToolResult, String> {
        self.run(move |s| json(s.audio_level(args.window_s)?)).await.map_err(message)
    }

    #[tool(description = "Set ALVR session values by path (ALVR's syntax from the session JSON root, e.g. session_settings.video.preferred_fps) and check ALVR stored them. Unless ALVR's settings schema flags every path real-time, SteamVR then restarts and the call returns once the headset streams again (restarted, restarts, streaming). Needs a vr_start session. Paths under client_connections, session_settings.connection or session_settings.headset.vmc (network exposure) are refused. A bad or refused path or value fails before any restart. With a restart the call can take minutes.")]
    async fn set_alvr_settings(&self, Parameters(args): Parameters<SettingsArgs>) -> Result<CallToolResult, String> {
        self.run(move |s| json(s.set_alvr_settings(args.values)?)).await.map_err(message)
    }

    #[tool(description = "How a bare tracked hand appears in SteamVR: \"index\" (ALVR's default; it drives the Index controller and pinches act as buttons) or \"skeletal_tracker\" (a separate full-skeleton hand tracker per hand). Sets ALVR's steamvr_input_2_0 through set_alvr_settings, so SteamVR restarts and the call can take minutes.")]
    async fn set_hand_tracking_flavour(&self, Parameters(args): Parameters<FlavourArgs>) -> Result<CallToolResult, String> {
        let skeletal = matches!(args.flavour, Flavour::SkeletalTracker);
        self.run(move |s| json(s.set_hand_tracking_flavour(skeletal)?)).await.map_err(message)
    }

    #[tool(description = "ALVR log events (the newest 1000 since MCP start) and the tails of SteamVR's vrserver, vrcompositor and vrmonitor logs, merged in time order: t_s, at (session clock: wall-clock seconds since MCP start, and RFC 3339 UTC; after the system clock is stepped back, newer lines can sort before older ones and fall outside limit), t (local time of day), source, level, text. Filter by source (alvr, vrserver, vrcompositor, vrmonitor or all), text (contains, ignoring case) and min_level (error, warning, info, debug; applies to ALVR events only, SteamVR lines are filtered by text only). Returns the newest limit lines (1 to 1000, default 100).")]
    async fn get_logs(&self, Parameters(args): Parameters<LogQuery>) -> Result<CallToolResult, String> {
        self.run(move |s| json(s.logs(args)?)).await.map_err(message)
    }

    #[tool(description = "Lines of a game's own log files, tailed as config.toml's [app_logs] table says for this app id: {app_id, files (matched now), lines (seq, t_s, at, file, text; seq >= since_seq, oldest first), next_seq}. Filter by contains (ignoring case); returns the oldest limit matching lines (1 to 500, default 500). t_s and at give the moment the MCP read the line. Each app keeps its newest 5000 lines; launch_app clears them. Pass the last next_seq as since_seq to read on.")]
    async fn read_app_log(&self, Parameters(args): Parameters<ReadAppLogArgs>) -> Result<CallToolResult, String> {
        self.run(move |s| json(s.read_app_log(args)?)).await.map_err(message)
    }

    #[tool(description = "Wait until a line of the app's [app_logs] files matches pattern (a regex) or fail_pattern, whichever line comes first, fail_pattern checked first on each line. Returns {result: \"match\" | \"fail\" | \"timeout\", line, next_seq}; fails with \"failed: app exited\" once the Proton launcher or the scene app running at the call's start is gone, after reading its files to their end, an unfinished last line included; that read lasts until the files stop growing, at most 2 s, which can run past timeout_s. Starts at since_seq (default: the oldest line kept, the first since launch_app), so lines logged before the call count. Bad arguments fail at once. Can take minutes (timeout_s, up to 600).")]
    async fn wait_for_log(&self, Parameters(args): Parameters<WaitForLogArgs>) -> Result<CallToolResult, String> {
        self.run(move |s| json(s.wait_for_log(args)?)).await.map_err(message)
    }

    #[tool(description = "Launch an app after checking it exists, and return once SteamVR reports its scene running: the app (app_id, game_id, kind, name, compat_tool such as a Proton version), scene_pid, via, run_log and notes (when any). If SteamVR's dashboard is open then, it clicks the right controller's system button once to close it (see close_dashboard). via \"steam\" (default) starts a Steam app or non-Steam shortcut through Steam; exe, args and env are refused. via \"proton\" runs a Steam app's compatibility tool directly, as Steam would (the app's tool, else Steam's default; through the Steam Linux Runtime when the tool requires it), not for SteamVR itself, with exe (required, relative to the install dir, \\ allowed, must stay inside it; a Windows .exe, not a shell, interpreter or loader), args (passed as given; Chromium/CEF switches that run a program are refused) and env (over the desktop session's, e.g. GPU selection; only the graphics, Wine and Proton tuning names the env field lists). The user's Steam launch options do not apply in proton mode, and Steam shows no dialogs; the launcher's output goes to run_log, and the call fails at once if the launcher exits first. Otherwise fails with the reason: a Steam dialog waiting on the desktop, or a timeout with the last vrserver log lines. Can take minutes (up to timeouts.app_s, default 180 s).")]
    async fn launch_app(&self, Parameters(args): Parameters<LaunchArgs>) -> Result<CallToolResult, String> {
        self.run(move |s| json(s.launch_app(args)?)).await.map_err(message)
    }

    #[tool(description = "Close SteamVR's dashboard if it is open: click the right controller's system button once, then wait up to 3 s for it to close. An app started under an open dashboard stays unfocused (OpenXR VISIBLE, not FOCUSED) until then; launch_app does this by itself, so call it after starting a game another way. Never clicks when the dashboard is closed or its state unknown. Returns closed, was_visible (null when unknown) and note; one click only, as a second would reopen it.")]
    async fn close_dashboard(&self) -> Result<CallToolResult, String> {
        self.run(|s| json(s.close_dashboard()?)).await.map_err(message)
    }

    #[tool(description = "Stop the app: SIGTERM to the scene app SteamVR reports (not SteamVR Home), to the process group of a launch_app via \"proton\", and to every process of this user with the game's STEAM_COMPAT_DATA_PATH and app id (its wine processes); SIGKILL after 15 s. Never signals SteamVR's own processes (app 250820), the MCP's own process group, its ancestors, or pid 0 or 1. Returns stopped_pid (the scene app's, or null) and stopped (every pid and group signalled).")]
    async fn stop_app(&self) -> Result<CallToolResult, String> {
        self.run(|s| json(s.stop_app()?)).await.map_err(message)
    }

    #[tool(description = "Put back the SteamVR and ALVR setup from before the first vr_start. Refuses while SteamVR runs. Returns restored and notes; restored is false when a step was skipped (see notes) and restore.json stays for a retry. A file whose directory is gone is skipped: mount its drive and run vr_restore again, or, if that install was removed on purpose, run it with forget_missing true to drop those files and finish.")]
    async fn vr_restore(&self, Parameters(args): Parameters<RestoreArgs>) -> Result<CallToolResult, String> {
        self.run(move |s| json(s.restore(args.forget_missing)?)).await.map_err(message)
    }
}

impl VrServer {
    /// Readiness read now, after re-running the startup checks if `recheck`.
    async fn readiness(&self, recheck: bool) -> Result<Readiness, ErrorData> {
        let stack = Arc::clone(&self.stack);
        tokio::task::spawn_blocking(move || {
            if recheck {
                stack.recheck();
            }
            stack.readiness()
        })
        .await
        .map_err(|e| ErrorData::internal_error(e.to_string(), None))
    }

    /// `not ready: <reason>` when `tool` may not run now; otherwise takes the lease for a mutating tool.
    async fn refusal(&self, tool: &str) -> Result<Option<String>, ErrorData> {
        // at each vr_start config.toml is re-read (applied while nothing runs) and the static checks run
        // again, so a fixed problem does not need an MCP restart
        let r = self.readiness(tool == "vr_start").await?;
        if let Err(reason) = checks::tool_ready(tool, &r) {
            return Ok(Some(format!("not ready: {reason}")));
        }
        if !checks::READ_ONLY.contains(&tool) {
            let stack = Arc::clone(&self.stack);
            let claimed = tokio::task::spawn_blocking(move || stack.claim())
                .await
                .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
            if let Err(e) = claimed {
                return Ok(Some(format!("not ready: {e:#}")));
            }
        }
        Ok(None)
    }

    /// Makes the watcher thread send `notifications/tools/list_changed` through `notify`.
    fn forward_changes<F: Future<Output = ()> + Send + 'static>(&self, notify: impl Fn() -> F + Send + Sync + 'static) {
        let runtime = tokio::runtime::Handle::current();
        self.stack.set_on_change(Box::new(move || {
            runtime.spawn(notify());
        }));
    }
}

impl ServerHandler for VrServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().enable_tool_list_changed().build())
            .with_server_info(Implementation::new("SteamVR-MCP", env!("CARGO_PKG_VERSION")))
    }

    /// Legacy lifecycle (`initialize`): notifications go straight to the peer.
    async fn on_initialized(&self, context: NotificationContext<RoleServer>) {
        let client = context.peer.peer_info().map(|i| i.client_info.name.clone()).unwrap_or_default();
        eprintln!("SteamVR-MCP: client {client:?} initialized");
        let peer = context.peer;
        self.forward_changes(move || {
            let peer = peer.clone();
            async move { peer.notify_tool_list_changed().await.unwrap_or_else(|e| eprintln!("SteamVR-MCP: {e}")) }
        });
    }

    fn accepted_subscription_filter(&self, requested: &SubscriptionFilter) -> Option<SubscriptionFilter> {
        Some(requested.supported_by(&self.get_info().capabilities))
    }

    /// 2026-07-28 lifecycle: notifications go through the client's `subscriptions/listen` request.
    async fn listen(&self, context: SubscriptionContext) -> Result<(), ErrorData> {
        eprintln!("SteamVR-MCP: client {:?} listens", client_name(context.request_context()));
        let sink = context.sink().clone();
        self.forward_changes(move || {
            let sink = sink.clone();
            async move { sink.notify_tool_list_changed().await.unwrap_or_else(|e| eprintln!("SteamVR-MCP: {e}")) }
        });
        context.cancelled().await;
        Ok(())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let r = self.readiness(false).await?;
        let list_all = HOSTS_WITHOUT_LIST_CHANGED.contains(&client_name(&context).as_str());
        let tools = self.router.list_all().into_iter().filter(|t| list_all || checks::tool_ready(&t.name, &r).is_ok()).collect();
        Ok(ListToolsResult { tools, ..Default::default() })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        if let Some(refused) = self.refusal(&request.name).await? {
            return Ok(CallToolResult::error(vec![ContentBlock::text(refused)]).into());
        }
        self.router.call(ToolCallContext::new(self, request, context)).await
    }
}

pub async fn serve(stack: Arc<Stack>) -> Result<()> {
    VrServer::new(stack).serve(rmcp::transport::stdio()).await?.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_spec_tools_registered() {
        let mut names: Vec<String> = VrServer::tool_router().list_all().into_iter().map(|t| t.name.to_string()).collect();
        names.sort();
        let mut want: Vec<String> = "vr_status vr_start vr_stop vr_restore set_alvr_settings set_hand_tracking_flavour get_logs \
            set_head_pose move_head animate_head set_controller_pose move_controller animate_controller set_controller_state \
            press_button release_button touch_button set_trigger set_grip set_thumbstick set_trackpad \
            set_finger_curl set_hand_mode set_hand_skeleton set_hand_gesture \
            set_playspace recenter run_sequence screenshot get_haptics get_tracked_state launch_app close_dashboard stop_app vr_acquire vr_release \
            read_app_log wait_for_log set_proximity get_frame_timing record_audio audio_level"
            .split_whitespace()
            .map(String::from)
            .collect();
        want.sort();
        assert_eq!((names.len(), names), (42, want));
    }

    #[tokio::test]
    async fn a_foreign_live_lease_refuses_mutating_calls_only() {
        let dir = std::env::temp_dir().join(format!("SteamVR-MCP-server-lease-{}", std::process::id()));
        crate::steamvr::private_dir(&dir).unwrap();
        let other = crate::lease::Lease {
            owner: "other agent".into(),
            pid: std::os::unix::process::parent_id(), // alive, and not us
            expires_at: crate::clock::rfc3339(std::time::SystemTime::now() + std::time::Duration::from_secs(600)),
        };
        std::fs::write(dir.join("owner.json"), serde_json::to_vec(&other).unwrap()).unwrap();
        let server = VrServer::new(Stack::for_tests(dir.clone()));
        let refused = server.refusal("vr_stop").await.unwrap().unwrap_or_default();
        assert!(refused.starts_with("not ready: ") && refused.contains("leased to other agent"), "{refused}");
        assert_eq!(server.refusal("vr_status").await.unwrap(), None, "read-only tools stay open");
        assert_eq!(crate::lease::read(&dir), Some(other), "the holder's lease is untouched");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn slow_tools_say_so() {
        for tool in VrServer::tool_router().list_all() {
            let slow = ["vr_start", "vr_stop", "set_alvr_settings", "set_hand_tracking_flavour", "launch_app", "run_sequence", "wait_for_log"];
            let says = tool.description.as_deref().unwrap_or_default().contains("minutes");
            assert_eq!(says, slow.contains(&tool.name.as_ref()), "{}", tool.name);
        }
    }
}
