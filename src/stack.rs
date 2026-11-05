use crate::{
    alvr_api::{self, Api},
    audio,
    applog::{self, AppLogs},
    checks::{self, Check, Readiness},
    clock,
    config::{Config, Paths},
    headset::{self, Eye, HapticEvent, Headset, HeadsetStatus, Screenshot, Source},
    lease::{self, Lease, Owner},
    openvr::{self, OpenVr},
    proton,
    rig::{self, AppliedStep, Button, ButtonArgs, Hand, Rig, RigAction, SequenceReport, SequenceStep},
    setup,
    steamvr::{self, STEAMVR_URL, VR_PROCESS_NAMES},
};
use alvr_common::{
    ConnectionState, LogEntry, LogSeverity,
    anyhow::{Context, Result, anyhow, bail},
    error,
    parking_lot::Mutex,
    warn,
};
use alvr_events::{Event, EventType};
use alvr_packets::{PathSegment, PathValuePair, parse_path, path_to_string};
use alvr_session::{RecenteringMode, SessionConfig, Settings, settings_schema::SchemaNode};
use chrono::{Local, NaiveDateTime, TimeZone};
use serde::{Deserialize, Serialize};
use regex::Regex;
use serde_json::Value;
use std::{
    collections::{BTreeMap, VecDeque},
    path::{Path, PathBuf},
    process::Child,
    sync::{
        Arc, Weak,
        atomic::{AtomicUsize, Ordering::SeqCst},
    },
    thread,
    time::{Duration, Instant, SystemTime},
};

// client_core/src/connection.rs SERVER_RESTART_MESSAGE, shown after ServerControlPacket::Restarting
const SERVER_RESTART_MESSAGE: &str = "The streamer is restarting\nPlease wait...";

/// client_core's set_hud_message puts version, hostname and IP before the message.
fn is_restart_hud(hud: &str) -> bool {
    hud.ends_with(SERVER_RESTART_MESSAGE)
}

const WATCHDOG: Duration = Duration::from_secs(30);

/// How long a stream must hold before the watchdog may fire again: one that drops at once would loop SteamVR restarts.
const WATCHDOG_REARM: Duration = Duration::from_secs(60);

/// Watchdog SteamVR restarts per vr_start; past them vr_status warns instead.
const WATCHDOG_MAX: u32 = 3;

const WATCHDOG_GAVE_UP: &str = "the headset went quiet again after 3 watchdog SteamVR restarts, which the watchdog no longer repeats; run vr_start";

const WATCHDOG_STAYED_QUIET: &str = "the watchdog restarted SteamVR but the headset stayed quiet; run vr_start";

/// The supervisor's watchdog, reset by vr_start.
#[derive(Debug, Default)]
struct Watchdog {
    fired: bool,
    restarts: u32,
    gave_up: bool,
    /// Fired, then the headset stayed quiet past 2 × WATCHDOG: no restart follows until a stream re-arms it.
    stayed_quiet: bool,
}

impl Watchdog {
    /// One supervisor tick: re-armed by a stream that held WATCHDOG_REARM; true when it fires on a headset quiet
    /// past WATCHDOG, once per arming and WATCHDOG_MAX times per vr_start, after which it gives up.
    fn fires(&mut self, streaming_for: Option<Duration>, quiet_for: Option<Duration>) -> bool {
        if streaming_for.is_some_and(|d| d >= WATCHDOG_REARM) {
            (self.fired, self.stayed_quiet) = (false, false);
        }
        // a fired watchdog waits for a stream that never came back or held too briefly: say so
        self.stayed_quiet |= self.fired && quiet_for.is_some_and(|q| q > 2 * WATCHDOG);
        if self.fired || !quiet_for.is_some_and(|q| q > WATCHDOG) {
            return false;
        }
        if self.restarts >= WATCHDOG_MAX {
            self.gave_up = true;
            return false;
        }
        (self.fired, self.restarts) = (true, self.restarts + 1);
        true
    }
}

const LEAKED: &str = "headset leaked; restart the MCP to free a CPU core";

const RESTART_FAILED: &str = "the last SteamVR restart failed and the headset is not streaming; run vr_start";

const SESSION_LOST: &str = "SteamVR exited without vr_stop (it crashed or was quit), so the session ended; run vr_start";

const WEDGED: &str = "OpenVR client wedged (an OpenVR call never returned); restart the MCP";

const HEADSET_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a cold vr_start waits for vrserver to log ALVR's HMD activation before it resumes the headset.
/// Below WATCHDOG, which counts the paused time as quiet and would otherwise restart SteamVR on a slow gate.
const HMD_ACTIVATION_WAIT: Duration = Duration::from_secs(20);

/// Bound on each OpenVR call, connect and disconnect: a stale connection must not hang a tool.
const OPENVR_TIMEOUT: Duration = Duration::from_secs(5);

/// Runs `f` on its own thread and returns its result, or None if it is still running after
/// `timeout`; the thread is then left running (detached).
pub(crate) fn within<T: Send + 'static>(timeout: Duration, f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || tx.send(f()).ok());
    rx.recv_timeout(timeout).ok()
}

/// One call counted in `openvr_calls`; dropping it, also in a panic, uncounts it.
struct Pending(Arc<AtomicUsize>);

impl Drop for Pending {
    fn drop(&mut self) {
        self.0.fetch_sub(1, SeqCst);
    }
}

/// `within(timeout, f)`, counted in `calls` until `f` returns and, after a timeout, until its result is dropped:
/// a late OpenVR reader's drop is VR_ShutdownInternal, which must not overlap a new connect.
fn counted_call<T: Send + 'static>(calls: &Arc<AtomicUsize>, timeout: Duration, f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    calls.fetch_add(1, SeqCst);
    let pending = Pending(Arc::clone(calls));
    // a tuple drops in field order: a result nobody received goes before its count
    within(timeout, move || (f(), pending)).map(|(out, _pending)| out)
}

/// A file stem no earlier MCP run used, so a stray old file is never read back.
fn unique_stem(wall: SystemTime, pid: u32) -> String {
    let ms = wall.duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_millis());
    format!("vr-{ms}-{pid}")
}

/// The file's bytes once its size is non-zero and the same on two polls 100 ms apart: vrserver writes it on its own schedule.
/// Fails at once when `exited` names a SteamVR process gone, or when vrcompositor's log since the request (`compositor_log`) shows
/// this request failed; the request is the one for `path`'s stem (`<stem>.png`).
fn read_when_stable(path: &Path, timeout: Duration, mut exited: impl FnMut() -> Option<&'static str>, mut compositor_log: impl FnMut() -> String) -> Result<Vec<u8>> {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
    let (mut last, mut gone, mut failed) = (0, None, None);
    let stable = steamvr::wait_for(timeout, || {
        gone = exited();
        failed = compositor_shot(&compositor_log(), stem).1.map(str::to_owned);
        let size = std::fs::metadata(path).map_or(0, |m| m.len());
        gone.is_some() || failed.is_some() || (size > 0 && size == std::mem::replace(&mut last, size))
    });
    if let Some(name) = gone {
        bail!("failed: {name} exited before SteamVR wrote the screenshot to {}", path.display());
    }
    if let Some(line) = failed {
        bail!("failed: vrcompositor could not capture the scene app's textures ({line}); OpenXR or Proton scene apps may not support compositor screenshots; use source headset");
    }
    if !stable {
        let why = match compositor_shot(&compositor_log(), stem).0 {
            true => "vrcompositor logged the request but wrote no file",
            false => "vrcompositor logged no screenshot request; vrserver may not see this directory",
        };
        bail!("SteamVR wrote no screenshot to {} (deadline {} s); {why}", path.display(), timeout.as_secs_f64());
    }
    std::fs::read(path).with_context(|| path.display().to_string())
}

/// Whether vrcompositor's log shows the stereo screenshot request for `stem`, and its `Screenshot: Failed` line if it logged one:
/// one before the next request, since tool calls run concurrently and another request's failure is not this one's.
fn compositor_shot<'a>(log: &'a str, stem: &str) -> (bool, Option<&'a str>) {
    const REQUEST: &str = "Take stereo screenshot request";
    let ours = format!("/{stem}-preview");
    let mut lines = log.lines().skip_while(|l| !(l.contains(REQUEST) && l.contains(&ours)));
    let requested = lines.next().is_some();
    let failed = lines.take_while(|l| !l.contains(REQUEST)).find_map(|l| l.find("Screenshot: Failed").map(|i| l[i..].trim_end()));
    (requested, failed)
}

/// True when the session lists `hostname` in any state but Disconnected: ALVR's handshake loop
/// connects only to Disconnected entries (server_core/src/connection.rs:395-405).
fn client_stuck(session: &SessionConfig, hostname: &str) -> bool {
    session.client_connections.get(hostname).is_some_and(|c| c.connection_state != ConnectionState::Disconnected)
}

/// client_stuck on the session file, which ALVR saves on every client state change (server_io
/// update_client_connections): read without the API, which a stuck ALVR may not answer. False when unreadable.
fn stuck_in_session_file(session_file: &Path, hostname: &str) -> bool {
    let mut session = SessionConfig::default();
    if let Some(json) = std::fs::read_to_string(session_file).ok().and_then(|t| serde_json::from_str(&t).ok()) {
        session.merge_from_json(&json).ok();
    }
    client_stuck(&session, hostname)
}

/// Whether `e` holds a socket read that timed out: ALVR took the request and never answered.
fn timed_out(e: &alvr_common::anyhow::Error) -> bool {
    use std::io::ErrorKind::{TimedOut, WouldBlock};
    e.chain().filter_map(|c| c.downcast_ref::<std::io::Error>()).any(|e| matches!(e.kind(), WouldBlock | TimedOut))
}

#[derive(Debug, Default, Serialize)]
pub struct StartReport {
    pub already_streaming: bool,
    pub adopted: bool,
    pub started_steam: bool,
    pub restarts: u32,
    pub unblocked_safe_mode: bool,
    pub view_resolution: [u32; 2],
    pub fps: f32,
    pub codec: Option<String>,
    /// Whether SteamVR's dashboard is open when vr_start returns; null when unknown.
    pub dashboard_visible: Option<bool>,
    pub notes: Vec<String>,
}

/// vr_restore's result. restored is false while restore.json stays for a retry (a step was skipped; see notes).
#[derive(Debug, Default, Serialize)]
pub struct RestoreReport {
    pub restored: bool,
    pub notes: Vec<String>,
}

#[derive(Debug, Default, Serialize)]
pub struct StopReport {
    pub remaining_processes: Vec<String>,
    /// Pids and groups of the `launch_app via proton` game that vr_stop ended before SteamVR.
    pub stopped_app: Vec<u32>,
    /// True when vr_stop asked the Steam vr_start started to exit and no Steam process is left, also when Steam had already exited.
    pub closed_steam: bool,
    /// Whether SteamVR needed signals.
    pub used_signals: bool,
    /// Names of the Steam processes still alive after that, zombies left out; a later vr_stop tries again only while the recorded client lives.
    pub remaining_steam: Vec<String>,
    /// Whether the Steam client vr_start started outlived -shutdown and got SIGTERM; no other Steam process is ever signalled.
    pub signalled_steam: bool,
    /// Why Steam may still run: the forwarded -shutdown failed, the Steam client this MCP started had already exited
    /// (and the Steam running now was left alone), or Steam outlived this stop with no client kept for a later one.
    pub steam_note: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct StatusReport {
    pub steam_running: bool,
    pub steamvr_running: bool,
    pub alvr_version: Option<String>,
    pub headset: Option<HeadsetStatus>,
    /// The proximity the headset reports at every stream start; true until set_proximity says otherwise.
    pub on_head: bool,
    /// Only while the OpenVR reader is connected, so vr_status never connects one.
    pub scene_app: Option<openvr::SceneApp>,
    /// What needs the agent's action, "; "-joined: a leaked headset, a wedged OpenVR client, a session SteamVR's exit ended, a failed restart.
    pub warning: Option<String>,
    /// The last SteamVR restart that failed, also one the supervisor ran: "<age> s ago: <reason>".
    pub last_restart_error: Option<String>,
    /// Session clock: wall-clock seconds since MCP start (a stepped system clock moves it), and the same instant and T0 as RFC 3339 UTC.
    pub now_t_s: f64,
    pub now_at: String,
    pub t0_at: String,
    /// The SteamVR lease; none when free, expired or dead.
    pub owner: Option<Owner>,
    /// `driver_*` sections SteamVR blocked in safe mode, which a start or restart would clear.
    pub blocked_drivers: Vec<String>,
}

/// One ALVR session value to set.
#[derive(Clone, Debug, Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct SettingValue {
    /// ALVR's path syntax from the session JSON root, e.g. "session_settings.video.preferred_fps" or
    /// "session_settings.headset.controllers.content.gestures[2].joystick_range".
    pub path: String,
    pub value: Value,
}

/// How launch_app starts an app.
#[derive(Clone, Copy, Debug, Default, PartialEq, Deserialize, Serialize, rmcp::schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(crate = "rmcp::schemars")]
pub enum Via {
    /// steam://rungameid/ through Steam, with the user's Steam launch options.
    #[default]
    Steam,
    /// The app's compatibility tool, run directly with `exe`, `args` and `env`; Steam apps only.
    Proton,
}

#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct LaunchArgs {
    /// Decimal: a Steam app id (e.g. "620980"), a non-Steam shortcut's 32-bit app id or its 64-bit game id.
    pub app_id: String,
    /// "steam" (default) or "proton".
    #[serde(default)]
    pub via: Via,
    /// Proton only, required: the exe relative to the app's install dir, e.g. "Game.exe" or "bin\\Game.exe". Must be a Windows .exe (MZ header) and not a shell, interpreter or loader (cmd, powershell, python*, java*, wine*, rundll32 and the like).
    pub exe: Option<String>,
    /// Proton only: the exe's arguments, passed as given (no shell). Chromium/CEF switches that run a program (--renderer-cmd-prefix, --gpu-launcher, --utility-cmd-prefix, --browser-subprocess-path) are refused.
    #[serde(default)]
    pub args: Vec<String>,
    /// Proton only: env vars over the desktop session's, e.g. GPU selection. Takes upper-case [A-Z0-9_] names starting MESA_, RADV_, VK_, DXVK_, VKD3D_, PROTON_, SDL_ or __GL_, except names holding PATH, FILENAMES, _FILES, LIBRARY, DYNAMIC_API or GL_DRIVER or ending in _CMD; also WINEDEBUG, WINEDLLOVERRIDES, DRI_PRIME, __NV_PRIME_RENDER_OFFLOAD, __VK_LAYER_NV_optimus, MANGOHUD, WINE_FULLSCREEN_FSR, WINEFSYNC, WINEESYNC, PULSE_LATENCY_MSEC, AMD_VULKAN_ICD, and __GLX_VENDOR_LIBRARY_NAME with a [a-z0-9_] value. Values must hold no "..".
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// launch_app's argument rules, checked before anything starts.
pub fn check_launch(a: &LaunchArgs) -> Result<(), String> {
    match a.via {
        Via::Steam if a.exe.is_some() || !a.args.is_empty() || !a.env.is_empty() => return Err("exe, args and env need via \"proton\"".into()),
        Via::Proton if a.exe.is_none() => return Err("via \"proton\" needs exe, relative to the app's install dir".into()),
        Via::Proton if a.app_id.trim().parse() == Ok(steamvr::STEAMVR_APP_ID) => return Err("via \"proton\" cannot launch SteamVR (app 250820)".into()),
        _ => (),
    }
    if let Some((arg, switch)) = a.args.iter().find_map(|arg| COMMAND_SWITCHES.iter().find(|s| arg.to_ascii_lowercase().contains(*s)).map(|s| (arg, s))) {
        return Err(format!("arg {arg:?} refused: {switch} is a Chromium/CEF switch that runs a program of the caller's choice"));
    }
    if let Some((k, _)) = a.env.iter().find(|(k, v)| k.is_empty() || k.contains(['=', '\0']) || v.contains('\0')) {
        return Err(format!("env key {k:?} must be non-empty and hold no '=' or NUL, and its value no NUL"));
    }
    if let Some(k) = a.env.iter().find_map(|(k, v)| (!env_allowed(k, v)).then_some(k)) {
        return Err(format!("env {k} refused: {ENV_ALLOWED}"));
    }
    match a.env.iter().find(|(_, v)| v.contains("..")) {
        Some((k, _)) => Err(format!("env {k} refused: a value must hold no \"..\", which name-based loaders follow out of their dir")),
        None => Ok(()),
    }
}

/// Chromium/CEF switches (any of `--`, `-` or `/` on Windows, any case) naming a program to run.
const COMMAND_SWITCHES: [&str; 4] = ["renderer-cmd-prefix", "gpu-launcher", "utility-cmd-prefix", "browser-subprocess-path"];

/// What launch_app's env takes, for its errors.
pub const ENV_ALLOWED: &str = "env takes names of [A-Z0-9_] only (Wine matches names ignoring case) starting MESA_*, RADV_*, VK_*, DXVK_*, \
    VKD3D_*, PROTON_*, SDL_* or __GL_*, except names holding PATH, FILENAMES, _FILES, LIBRARY, DYNAMIC_API, GL_DRIVER or ending in _CMD, \
    which run or load code (Proton runs PROTON_REMOTE_DEBUG_CMD; VK_ICD_FILENAMES and VK_LAYER_PATH load drivers and layers); \
    also WINEDEBUG, WINEDLLOVERRIDES, DRI_PRIME, __NV_PRIME_RENDER_OFFLOAD, __VK_LAYER_NV_optimus, MANGOHUD, WINE_FULLSCREEN_FSR, \
    WINEFSYNC, WINEESYNC, PULSE_LATENCY_MSEC, AMD_VULKAN_ICD, and __GLX_VENDOR_LIBRARY_NAME with a [a-z0-9_] value";

/// Whether launch_app may pass env var `name=value` to the Proton launcher: tuning a game, never a
/// command, library or search path it would run or load.
fn env_allowed(name: &str, value: &str) -> bool {
    let only = |s: &str, ok: fn(&u8) -> bool| !s.is_empty() && s.bytes().all(|b| ok(&b) || b.is_ascii_digit() || b == b'_');
    if name == "__GLX_VENDOR_LIBRARY_NAME" {
        return only(value, u8::is_ascii_lowercase); // loads libGLX_<value>.so: a name, never a path
    }
    let names = [
        "WINEDEBUG", "WINEDLLOVERRIDES", "DRI_PRIME", "__NV_PRIME_RENDER_OFFLOAD", "__VK_LAYER_NV_optimus", "MANGOHUD",
        "WINE_FULLSCREEN_FSR", "WINEFSYNC", "WINEESYNC", "PULSE_LATENCY_MSEC", "AMD_VULKAN_ICD",
    ];
    let prefixes = ["MESA_", "RADV_", "VK_", "DXVK_", "VKD3D_", "PROTON_", "SDL_", "__GL_"];
    let loads_code = ["PATH", "FILENAMES", "_FILES", "LIBRARY", "DYNAMIC_API", "GL_DRIVER"].iter().any(|p| name.contains(p)) || name.ends_with("_CMD");
    names.contains(&name) || (only(name, u8::is_ascii_uppercase) && prefixes.iter().any(|p| name.starts_with(p)) && !loads_code)
}

#[derive(Debug, Serialize)]
pub struct AppReport {
    pub app: steamvr::AppInfo,
    pub scene_pid: u32,
    pub via: Via,
    /// The Proton launcher's stdout and stderr; none via Steam.
    pub run_log: Option<PathBuf>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct DashboardReport {
    pub closed: bool,
    pub was_visible: Option<bool>,
    pub note: &'static str,
}

/// Whether to click system, and what to say, from the dashboard state before the click and after
/// the polls that followed it; a second click would reopen a dashboard that stayed open, so there is only one.
fn dashboard_outcome(before: Option<bool>, after: Option<bool>) -> (bool, &'static str) {
    match (before, after) {
        (Some(true), Some(false)) => (true, "closed the SteamVR dashboard"),
        (Some(true), Some(true)) => (true, "SteamVR dashboard still open after one system click"),
        (Some(true), None) => (true, "SteamVR dashboard state unknown after one system click"),
        (Some(false), _) => (false, "SteamVR dashboard not open, nothing clicked"),
        (None, _) => (false, "SteamVR dashboard state unknown, nothing clicked"),
    }
}

#[derive(Debug, Default, Serialize)]
pub struct SettingsReport {
    /// SteamVR restarted at least once.
    pub restarted: bool,
    /// How many SteamVR restarts happened: ALVR may ask for one more once the headset negotiates.
    pub restarts: u32,
    pub streaming: bool,
}

/// ALVR's `steamvr_input_2_0`: on, a bare hand is a separate skeletal hand-tracker device.
pub const STEAMVR_INPUT_2_0: &str = "session_settings.headset.controllers.content.hand_skeleton.content.steamvr_input_2_0";

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LogLine {
    /// Seconds on the session clock; `at` is the same instant as RFC 3339 UTC.
    pub t_s: f64,
    pub at: String,
    /// Local time of day, HH:MM:SS.mmm.
    pub t: String,
    /// "alvr" | "vrserver" | "vrcompositor" | "vrmonitor"
    pub source: String,
    /// "error" | "warning" | "info" | "debug"
    pub level: String,
    pub text: String,
}

#[derive(Debug, Default, Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct LogQuery {
    /// "alvr", "vrserver", "vrcompositor", "vrmonitor" or "all" (default).
    pub source: Option<String>,
    /// Keep only lines whose text contains this, ignoring case.
    pub contains: Option<String>,
    /// "error", "warning", "info" or "debug": the lowest level kept for ALVR events. SteamVR lines are not filtered by level.
    pub min_level: Option<String>,
    /// How many of the newest matching lines to return, 1 to 1000 (default 100).
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct ReadAppLogArgs {
    /// Decimal Steam app id with an `[app_logs]` entry in config.toml.
    pub app_id: String,
    /// Only lines with seq at or above this (default 0); pass the next_seq of the last read.
    pub since_seq: Option<u64>,
    /// Keep only lines whose text contains this, ignoring case.
    pub contains: Option<String>,
    /// How many of the oldest matching lines to return, 1 to 500 (default 500).
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct WaitForLogArgs {
    /// Decimal Steam app id with an `[app_logs]` entry in config.toml.
    pub app_id: String,
    /// Regex a line must match (unanchored, Rust regex syntax).
    pub pattern: String,
    /// Regex that ends the wait with result "fail"; checked before `pattern` on each line.
    pub fail_pattern: Option<String>,
    /// First seq to look at (default: the oldest line kept, which is the first one since the last launch_app).
    pub since_seq: Option<u64>,
    /// Seconds to wait, 1 to 600 (default 60).
    pub timeout_s: Option<f64>,
}

/// The checked arguments of `wait_for_log`: app id, pattern, fail pattern and timeout. Nothing waits on a bad one.
pub fn wait_plan(a: &WaitForLogArgs, configured: impl Fn(u32) -> bool) -> Result<(u32, Regex, Option<Regex>, Duration), String> {
    let app_id = app_id(&a.app_id).map_err(|e| e.to_string())?;
    if !configured(app_id) {
        return Err(format!("app {app_id} has no entry in [app_logs] of config.toml"));
    }
    let pattern = Regex::new(&a.pattern).map_err(|e| format!("pattern: {e}"))?;
    let fail = a.fail_pattern.as_deref().map(Regex::new).transpose().map_err(|e| format!("fail_pattern: {e}"))?;
    let timeout_s = a.timeout_s.unwrap_or(60.0);
    if !(1.0..=600.0).contains(&timeout_s) {
        return Err(format!("timeout_s must be from 1 to 600, got {timeout_s}"));
    }
    Ok((app_id, pattern, fail, Duration::from_secs_f64(timeout_s)))
}

fn app_id(id: &str) -> Result<u32> {
    id.trim().parse().with_context(|| format!("app_id must be a decimal app id, got {id:?}"))
}

const LOG_SOURCES: [&str; 5] = ["alvr", "vrserver", "vrcompositor", "vrmonitor", "all"];
const STEAMVR_LOGS: [&str; 3] = ["vrserver", "vrcompositor", "vrmonitor"];
/// ALVR events kept, and SteamVR lines read per log.
const LOGS_KEPT: usize = 1000;

/// How long a log wait keeps reading files that still grow after the app exited, before it fails.
const LOG_DRAIN: Duration = Duration::from_secs(2);
/// ALVR statistics summaries kept: ALVR sends one every 500 ms, so two of get_frame_timing's longest (60 s) windows.
const STATS_KEPT: usize = 240;

fn level_name(severity: LogSeverity) -> String {
    format!("{severity:?}").to_lowercase()
}

fn severity(level: &str) -> Option<LogSeverity> {
    [LogSeverity::Error, LogSeverity::Warning, LogSeverity::Info, LogSeverity::Debug].into_iter().find(|s| level_name(*s) == level)
}

/// Session paths set_alvr_settings refuses, with everything above and below them: they open ALVR
/// to the network (a trusted client it dials and streams to, the API on every interface, discovery,
/// ports, connect scripts) or send tracking to any host (VMC).
pub const REFUSED_PATHS: [&str; 3] = ["client_connections", "session_settings.connection", "session_settings.headset.vmc"];

/// The values in ALVR's own types; a bad or refused path fails here, before any request.
pub fn parse_values(values: Vec<SettingValue>) -> Result<Vec<PathValuePair>> {
    values
        .into_iter()
        .map(|v| {
            let path = parse_path(&v.path)?;
            // one is a prefix of the other when they agree up to the shorter one's length
            let same = |s: (&PathSegment, &PathSegment)| match s {
                (PathSegment::Name(a), PathSegment::Name(b)) => a == b,
                (PathSegment::Index(a), PathSegment::Index(b)) => a == b,
                _ => false,
            };
            let overlaps = |r: &&str| parse_path(r).is_ok_and(|r| path.iter().zip(&r).all(same));
            if REFUSED_PATHS.iter().any(overlaps) {
                bail!("{} refused: set_alvr_settings cannot change {REFUSED_PATHS:?} or a path holding them", v.path);
            }
            Ok(PathValuePair { path, value: v.value })
        })
        .collect()
}

/// Ok when `session` (ALVR's session as JSON) holds every value.
pub fn verify_applied(session: &Value, values: &[PathValuePair]) -> Result<(), String> {
    for v in values {
        let path = path_to_string(&v.path);
        let found = v.path.iter().try_fold(session, |node, segment| match segment {
            PathSegment::Name(name) => node.get(name),
            PathSegment::Index(index) => node.get(index),
        });
        match found {
            None => return Err(format!("{path} is not in ALVR's session")),
            Some(found) if !json_eq(found, &v.value) => return Err(format!("{path} is {found} in ALVR's session, not {}", v.value)),
            Some(_) => (),
        }
    }
    Ok(())
}

/// Reads events from `next` until ALVR's session holds `values`, by `deadline`.
// ALVR answers 200 even when it rejects the values: it logs why and sends no new session
// (server_core web_server.rs:181-185, server_io lib.rs:156-187)
pub fn wait_applied(mut next: impl FnMut(Duration) -> Result<Option<Event>>, values: &[PathValuePair], deadline: Instant) -> Result<()> {
    let mut mismatch = "ALVR sent no session".to_owned();
    while Instant::now() < deadline {
        match next(Duration::from_millis(200))? {
            Some(Event { event_type: EventType::Session(session), .. }) => match verify_applied(&serde_json::to_value(&*session)?, values) {
                Ok(()) => return Ok(()),
                Err(e) => mismatch = e,
            },
            Some(Event { event_type: EventType::Log(LogEntry { severity: LogSeverity::Error, content }), .. })
                if content.starts_with("Failed to set session values") =>
            {
                bail!("failed: {content}")
            }
            _ => (),
        }
    }
    bail!("ALVR did not apply the values: {mismatch} (deadline 3 s)")
}

/// Equality with numbers compared at f32 precision: ALVR stores most numbers as f32.
fn json_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => {
            let (x, y) = (x.as_f64().unwrap_or(f64::NAN), y.as_f64().unwrap_or(f64::NAN));
            (x - y).abs() <= f32::EPSILON as f64 * x.abs().max(1.0)
        }
        (Value::Array(x), Value::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(x, y)| json_eq(x, y)),
        (Value::Object(x), Value::Object(y)) => x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| json_eq(v, w))),
        _ => a == b,
    }
}

/// Whether ALVR's settings schema flags `path`, or an entry above it, "real-time": ALVR applies
/// it while streaming, so no restart is needed (the dashboard's "can be changed in real-time").
pub fn is_real_time(path: &[PathSegment]) -> bool {
    match path {
        [PathSegment::Name(root), rest @ ..] if root == "session_settings" => {
            real_time_in(&Settings::schema(alvr_session::session_settings_default()), rest)
        }
        _ => false,
    }
}

fn real_time_in(node: &SchemaNode, path: &[PathSegment]) -> bool {
    use PathSegment::{Index, Name};
    match (node, path) {
        (SchemaNode::Section { entries, .. }, [Name(name), rest @ ..]) => {
            entries.iter().find(|e| e.name == *name).is_some_and(|e| e.flags.contains("real-time") || real_time_in(&e.content, rest))
        }
        (SchemaNode::Choice { variants, .. }, [Name(name), rest @ ..]) => variants
            .iter()
            .find(|v| v.name == *name)
            .is_some_and(|v| v.flags.contains("real-time") || v.content.as_ref().is_some_and(|c| real_time_in(c, rest))),
        (SchemaNode::Optional { content, .. } | SchemaNode::Switch { content, .. }, [Name(name), rest @ ..]) if name == "content" => {
            real_time_in(content, rest)
        }
        (SchemaNode::Array(items), [Index(index), rest @ ..]) => items.get(*index).is_some_and(|c| real_time_in(c, rest)),
        // shortcut: below vectors and dictionaries, and at an enum's `variant`, every path counts as restart-relevant, so it restarts SteamVR; walk them if a real-time path there matters
        _ => false,
    }
}

/// SteamVR's "Wed Oct 21 2026 05:00:36.212461 [Info] - text" lines; a line without that prefix
/// continues the one before and gets its time and level. One before any stamped line is dropped.
pub fn steamvr_log_lines(source: &str, lines: Vec<String>) -> Vec<LogLine> {
    let mut last: Option<(String, f64, String, String)> = None;
    lines
        .into_iter()
        .filter_map(|line| {
            let parsed = line.split_once(" [").and_then(|(stamp, rest)| {
                let (lvl, text) = rest.split_once("] - ")?;
                let naive = NaiveDateTime::parse_from_str(stamp, "%a %b %d %Y %H:%M:%S%.f").ok()?;
                let local = Local.from_local_datetime(&naive).earliest()?;
                let t_s = clock::wall_t_s(local.into());
                Some(((naive.format("%H:%M:%S%.3f").to_string(), t_s, lvl.to_lowercase()), text.to_owned()))
            });
            let text = match parsed {
                Some(((t, t_s, level), text)) => {
                    last = Some((t, t_s, clock::at(t_s), level));
                    text
                }
                None => line,
            };
            let (t, t_s, at, level) = last.clone()?;
            Some(LogLine { t_s, at, t, source: source.to_owned(), level, text })
        })
        .collect()
}

/// `lines` in time order, filtered by `q`, newest `limit` kept. `min_level` applies to ALVR events only.
pub fn filter_logs(mut lines: Vec<LogLine>, q: &LogQuery) -> Result<Vec<LogLine>, String> {
    let source = q.source.as_deref().unwrap_or("all");
    if !LOG_SOURCES.contains(&source) {
        return Err(format!("source must be one of {LOG_SOURCES:?}"));
    }
    let min = match q.min_level.as_deref() {
        Some(level) => Some(severity(level).ok_or("min_level must be error, warning, info or debug")?),
        None => None,
    };
    let limit = q.limit.unwrap_or(100);
    if !(1..=LOGS_KEPT).contains(&limit) {
        return Err(format!("limit must be 1 to {LOGS_KEPT}"));
    }
    let contains = q.contains.as_deref().map(str::to_lowercase);
    lines.sort_by(|a, b| a.t_s.total_cmp(&b.t_s)); // stable: equal times keep their source order
    lines.retain(|l| {
        (source == "all" || l.source == source)
            && (l.source != "alvr" || min.is_none_or(|m| severity(&l.level).is_some_and(|s| s >= m)))
            && contains.as_ref().is_none_or(|c| l.text.to_lowercase().contains(c))
    });
    Ok(lines.split_off(lines.len().saturating_sub(limit)))
}

#[derive(Default)]
struct State {
    headset: Option<Headset>,
    dashboard: Option<Child>,
    started_steam: bool,
    steam_client: Option<(u32, u64)>,
    restarts: u32,
    watchdog: Watchdog,
    leaked: bool,
    managing: bool,
    /// Bumped by every vr_stop; a vr_start that saw another value bails at its next check.
    stops: u64,
    /// vr_stops that bumped `stops` and wait for `starting`; vr_start refuses meanwhile, or it would read the new epoch and run in full.
    stopping: u32,
    /// Why the session ended without vr_stop, for vr_status; cleared by vr_start and vr_stop.
    ended: Option<&'static str>,
    /// When and why the last SteamVR restart failed; cleared by a restart that works, a vr_start that works and vr_stop.
    last_restart_error: Option<(Instant, String)>,
    /// The last `launch_app via proton` run, until stop_app ends it.
    launched: Option<proton::Launched>,
}

/// Steam, SteamVR with ALVR, and the headset, plus the dashboard role ALVR expects.
pub struct Stack {
    /// Config, Paths and the API port: reloaded by `recheck` while nothing runs. Locked only briefly.
    cfg: Mutex<Config>,
    paths: Mutex<Paths>,
    api: Mutex<Api>,
    state: Mutex<State>,
    /// Held for a whole SteamVR restart, so stop() never races one.
    restarting: Mutex<()>,
    /// Held for a whole vr_start, so a second one refuses.
    starting: Mutex<()>,
    /// Held while vr_stop or a SteamVR restart takes SteamVR down, so read() does not reconnect to a vrserver going away.
    vr_down: Mutex<()>,
    /// Results of the last startup checks.
    checks: Mutex<Vec<Check>>,
    /// config.toml's text when it was last read, so vr_status notices an edit.
    config_text: Mutex<Option<String>>,
    /// The OpenVR reader and the vrserver pid it connected to. Its own lock, held across OpenVR
    /// calls (bounded by OPENVR_TIMEOUT), never together with `state`.
    reader: Mutex<Option<(u32, OpenVr)>>,
    /// OpenVR calls started and not returned yet, also those a timeout left on their thread. Above 0
    /// while the reader lock is held, an earlier call hangs (a wedged vrserver).
    openvr_calls: Arc<AtomicUsize>,
    on_change: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    /// The poses every headset sends; outlives headset restarts, and vr_start resets it when it begins a session. Locked only briefly.
    rig: Arc<Mutex<Rig>>,
    /// The newest LOGS_KEPT ALVR log events the supervisor saw, oldest first. Locked only briefly.
    alvr_logs: Mutex<VecDeque<LogLine>>,
    /// The newest STATS_KEPT ALVR statistics summaries the supervisor saw, with when, oldest first, for
    /// get_frame_timing: a stream of its own per call made ALVR log a failed send when it closed. Locked only briefly.
    alvr_stats: Mutex<VecDeque<(Instant, alvr_events::StatisticsSummary)>>,
    /// The owner name and ttl the lease is taken and renewed with.
    lease: Mutex<(String, Duration)>,
    /// The game log rings from `[app_logs]`; rebuilt when the config reloads. Locked briefly, never while sleeping.
    app_logs: Mutex<AppLogs>,
    /// The processes vr_stop, a restart and a cold vr_start signal.
    vr: steamvr::VrProcs,
}

const RECENTER_NOT_STAGE: &str =
    "recentering_mode is not Stage: ALVR re-origins the playspace around the head's last pose on every playspace update, so poses sent are no longer absolute";

/// The scene app's pid, unless the scene is SteamVR's own (Home runs as steamtours). `name` names a
/// pid (steamvr::process_name); one without a readable name, or out of kill()'s single-process range, is refused.
pub fn game_pid(scene: Option<&openvr::SceneApp>, name: impl Fn(u32) -> Option<String>) -> Result<u32, &'static str> {
    let pid = scene.map(|a| a.pid).filter(|&pid| pid != 0).ok_or("no scene app is running")?;
    let name = name(pid).filter(|_| steamvr::signalable(pid).is_some()).ok_or("the scene app's pid is not a running process")?;
    match VR_PROCESS_NAMES.contains(&name.as_str()) {
        true => Err("no app running (scene is SteamVR Home)"),
        false => Ok(pid),
    }
}

/// vr_stop signals SteamVR only when it is ALVR's (`alvrs`), never one the user runs without ALVR.
pub fn stop_guard(steamvr_running: bool, alvrs: impl FnOnce() -> bool) -> Result<()> {
    if steamvr_running && !alvrs() {
        bail!("SteamVR is running without ALVR; close it yourself");
    }
    Ok(())
}

/// Whether vr_stop may end a running SteamVR as this MCP's. A failed start's dashboard proves this MCP launched it only
/// together with ALVR's safe-mode block: that SteamVR has no ALVR to answer, and the start's error said to run vr_stop.
fn stop_is_ours(managing: bool, started_steam: bool, dashboard: bool, alvr_blocked: impl FnOnce() -> bool) -> bool {
    managing || started_steam || (dashboard && alvr_blocked())
}

/// An ALVR answering while none of this user's vrservers runs belongs to someone else (another user's SteamVR):
/// starting ours would talk to it, and ours could not bind the port.
pub fn foreign_alvr_guard(our_vrserver: bool, api_answers: impl FnOnce() -> bool, port: u16) -> Result<()> {
    if !our_vrserver && api_answers() {
        bail!("port {port} answers, but no SteamVR of this user runs (another user's SteamVR/ALVR?); close it and retry");
    }
    Ok(())
}

/// How long vr_start waits for the API of a running SteamVR's loaded ALVR driver: it answers a few seconds after vrserver starts.
const ALVR_API_WAIT: Duration = Duration::from_secs(10);

/// Whether vr_start can adopt a running SteamVR: its vrserver loaded ALVR's driver (`has_alvr`) and ALVR's API answers within `wait`.
/// A loaded driver whose API stays silent is ALVR's TCP EOF spin or a hung start, which only vr_stop clears.
fn adoptable(has_alvr: bool, answers: impl FnMut() -> bool, wait: Duration, port: u16) -> Result<()> {
    if !has_alvr {
        bail!("SteamVR is running without the ALVR driver; close SteamVR and retry");
    }
    if !steamvr::wait_for(wait, answers) {
        bail!("ALVR's API does not answer on port {port} (deadline {} s); run vr_stop", wait.as_secs_f64());
    }
    Ok(())
}

/// A game that finds no SteamVR would start one from its own process, under its own prefix and process group.
pub fn launch_guard(steamvr_running: bool) -> Result<()> {
    if !steamvr_running {
        bail!("SteamVR is not running; run vr_start first");
    }
    Ok(())
}

/// Any live VR_PROCESS_NAMES process (vrmonitor, vrcompositor, ...) can rewrite steamvr.vrsettings after the restore.
pub fn restore_guard(alive: &[&str]) -> Result<()> {
    if !alive.is_empty() {
        bail!("SteamVR processes still run ({}) and may rewrite steamvr.vrsettings; run vr_stop first", alive.join(", "));
    }
    Ok(())
}

impl Stack {
    pub fn new(cfg: Config, paths: Paths) -> Arc<Stack> {
        // a unit test that reaches stop() must never signal the user's SteamVR
        Self::with_procs(cfg, paths, if cfg!(test) { steamvr::VrProcs::NONE } else { steamvr::VrProcs::STEAMVR })
    }

    fn with_procs(cfg: Config, paths: Paths, vr: steamvr::VrProcs) -> Arc<Stack> {
        let app_log_patterns = cfg.app_logs.clone();
        let stack = Arc::new(Stack {
            api: Mutex::new(api_for(&paths)),
            cfg: Mutex::new(cfg),
            paths: Mutex::new(paths),
            state: Mutex::default(),
            restarting: Mutex::new(()),
            starting: Mutex::new(()),
            vr_down: Mutex::new(()),
            checks: Mutex::default(),
            config_text: Mutex::new(Config::text()),
            reader: Mutex::new(None),
            openvr_calls: Arc::default(),
            on_change: Mutex::new(None),
            rig: Arc::default(),
            alvr_logs: Mutex::default(),
            alvr_stats: Mutex::default(),
            lease: Mutex::new(default_lease()),
            app_logs: Mutex::new(AppLogs::new(&app_log_patterns)),
            vr,
        });
        stack.run_checks();
        let weak = Arc::downgrade(&stack);
        thread::spawn(move || supervise(weak));
        // a test drives the watcher's parts itself: on its own it ends a test's managed session once no SteamVR runs
        if !cfg!(test) {
            let weak = Arc::downgrade(&stack);
            thread::spawn(move || watch(weak));
        }
        stack
    }

    /// A stack with no Steam and its lease in `runtime_dir`; Stack::new and api_for keep every test stack
    /// off the user's SteamVR processes and ALVR port.
    #[cfg(test)]
    pub(crate) fn for_tests(runtime_dir: PathBuf) -> Arc<Stack> {
        let paths = Paths { steam_dir: None, flatpak_steam: false, steamvr_dir: None, streamer_dir: None, state_dir: "/nonexistent".into(), runtime_dir };
        Stack::new(Config::default(), paths)
    }

    fn cfg(&self) -> Config {
        self.cfg.lock().clone()
    }

    fn paths(&self) -> Paths {
        self.paths.lock().clone()
    }

    fn api(&self) -> Api {
        *self.api.lock()
    }

    /// Re-reads config.toml, applies it while nothing runs, and runs all startup checks again. At
    /// vr_start, and at vr_status after an edit or while a core check fails: some checks are slow.
    pub fn recheck(&self) {
        *self.config_text.lock() = Config::text();
        if let Ok(cfg) = Config::load() {
            self.reload(cfg);
        }
        self.run_checks();
    }

    /// Applies `cfg` with freshly detected Paths, unless a vr_start, its session, an MCP-started Steam or SteamVR runs.
    fn reload(&self, cfg: Config) -> bool {
        let busy = {
            let state = self.state.lock();
            state.managing || state.headset.is_some() || state.dashboard.is_some() || state.started_steam
        };
        if busy || self.starting.is_locked() || steamvr::is_steamvr_running() {
            return false;
        }
        let paths = Paths::detect(&cfg);
        *self.api.lock() = api_for(&paths);
        *self.paths.lock() = paths;
        // a rebuild restarts seq at 1, which strands a caller's since_seq
        if cfg.app_logs != self.cfg.lock().app_logs {
            *self.app_logs.lock() = AppLogs::new(&cfg.app_logs);
        }
        *self.cfg.lock() = cfg;
        true
    }

    fn run_checks(&self) {
        *self.checks.lock() = checks::startup_checks(&self.cfg(), &self.paths());
    }

    /// The kept check results, with `alvr_dashboard_absent` re-run. All checks run again, with
    /// config.toml re-read, when the file changed or a core check fails, so fixing config.toml or the
    /// machine un-hides vr_start without an MCP restart.
    pub fn checks(&self) -> Vec<Check> {
        let core_failing = core_failing(&self.checks.lock());
        let edited = Config::text() != *self.config_text.lock();
        if core_failing || edited {
            self.recheck();
        }
        let mut checks = self.checks.lock();
        let dashboard = checks::dashboard_check();
        checks.iter_mut().filter(|c| c.name == dashboard.name).for_each(|c| *c = dashboard.clone());
        checks.clone()
    }

    /// The kept check results plus the runtime state, read now.
    pub fn readiness(&self) -> Readiness {
        let (streaming, ours_running, launched) = {
            let state = self.state.lock();
            let streaming = state.headset.as_ref().is_some_and(|h| h.status().streaming);
            (streaming, state.managing || state.headset.is_some() || state.dashboard.is_some() || state.started_steam, state.launched.is_some())
        };
        let steamvr_running = steamvr::is_steamvr_running();
        let alvr_api = self.api().ping();
        Readiness {
            failed: self.checks.lock().iter().filter(|c| !c.ok).map(|c| (c.name, c.reason.clone())).collect(),
            restore_file: self.paths().restore_file().exists(),
            steamvr_running,
            alvr_api,
            streaming,
            running: ours_running || (steamvr_running && (alvr_api || steamvr::vrserver_has_alvr())),
            leased_to: lease::blocking(lease::read(&self.paths().runtime_dir).as_ref(), std::process::id(), SystemTime::now(), lease::alive)
                .map(lease::holder_text),
            app_logs: !self.cfg.lock().app_logs.is_empty(),
            launched,
        }
    }

    /// Before a mutating tool: takes the lease, or renews ours once half the ttl is gone.
    pub fn claim(&self) -> Result<()> {
        let (owner, ttl) = self.lease.lock().clone();
        let dir = self.paths().runtime_dir;
        let ours = lease::read(&dir).is_some_and(|l| l.pid == std::process::id() && !lease::renew_due(&l, ttl, SystemTime::now()));
        if ours {
            return Ok(());
        }
        lease::acquire(&dir, &owner, ttl).map(drop)
    }

    /// vr_acquire: takes or renews the lease under `owner` and `ttl_s`, which later claims keep using until the lease is released.
    pub fn acquire_lease(&self, owner: String, ttl_s: Option<u64>) -> Result<Lease> {
        if !(1..=64).contains(&owner.chars().count()) {
            bail!("owner must be from 1 to 64 characters, got {}", owner.chars().count());
        }
        let ttl = match ttl_s {
            None => lease::DEFAULT_TTL,
            Some(s) if lease::TTL_S.contains(&s) => Duration::from_secs(s),
            Some(s) => bail!("ttl_s must be from {} to {}, got {s}", lease::TTL_S.start(), lease::TTL_S.end()),
        };
        let taken = lease::acquire(&self.paths().runtime_dir, &owner, ttl)?;
        *self.lease.lock() = (owner, ttl);
        Ok(taken)
    }

    /// Whether this MCP held the lease. Forgets vr_acquire's owner name and ttl, so the next claim is the MCP's own.
    pub fn release_lease(&self) -> Result<bool> {
        let released = lease::release(&self.paths().runtime_dir)?;
        *self.lease.lock() = default_lease();
        Ok(released)
    }

    /// vr_release: gives up our lease, unless this MCP still runs something on SteamVR: the next
    /// holder could not start, and our supervisor would go on restarting SteamVR without the lease.
    pub fn release(&self) -> Result<bool> {
        // held to the release, so no vr_start begins before the lease goes
        let (Some(_starting), Some(_restarting)) = (self.starting.try_lock(), self.restarting.try_lock()) else {
            bail!("vr_start, vr_stop or a SteamVR restart is running; run vr_stop, then vr_release");
        };
        let state = self.state.lock();
        if state.managing || state.headset.is_some() || state.dashboard.is_some() {
            bail!("this MCP still runs a session (headset or ALVR dashboard); run vr_stop first");
        }
        drop(state);
        self.release_lease()
    }

    /// Called from the watcher thread whenever `readiness` changes.
    pub fn set_on_change(&self, f: Box<dyn Fn() + Send + Sync>) {
        *self.on_change.lock() = Some(f);
    }

    /// From the watcher: renews our lease while this MCP manages a session, so a holder that only
    /// calls read-only tools keeps it.
    fn keep_lease(&self) {
        let managing = self.state.lock().managing;
        let (owner, ttl) = self.lease.lock().clone();
        let dir = self.paths().runtime_dir;
        // the unlocked read only spares the lock on most ticks; renew checks again under it
        let held = lease::read(&dir);
        if watch_renews(managing, managing && steamvr::is_steamvr_running(), held.as_ref(), std::process::id(), ttl, SystemTime::now()) {
            lease::renew(&dir, &owner, ttl).map_err(|e| warn!("{e:#}")).ok();
        }
    }

    /// From the watcher, once VrGone::lost: ends a session whose SteamVR is gone (it crashed, the user quit
    /// it, or a restart failed after vrserver last ran at `vrserver_seen`), so its lease is no longer renewed,
    /// no supervisor restart starts SteamVR again and vr_release works. Not while a vr_start, vr_stop or
    /// restart runs, which bring SteamVR down on purpose. Whether it ended one.
    fn end_lost_session(&self, vrserver_seen: Option<Instant>) -> bool {
        let (Some(_start), Some(_restart)) = (self.starting.try_lock(), self.restarting.try_lock()) else { return false };
        let (headset, dashboard, why) = {
            let mut state = self.state.lock();
            if !state.managing {
                return false;
            }
            state.managing = false;
            let restart_failed = state.last_restart_error.as_ref().is_some_and(|(t, _)| vrserver_seen.is_none_or(|s| *t > s));
            let why = if restart_failed { RESTART_FAILED } else { SESSION_LOST };
            state.ended = Some(why);
            // started_steam stays: vr_stop still closes the Steam this MCP started
            (state.headset.take(), state.dashboard.take(), why)
        };
        warn!("{why}");
        if let Some(h) = headset {
            self.dispose(h);
        }
        if let Some(mut child) = dashboard {
            child.kill().ok();
            child.wait().ok();
        }
        true
    }

    /// A finished stop or restore leaves nothing running, so hold nothing.
    fn release_after_success(&self) {
        self.release_lease().map_err(|e| error!("{e:#}")).ok();
    }

    /// A stop that left SteamVR processes behind still owns the machine.
    fn release_if_clean(&self, report: &StopReport) {
        if report.remaining_processes.is_empty() {
            self.release_after_success();
        }
    }

    /// Holds `starting` and `restarting` throughout, so no vr_start snapshots or prepares while restore.json goes away.
    pub fn restore(&self, forget_missing: bool) -> Result<RestoreReport> {
        let Some(_starting) = self.starting.try_lock() else { bail!("vr_start is running; run vr_stop first") };
        let Some(_restarting) = self.restarting.try_lock() else { bail!("SteamVR is stopping or restarting") };
        if self.state.lock().managing {
            bail!("a vr_start session runs; run vr_stop first");
        }
        restore_guard(&self.vr.alive())?; // the Stack's VrProcs: a unit test never sees the user's SteamVR
        let notes = setup::restore(&self.paths(), forget_missing)?;
        Ok(self.finish_restore(notes))
    }

    /// A restore that kept restore.json is not done: the lease stays until a retry finishes it.
    fn finish_restore(&self, notes: Vec<String>) -> RestoreReport {
        let restored = !self.paths().restore_file().exists();
        if restored {
            self.release_after_success();
        }
        RestoreReport { restored, notes }
    }

    /// A screenshot of the headset's stream, or of the compositor's scene textures. The waits run outside the state lock.
    pub fn screenshot(&self, source: Source, eye: Eye) -> Result<Screenshot> {
        match source {
            Source::Compositor => return self.compositor_screenshot(eye),
            Source::Headset => (),
        }
        let grabber = self.state.lock().headset.as_ref().map(Headset::grabber).context("no headset")?;
        grabber.screenshot(&self.api(), eye)
    }

    fn compositor_screenshot(&self, eye: Eye) -> Result<Screenshot> {
        // shortcut: assumes vrserver's container can write state_dir (true under $HOME), as for the action manifest
        let dir = self.paths().state_dir.join("screenshots");
        steamvr::private_dir(&dir)?;
        let t_s = clock::now_s();
        let stem = unique_stem(SystemTime::now(), std::process::id());
        let (preview, vr) = (dir.join(format!("{stem}-preview")), dir.join(&stem));
        let (p, v) = (preview.clone(), vr.clone());
        // TakeStereoScreenshot only queues the request; vrcompositor logs whether it took it
        let steam = self.paths().steam_dir;
        let log_from = steam.as_deref().map(|d| steamvr::log_mark(d, "vrcompositor"));
        let compositor_log = || steam.as_deref().zip(log_from).map(|(d, m)| steamvr::log_since(d, "vrcompositor", m)).unwrap_or_default();
        let taken = self.read(move |openvr| openvr.take_stereo_screenshot(&p, &v));
        let mut seen = Vec::new();
        let png = taken.and_then(|()| read_when_stable(&dir.join(format!("{stem}.png")), Duration::from_secs(10), || self.vr_exited(&mut seen), compositor_log));
        for f in [format!("{stem}-preview.png"), format!("{stem}.png")] {
            let _ = std::fs::remove_file(dir.join(f));
        }
        let (width, height, png) = headset::split_png(&png?, eye)?;
        Ok(Screenshot { png, width, height, source: Source::Compositor.name(), frame_timestamp_ms: None, t_s, at: clock::at(t_s) })
    }

    /// Changes the head or a controller; returns how long until an animation is done.
    pub fn apply(&self, a: RigAction) -> Result<Duration> {
        self.rig.lock().apply(a, Instant::now()).map_err(|e| anyhow!(e))
    }

    /// Runs the steps at their `at_s` offsets and returns when the last one and the latest animation are done.
    /// A step the rig would refuse fails the call before any runs (checked on a copy of the rig); one that fails anyway, because the rig changed meanwhile, is reported and the rest still run. No lock is held while waiting.
    pub async fn run_sequence(&self, steps: Vec<SequenceStep>) -> Result<SequenceReport> {
        let rig = self.rig.lock().clone(); // own statement: no lock while planning
        let plan = rig::plan_sequence(steps, &rig).map_err(|e| anyhow!(e))?;
        let (started_t_s, start) = (clock::now_s(), tokio::time::Instant::now());
        let (mut end, mut steps) = (start, vec![]);
        for (step, action) in plan {
            tokio::time::sleep_until(start + Duration::from_secs_f32(step.at_s)).await;
            let result = self.apply(action);
            let applied_s = start.elapsed().as_secs_f32();
            if let Ok(done_after) = &result {
                end = end.max(tokio::time::Instant::now() + *done_after);
            }
            let (ok, error) = (result.is_ok(), result.err().map(|e| e.to_string()));
            steps.push(AppliedStep { at_s: step.at_s, action: step.action, applied_s, ok, error });
        }
        tokio::time::sleep_until(end).await;
        Ok(SequenceReport { started_t_s, started_at: clock::at(started_t_s), steps, total_s: start.elapsed().as_secs_f32() })
    }

    /// Sets the play area (meters). The tracking thread sends it; the result says what ALVR's recentering mode does with it.
    pub fn set_playspace(&self, width: f32, depth: f32) -> Result<Value> {
        self.rig.lock().set_playspace(width, depth).map_err(|e| anyhow!(e))?;
        let warning = match self.session() {
            Ok(session) if matches!(session.to_settings().headset.recentering_mode, RecenteringMode::Stage) => None,
            Ok(_) => Some(RECENTER_NOT_STAGE.to_owned()),
            Err(e) => Some(format!("could not read recentering_mode ({e:#}); {RECENTER_NOT_STAGE}")),
        };
        Ok(serde_json::json!({ "ok": true, "width": width, "depth": depth, "warning": warning }))
    }

    /// Sends the current play area again, which is what makes ALVR recenter.
    pub fn recenter(&self) -> Value {
        self.rig.lock().recenter();
        serde_json::json!({ "ok": true, "note": "In Stage recentering, the MCP's default, ALVR's recenter is the identity: poses stay absolute and nothing moves. Other recentering_mode values re-origin the playspace around the head's last pose." })
    }

    /// Tells SteamVR the headset is worn or removed.
    pub fn set_proximity(&self, on_head: bool) -> Result<()> {
        // the reader sees proximity only as events, so connect it before the event is sent
        self.read(|_| Ok(())).ok();
        let context = self.state.lock().headset.as_ref().context("no headset")?.context();
        // only the ordering lock is held across the send: see the StreamingStarted arm
        let order = self.rig.lock().proximity_send.clone();
        let _order = order.lock();
        self.rig.lock().on_head = on_head;
        context.send_proximity_state(on_head);
        Ok(())
    }

    /// Haptic events the app sent with `t_s` after `since_s`, oldest first; `clear` empties the log.
    pub fn haptics(&self, since_s: f64, clear: bool) -> Vec<HapticEvent> {
        self.state.lock().headset.as_ref().map(|h| h.haptics(Some(since_s), clear)).unwrap_or_default()
    }

    /// SteamVR's own view of devices and hand skeletons.
    pub fn tracked_state(&self, include_bones: bool) -> Result<openvr::TrackedState> {
        self.read(move |vr| {
            vr.poll_quit(); // updates head_proximity; a Quit seen here stays flagged for pump_reader
            vr.tracked_state(include_bones)
        })
    }

    /// None when SteamVR is not running or the reader cannot connect.
    pub fn scene_app(&self) -> Option<openvr::SceneApp> {
        self.read(|vr| Ok(vr.scene_app())).ok()
    }

    /// None when SteamVR is not running, the reader cannot connect or lacks the overlay interface.
    pub fn dashboard_visible(&self) -> Option<bool> {
        self.read(|vr| Ok(vr.dashboard_visible())).ok().flatten()
    }

    /// Clicks the right controller's system button once if SteamVR's dashboard is open, then waits up
    /// to 3 s for it to close. A game started under an open dashboard stays unfocused until then.
    pub fn close_dashboard(&self) -> Result<DashboardReport> {
        let before = self.dashboard_visible();
        let (click, note) = dashboard_outcome(before, None);
        if !click {
            return Ok(DashboardReport { closed: false, was_visible: before, note });
        }
        let system = |down| {
            let args = ButtonArgs { hand: Hand::Right, button: Button::System };
            self.apply(if down { RigAction::PressButton(args) } else { RigAction::ReleaseButton(args) })
        };
        let pressed = system(true);
        std::thread::sleep(Duration::from_millis(100));
        // release even after a failed press, so the system button never stays down
        let released = system(false);
        pressed?;
        released?;
        let mut after = Some(true);
        for _ in 0..6 {
            std::thread::sleep(Duration::from_millis(500));
            after = self.dashboard_visible();
            if after != Some(true) {
                break;
            }
        }
        let (_, note) = dashboard_outcome(before, after);
        Ok(DashboardReport { closed: after == Some(false), was_visible: before, note })
    }

    /// Err when SteamVR is not running, no scene app runs, or the reader cannot connect.
    pub fn frame_stats(&self) -> Result<openvr::FrameStats> {
        self.read(|vr| vr.frame_stats())
    }

    /// Captures `seconds` (default 5, 0.1 to 60) of the default sink's monitor into a WAV file in the private runtime dir.
    // shortcut: recordings accumulate until logout (tmpfs), prune when a test records many
    pub fn record_audio(&self, seconds: Option<f64>) -> Result<Value> {
        let seconds = seconds.unwrap_or(5.0);
        if !(0.1..=60.0).contains(&seconds) {
            bail!("seconds must be from 0.1 to 60, got {seconds}");
        }
        let samples = audio::capture(seconds)?;
        let (t_s, [left, right]) = (clock::now_s(), audio::levels(&samples));
        let dir = self.paths().runtime_dir.join("audio");
        steamvr::private_dir(&self.paths().runtime_dir)?;
        steamvr::private_dir(&dir)?;
        let path = dir.join(format!("{}.wav", unique_stem(SystemTime::now(), std::process::id())));
        std::fs::write(&path, audio::wav(&samples)).with_context(|| path.display().to_string())?;
        let seconds = samples.len() as f64 / f64::from(audio::CHANNELS) / f64::from(audio::RATE);
        Ok(serde_json::json!({ "path": path, "seconds": seconds, "sample_rate": audio::RATE, "channels": audio::CHANNELS, "left": left, "right": right, "t_s": t_s, "at": clock::at(t_s) }))
    }

    /// Captures `window_s` (default 1, 0.05 to 10) of the default sink's monitor and reports each channel's level.
    pub fn audio_level(&self, window_s: Option<f64>) -> Result<Value> {
        let window_s = window_s.unwrap_or(1.0);
        if !(0.05..=10.0).contains(&window_s) {
            bail!("window_s must be from 0.05 to 10, got {window_s}");
        }
        let [left, right] = audio::levels(&audio::capture(window_s)?);
        let t_s = clock::now_s();
        Ok(serde_json::json!({ "window_s": window_s, "left": left, "right": right, "t_s": t_s, "at": clock::at(t_s) }))
    }

    /// Waits `window_s` (default 5, 0.5 to 60), then reports the compositor's frames in it, the
    /// cumulative-counter deltas and the mean of ALVR's statistics. A part that failed is null, with
    /// its error in `notes`.
    // shortcut: at most TIMINGS_MAX frames of SteamVR's history, raise if longer windows need full coverage
    pub fn frame_timing(&self, window_s: Option<f64>) -> Result<Value> {
        let window_s = window_s.unwrap_or(5.0);
        if !(0.5..=60.0).contains(&window_s) {
            bail!("window_s must be from 0.5 to 60, got {window_s}");
        }
        let mut notes = Vec::new();
        let start = Instant::now();
        let s0 = self.frame_stats().map_err(|e| notes.push(format!("counts: {e:#}")));
        std::thread::sleep(Duration::from_secs_f64(window_s).saturating_sub(start.elapsed()));
        let summaries = self.stats_since(start);
        let s1 = self.frame_stats().map_err(|e| notes.push(format!("counts: {e:#}")));
        let counts = s0.ok().zip(s1.ok()).map(|(a, b)| {
            serde_json::json!({
                "presents": b.presents.saturating_sub(a.presents),
                "dropped": b.dropped.saturating_sub(a.dropped),
                "reprojected": b.reprojected.saturating_sub(a.reprojected),
            })
        });
        let compositor = match self.read(|vr| Ok((vr.frame_timings()?, vr.nominal_hz()))) {
            Ok((t, hz)) => openvr::summarize(&t, window_s).map(|c| openvr::CompositorSummary { nominal_hz: hz, throttled: openvr::throttled(c.fps, hz), ..c }),
            Err(e) => {
                notes.push(format!("compositor: {e:#}"));
                None
            }
        };
        let alvr = alvr_api::average(&summaries);
        if alvr.is_none() {
            notes.push("alvr: no statistics summary arrived, the headset may not be streaming or the MCP's ALVR events stream is down".to_owned());
        }
        let t_s = clock::now_s();
        Ok(serde_json::json!({ "window_s": window_s, "t_s": t_s, "at": clock::at(t_s), "compositor": compositor, "counts": counts, "alvr": alvr, "notes": notes }))
    }

    /// Runs `f` on the OpenVR reader, which connects on first use and again when vrserver's pid
    /// changed. A reader that does not answer within OPENVR_TIMEOUT is left on its thread, and reads
    /// fail at once with WEDGED until that call returns.
    // shortcut: a stuck call keeps OpenVR's process-global client, so only an MCP restart frees it unless the call returns; reset the client in a child process if hangs become common
    fn read<T: Send + 'static>(&self, f: impl FnOnce(&OpenVr) -> Result<T> + Send + 'static) -> Result<T> {
        let mut slot = self.reader.lock();
        // the reader was dropped for a vrserver that is going away; do not reconnect to it. Checked with
        // the slot held: a stop or restart that begins later drops the reader only after this read
        if self.vr_down.is_locked() {
            bail!("SteamVR is stopping or restarting");
        }
        let vrserver = steamvr::pids_named("vrserver").first().copied();
        let reader = match slot.take() {
            Some((pid, reader)) if Some(pid) == vrserver => Some(reader),
            Some((_, stale)) => {
                self.dispose_reader(stale);
                None
            }
            None => None,
        };
        // a reconnect would block the full timeout on the stuck client and leave one more thread
        if self.openvr_wedged() {
            bail!("{WEDGED}");
        }
        let pid = vrserver.context("SteamVR is not running")?;
        let steamvr_dir = self.paths().steamvr_dir.clone().context("SteamVR not found; set steamvr_dir in config.toml")?;
        let state_dir = self.paths().state_dir.clone();
        let (reader, result) = self.openvr_call(move || -> Result<_> {
            let reader = match reader {
                Some(r) => r,
                None => OpenVr::connect(&steamvr_dir, &state_dir)?,
            };
            let result = f(&reader);
            Ok((reader, result))
        })
        .with_context(|| format!("OpenVR did not answer (deadline {} s)", OPENVR_TIMEOUT.as_secs()))??;
        *slot = Some((pid, reader));
        result
    }

    /// `within(OPENVR_TIMEOUT, f)`, counted in `openvr_calls` until `f` returns, also after a timeout.
    fn openvr_call<T: Send + 'static>(&self, f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
        counted_call(&self.openvr_calls, OPENVR_TIMEOUT, f)
    }

    /// Whether an earlier OpenVR call still hangs; meaningful only while the reader lock is held.
    fn openvr_wedged(&self) -> bool {
        self.openvr_calls.load(SeqCst) > 0
    }

    /// Drops an OpenVR reader within OPENVR_TIMEOUT, or leaves it on its thread.
    fn dispose_reader(&self, reader: OpenVr) {
        if self.openvr_call(move || drop(reader)).is_none() {
            warn!("OpenVR reader did not disconnect within {} s; left running", OPENVR_TIMEOUT.as_secs());
        }
    }

    /// Polls the reader's events, so a SteamVR quit from outside drops it instead of SteamVR
    /// killing the MCP. Skipped while a read holds the reader; the watcher calls it every 1 s.
    fn pump_reader(&self) {
        let Some(mut slot) = self.reader.try_lock() else { return };
        let Some((pid, reader)) = slot.take() else { return };
        match self.openvr_call(move || {
            let quit = reader.poll_quit();
            (reader, quit)
        }) {
            Some((reader, false)) => *slot = Some((pid, reader)),
            Some((reader, true)) => self.dispose_reader(reader),
            None => warn!("OpenVR events did not answer within {} s; reader left running", OPENVR_TIMEOUT.as_secs()),
        }
    }

    /// Disconnects the reader while vrserver is still up; called before every stop and restart.
    fn drop_reader(&self) {
        if let Some((_, reader)) = self.reader.lock().take() {
            self.dispose_reader(reader);
        }
    }

    fn headset_status(&self) -> Option<HeadsetStatus> {
        self.state.lock().headset.as_ref().map(Headset::status)
    }

    fn check_version(&self) -> Result<()> {
        let version = self.api().version()?;
        if !alvr_api::compatible(&version) {
            bail!("ALVR server {} is not compatible with {}", version.trim(), *alvr_common::ALVR_VERSION);
        }
        Ok(())
    }

    pub fn start(&self) -> Result<StartReport> {
        // vr_stop holds it too, to wait for a start it ended
        let Some(_starting) = self.starting.try_lock() else { bail!("not ready: vr_start or vr_stop is running") };
        // held until start_inner's waits, so no SteamVR restart moves the headset while it sets up
        let Some(no_restart) = self.restarting.try_lock() else { bail!("not ready: SteamVR is restarting") };
        let epoch = {
            let mut state = self.state.lock();
            if state.stopping > 0 {
                bail!("not ready: vr_stop is running");
            }
            state.watchdog = Watchdog::default();
            state.ended = None;
            state.stops
        };
        let started = self.start_inner(epoch, no_restart);
        if let Err(e) = &started {
            let mut state = self.state.lock();
            state.managing = false;
            // the auto-claimed lease stays while leftovers hold the machine; vr_stop frees both (and one that ended this start is about to)
            if state.stops == epoch && (state.headset.is_some() || state.dashboard.is_some() || state.started_steam) {
                bail!("{e:#}; run vr_stop to free SteamVR");
            }
        }
        started.map(|r| self.finish_start(r))
    }

    /// A start that succeeded, also one that found a stream: an older restart failure no longer applies.
    fn finish_start(&self, report: StartReport) -> StartReport {
        self.state.lock().last_restart_error = None;
        StartReport { dashboard_visible: self.dashboard_visible(), ..report }
    }

    /// Err once a vr_stop ran since `epoch`, State::stops when the vr_start began.
    fn check_stop(&self, epoch: u64) -> Result<()> {
        if self.state.lock().stops != epoch {
            bail!("failed: vr_stop ended the session");
        }
        Ok(())
    }

    /// Hands the session to the supervisor, unless a vr_stop ran since `epoch`: it already gave the session up.
    fn begin_managing(&self, epoch: u64) -> Result<u32> {
        let mut state = self.state.lock();
        if state.stops != epoch {
            bail!("failed: vr_stop ended the session");
        }
        state.managing = true;
        Ok(state.restarts)
    }

    /// The error of a vr_start that vr_stop ended. After a launch it first lets Steam's hand-off start SteamVR, so the stop that waits for this start finds it,
    /// and the Steam client this start started (`started_steam`).
    fn stopped_start(&self, launched: Option<Instant>, started_steam: bool) -> alvr_common::anyhow::Error {
        if let Some(t) = launched {
            self.vr.settle(t);
        }
        if started_steam {
            self.record_steam_client();
        }
        anyhow!("failed: vr_stop ended the session")
    }

    /// Keeps the running Steam client for vr_stop, after this MCP started Steam: Steam runs one instance, so it is ours.
    /// None found drops an older record: launch() started Steam only because none ran, so that client is gone.
    fn record_steam_client(&self) {
        self.state.lock().steam_client = steamvr::steam_client();
    }

    fn start_inner(&self, epoch: u64, no_restart: alvr_common::parking_lot::MutexGuard<'_, ()>) -> Result<StartReport> {
        let began = Instant::now();
        // one deadline for the whole start, an adopted SteamVR's restart included
        let timeout = Duration::from_secs(self.cfg().timeouts.streaming_s);
        let deadline = began + timeout;
        let left = || deadline.saturating_duration_since(Instant::now());
        let mut report = StartReport::default();
        if let Some(h) = self.headset_status().filter(|h| h.streaming) {
            let restarts = self.begin_managing(epoch)?; // a stream the MCP left unmanaged gets its supervisor back
            return Ok(StartReport { already_streaming: true, restarts, view_resolution: h.view_resolution, fps: h.fps, codec: h.codec, ..report });
        }
        let session_file = self.paths().session_file().context("ALVR streamer not found; set streamer_dir in config.toml")?;

        if !self.cfg().steam.env.is_empty() && !steamvr::is_steamvr_running() && steamvr::is_steam_running() {
            // shortcut: unreadable environ (other user, hidepid) skips the check, fail late as before
            if let Some(environ) = steamvr::running_steam_environ() {
                let bad = steamvr::env_mismatches(&environ, &self.cfg().steam.env);
                if !bad.is_empty() {
                    bail!("Steam is running without steam.env ({}); close Steam, then vr_start starts it with steam.env", bad.join(", "));
                }
            }
        }
        // a new session starts from the defaults; an already-streaming vr_start returned above, and restarts never come here
        self.rig.lock().reset();
        if cfg!(test) {
            bail!("unit tests never start a headset"); // a second ALVR client, and setup on the user's real files
        }
        let steamvr_running = steamvr::is_steamvr_running();
        let up = headset_up(steamvr_running);
        // ClientCoreContext::new writes the client session.json that client_hostname reads, so a client exists first
        if up == HeadsetUp::BeforeLaunch {
            // resume: a failed cold start can leave its client paused
            self.state.lock().headset.get_or_insert_with(|| Headset::start(Arc::clone(&self.rig))).resume();
        } else {
            // as in restart_steamvr: the client stays paused until ALVR's HMD is active
            let taken = self.state.lock().headset.take(); // own statement: the guard must not outlive it
            let h = taken.unwrap_or_else(|| Headset::start(Arc::clone(&self.rig)));
            if let Some(h) = self.paused(h) {
                self.state.lock().headset.get_or_insert(h);
            }
        }
        // a stop that came meanwhile disposes the headset once this start returns
        self.check_stop(epoch)?;
        let hostname = headset::client_hostname()?;
        let values = setup::session_values(&self.cfg())?;
        if !steamvr_running {
            foreign_alvr_guard(false, || self.api().ping(), self.api().port)?;
            if steamvr::has_leftovers(&self.vr.alive()) && !self.vr.kill() {
                bail!("SteamVR processes still running after SIGKILL: {:?} (deadline {} s)", self.vr.alive(), (4 * self.vr.kill_wait).as_secs());
            }
            // nothing runs, so SteamVR no longer rewrites the file; a crash or kill left blocks behind
            if setup::refresh_stale_snapshot(&self.paths())? {
                report.notes.push("restore.json predated a SteamVR setup without the MCP's changes; took a fresh snapshot".into());
            }
            setup::snapshot(&self.paths())?;
            let unblocked = setup::undo_safe_mode_blocks(&self.paths())?;
            if !unblocked.is_empty() {
                report.notes.push(format!("unblocked add-ons: {}", unblocked.join(", ")));
            }
            report.unblocked_safe_mode = setup::prepare(&self.paths(), &values, &hostname)?.unblocked_safe_mode
                || unblocked.iter().any(|d| d == "driver_alvr_server");
        } else {
            // a vr_stop ends the wait too
            adoptable(steamvr::vrserver_has_alvr(), || self.check_stop(epoch).is_err() || self.api().ping(), ALVR_API_WAIT, self.api().port)?;
            self.check_stop(epoch)?;
            self.check_version()?;
            setup::snapshot(&self.paths())?;
            report.adopted = true;
            // shortcut: ALVR TCP EOF spin (sockets/src/stream_socket/tcp.rs:137-147) leaves the server stuck Disconnecting, and
            // such a server may never answer a write, so decide before any POST; drop when fixed upstream
            let stuck = stuck_in_session_file(&session_file, &hostname);
            if stuck {
                self.restart_adopted(epoch, &no_restart, left())?;
            }
            match self.adopt_posts(&values, &hostname) {
                // a POST ALVR took and never answered: stuck too, so restart it and post once more to the fresh ALVR
                Err(e) if !stuck && timed_out(&e) => {
                    warn!("{e:#}; restarting the adopted SteamVR");
                    self.restart_adopted(epoch, &no_restart, left())?;
                    self.adopt_posts(&values, &hostname)?;
                }
                posted => posted?,
            }
        }

        self.ensure_dashboard(epoch)?;
        let paths = self.paths();
        let steam_dir = paths.steam_dir.as_deref();
        let log_from = steam_dir.map_or(0, steamvr::console_log_len);
        let vrserver_from = steam_dir.map_or((0, None), |d| steamvr::log_mark(d, "vrserver"));
        let (mut handed, mut launched) = (None, None);
        if !steamvr_running {
            report.started_steam = steamvr::launch(&self.cfg(), STEAMVR_URL)?;
            launched = Some(Instant::now());
            self.state.lock().started_steam |= report.started_steam;
            handed = (!report.started_steam).then_some(STEAMVR_URL);
        }

        self.begin_managing(epoch).map_err(|_| self.stopped_start(launched, report.started_steam))?;
        drop(no_restart);
        let stopped = || self.check_stop(epoch).is_err();
        let (mut seen, mut exited) = (Vec::new(), None);
        let answered = || {
            exited = self.vr_exited(&mut seen);
            stopped() || exited.is_some() || self.api().ping()
        };
        // start() adds its own vr_stop advice to a block
        let waited = steamvr::wait_alvr(paths.vrsettings_file().as_deref(), steam_dir, log_from, handed, left(), "", answered);
        if report.started_steam {
            self.record_steam_client(); // Steam is up by now, whatever the wait found
        }
        if !waited? {
            bail!("ALVR server did not answer on port {} (deadline {} s)", self.api().port, timeout.as_secs());
        }
        if stopped() {
            return Err(self.stopped_start(launched, report.started_steam));
        }
        if let Some(name) = exited {
            bail!("failed: {name} exited before ALVR answered");
        }
        // a foreign ALVR that came up during the wait answers before ours could
        foreign_alvr_guard(steamvr::is_steamvr_running(), || true, self.api().port)?;
        self.check_version()?;
        if up == HeadsetUp::AfterAlvrAnswers {
            // ALVR answers from inside DriverProvider::Init, before Hmd::activate, so wait for vrserver to log the activation
            let activated = steam_dir.is_some_and(|d| {
                let deadline = Instant::now() + HMD_ACTIVATION_WAIT;
                while !hmd_activated(&steamvr::log_since(d, "vrserver", vrserver_from)) {
                    if stopped() || Instant::now() >= deadline {
                        return false;
                    }
                    thread::sleep(Duration::from_secs(1));
                }
                true
            });
            if !activated && !stopped() {
                report.notes.push(format!(
                    "vrserver.txt showed no ALVR HMD activation within {} s; resumed the headset anyway, so it may stream 0 frames",
                    HMD_ACTIVATION_WAIT.as_secs()
                ));
            }
            let mut state = self.state.lock(); // managing is read under the same lock as the resume, so vr_stop cannot leave a client behind
            match headset_step(state.managing, state.headset.is_some()) {
                HeadsetStep::Resume => state.headset.iter().for_each(Headset::resume),
                HeadsetStep::StartNew => state.headset = Some(Headset::start(Arc::clone(&self.rig))),
                HeadsetStep::Skip => (),
            }
        }
        let streaming = self.wait_streaming(began, left(), 0)?;
        let h = self.headset_status().unwrap_or_default();
        if !streaming {
            let mut reason = format!("headset not streaming (deadline {} s); last HUD message: {:?}", timeout.as_secs(), h.hud_message);
            if !self.state.lock().started_steam {
                reason += "; Steam was already running, so the MCP could not set its environment (steam.args)";
            }
            bail!(reason);
        }
        report.restarts = self.state.lock().restarts;
        Ok(StartReport { view_resolution: h.view_resolution, fps: h.fps, codec: h.codec, ..report })
    }

    /// Spawns the alvr_dashboard child unless one runs: ALVR creates the headset at SteamVR start only while it exists.
    fn ensure_dashboard(&self, epoch: u64) -> Result<()> {
        let mut state = self.state.lock();
        if state.stops != epoch {
            bail!("failed: vr_stop ended the session");
        }
        if state.dashboard.is_none() {
            state.dashboard = Some(steamvr::spawn_dashboard_child(&self.paths().runtime_dir)?);
        }
        Ok(())
    }

    /// An adopt's writes to ALVR: vr_start's session values and the headset's client entry.
    fn adopt_posts(&self, values: &[PathValuePair], hostname: &str) -> Result<()> {
        self.api().post_json("/api/session/values", &values)?;
        for action in setup::client_actions() {
            self.api().post_json("/api/session/client-connections", &(hostname, action))?;
        }
        Ok(())
    }

    /// Restarts an adopted SteamVR whose ALVR is stuck, keeping vr_start's `held` restarting guard. The
    /// session is handed to the supervisor first: a restart runs only while one is managed.
    fn restart_adopted(&self, epoch: u64, held: &alvr_common::parking_lot::MutexGuard<'_, ()>, timeout: Duration) -> Result<()> {
        self.ensure_dashboard(epoch)?;
        self.begin_managing(epoch)?;
        self.restart_locked(held, false, timeout)
    }

    pub fn stop(&self) -> Result<StopReport> {
        let (managing, started_steam, dashboard) = {
            let state = self.state.lock();
            (state.managing, state.started_steam, state.dashboard.is_some())
        };
        let blocked = || self.paths().vrsettings_file().is_some_and(|f| setup::alvr_blocked(&f));
        let ours = || stop_is_ours(managing, started_steam, dashboard, blocked);
        // the Stack's VrProcs: a unit test never sees the user's SteamVR
        stop_guard(self.vr.vrserver_running(), || ours() || self.api().ping() || steamvr::vrserver_has_alvr())?;
        {
            let mut state = self.state.lock();
            state.managing = false;
            state.stops += 1;
            state.stopping += 1;
            state.ended = None;
            state.last_restart_error = None;
        }
        // an in-flight vr_start bails at its next check; what it left (headset, dashboard, the SteamVR it launched) goes below
        let _no_start = self.starting.lock();
        self.state.lock().stopping -= 1; // a vr_start now waits for `starting` instead
        let _no_restart = self.restarting.lock();
        let _vr_down = self.vr_down.lock();
        self.drop_reader();
        // a game spawned by the MCP may not quit with SteamVR, and its wineserver blocks the next proton launch
        let mut report = StopReport { stopped_app: self.stop_launched(), ..Default::default() };
        // ALVR's client spins forever on a TCP stream whose server went away
        // (sockets/src/stream_socket/tcp.rs:137-147), so disconnect it while the server is up
        let headset = self.state.lock().headset.take();
        if let Some(h) = headset {
            self.dispose(h);
        }
        // by pid: the name checks missed a vrserver that lived on 30 s and needed SIGKILL
        let snapshot = self.vr.snapshot();
        if self.api().ping() {
            // on failure the signals below still stop SteamVR
            self.api().post("/api/steamvr/shutdown").map_err(|e| error!("{e:#}")).ok();
        }
        let ended = self.vr.end(&snapshot);
        report.used_signals = ended.used_signals;
        if ended.gone {
            // SteamVR has exited, so it no longer rewrites the file
            setup::undo_safe_mode_blocks(&self.paths()).map_err(|e| error!("{e:#}")).ok();
        }

        let (dashboard, started_steam, client) = {
            let mut state = self.state.lock();
            (state.dashboard.take(), std::mem::take(&mut state.started_steam), state.steam_client.take())
        };
        if let Some(mut child) = dashboard {
            child.kill().ok();
            child.wait().ok();
        }
        let client_alive = |c: (u32, u64)| !steamvr::survivors(&[c], steamvr::stat).is_empty();
        let live = client.filter(|&c| client_alive(c));
        let mut notes = Vec::new();
        // a dead record leaves Steam alone: the Steam running now is not the one vr_start started
        let dead_record = client.is_some() && live.is_none();
        if started_steam || client.is_some() {
            let mut kept = None;
            if !dead_record {
                // self.vr's first Steam name is the client (`steam`); a unit test's is a fake, and launch() refuses there
                if self.vr.steam.first().is_some_and(|n| steamvr::has_live(n)) {
                    // Steam is running, so this only forwards -shutdown
                    if let Err(e) = steamvr::launch(&self.cfg(), "-shutdown") {
                        notes.push(format!("forwarding -shutdown to Steam failed: {e:#}"));
                    }
                }
                let gone = || self.vr.live_steam().is_empty();
                // after an online session Steam can ignore -shutdown for 40 s or more; SIGTERM ends its client in seconds
                if !steamvr::wait_for(self.vr.steam_wait, gone) && let Some((pid, _)) = live.filter(|&c| client_alive(c)) {
                    steamvr::terminate(pid, libc::SIGTERM);
                    report.signalled_steam = true;
                    steamvr::wait_for(self.vr.steam_term_wait, gone);
                }
                // only the recorded client, while it lives, carries over: a Steam started later is the user's
                kept = live.filter(|&c| client_alive(c));
                self.state.lock().steam_client = kept;
            }
            let left = self.vr.live_steam();
            report.closed_steam = left.is_empty();
            report.remaining_steam = left.iter().map(|n| n.to_string()).collect();
            if !left.is_empty() && kept.is_none() {
                notes.push(if dead_record {
                    "the Steam client this MCP started has exited, perhaps restarted by Steam itself; the Steam running now was left alone".into()
                } else {
                    "Steam is still running and this MCP recorded no client of it, so no later vr_stop will close it".into()
                });
            }
        }
        report.steam_note = (!notes.is_empty()).then(|| notes.join("; "));
        report.remaining_processes = self.vr.remaining(&snapshot);
        self.release_if_clean(&report);
        Ok(report)
    }

    /// Own statement for callers: read() locks the reader again.
    pub fn reader_connected(&self) -> bool {
        self.reader.lock().is_some()
    }

    /// Whether an OpenVR call hangs: taken under the reader lock, so a read in progress does not count.
    fn reader_wedged(&self) -> bool {
        let _slot = self.reader.lock();
        self.openvr_wedged()
    }

    pub fn status(&self) -> StatusReport {
        let (reader_connected, wedged) = (self.reader_connected(), self.reader_wedged());
        let (warnings, last_restart_error) = {
            let state = self.state.lock();
            let streaming = state.headset.as_ref().is_some_and(|h| h.status().streaming);
            // the supervisor does not retry a failed restart
            let stuck = state.managing && state.last_restart_error.is_some() && !streaming && !self.starting.is_locked() && !self.restarting.is_locked();
            let gave_up = state.managing && state.watchdog.gave_up && !streaming;
            let stayed_quiet = state.managing && state.watchdog.stayed_quiet && !streaming && !gave_up && !stuck;
            let warnings: Vec<&str> = [
                state.leaked.then_some(LEAKED),
                wedged.then_some(WEDGED),
                state.ended,
                stuck.then_some(RESTART_FAILED),
                gave_up.then_some(WATCHDOG_GAVE_UP),
                stayed_quiet.then_some(WATCHDOG_STAYED_QUIET),
            ]
            .into_iter()
            .flatten()
            .collect();
            (warnings, state.last_restart_error.as_ref().map(|(t, e)| format!("{} s ago: {e}", t.elapsed().as_secs())))
        };
        let now = clock::now_s();
        let held = lease::read(&self.paths().runtime_dir);
        // pid 0 is nobody, so our own live lease shows too
        let owner = lease::blocking(held.as_ref(), 0, SystemTime::now(), lease::alive)
            .map(|l| Owner { mine: l.pid == std::process::id(), lease: l.clone() });
        StatusReport {
            steam_running: steamvr::is_steam_running(),
            steamvr_running: steamvr::is_steamvr_running(),
            alvr_version: self.api().quick_version().ok().map(|v| v.trim().to_owned()),
            headset: self.headset_status(),
            on_head: self.rig.lock().on_head,
            scene_app: reader_connected.then(|| self.scene_app()).flatten(),
            warning: (!warnings.is_empty()).then(|| warnings.join("; ")),
            last_restart_error,
            now_t_s: now,
            now_at: clock::at(now),
            t0_at: clock::at(0.0),
            owner,
            blocked_drivers: self.paths().vrsettings_file().map(|f| setup::blocked_drivers(&f)).unwrap_or_default(),
        }
    }

    /// Waits up to `timeout` for a stream with a codec (DecoderConfig follows StreamingStarted) and a
    /// generation above `after`. Fails at once when vr_stop ends the session, a SteamVR restart
    /// that failed after `since` (one the supervisor ran too) means no stream will come, or SteamVR exits.
    fn wait_streaming(&self, since: Instant, timeout: Duration, after: u64) -> Result<bool> {
        let (mut aborted, mut seen) = (None, Vec::new());
        let streaming = steamvr::wait_for(timeout, || {
            let exited = self.vr_exited(&mut seen); // own statement: it reads `restarting`, not `state`
            let state = self.state.lock();
            aborted = match &state.last_restart_error {
                _ if !state.managing => Some(state.ended.unwrap_or("vr_stop ended the session").to_owned()),
                Some((t, e)) if *t >= since => Some(e.clone()),
                _ => exited.map(|name| format!("{name} exited before the headset streamed")),
            };
            let h = state.headset.as_ref().map(Headset::status).unwrap_or_default();
            aborted.is_some() || (h.streaming && h.codec.is_some() && h.stream_generation > after)
        });
        match aborted {
            Some(why) => bail!("failed: {why}"),
            None => Ok(streaming),
        }
    }

    /// steamvr::vr_exited, except during a SteamVR restart, which stops SteamVR on purpose: it starts the watch over.
    fn vr_exited(&self, seen: &mut Vec<(&'static str, Option<Instant>)>) -> Option<&'static str> {
        if self.restarting.is_locked() {
            seen.clear();
            return None;
        }
        steamvr::vr_exited(seen)
    }

    /// Sets ALVR session values and checks ALVR stored them. Then, unless ALVR's schema flags every
    /// path real-time, restarts SteamVR and waits for a new stream.
    pub fn set_alvr_settings(&self, values: Vec<SettingValue>) -> Result<SettingsReport> {
        let began = Instant::now();
        // held until the values are applied, so no SteamVR restart or vr_stop takes ALVR down meanwhile
        let Some(no_restart) = self.restarting.try_lock() else { bail!("not ready: SteamVR is restarting") };
        let values = parse_values(values)?;
        let restart = !values.iter().all(|v| is_real_time(&v.path));
        // without vr_start the session.json is the user's own, with no snapshot for vr_restore
        if !self.state.lock().managing {
            bail!("set_alvr_settings changes only the session vr_start set up; run vr_start first");
        }
        let mut events = self.api().events()?;
        self.api().post_json("/api/session/values", &values)?;
        wait_applied(|t| events.next(t), &values, Instant::now() + Duration::from_secs(3))?;
        drop(events);
        drop(no_restart);

        let status = || self.headset_status().unwrap_or_default();
        if !restart {
            return Ok(SettingsReport { streaming: status().streaming, ..Default::default() });
        }
        let restarts = self.state.lock().restarts; // own statement: status() locks state too
        let generation = status().stream_generation;
        const STORED: &str = "the values are already stored in ALVR; SteamVR may be down";
        // shortcut: ALVR TCP EOF spin (sockets/src/stream_socket/tcp.rs:137-147): on loopback a headset reconnect strands the server, so restart SteamVR instead of reconnecting for ALVR's handshake check; drop when fixed upstream
        self.restart_steamvr(false).context(STORED)?;
        // a restart ALVR asks for once the headset negotiates (e.g. a new fps) is the supervisor's
        let timeout = Duration::from_secs(self.cfg().timeouts.streaming_s);
        let streaming = self.wait_streaming(began, timeout, generation).context(STORED)?;
        if !streaming {
            bail!("headset not streaming after the SteamVR restart (deadline {} s); last HUD message: {:?}", timeout.as_secs(), status().hud_message);
        }
        let restarts = self.state.lock().restarts.saturating_sub(restarts);
        Ok(SettingsReport { restarted: restarts > 0, restarts, streaming })
    }

    /// Launches a Steam app or non-Steam shortcut through Steam, or a Steam app through its Proton,
    /// and returns once SteamVR reports a new scene app running.
    pub fn launch_app(&self, a: LaunchArgs) -> Result<AppReport> {
        check_launch(&a).map_err(|e| anyhow!(e))?;
        // its scene wait reads through OpenVR, so a wedged client would only time out
        if self.reader_wedged() {
            bail!("{WEDGED}");
        }
        launch_guard(steamvr::is_steamvr_running())?;
        let id: u64 = a.app_id.trim().parse().with_context(|| format!("app_id must be a decimal app id, got {:?}", a.app_id))?;
        let paths = self.paths();
        let steam = paths.steam_dir.as_deref().context("Steam not found; set steam_dir in config.toml")?;
        let app = steamvr::find_app(steam, id)?;
        self.app_logs.lock().reset(app.app_id, Some(steam));
        // SteamVR Home is a scene app too, so wait for a scene pid that was not there before
        // shortcut: relaunching an app that already runs times out, as Steam only focuses it; check its process if that matters
        let before = self.scene_app().map(|a| a.pid);
        let log_from = steamvr::console_log_len(steam);
        let (mut run_log, mut exited, mut handed) = (None, None, None);
        // shortcut: a new launch forgets the last proton run's group, which stop_app then reaches only while it is the scene; refuse while it runs if one lingers
        self.state.lock().launched = None;
        match a.via {
            Via::Steam => {
                let url = format!("steam://rungameid/{}", app.game_id);
                if !steamvr::launch(&self.cfg(), &url)? {
                    handed = Some(url);
                }
            }
            Via::Proton => {
                // shortcut: no non-Steam shortcuts in proton mode, upgrade by extending parse_shortcuts
                if app.kind != steamvr::AppKind::Steam {
                    bail!("via \"proton\" takes Steam apps only; {} is a non-Steam shortcut", app.name);
                }
                std::fs::create_dir_all(&app.compatdata).with_context(|| app.compatdata.display().to_string())?;
                let base = steamvr::steam_env(std::env::vars_os(), &self.cfg().steam.env);
                let l = proton::plan(steam, &app, a.exe.as_deref().unwrap_or_default(), &a.args, base, &a.env)?;
                let runs = paths.state_dir.join("runs");
                steamvr::private_dir(&runs)?;
                let log = runs.join(format!("{}.log", app.app_id));
                exited = Some(self.spawn_launched(&l, &log)?.exited);
                run_log = Some(log);
            }
        }
        let timeout = Duration::from_secs(self.cfg().timeouts.app_s);
        let (mut scene_pid, mut seen, mut scene_exited) = (0, Vec::new(), None);
        let launcher_exited = || exited.as_ref().and_then(|e| *e.lock());
        let started = || {
            let scene = self.scene_app().filter(|a| Some(a.pid) != before);
            let pid = game_pid(scene.as_ref(), steamvr::process_name).ok();
            scene_pid = pid.filter(|_| scene.as_ref().is_some_and(|a| a.state == "running")).unwrap_or(0);
            // a new scene pid, in any state, whose process is gone again: the game quit or crashed
            // shortcut: a scene pid that hands off to a child process reads as a crash, ignore an exit while a new scene pid follows within a grace if a game does this
            scene_exited = steamvr::gone_after_seen(&mut seen, pid, |p| steamvr::comm(p).is_some(), Instant::now(), Duration::ZERO);
            scene_pid != 0 || launcher_exited().is_some() || scene_exited.is_some()
        };
        let running = match a.via {
            Via::Steam => steamvr::wait_launched(Some(steam), log_from, handed.as_deref(), timeout, started)?,
            Via::Proton => steamvr::wait_for(timeout, started),
        };
        if let (0, Some(status), Some(log)) = (scene_pid, launcher_exited(), &run_log) {
            bail!("{}: failed: the Proton launcher exited with {status} before a VR scene ran; output in {}", app.name, log.display());
        }
        if let (0, Some(pid)) = (scene_pid, scene_exited) {
            bail!("{} ({}): failed: scene pid {pid} exited before its VR scene ran; last vrserver log lines:\n{}", app.name, app.game_id, steamvr::log_tail(steam, "vrserver", 20).join("\n"));
        }
        if !running {
            bail!(
                "{} ({}) has no running VR scene (deadline {} s); compatibility tool: {}; last vrserver log lines:\n{}",
                app.name,
                app.game_id,
                timeout.as_secs(),
                app.compat_tool.as_deref().unwrap_or("none"),
                steamvr::log_tail(steam, "vrserver", 20).join("\n")
            );
        }
        let mut notes = Vec::new();
        // a failed click must not fail a launch that already worked
        match self.close_dashboard() {
            Ok(d) if d.was_visible == Some(true) => notes.push(d.note.to_owned()),
            Ok(_) => {}
            Err(e) => notes.push(format!("dashboard: {e:#}")),
        }
        Ok(AppReport { app, scene_pid, via: a.via, run_log, notes })
    }

    /// Spawns a proton run and keeps it for stop_app and vr_stop. Checks `managing` under the same lock
    /// as the store, so a concurrent vr_stop either finds the run or ended the session before it spawned.
    fn spawn_launched(&self, l: &proton::ProtonLaunch, log: &Path) -> Result<proton::Launched> {
        let mut state = self.state.lock();
        if !state.managing {
            bail!("no vr_start session is being managed; run vr_start");
        }
        let launched = proton::spawn(l, log)?;
        state.launched = Some(launched.clone());
        Ok(launched)
    }

    /// Stops the app: SIGTERM to the scene pid, to the group launch_app started, and to every
    /// process with the game's STEAM_COMPAT_DATA_PATH and app id (wine processes leave the group); SIGKILL after 15 s.
    pub fn stop_app(&self) -> Result<Value> {
        let scene = game_pid(self.scene_app().as_ref(), steamvr::process_name);
        let launched = self.state.lock().launched.clone();
        if launched.is_none() {
            scene.map_err(|e| anyhow!(e))?;
        }
        let scene = scene.ok();
        let stopped = proton::stop(scene, launched.as_ref())?; // fails only while something still runs: keep tracking it
        let mut state = self.state.lock();
        // a launch_app during the wait stored a new run: keep that one
        if state.launched.as_ref().is_none_or(|now| launched.as_ref().is_some_and(|then| Arc::ptr_eq(&now.exited, &then.exited))) {
            state.launched = None;
        }
        drop(state);
        if stopped.is_empty() {
            bail!("no app running");
        }
        Ok(serde_json::json!({ "stopped_pid": scene, "stopped": stopped }))
    }

    /// Ends the last `launch_app via proton` game and forgets it; keeps it tracked when it outlives SIGKILL.
    fn stop_launched(&self) -> Vec<u32> {
        let Some(launched) = self.state.lock().launched.clone() else { return vec![] };
        match proton::stop(None, Some(&launched)) {
            Ok(stopped) => {
                let mut state = self.state.lock();
                if state.launched.as_ref().is_some_and(|now| Arc::ptr_eq(&now.exited, &launched.exited)) {
                    state.launched = None;
                }
                stopped
            }
            Err(e) => {
                error!("{e:#}");
                vec![]
            }
        }
    }

    /// The app's log lines from `since_seq`, after reading what its files gained.
    pub fn read_app_log(&self, a: ReadAppLogArgs) -> Result<applog::Page> {
        let id = app_id(&a.app_id)?;
        let limit = a.limit.unwrap_or(applog::PAGE);
        if !(1..=applog::PAGE).contains(&limit) {
            bail!("limit must be 1 to {}", applog::PAGE);
        }
        let steam = self.paths().steam_dir;
        let mut logs = self.app_logs.lock();
        logs.poll_app(id, steam.as_deref());
        logs.page(id, a.since_seq.unwrap_or(0), a.contains.as_deref(), limit).map_err(|e| anyhow!(e))
    }

    /// Polls the app's logs every 200 ms until a line matches `fail_pattern` or `pattern`, or the timeout passes. Once the app
    /// exits it reads every 50 ms until two reads in a row add no byte (at most LOG_DRAIN, which can run past the timeout), counts an
    /// unfinished last line, then fails.
    pub fn wait_for_log(&self, a: WaitForLogArgs) -> Result<Value> {
        let (id, pattern, fail, timeout) = wait_plan(&a, |id| self.app_logs.lock().has_app(id)).map_err(|e| anyhow!(e))?;
        let deadline = Instant::now() + timeout;
        let mut cursor = a.since_seq.unwrap_or(0);
        let launcher = self.state.lock().launched.as_ref().filter(|l| l.app_id == id).map(|l| l.exited.clone());
        // shortcut: the scene may be another app than `id`, compare the scene's app key if two apps run at once
        let scene = self.scene_app().filter(|s| s.state == "running");
        let scene = game_pid(scene.as_ref(), steamvr::process_name).ok().map(|pid| (pid, steamvr::comm(pid)));
        let mut exited: Option<(String, Instant)> = None;
        let mut quiet = 0;
        loop {
            // sampled before the read, so a line written before the exit is always read after it
            if exited.is_none() {
                let launcher = launcher.as_ref().and_then(|e| *e.lock());
                let alive = |pid| steamvr::comm(pid) == scene.as_ref().and_then(|s| s.1.clone());
                exited = app_exited(launcher, scene.as_ref().map(|s| s.0), alive).map(|why| (why, Instant::now() + LOG_DRAIN));
            }
            // checked before the read, so that read still sees the writer's last bytes
            let drain_over = exited.as_ref().is_some_and(|(_, until)| Instant::now() >= *until);
            let steam = self.paths().steam_dir;
            let hit = {
                let mut logs = self.app_logs.lock();
                // bytes, not lines: a last line still being written is activity
                let mark = |logs: &AppLogs| (logs.next_seq(id), logs.bytes_read(id));
                let before = mark(&logs);
                logs.poll_app(id, steam.as_deref());
                let grew = mark(&logs) != before;
                // counted from the exit; one quiet read is too few, a child may write a line in pieces
                quiet = if grew || exited.is_none() { 0 } else { quiet + 1 };
                // a writer still adding to the line at LOG_DRAIN has run past the bound, so a split there is accepted
                if quiet >= 2 || drain_over {
                    logs.flush_partial(id);
                }
                // scanned every round: the ring evicts past LINES_KEPT before a long drain ends
                let hit = loop {
                    let page = logs.page(id, cursor, None, applog::PAGE).map_err(|e| anyhow!(e))?;
                    cursor = page.next_seq;
                    if let Some((result, line)) = applog::find_match(&page.lines, &pattern, fail.as_ref()) {
                        break Some((result, line.clone()));
                    }
                    if page.lines.len() < applog::PAGE {
                        break None;
                    }
                };
                hit
            };
            if let Some((result, line)) = hit {
                return Ok(serde_json::json!({ "result": result, "line": line, "next_seq": line.seq + 1 }));
            }
            if let Some((why, _)) = &exited {
                if quiet >= 2 || drain_over {
                    bail!("app {id}: failed: app exited ({why}) before a line matched");
                }
                thread::sleep(Duration::from_millis(50));
                continue;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(serde_json::json!({ "result": "timeout", "line": null, "next_seq": cursor }));
            }
            thread::sleep(left.min(Duration::from_millis(200)));
        }
    }

    /// The Index flavour (`steamvr_input_2_0` off) or the skeletal-tracker flavour (on).
    pub fn set_hand_tracking_flavour(&self, skeletal_tracker: bool) -> Result<SettingsReport> {
        self.set_alvr_settings(vec![SettingValue { path: STEAMVR_INPUT_2_0.into(), value: skeletal_tracker.into() }])
    }

    /// ALVR log events the supervisor kept, merged with the tails of SteamVR's own logs, filtered.
    pub fn logs(&self, q: LogQuery) -> Result<Vec<LogLine>> {
        let mut lines: Vec<LogLine> = self.alvr_logs.lock().iter().cloned().collect();
        if let Some(steam) = &self.paths().steam_dir {
            for name in STEAMVR_LOGS {
                lines.extend(steamvr_log_lines(name, steamvr::log_tail(steam, name, LOGS_KEPT)));
            }
        }
        filter_logs(lines, &q).map_err(|e| anyhow!(e))
    }

    fn keep_log(&self, t: String, entry: LogEntry) {
        let t_s = clock::now_s();
        let mut ring = self.alvr_logs.lock();
        if ring.len() == LOGS_KEPT {
            ring.pop_front();
        }
        ring.push_back(LogLine { t_s, at: clock::at(t_s), t, source: "alvr".into(), level: level_name(entry.severity), text: entry.content });
    }

    fn keep_stats(&self, at: Instant, summary: alvr_events::StatisticsSummary) {
        let mut ring = self.alvr_stats.lock();
        if ring.len() == STATS_KEPT {
            ring.pop_front();
        }
        ring.push_back((at, summary));
    }

    /// The kept statistics summaries that arrived at or after `start`, oldest first.
    fn stats_since(&self, start: Instant) -> Vec<alvr_events::StatisticsSummary> {
        self.alvr_stats.lock().iter().filter(|(at, _)| *at >= start).map(|(_, s)| s.clone()).collect()
    }

    /// Replaces the headset with a fresh one and restarts SteamVR so ALVR accepts it.
    pub fn recreate_headset(&self) -> Result<()> {
        // shortcut: ALVR TCP EOF spin (sockets/src/stream_socket/tcp.rs:137-147) leaves the server stuck Disconnecting; drop when fixed upstream
        self.restart_steamvr(true)
    }

    /// The server's session, which ALVR sends as an event in reply to GET /api/session.
    fn session(&self) -> Result<SessionConfig> {
        let mut events = self.api().events()?;
        self.api().get("/api/session")?;
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Some(Event { event_type: EventType::Session(session), .. }) = events.next(Duration::from_millis(500))? {
                return Ok(*session);
            }
        }
        bail!("ALVR sent no session (deadline 5 s)")
    }

    /// Drops a headset taken out of the state; one whose drop does not return in time is leaked.
    fn dispose(&self, headset: Headset) {
        if within(HEADSET_TIMEOUT, move || drop(headset)).is_none() {
            self.mark_leaked();
        }
    }

    // shortcut: ALVR TCP EOF spin on the client after the server exits (sockets/src/stream_socket/tcp.rs:137-147) never returns from pause/drop; after a 5 s timeout the headset is abandoned on its helper thread, which spins one core until the MCP exits; drop when fixed upstream
    fn mark_leaked(&self) {
        warn!("{LEAKED}");
        self.state.lock().leaked = true;
    }

    /// Pauses a headset taken out of the state; one whose pause does not return in time is leaked.
    fn paused(&self, headset: Headset) -> Option<Headset> {
        let paused = within(HEADSET_TIMEOUT, move || {
            headset.pause();
            headset
        });
        if paused.is_none() {
            self.mark_leaked();
        }
        paused
    }

    /// Restarts SteamVR around the headset, or with a new headset if `fresh_headset`. A failure is
    /// kept for vr_status and for the streaming waits.
    fn restart_steamvr(&self, fresh_headset: bool) -> Result<()> {
        self.restart_within(fresh_headset, Duration::from_secs(self.cfg().timeouts.streaming_s))
    }

    /// restart_steamvr with up to `timeout` for ALVR to come back.
    fn restart_within(&self, fresh_headset: bool, timeout: Duration) -> Result<()> {
        let held = self.restarting.lock();
        self.restart_locked(&held, fresh_headset, timeout)
    }

    /// restart_within for a caller that holds `restarting` (`_held`).
    fn restart_locked(&self, _held: &alvr_common::parking_lot::MutexGuard<'_, ()>, fresh_headset: bool, timeout: Duration) -> Result<()> {
        if !self.state.lock().managing {
            bail!("SteamVR is being stopped; no restart");
        }
        // as in stop(): a client still streaming when vrserver exits never disconnects, so pause it
        // first, outside the state lock; a client whose server is already gone is replaced
        let mut taken = self.state.lock().headset.take(); // own statement: the guard must not outlive it
        if fresh_headset && let Some(old) = taken.take() {
            self.dispose(old); // the old client must stop listening on 9943 before the new one starts
        }
        let headset = taken.and_then(|h| self.paused(h));
        let _vr_down = self.vr_down.lock();
        self.drop_reader();
        let mut started_steam = false;
        let stopped = || !self.state.lock().managing;
        let restarted = steamvr::restart_steamvr(&self.cfg(), &self.paths(), &self.api(), &self.vr, timeout, stopped, &mut started_steam);
        if started_steam {
            self.record_steam_client();
        }
        // after a vr_stop no client comes back: the stop waiting for this restart would have to dispose it
        let managing = self.state.lock().managing;
        let headset = match headset_step(managing, headset.is_some()) {
            HeadsetStep::Resume => headset.inspect(Headset::resume),
            HeadsetStep::StartNew => Some(Headset::start(Arc::clone(&self.rig))),
            HeadsetStep::Skip => {
                if let Some(h) = headset {
                    self.dispose(h);
                }
                None
            }
        };
        let spare = headset.and_then(|headset| {
            let mut state = self.state.lock();
            match state.headset {
                None => state.headset.replace(headset),
                Some(_) => Some(headset), // start() made one meanwhile; ours goes, outside the lock
            }
        });
        if let Some(h) = spare {
            self.dispose(h);
        }
        let mut state = self.state.lock();
        state.started_steam |= started_steam;
        record_restart(&mut state, restarted)
    }
}

/// Counts a restart that worked and forgets an older failure; keeps one that failed for vr_status
/// and the streaming waits, unless vr_stop caused it.
fn record_restart(state: &mut State, restarted: Result<()>) -> Result<()> {
    match restarted.context("SteamVR restart failed") {
        Ok(()) => {
            state.restarts += 1;
            state.last_restart_error = None;
            Ok(())
        }
        Err(e) => {
            if state.managing {
                state.last_restart_error = Some((Instant::now(), format!("{e:#}")));
            }
            Err(e)
        }
    }
}

/// The owner name and ttl claims use until vr_acquire names others.
fn default_lease() -> (String, Duration) {
    (format!("mcp-{}", std::process::id()), lease::DEFAULT_TTL)
}

/// The API at the session's port, ALVR's default 8082 while no streamer is known.
// shortcut: port read at MCP start and reload, re-read after prepare if web_server_port overrides must apply to a first start
fn api_for(paths: &Paths) -> Api {
    if cfg!(test) {
        return Api { port: 1 }; // a unit test must never ask the user's ALVR to shut SteamVR down
    }
    paths.session_file().map_or(Api { port: 8082 }, |f| Api::from_session_file(&f))
}

/// Whether a kept core check failed, so vr_status re-runs all checks. Not for the dashboard check alone:
/// checks() re-runs it each time, and in an observer it fails for as long as another MCP streams.
fn core_failing(checks: &[Check]) -> bool {
    checks.iter().any(|c| !c.ok && c.name != "alvr_dashboard_absent" && checks::CORE.contains(&c.name))
}

/// Whether the watcher renews the lease: only our own (vr_release gave it up for good), only
/// while we manage a session whose SteamVR runs, and once half the ttl is gone.
fn watch_renews(managing: bool, steamvr_running: bool, held: Option<&Lease>, me: u32, ttl: Duration, now: SystemTime) -> bool {
    managing && steamvr_running && held.is_some_and(|l| l.pid == me && lease::renew_due(l, ttl, now))
}

/// Why a log wait ends early: the Proton launcher exited, or the scene pid seen at the wait's start is gone.
fn app_exited(launcher: Option<std::process::ExitStatus>, scene_pid: Option<u32>, alive: impl Fn(u32) -> bool) -> Option<String> {
    if let Some(status) = launcher {
        return Some(format!("the Proton launcher exited with {status}"));
    }
    scene_pid.filter(|&pid| !alive(pid)).map(|pid| format!("scene pid {pid} is gone"))
}

/// The watcher's clocks on a managed session's SteamVR.
#[derive(Debug, Default)]
struct VrGone {
    /// Since when vrserver is gone, and since when every SteamVR process is.
    vrserver: Option<Instant>,
    all: Option<Instant>,
    /// When vrserver last ran.
    seen: Option<Instant>,
}

impl VrGone {
    /// Takes this tick's live SteamVR processes; true once the session is lost: no SteamVR process for VR_EXIT_GRACE,
    /// or no vrserver for LAUNCH_SETTLE (SteamVR's own restart keeps vrmonitor up while vrserver is gone for seconds).
    /// Not `watching` (no session, or a vr_start, vr_stop or restart, which take SteamVR down on purpose) resets the clocks.
    fn lost(&mut self, watching: bool, alive: &[&str], now: Instant) -> bool {
        let vrserver = alive.contains(&"vrserver");
        if vrserver {
            self.seen = Some(now);
        }
        let since = |t: Option<Instant>, gone: bool| (watching && gone).then(|| t.unwrap_or(now));
        self.vrserver = since(self.vrserver, !vrserver);
        self.all = since(self.all, alive.is_empty());
        let held = |t: Option<Instant>, grace| t.is_some_and(|t| now.duration_since(t) >= grace);
        held(self.all, steamvr::VR_EXIT_GRACE) || held(self.vrserver, steamvr::LAUNCH_SETTLE)
    }
}

/// Recomputes readiness every 1 s and reports changes. Not in `supervise`, which blocks for a whole restart.
fn watch(stack: Weak<Stack>) {
    let mut last = Readiness::default();
    let mut gone = VrGone::default();
    loop {
        let Some(s) = stack.upgrade() else { return };
        s.pump_reader();
        let managing = s.state.lock().managing;
        let alive = if managing { s.vr.alive() } else { vec![] };
        let watching = managing && !s.starting.is_locked() && !s.restarting.is_locked();
        if gone.lost(watching, &alive, Instant::now()) {
            s.end_lost_session(gone.seen);
        }
        s.keep_lease();
        let steam = s.paths().steam_dir;
        s.app_logs.lock().poll(steam.as_deref());
        let now = s.readiness();
        if now != last {
            if let Some(f) = &*s.on_change.lock() {
                f();
            }
            last = now;
        }
        drop(s);
        thread::sleep(Duration::from_secs(1));
    }
}

/// When vr_start brings up the headset client: before it launches SteamVR, or only once ALVR answers.
#[derive(Debug, PartialEq)]
enum HeadsetUp {
    BeforeLaunch,
    AfterAlvrAnswers,
}

fn headset_up(steamvr_running: bool) -> HeadsetUp {
    // a client listening as vrserver starts handshakes during DriverProvider::Init, before Hmd::activate; InitializeStreaming (ALVR alvr_server.cpp) then skips RequestEncoderReset and the encoder stays Idle
    if steamvr_running {
        HeadsetUp::BeforeLaunch
    } else {
        HeadsetUp::AfterAlvrAnswers
    }
}

/// Whether `vrserver_log` (vrserver.txt since the launch) shows ALVR's HMD activated: the active HMD
/// set to alvr_server, then ALVR's driver done adding that device.
fn hmd_activated(vrserver_log: &str) -> bool {
    vrserver_log
        .split_once("Active HMD set to alvr_server")
        .is_some_and(|(_, after)| after.contains("Driver 'alvr_server' finished adding tracked device"))
}

/// What vr_start does with the headset client once SteamVR is up.
#[derive(Debug, PartialEq)]
enum HeadsetStep {
    Resume,
    StartNew,
    Skip,
}

fn headset_step(managing: bool, has_client: bool) -> HeadsetStep {
    match (managing, has_client) {
        (false, _) => HeadsetStep::Skip,
        (true, true) => HeadsetStep::Resume,
        (true, false) => HeadsetStep::StartNew,
    }
}

/// Whether the watchdog counts this moment toward "quiet": the events stream is up and the headset
/// is not streaming. Not while a restart runs: it holds the headset out of state, and the old
/// server keeps the events stream up until it exits.
fn counts_as_quiet(streaming: bool, events_attached: bool, restarting: bool) -> bool {
    events_attached && !streaming && !restarting
}

/// The dashboard's other job: restart SteamVR when ALVR asks, and reconnect a headset that went quiet.
fn supervise(stack: Weak<Stack>) {
    let mut events = None;
    // since when the API is up but the headset is not streaming
    let mut quiet_since: Option<Instant> = None;
    let mut streaming_since: Option<Instant> = None;
    let mut last_hud = String::new();
    loop {
        let Some(s) = stack.upgrade() else { return };
        if !s.state.lock().managing {
            (events, quiet_since, streaming_since) = (None, None, None);
            drop(s);
            thread::sleep(Duration::from_millis(500));
            continue;
        }

        let mut restart = false;
        match &mut events {
            None if s.api().ping() => events = s.api().events().map_err(|e| error!("ALVR events: {e:#}")).ok(),
            None => thread::sleep(Duration::from_millis(500)),
            Some(stream) => match stream.next(Duration::from_millis(500)) {
                Ok(Some(Event { event_type: EventType::ServerRequestsSelfRestart, .. })) => restart = true,
                Ok(Some(Event { timestamp, event_type: EventType::Log(entry) })) => s.keep_log(timestamp, entry),
                Ok(Some(Event { event_type: EventType::StatisticsSummary(summary), .. })) => s.keep_stats(Instant::now(), summary),
                Ok(_) => (),
                Err(e) => {
                    warn!("ALVR events stream closed: {e:#}");
                    events = None;
                }
            },
        }
        let status = s.headset_status().unwrap_or_default();
        streaming_since = status.streaming.then(|| streaming_since.unwrap_or_else(Instant::now));
        // a request sent before the events stream attached still shows on the headset's HUD
        restart |= is_restart_hud(&status.hud_message) && !is_restart_hud(&last_hud);
        last_hud = status.hud_message;
        if counts_as_quiet(status.streaming, events.is_some(), s.restarting.is_locked()) {
            quiet_since.get_or_insert_with(Instant::now);
        } else {
            quiet_since = None;
        }

        let elapsed = |t: Option<Instant>| t.map(|t| t.elapsed());
        if restart {
            s.restart_steamvr(false).map_err(|e| error!("{e:#}")).ok();
            (events, quiet_since) = (None, None);
            last_hud = s.headset_status().map(|h| h.hud_message).unwrap_or_default();
        } else if s.state.lock().watchdog.fires(elapsed(streaming_since), elapsed(quiet_since)) {
            // seen in the spike: an old client stops listening after a restart
            // shortcut: ALVR TCP EOF spin (sockets/src/stream_socket/tcp.rs:137-147) leaves the server stuck Disconnecting; drop when fixed upstream
            s.recreate_headset().map_err(|e| error!("{e:#}")).ok(); // restarts SteamVR too
            (events, quiet_since) = (None, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dashboard_click_decision() {
        let t = |b, a| dashboard_outcome(b, a);
        assert_eq!(t(Some(true), Some(false)), (true, "closed the SteamVR dashboard"));
        assert_eq!(t(Some(true), Some(true)), (true, "SteamVR dashboard still open after one system click"));
        assert_eq!(t(Some(true), None), (true, "SteamVR dashboard state unknown after one system click"));
        assert_eq!(t(Some(false), Some(false)), (false, "SteamVR dashboard not open, nothing clicked"));
        assert_eq!(t(Some(false), Some(true)), (false, "SteamVR dashboard not open, nothing clicked"));
        assert_eq!(t(None, None), (false, "SteamVR dashboard state unknown, nothing clicked"));
    }
    use serde_json::json;

    #[test]
    fn check_launch_rules() {
        let a = |via, exe: Option<&str>, args: &[&str], env: &[(&str, &str)]| LaunchArgs {
            app_id: "1".into(),
            via,
            exe: exe.map(Into::into),
            args: args.iter().map(|&a| a.into()).collect(),
            env: env.iter().map(|&(k, v)| (k.into(), v.into())).collect(),
        };
        assert_eq!(check_launch(&a(Via::Steam, None, &[], &[])), Ok(()));
        for steam in [a(Via::Steam, Some("x.exe"), &[], &[]), a(Via::Steam, None, &["-a"], &[]), a(Via::Steam, None, &[], &[("A", "1")])] {
            assert!(check_launch(&steam).unwrap_err().contains("need via \"proton\""), "{steam:?}");
        }
        assert!(check_launch(&a(Via::Proton, None, &[], &[])).unwrap_err().contains("exe"));
        for env in [("A=B", "1"), ("", "1"), ("A\0", "1"), ("A", "1\0")] {
            assert!(check_launch(&a(Via::Proton, Some("Game.exe"), &[], &[env])).is_err(), "{env:?}");
        }
        for id in ["250820", " 250820"] {
            let vr = LaunchArgs { app_id: id.into(), ..a(Via::Proton, Some("vrstartup.exe"), &[], &[]) };
            assert!(check_launch(&vr).unwrap_err().contains("SteamVR"), "{id:?}");
        }
        let refused = [
            "STEAM_COMPAT_DATA_PATH", "STEAM_COMPAT_MOUNTS", "SteamAppId", "SteamGameId", "PROTON_REMOTE_DEBUG_CMD", "LD_PRELOAD",
            "LD_LIBRARY_PATH", "PATH", "PYTHONPATH", "PRESSURE_VESSEL_FILESYSTEMS_RW", "VK_ICD_FILENAMES", "VK_DRIVER_FILES",
            "VK_LAYER_PATH", "VK_ADD_LAYER_PATH", "SDL_DYNAMIC_API", "SDL_VIDEO_GL_DRIVER", "WINEDLLPATH", "HOME",
            // Wine looks env names up ignoring case on the Windows side, so these would dodge the substring rules
            "SDL_Dynamic_Api", "DXVK_Log_Path", "Mesa_Loader_Driver_Override", "mangohud",
        ];
        for key in refused {
            let e = check_launch(&a(Via::Proton, Some("Game.exe"), &[], &[(key, "1")])).unwrap_err();
            assert!(e.contains(key) && e.contains("MESA_*") && e.contains("[A-Z0-9_]"), "{key}: {e}");
        }
        for glx in ["/tmp/x", "Mesa", ""] {
            let e = check_launch(&a(Via::Proton, Some("Game.exe"), &[], &[("__GLX_VENDOR_LIBRARY_NAME", glx)])).unwrap_err();
            assert!(e.contains("__GLX_VENDOR_LIBRARY_NAME"), "{glx:?}: {e}");
        }
        for arg in ["--renderer-cmd-prefix=Z:\\usr\\bin\\sh -c x", "--gpu-launcher", "/Utility-Cmd-Prefix=x", "-browser-subprocess-path=Z:\\x"] {
            let e = check_launch(&a(Via::Proton, Some("Game.exe"), &["-vr", arg], &[])).unwrap_err();
            assert!(e.contains(&format!("{arg:?}")) && e.contains("Chromium"), "{arg}: {e}");
        }
        let e = check_launch(&a(Via::Proton, Some("Game.exe"), &[], &[("MESA_LOADER_DRIVER_OVERRIDE", "../../tmp/x")])).unwrap_err();
        assert!(e.contains(".."), "{e}");
        let allowed = [
            ("DRI_PRIME", "1"), ("MESA_VK_DEVICE_SELECT", "1002:73bf"), ("RADV_PERFTEST", "gpl"), ("VK_INSTANCE_LAYERS", "x"),
            ("DXVK_HUD", "fps"), ("VKD3D_CONFIG", "dxr"), ("WINEDEBUG", "-all"), ("WINEDLLOVERRIDES", "dxgi=n,b"),
            ("PROTON_LOG", "1"), ("SDL_VIDEODRIVER", "x11"), ("__GL_SYNC_TO_VBLANK", "0"), ("__NV_PRIME_RENDER_OFFLOAD", "1"),
            ("__VK_LAYER_NV_optimus", "NVIDIA_only"), ("__GLX_VENDOR_LIBRARY_NAME", "mesa"), ("MANGOHUD", "1"), ("WINE_FULLSCREEN_FSR", "1"),
            ("WINEFSYNC", "1"), ("WINEESYNC", "0"), ("PULSE_LATENCY_MSEC", "60"), ("AMD_VULKAN_ICD", "RADV"),
        ];
        assert_eq!(check_launch(&a(Via::Proton, Some("Game.exe"), &["-a"], &allowed)), Ok(()));
        let parsed: LaunchArgs = serde_json::from_value(json!({"app_id": "1"})).unwrap();
        assert_eq!((parsed.via, parsed.exe, parsed.args.len(), parsed.env.len()), (Via::Steam, None, 0, 0));
        let parsed: LaunchArgs = serde_json::from_value(json!({"app_id": "1", "via": "proton", "exe": "a.exe"})).unwrap();
        assert_eq!(parsed.via, Via::Proton);
    }

    #[test]
    fn start_report_serializes_unknown_dashboard_as_null() {
        let v = serde_json::to_value(StartReport::default()).unwrap();
        assert_eq!(v["dashboard_visible"], serde_json::Value::Null);
        assert!(v.as_object().unwrap().contains_key("dashboard_visible"));
    }

    #[test]
    fn frame_timing_rejects_bad_window() {
        for w in [0.1, 61.0] {
            assert!(test_stack().frame_timing(Some(w)).unwrap_err().to_string().contains("window_s"), "{w}");
        }
    }

    #[test]
    fn audio_args_bounds() {
        let stack = test_stack();
        for s in [0.0, 61.0, f64::NAN] {
            assert!(stack.record_audio(Some(s)).unwrap_err().to_string().contains("seconds"), "{s}");
        }
        for w in [0.01, 11.0, f64::NAN] {
            assert!(stack.audio_level(Some(w)).unwrap_err().to_string().contains("window_s"), "{w}");
        }
    }

    fn test_stack() -> Arc<Stack> {
        Stack::for_tests("/nonexistent".into())
    }

    /// A temp Steam tree with a blocked driver and, when `restore`, a restore.json that unblocks it.
    fn blocked_stack(name: &str, restore: bool) -> (Arc<Stack>, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!("SteamVrMcp-stack-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&root).ok();
        let (steam_dir, state_dir) = (root.join("steam"), root.join("state"));
        std::fs::create_dir_all(steam_dir.join("config")).unwrap();
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(steam_dir.join("config/steamvr.vrsettings"), r#"{"driver_lighthouse":{"blocked_by_safe_mode":true}}"#).unwrap();
        if restore {
            let state = setup::RestoreState {
                vrsettings_file: Some(setup::FileState::Contents(r#"{"driver_lighthouse":{}}"#.into())),
                ..Default::default()
            };
            std::fs::write(state_dir.join("restore.json"), serde_json::to_vec(&state).unwrap()).unwrap();
        }
        let paths = Paths {
            steam_dir: Some(steam_dir),
            flatpak_steam: false,
            steamvr_dir: None,
            streamer_dir: None,
            runtime_dir: root.join("run"),
            state_dir,
        };
        (Stack::new(Config::default(), paths), root)
    }

    #[test]
    fn vr_stop_clears_safe_mode_blocks_when_steamvr_is_gone() {
        let (stack, root) = blocked_stack("stop", true);
        assert_eq!(stack.vr.names, steamvr::VrProcs::NONE.names, "unit tests never see the user's SteamVR");
        assert_eq!(stack.api().port, 1, "nor ask the user's ALVR to shut it down");
        let file = stack.paths().vrsettings_file().unwrap();
        stack.claim().unwrap();
        stack.state.lock().managing = true; // passes stop_guard whatever SteamVR runs on this machine
        stack.state.lock().started_steam = true;
        let report = stack.stop().unwrap();
        assert!(!report.used_signals && report.remaining_processes.is_empty(), "{report:?}");
        // under test launch() refuses and its error lands in steam_note, so no note means no -shutdown was forwarded
        assert!(report.closed_steam && report.steam_note.is_none(), "NONE sees no Steam, so it never forwards to the user's: {report:?}");
        assert!(setup::blocked_drivers(&file).is_empty());
        assert!(stack.status().owner.is_none(), "a finished stop gives up the lease");
        std::fs::remove_dir_all(&root).ok();
    }

    /// A name only this test's fake processes carry.
    fn fake_name(tag: &str) -> &'static str {
        Box::leak(format!("vrfake{}{tag}", std::process::id()).into_boxed_str())
    }

    /// A stack whose stop() ends only processes named `name`, holding its lease in a temp dir.
    fn fake_vr_stack(name: &'static str, grace_ms: u64) -> (Arc<Stack>, PathBuf) {
        let root = std::env::temp_dir().join(format!("SteamVrMcp-stack-{name}"));
        std::fs::remove_dir_all(&root).ok();
        let paths = Paths { steam_dir: None, flatpak_steam: false, steamvr_dir: None, streamer_dir: None, state_dir: root.join("state"), runtime_dir: root.join("run") };
        let names = Box::leak(Box::new([name]));
        let procs = steamvr::VrProcs { names, grace: Duration::from_millis(grace_ms), kill_wait: Duration::from_millis(300), steam: &[], steam_wait: Duration::ZERO, steam_term_wait: Duration::ZERO };
        let stack = Stack::with_procs(Config::default(), paths, procs);
        stack.claim().unwrap();
        stack.state.lock().managing = true;
        (stack, root)
    }

    /// Runs `script` in sh with argv0 `name`, reaped by a thread so a killed one leaves no zombie; waits until /proc shows it.
    fn fake_vr(name: &str, script: &str) -> u32 {
        use std::os::unix::process::CommandExt;
        let mut child = std::process::Command::new("sh").arg0(name).args(["-c", script]).spawn().unwrap();
        let pid = child.id();
        thread::spawn(move || child.wait());
        assert!(steamvr::wait_for(Duration::from_secs(5), || steamvr::pids_named(name) == [pid]));
        pid
    }

    #[test]
    fn vr_stop_kills_a_process_that_ignores_sigterm() {
        let name = fake_name("t");
        let (stack, root) = fake_vr_stack(name, 200);
        let pid = fake_vr(name, "trap '' TERM; while :; do sleep 1; done");
        let report = stack.stop().unwrap();
        assert!(report.used_signals && report.remaining_processes.is_empty(), "{report:?}");
        assert!(steamvr::wait_for(Duration::from_secs(2), || steamvr::stat(pid).is_none()), "SIGKILL ended it");
        assert!(stack.status().owner.is_none(), "clean, so the lease is given up");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn vr_stop_kills_a_snapshot_pid_that_changed_its_name() {
        let name = fake_name("r");
        let (stack, root) = fake_vr_stack(name, 5000);
        // gone by name after 1 s, as a vrserver that renames itself
        let pid = fake_vr(name, "sleep 1; exec sleep 30");
        let began = Instant::now();
        let report = stack.stop().unwrap();
        assert!(began.elapsed() < Duration::from_secs(4), "the name wait ended when the name went");
        assert!(report.used_signals && report.remaining_processes.is_empty(), "{report:?}");
        assert!(steamvr::wait_for(Duration::from_secs(2), || steamvr::stat(pid).is_none()));
        assert!(stack.status().owner.is_none());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn vr_stop_keeps_the_lease_while_a_process_remains() {
        let name = fake_name("z");
        let (stack, root) = fake_vr_stack(name, 200);
        // an unreaped zombie keeps its comm in /proc and outlives every signal
        let link = root.join(name);
        std::os::unix::fs::symlink("/usr/bin/true", &link).unwrap();
        let mut zombie = std::process::Command::new(&link).spawn().unwrap();
        assert!(steamvr::wait_for(Duration::from_secs(5), || steamvr::stat(zombie.id()).is_some_and(|(s, _)| s == 'Z')));
        let report = stack.stop().unwrap();
        assert!(report.used_signals && report.remaining_processes == [name], "{report:?}");
        assert!(stack.status().owner.is_some_and(|o| o.mine), "processes remain, so the lease stays");
        zombie.wait().unwrap();
        stack.release_lease().unwrap();
        std::fs::remove_dir_all(&root).ok();
    }

    /// A stack whose stop() ends no VR process and waits 300 ms on the fake Steam `names`, with its lease in a temp dir.
    fn fake_steam_stack(tag: &str, names: &[&'static str]) -> (Arc<Stack>, PathBuf) {
        let root = std::env::temp_dir().join(format!("SteamVrMcp-stack-steam-{tag}-{}", std::process::id()));
        std::fs::remove_dir_all(&root).ok();
        let paths = Paths { steam_dir: None, flatpak_steam: false, steamvr_dir: None, streamer_dir: None, state_dir: root.join("state"), runtime_dir: root.join("run") };
        let steam: &'static [&'static str] = Box::leak(names.to_vec().into_boxed_slice());
        let procs = steamvr::VrProcs { names: &[], grace: Duration::ZERO, kill_wait: Duration::from_millis(300), steam, steam_wait: Duration::from_millis(300), steam_term_wait: Duration::from_millis(300) };
        (Stack::with_procs(Config::default(), paths, procs), root)
    }

    /// What stop() notes when it forwards -shutdown under test, where launch() refuses.
    const FORWARD_NOTE: &str = "forwarding -shutdown to Steam failed: unit tests never run steam";

    #[test]
    fn vr_stop_without_a_record_asks_steam_once_and_keeps_nothing() {
        let name = fake_name("webhelper");
        let (stack, root) = fake_steam_stack("norecord", &[name]);
        stack.state.lock().started_steam = true;
        let pid = fake_vr(name, "while :; do sleep 0.1; done");
        let start = steamvr::stat(pid).unwrap().1;
        // under test launch() refuses, so -shutdown changes nothing and the fake stays
        let report = stack.stop().unwrap();
        assert!(!report.closed_steam && report.remaining_steam == [name] && !report.signalled_steam, "{report:?}");
        assert!(report.steam_note.as_deref().is_some_and(|n| n.contains(FORWARD_NOTE) && n.contains("no later vr_stop")), "{report:?}");
        {
            let state = stack.state.lock();
            assert!(!state.started_steam && state.steam_client.is_none());
            assert!(!stop_is_ours(false, state.started_steam, false, || false), "a Steam left over never makes SteamVR ours");
        }
        let report = stack.stop().unwrap();
        assert!(!report.closed_steam && report.remaining_steam.is_empty() && report.steam_note.is_none(), "skipped: {report:?}");
        assert_eq!(steamvr::stat(pid).map(|(_, t)| t), Some(start), "never signalled");
        steamvr::terminate(pid, libc::SIGKILL);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn vr_stop_signals_the_steam_client_vr_start_started() {
        let (client_name, helper_name) = (fake_name("client"), fake_name("helper"));
        let (stack, root) = fake_steam_stack("signal", &[client_name, helper_name]);
        let client = fake_vr(client_name, "while :; do sleep 0.1; done");
        fake_vr(helper_name, &format!("while kill -0 {client} 2>/dev/null; do sleep 0.1; done"));
        {
            let mut state = stack.state.lock();
            (state.started_steam, state.steam_client) = (true, Some((client, steamvr::stat(client).unwrap().1)));
        }
        let report = stack.stop().unwrap();
        assert!(report.signalled_steam && report.closed_steam && report.remaining_steam.is_empty() && report.steam_note.as_deref() == Some(FORWARD_NOTE), "{report:?}");
        let state = stack.state.lock();
        assert!(!state.started_steam && state.steam_client.is_none());
        drop(state);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn vr_stop_leaves_a_steam_that_is_not_the_recorded_client_alone() {
        let name = fake_name("other");
        let (stack, root) = fake_steam_stack("other", &[name]);
        let pid = fake_vr(name, "while :; do sleep 0.1; done");
        let start = steamvr::stat(pid).unwrap().1;
        {
            let mut state = stack.state.lock();
            (state.started_steam, state.steam_client) = (true, Some((pid, start + 1)));
        }
        let report = stack.stop().unwrap();
        assert!(!report.signalled_steam && !report.closed_steam && report.remaining_steam == [name], "{report:?}");
        assert!(report.steam_note.as_deref().is_some_and(|n| n.contains("left alone") && !n.contains("forwarding")), "{report:?}");
        {
            let state = stack.state.lock();
            assert!(!state.started_steam && state.steam_client.is_none(), "the Steam vr_start started is gone; this one is the user's");
        }
        assert_eq!(steamvr::stat(pid).map(|(_, t)| t), Some(start), "never signalled");
        steamvr::terminate(pid, libc::SIGKILL);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_webhelper_left_after_the_client_keeps_nothing() {
        let (client_name, helper_name) = (fake_name("client2"), fake_name("helper2"));
        let (stack, root) = fake_steam_stack("helper-left", &[client_name, helper_name]);
        let client = fake_vr(client_name, "while :; do sleep 0.1; done");
        let helper = fake_vr(helper_name, "while :; do sleep 0.1; done");
        {
            let mut state = stack.state.lock();
            (state.started_steam, state.steam_client) = (true, Some((client, steamvr::stat(client).unwrap().1)));
        }
        let report = stack.stop().unwrap();
        assert!(report.signalled_steam && !report.closed_steam && report.remaining_steam == [helper_name], "{report:?}");
        assert!(report.steam_note.as_deref().is_some_and(|n| n.contains(FORWARD_NOTE) && n.contains("no later vr_stop")), "{report:?}");
        {
            let state = stack.state.lock();
            assert!(!state.started_steam && state.steam_client.is_none());
        }
        let report = stack.stop().unwrap();
        assert!(!report.signalled_steam && !report.closed_steam && report.remaining_steam.is_empty(), "skipped: {report:?}");
        steamvr::terminate(helper, libc::SIGKILL);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn only_a_live_recorded_client_outlives_vr_stop() {
        let name = fake_name("stubborn");
        let (stack, root) = fake_steam_stack("stubborn", &[name]);
        let marker = root.join("trapped");
        std::fs::create_dir_all(&root).unwrap();
        let client = fake_vr(name, &format!("trap '' TERM; touch {}; while :; do sleep 0.1; done", marker.display()));
        assert!(steamvr::wait_for(Duration::from_secs(5), || marker.exists()), "TERM is ignored before stop() runs");
        let record = (client, steamvr::stat(client).unwrap().1);
        {
            let mut state = stack.state.lock();
            (state.started_steam, state.steam_client) = (true, Some(record));
        }
        let report = stack.stop().unwrap();
        assert!(report.signalled_steam && !report.closed_steam && report.remaining_steam == [name] && report.steam_note.as_deref() == Some(FORWARD_NOTE), "{report:?}");
        {
            let state = stack.state.lock();
            assert_eq!((state.started_steam, state.steam_client), (false, Some(record)), "the record is all that is kept");
        }
        let report = stack.stop().unwrap();
        assert!(report.signalled_steam && report.remaining_steam == [name] && report.steam_note.as_deref() == Some(FORWARD_NOTE), "the next vr_stop tries again: {report:?}");
        steamvr::terminate(client, libc::SIGKILL);
        assert!(steamvr::wait_for(Duration::from_secs(2), || steamvr::stat(client).is_none()));
        // the record a stop kept is dead, and the Steam running now is another one
        let other = fake_vr(name, "while :; do sleep 0.1; done");
        let report = stack.stop().unwrap();
        assert!(!report.signalled_steam && !report.closed_steam && report.remaining_steam == [name], "{report:?}");
        assert!(report.steam_note.as_deref().is_some_and(|n| n.contains("left alone") && !n.contains("forwarding")), "{report:?}");
        assert_eq!(stack.state.lock().steam_client, None);
        steamvr::terminate(other, libc::SIGKILL);
        assert!(steamvr::wait_for(Duration::from_secs(2), || steamvr::stat(other).is_none()));
        stack.state.lock().steam_client = Some(record);
        let report = stack.stop().unwrap();
        assert!(!report.signalled_steam && report.closed_steam && report.remaining_steam.is_empty() && report.steam_note.is_none(), "already quit: {report:?}");
        assert_eq!(stack.state.lock().steam_client, None);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_start_that_launched_steam_and_finds_no_client_drops_the_old_record() {
        let (stack, root) = fake_steam_stack("rerecord", &[]);
        // under test steam_client() finds none, as when Steam's client is not up yet or two run
        stack.state.lock().steam_client = Some((1, 1));
        stack.record_steam_client();
        assert_eq!(stack.state.lock().steam_client, None, "launch() started Steam, so no older client is alive");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn kill_waits_for_the_earlier_names_before_the_last() {
        let (a, b) = (fake_name("ka"), fake_name("kb"));
        let marker = std::env::temp_dir().join(format!("SteamVrMcp-stack-kill-marker-{}", std::process::id()));
        std::fs::remove_file(&marker).ok();
        let first = fake_vr(a, &format!("trap '' TERM; touch {}; while :; do sleep 0.1; done", marker.display()));
        assert!(steamvr::wait_for(Duration::from_secs(5), || marker.exists()), "TERM is ignored before kill() runs");
        let last = fake_vr(b, "while :; do sleep 0.1; done");
        let names: &'static [&'static str] = Box::leak(Box::new([a, b]));
        let procs = steamvr::VrProcs { names, kill_wait: Duration::from_secs(1), ..steamvr::VrProcs::NONE };
        let began = Instant::now();
        let killing = thread::spawn(move || procs.kill());
        thread::sleep(Duration::from_millis(500));
        assert!(steamvr::stat(last).is_some(), "the last name waits for the earlier ones");
        assert!(killing.join().unwrap());
        assert!(steamvr::stat(first).is_none() && steamvr::stat(last).is_none());
        assert!(began.elapsed() < Duration::from_secs(5), "{:?}", began.elapsed());
        std::fs::remove_file(&marker).ok();
    }

    #[test]
    fn kill_does_not_wait_on_an_earlier_zombie() {
        let (a, b) = (fake_name("z"), fake_name("kz"));
        // a zombie by comm: its cmdline is gone, and nothing reaps it until the end
        let mut zombie = std::process::Command::new("sh").args(["-c", &format!("printf {a} > /proc/self/comm; while :; do sleep 0.1; done")]).spawn().unwrap();
        let zpid = zombie.id();
        assert!(steamvr::wait_for(Duration::from_secs(5), || steamvr::comm(zpid).as_deref() == Some(a)));
        steamvr::terminate(zpid, libc::SIGKILL);
        assert!(steamvr::wait_for(Duration::from_secs(5), || steamvr::stat(zpid).is_some_and(|(s, _)| s == 'Z')));
        let last = fake_vr(b, "while :; do sleep 0.1; done");
        let names: &'static [&'static str] = Box::leak(Box::new([a, b]));
        let procs = steamvr::VrProcs { names, kill_wait: Duration::from_secs(1), ..steamvr::VrProcs::NONE };
        let killing = thread::spawn(move || procs.kill());
        assert!(steamvr::wait_for(Duration::from_millis(600), || steamvr::stat(last).is_none()), "the last name goes without waiting on the zombie");
        killing.join().unwrap();
        zombie.wait().ok();
    }

    #[test]
    fn undo_safe_mode_blocks_ignores_the_snapshot_and_names_drivers() {
        let (stack, root) = blocked_stack("undo", true);
        let file = stack.paths().vrsettings_file().unwrap();
        let state = setup::RestoreState {
            vrsettings_file: Some(setup::FileState::Contents(r#"{"driver_lighthouse":{"blocked_by_safe_mode":true}}"#.into())),
            ..Default::default()
        };
        std::fs::write(stack.paths().restore_file(), serde_json::to_vec(&state).unwrap()).unwrap();
        assert_eq!(setup::undo_safe_mode_blocks(&stack.paths()).unwrap(), ["driver_lighthouse"]);
        assert!(setup::blocked_drivers(&file).is_empty());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn undo_safe_mode_blocks_needs_a_snapshot() {
        let (stack, root) = blocked_stack("nosnap", false);
        let file = stack.paths().vrsettings_file().unwrap();
        assert!(setup::undo_safe_mode_blocks(&stack.paths()).unwrap().is_empty());
        assert_eq!(setup::blocked_drivers(&file), ["driver_lighthouse"]);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn stop_releases_the_lease_only_when_clean() {
        // restore() itself rewrites the real openvrpaths.vrpath, so only the release step is tested
        let (stack, root) = blocked_stack("release", false);
        stack.claim().unwrap();
        let dirty = StopReport { remaining_processes: vec!["vrserver".into()], ..Default::default() };
        stack.release_if_clean(&dirty);
        assert!(stack.status().owner.is_some(), "processes remain, so the lease stays");
        stack.release_if_clean(&StopReport::default());
        assert!(stack.status().owner.is_none());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_restore_that_kept_restore_json_reports_pending_and_keeps_the_lease() {
        // as above: restore() itself rewrites the real openvrpaths.vrpath, so only its last step is tested
        let (stack, root) = blocked_stack("restore-kept", true);
        stack.claim().unwrap();
        let kept = stack.finish_restore(vec!["openvrpaths.vrpath unreadable".into()]);
        assert!(!kept.restored && kept.notes.len() == 1, "{kept:?}");
        assert!(stack.status().owner.is_some(), "restore.json stays for a retry, and so does the lease");
        std::fs::remove_file(stack.paths().restore_file()).unwrap();
        assert!(stack.finish_restore(Vec::new()).restored);
        assert!(stack.status().owner.is_none());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn vr_status_reports_blocked_drivers() {
        let (stack, root) = blocked_stack("status", false);
        assert_eq!(stack.status().blocked_drivers, ["driver_lighthouse"]);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn claim_takes_the_lease_and_status_shows_it() {
        let dir = std::env::temp_dir().join(format!("SteamVrMcp-stack-lease-{}", std::process::id()));
        let stack = test_stack();
        stack.paths.lock().runtime_dir = dir.clone();
        assert!(stack.status().owner.is_none());
        assert!(stack.acquire_lease(String::new(), None).is_err());
        assert!(stack.acquire_lease("x".into(), Some(599)).is_err());
        stack.claim().unwrap();
        let owner = stack.status().owner.unwrap();
        assert!(owner.mine && owner.lease.owner.starts_with("mcp-"));
        assert_eq!(stack.readiness().leased_to, None, "ours does not block us");
        assert_eq!(stack.acquire_lease("driver".into(), Some(600)).unwrap().owner, "driver");
        assert!(stack.release_lease().unwrap());
        assert!(stack.status().owner.is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_release_forgets_the_holder_name_and_ttl() {
        let dir = std::env::temp_dir().join(format!("SteamVrMcp-stack-lease-forget-{}", std::process::id()));
        let stack = test_stack();
        stack.paths.lock().runtime_dir = dir.clone();
        stack.acquire_lease("driver".into(), Some(7200)).unwrap();
        assert!(stack.release_lease().unwrap());
        stack.claim().unwrap();
        assert!(stack.status().owner.unwrap().lease.owner.starts_with("mcp-"), "the next claim is the MCP's own again");
        assert_eq!(*stack.lease.lock(), default_lease());
        stack.release_lease().unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn vr_release_refuses_while_this_mcp_runs_a_session() {
        let dir = std::env::temp_dir().join(format!("SteamVrMcp-stack-vr-release-{}", std::process::id()));
        let stack = test_stack();
        stack.paths.lock().runtime_dir = dir.clone();
        stack.claim().unwrap();
        stack.state.lock().managing = true;
        let e = stack.release().unwrap_err().to_string();
        assert!(e.contains("vr_stop"), "{e}");
        assert!(stack.status().owner.is_some_and(|o| o.mine), "the lease stays with the session");
        stack.state.lock().managing = false;
        // a cold vr_start holds the headset outside state and sets managing only late
        for lock in [&stack.starting, &stack.restarting] {
            let held = lock.lock();
            let e = stack.release().unwrap_err().to_string();
            assert!(e.contains("vr_stop"), "{e}");
            drop(held);
        }
        assert!(stack.status().owner.is_some_and(|o| o.mine));
        assert!(stack.release().unwrap());
        std::fs::remove_dir_all(&dir).ok();
    }

    fn seq_step(at_s: f32, action: &str, args: Value) -> SequenceStep {
        SequenceStep { at_s, action: action.into(), args }
    }

    #[tokio::test]
    async fn sequence_waits_for_animations_and_refuses_a_bad_step_before_any_runs() {
        let stack = test_stack();
        let head_y = |s: &Stack| s.rig.lock().tracking(Instant::now(), Duration::ZERO).device_motions[0].1.pose.position.y;
        let animate = seq_step(0.0, "animate_head", json!({ "waypoints": [{ "position": [0, 1.0, 0] }], "duration_s": 0.3 }));
        let bad = seq_step(0.1, "set_trigger", json!({ "hand": "left", "value": 7 }));
        let err = stack.run_sequence(vec![animate.clone(), bad]).await.unwrap_err();
        assert!(err.to_string().starts_with("step 1:"), "{err:#}");
        assert_eq!(head_y(&stack), 1.6, "refused before any step ran: the animation never started");
        assert!(stack.run_sequence(vec![seq_step(0.0, "screenshot", json!({}))]).await.is_err());

        let r = stack.run_sequence(vec![seq_step(0.1, "set_trigger", json!({ "hand": "left", "value": 1 })), animate]).await.unwrap();
        assert!(r.steps.iter().all(|s| s.ok) && r.steps[1].applied_s >= 0.1 && r.total_s >= 0.3 && r.total_s < 5.0, "{r:?}");
        assert_eq!(head_y(&stack), 1.0);
    }

    #[tokio::test]
    async fn sequence_reports_a_step_that_fails_mid_run_and_goes_on() {
        let stack = test_stack();
        let off = RigAction::SetControllerState(serde_json::from_value(json!({ "hand": "left", "connected": false })).unwrap());
        let steps = vec![
            seq_step(0.2, "set_trigger", json!({ "hand": "left", "value": 1 })),
            seq_step(0.3, "set_head_pose", json!({ "position": [0, 1, 0] })),
        ];
        // the controller drops out after the plan was checked
        let (r, ()) = tokio::join!(stack.run_sequence(steps), async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            stack.apply(off).unwrap();
        });
        let r = r.unwrap();
        assert_eq!(r.steps.iter().map(|s| s.ok).collect::<Vec<_>>(), [false, true]);
        assert!(r.steps[0].error.as_deref().unwrap().contains("not tracked"), "{r:?}");
    }

    #[test]
    fn streaming_waits_end_at_a_failed_restart_or_a_stop() {
        let stack = test_stack();
        let since = Instant::now();
        stack.state.lock().managing = true;
        stack.state.lock().last_restart_error = Some((since - Duration::from_secs(1), "older".into()));
        assert!(!stack.wait_streaming(since, Duration::from_millis(300), 0).unwrap(), "a failure before the wait is not its own");
        stack.state.lock().last_restart_error = Some((Instant::now(), "SteamVR restart failed: boom".into()));
        let waited = Instant::now();
        let e = stack.wait_streaming(since, Duration::from_secs(30), 0).unwrap_err();
        assert!(e.to_string().contains("boom") && waited.elapsed() < Duration::from_secs(2), "{e:#}");
        assert!(stack.status().last_restart_error.is_some_and(|e| e.contains("boom")), "vr_status shows it");
        stack.state.lock().managing = false;
        let e = stack.wait_streaming(Instant::now(), Duration::from_secs(30), 0).unwrap_err();
        assert_eq!(e.to_string(), "failed: vr_stop ended the session");
    }

    #[test]
    fn a_steamvr_restart_resets_the_exit_watch() {
        let stack = test_stack();
        let mut seen = vec![("vrserver", None), ("vrmonitor", None)];
        let _restart = stack.restarting.lock();
        assert_eq!(stack.vr_exited(&mut seen), None, "the restart stops them on purpose");
        assert!(seen.is_empty());
    }

    #[test]
    fn config_reloads_only_while_nothing_runs() {
        let stack = test_stack();
        let cfg = |streaming_s| Config { timeouts: crate::config::Timeouts { streaming_s, app_s: 1 }, ..Default::default() };
        stack.state.lock().managing = true;
        assert!(!stack.reload(cfg(7)), "a vr_start session runs");
        assert_eq!(stack.cfg().timeouts.streaming_s, 240);
        stack.state.lock().managing = false;
        if !steamvr::is_steamvr_running() {
            assert!(stack.reload(cfg(7)));
            assert_eq!(stack.cfg().timeouts.streaming_s, 7);
        }
    }

    #[test]
    fn a_successful_restart_clears_the_last_failure() {
        let mut state = State { managing: true, ..Default::default() };
        assert!(record_restart(&mut state, Err(anyhow!("boom"))).is_err());
        assert!(state.last_restart_error.as_ref().is_some_and(|(_, e)| e.contains("boom")), "kept for vr_status");
        record_restart(&mut state, Ok(())).unwrap();
        assert_eq!((state.last_restart_error.is_none(), state.restarts), (true, 1));
    }

    #[test]
    fn vr_stop_clears_the_last_restart_error() {
        let (stack, root) = blocked_stack("stop-restart-error", false);
        {
            let mut state = stack.state.lock();
            (state.managing, state.last_restart_error) = (true, Some((Instant::now(), "boom".into())));
        }
        stack.stop().unwrap();
        assert_eq!(stack.status().last_restart_error, None);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_successful_start_clears_the_last_restart_error() {
        let stack = test_stack();
        stack.state.lock().last_restart_error = Some((Instant::now(), "boom".into()));
        stack.finish_start(StartReport::default());
        assert_eq!(stack.status().last_restart_error, None);
    }

    #[test]
    fn vr_start_resets_the_rig_only_when_it_starts_a_session() {
        let head_y = |stack: &Stack| {
            let data = stack.rig.lock().tracking(Instant::now(), Duration::ZERO);
            data.device_motions.iter().find(|(d, _)| *d == *alvr_common::HEAD_ID).unwrap().1.pose.position.y
        };
        let raise = |stack: &Stack| stack.apply(serde_json::from_value(serde_json::json!({ "action": "move_head", "args": { "offset": [0, 0.5, 0] } })).unwrap()).unwrap();
        let stack = test_stack();
        raise(&stack);
        assert_eq!(stack.start().unwrap_err().to_string(), "ALVR streamer not found; set streamer_dir in config.toml");
        assert_eq!(head_y(&stack), 2.1, "a refused start keeps the rig");
        let stack = test_stack();
        stack.paths.lock().streamer_dir = Some(std::env::temp_dir().join(format!("SteamVrMcp-stack-rig-reset-{}", std::process::id())));
        raise(&stack);
        assert_eq!(stack.start().unwrap_err().to_string(), "unit tests never start a headset");
        assert_eq!(head_y(&stack), 1.6, "a start that begins a session starts from the defaults");
    }

    #[test]
    fn vr_stop_waits_for_an_in_flight_vr_start_to_bail() {
        let (stack, root) = blocked_stack("stop-start", false);
        let epoch = stack.state.lock().stops;
        let (held, start_holds) = std::sync::mpsc::channel();
        let start = {
            let stack = Arc::clone(&stack);
            thread::spawn(move || {
                let _starting = stack.starting.lock(); // as start() holds it
                held.send(()).unwrap();
                // start_inner's next check sees the stop
                assert!(steamvr::wait_for(Duration::from_secs(5), || stack.check_stop(epoch).is_err()));
                assert!(stack.begin_managing(epoch).is_err() && !stack.state.lock().managing, "a stopped start does not take the session back");
                thread::sleep(Duration::from_millis(300)); // bailing out of a wait takes a moment
                Instant::now()
            })
        };
        start_holds.recv().unwrap();
        stack.stop().unwrap();
        let stopped = Instant::now();
        assert!(stopped >= start.join().unwrap(), "stop cleaned up only after the start let go");
        assert_eq!(stack.state.lock().stopping, 0, "vr_start is allowed again");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn vr_start_refuses_while_a_start_or_restart_runs() {
        let stack = test_stack();
        stack.state.lock().managing = true;
        let starting = stack.starting.lock();
        assert_eq!(stack.start().unwrap_err().to_string(), "not ready: vr_start or vr_stop is running");
        drop(starting);
        let restarting = stack.restarting.lock();
        assert_eq!(stack.start().unwrap_err().to_string(), "not ready: SteamVR is restarting");
        drop(restarting);
        assert!(stack.state.lock().managing, "a refused start leaves the running session managed");
        // what vr_stop holds between its epoch bump and taking `starting`
        stack.state.lock().stopping = 1;
        assert_eq!(stack.start().unwrap_err().to_string(), "not ready: vr_stop is running");
        stack.state.lock().stopping = 0;
    }

    #[test]
    fn set_alvr_settings_refuses_during_a_restart_and_frees_the_slot_when_it_fails() {
        let stack = test_stack();
        let values = || vec![SettingValue { path: STEAMVR_INPUT_2_0.into(), value: json!("index") }];
        let restarting = stack.restarting.lock();
        assert_eq!(stack.set_alvr_settings(values()).unwrap_err().to_string(), "not ready: SteamVR is restarting");
        drop(restarting);
        assert!(stack.set_alvr_settings(values()).unwrap_err().to_string().contains("run vr_start first"));
        assert!(!stack.restarting.is_locked(), "a failed call must not keep SteamVR restarts out");
    }

    #[test]
    fn vr_restore_refuses_while_any_steamvr_process_runs() {
        assert_eq!(
            restore_guard(&["vrcompositor", "vrmonitor"]).unwrap_err().to_string(),
            "SteamVR processes still run (vrcompositor, vrmonitor) and may rewrite steamvr.vrsettings; run vr_stop first"
        );
        assert!(restore_guard(&[]).is_ok());
    }

    #[test]
    fn vr_restore_refuses_while_a_start_restart_or_session_runs() {
        let stack = test_stack();
        let starting = stack.starting.lock();
        assert_eq!(stack.restore(false).unwrap_err().to_string(), "vr_start is running; run vr_stop first");
        drop(starting);
        let restarting = stack.restarting.lock();
        assert_eq!(stack.restore(false).unwrap_err().to_string(), "SteamVR is stopping or restarting");
        drop(restarting);
        stack.state.lock().managing = true;
        assert_eq!(stack.restore(false).unwrap_err().to_string(), "a vr_start session runs; run vr_stop first");
    }

    #[test]
    fn vr_stop_leaves_a_steamvr_without_alvr_alone() {
        assert_eq!(stop_guard(true, || false).unwrap_err().to_string(), "SteamVR is running without ALVR; close it yourself");
        assert!(stop_guard(true, || true).is_ok());
        assert!(stop_guard(false, || panic!("not asked while SteamVR is down")).is_ok());
        assert!(!steamvr::VrProcs::NONE.vrserver_running(), "a test stack's stop never sees the user's vrserver");
    }

    #[test]
    fn vr_stop_owns_a_failed_start_that_left_its_dashboard() {
        assert!(!stop_is_ours(false, false, false, || panic!("no dashboard: the file is not read")));
        assert!(!stop_is_ours(false, false, true, || false), "a dashboard alone: SteamVR may be the user's");
        assert!(stop_is_ours(false, false, true, || true), "its dashboard and ALVR's block: our failed start");
        assert!(stop_is_ours(true, false, false, || false) && stop_is_ours(false, true, false, || false));
        let file = std::env::temp_dir().join(format!("SteamVrMcp-stack-ours-{}.vrsettings", std::process::id()));
        std::fs::write(&file, r#"{"driver_alvr_server":{"blocked_by_safe_mode":true}}"#).unwrap();
        assert!(stop_is_ours(false, false, true, || setup::alvr_blocked(&file)));
        std::fs::write(&file, r#"{"driver_lighthouse":{"blocked_by_safe_mode":true}}"#).unwrap();
        assert!(!stop_is_ours(false, false, true, || setup::alvr_blocked(&file)));
        std::fs::remove_file(&file).ok();
    }

    #[test]
    fn an_alvr_without_our_vrserver_is_foreign() {
        assert_eq!(
            foreign_alvr_guard(false, || true, 8082).unwrap_err().to_string(),
            "port 8082 answers, but no SteamVR of this user runs (another user's SteamVR/ALVR?); close it and retry"
        );
        assert!(foreign_alvr_guard(false, || false, 8082).is_ok());
        assert!(foreign_alvr_guard(true, || panic!("our vrserver hosts the ALVR that answers"), 8082).is_ok());
    }

    #[test]
    fn adoption_tells_a_missing_driver_from_a_silent_api() {
        let wait = Duration::from_millis(300);
        let e = adoptable(false, || panic!("no driver: nothing to ask"), wait, 8082).unwrap_err();
        assert_eq!(e.to_string(), "SteamVR is running without the ALVR driver; close SteamVR and retry");
        let mut pings = 0;
        assert!(adoptable(true, || { pings += 1; pings == 3 }, Duration::from_secs(5), 8082).is_ok(), "ALVR still starting");
        let began = Instant::now();
        let e = adoptable(true, || false, wait, 8082).unwrap_err().to_string();
        assert_eq!(e, "ALVR's API does not answer on port 8082 (deadline 0.3 s); run vr_stop");
        assert!(began.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn adoption_restarts_only_for_a_stuck_entry() {
        let mut session = alvr_session::SessionConfig::default();
        assert!(!client_stuck(&session, "h"), "no entry: nothing to unstick");
        let entry = |state| alvr_session::ClientConnectionConfig {
            display_name: String::new(),
            current_ip: None,
            manual_ips: Default::default(),
            trusted: true,
            connection_state: state,
        };
        session.client_connections.insert("h".into(), entry(ConnectionState::Disconnected));
        assert!(!client_stuck(&session, "h"));
        session.client_connections.insert("h".into(), entry(ConnectionState::Disconnecting));
        assert!(client_stuck(&session, "h"));
    }

    #[test]
    fn adoption_reads_a_stuck_entry_from_the_session_file() {
        let dir = std::env::temp_dir().join(format!("SteamVrMcp-stack-session-file-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("session.json");
        // as ALVR saves it, cut to the part read here
        let session = |state: &str| {
            let entry = json!({ "display_name": "Unknown", "current_ip": "127.0.0.1", "manual_ips": ["127.0.0.1"], "trusted": true, "connection_state": state });
            std::fs::write(&file, json!({ "client_connections": { "h": entry } }).to_string()).unwrap();
        };
        session("Disconnecting");
        assert!(stuck_in_session_file(&file, "h"), "a dead client's EOF spin left it Disconnecting");
        assert!(!stuck_in_session_file(&file, "other"), "no entry for this headset");
        session("Streaming");
        assert!(stuck_in_session_file(&file, "h"), "only a Disconnected entry is connected to again");
        session("Disconnected");
        assert!(!stuck_in_session_file(&file, "h"));
        std::fs::write(&file, "not json").unwrap();
        assert!(!stuck_in_session_file(&file, "h"), "unreadable: the POSTs decide");
        assert!(!stuck_in_session_file(&dir.join("missing.json"), "h"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_unanswered_adopt_post_counts_as_stuck() {
        let read = |kind| alvr_common::anyhow::Error::from(std::io::Error::from(kind)).context("POST /api/session/values");
        assert!(timed_out(&read(std::io::ErrorKind::WouldBlock)), "SO_RCVTIMEO: os error 11");
        assert!(timed_out(&read(std::io::ErrorKind::TimedOut)));
        assert!(!timed_out(&read(std::io::ErrorKind::ConnectionRefused)), "ALVR not up: no restart helps");
        assert!(!timed_out(&anyhow!("POST /api/session/values: HTTP/1.1 400 Bad Request: x")), "ALVR answered");
    }

    #[test]
    fn steamvr_home_is_not_the_app() {
        let scene = |pid| openvr::SceneApp { pid, state: "running".into() };
        let comm = |pid| Some(if pid == 7 { "steamtours" } else { "Beat Saber.exe" }.to_owned());
        assert_eq!(game_pid(Some(&scene(9)), comm), Ok(9));
        assert_eq!(game_pid(Some(&scene(7)), comm), Err("no app running (scene is SteamVR Home)"));
        assert_eq!(game_pid(Some(&scene(0)), comm), Err("no scene app is running"));
        assert_eq!(game_pid(None, comm), Err("no scene app is running"));
        assert_eq!(game_pid(Some(&scene(9)), |_| None), Err("the scene app's pid is not a running process"), "unreadable comm");
        let big = i32::MAX as u32 + 1;
        assert_eq!(game_pid(Some(&scene(big)), comm), Err("the scene app's pid is not a running process"), "kill() would take it as a group");
    }

    #[test]
    fn unique_stems_differ_across_restarts() {
        let at = SystemTime::UNIX_EPOCH + Duration::from_millis(1_700_000_000_123);
        assert_eq!(unique_stem(at, 7), "vr-1700000000123-7");
        assert_ne!(unique_stem(at, 7), unique_stem(at, 8));
        assert_ne!(unique_stem(at, 7), unique_stem(at + Duration::from_millis(1), 7));
    }

    #[test]
    fn a_foreign_dashboard_alone_does_not_rerun_the_checks() {
        let failed = |name| Check { name, ok: false, reason: "x".into(), fix: String::new() };
        assert!(!core_failing(&[failed("alvr_dashboard_absent")]), "another MCP's stream: re-evaluated on its own");
        assert!(core_failing(&[failed("alvr_dashboard_absent"), failed("audio_input")]));
        assert!(!core_failing(&[failed("h264_encoder")]), "not a core check");
    }

    #[test]
    fn a_wedged_openvr_call_fails_reads_fast_and_shows_in_vr_status() {
        let stack = test_stack();
        assert_eq!(stack.openvr_call(|| 7), Some(7));
        assert!(!stack.status().warning.is_some_and(|w| w.contains("wedged")), "a call that returned is not counted");
        stack.openvr_calls.fetch_add(1, SeqCst); // what a call stuck past OPENVR_TIMEOUT leaves
        let started = Instant::now();
        let e = stack.read(|_| Ok(())).unwrap_err().to_string();
        assert!(e.contains("wedged") && e.contains("restart the MCP"), "{e}");
        assert!(started.elapsed() < Duration::from_secs(1), "no new connect, no new thread");
        assert!(stack.status().warning.is_some_and(|w| w.contains("wedged")));
        let a = LaunchArgs { app_id: "620".into(), via: Via::Steam, exe: None, args: vec![], env: Default::default() };
        assert!(stack.launch_app(a).unwrap_err().to_string().contains("wedged"), "fails at once, not at its deadline"); // test_stack has no Steam: never launches
        stack.openvr_calls.fetch_sub(1, SeqCst); // the stuck call returned after all
        assert!(!stack.read(|_| Ok(())).unwrap_err().to_string().contains("wedged"));
    }

    #[test]
    fn reads_refuse_only_while_steamvr_is_taken_down() {
        let stack = test_stack();
        let restarting = stack.restarting.lock(); // as vr_start's adopt setup and set_alvr_settings' apply hold it
        let e = stack.read(|_| Ok(())).unwrap_err().to_string();
        assert!(!e.contains("stopping or restarting"), "{e}");
        drop(restarting);
        let _down = stack.vr_down.lock(); // as vr_stop and restart_within hold it around drop_reader
        assert_eq!(stack.read(|_| Ok(())).unwrap_err().to_string(), "SteamVR is stopping or restarting");
    }

    #[test]
    fn launch_needs_steamvr_running() {
        assert!(launch_guard(false).unwrap_err().to_string().contains("vr_start"));
        assert!(launch_guard(true).is_ok());
    }

    #[test]
    fn within_reports_a_stuck_closure_as_leaked() {
        assert_eq!(within(Duration::from_secs(1), || 5), Some(5));
        assert_eq!(within(Duration::from_millis(20), || thread::sleep(Duration::from_millis(300))), None);
    }

    /// Records the call count when dropped, as a late OpenVR reader's VR_ShutdownInternal would run then.
    struct DropProbe(Arc<AtomicUsize>, Arc<Mutex<Option<usize>>>);
    impl Drop for DropProbe {
        fn drop(&mut self) {
            *self.1.lock() = Some(self.0.load(SeqCst));
        }
    }

    #[test]
    fn a_late_openvr_call_counts_until_its_result_is_dropped() {
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(Mutex::new(None));
        let probe = DropProbe(Arc::clone(&calls), Arc::clone(&seen));
        let late = counted_call(&calls, Duration::from_millis(20), move || {
            thread::sleep(Duration::from_millis(200));
            probe
        });
        assert!(late.is_none());
        assert!(steamvr::wait_for(Duration::from_secs(2), || seen.lock().is_some()));
        assert_eq!(*seen.lock(), Some(1), "still counted while the late result shuts down");
        assert!(steamvr::wait_for(Duration::from_secs(1), || calls.load(SeqCst) == 0));
        assert_eq!(counted_call(&calls, Duration::from_secs(1), || 5), Some(5));
        assert_eq!(calls.load(SeqCst), 0, "a call that returned in time is not counted");
        assert_eq!(counted_call(&calls, Duration::from_secs(1), || -> () { panic!("boom") }), None);
        assert!(steamvr::wait_for(Duration::from_secs(1), || calls.load(SeqCst) == 0), "a panic counts as returned");
    }

    #[test]
    fn frame_timing_reads_the_supervisors_statistics() {
        let stack = test_stack();
        let summary = |ms| alvr_events::StatisticsSummary { total_latency_ms: ms, ..Default::default() };
        let start = Instant::now();
        let ms = |n| start + Duration::from_millis(n);
        stack.keep_stats(start - Duration::from_millis(1), summary(1.0));
        stack.keep_stats(start, summary(2.0));
        stack.keep_stats(ms(500), summary(3.0));
        let got: Vec<f32> = stack.stats_since(start).iter().map(|s| s.total_latency_ms).collect();
        assert_eq!(got, [2.0, 3.0], "only the summaries that arrived in the window");
        for _ in 0..STATS_KEPT {
            stack.keep_stats(ms(600), summary(4.0));
        }
        assert_eq!(stack.stats_since(start).len(), STATS_KEPT, "the ring keeps the newest STATS_KEPT");
    }

    #[test]
    fn read_when_stable_waits_for_the_writer() {
        let dir = std::env::temp_dir().join(format!("SteamVrMcp-stack-stable-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("shot.png");
        // `exited` runs at the start of each poll, so the writer acts at known polls whatever the load
        let mut polls = 0;
        let writer = || {
            polls += 1;
            match polls {
                2 => std::fs::write(&path, b"ab").unwrap(),
                3 => std::fs::write(&path, b"abcd").unwrap(), // grows between two polls
                _ => (),
            }
            None
        };
        assert_eq!(read_when_stable(&path, Duration::from_secs(5), writer, String::new).unwrap(), b"abcd");
        assert_eq!(polls, 4, "stable once two polls see the same size");
        let err = read_when_stable(&dir.join("never.png"), Duration::from_millis(300), || None, String::new).unwrap_err().to_string();
        assert!(err.contains("never.png") && err.contains("deadline 0.3 s") && err.contains("vrserver may not see this directory"), "{err}");
        let err = read_when_stable(&dir.join("vr-1-2.png"), Duration::from_millis(300), || None, || COMPOSITOR_REQUESTED.into()).unwrap_err().to_string();
        assert!(err.contains("vrcompositor logged the request") && !err.contains("may not see"), "{err}");
        let other = || format!("{COMPOSITOR_REQUESTED}{COMPOSITOR_FAILED}").replace("vr-1-2", "vr-1-1");
        let err = read_when_stable(&dir.join("vr-1-2.png"), Duration::from_millis(300), || None, other).unwrap_err().to_string();
        assert!(err.contains("deadline") && err.contains("logged no screenshot request"), "another request's failure is not ours: {err}");
        let began = Instant::now();
        let err = read_when_stable(&dir.join("never.png"), Duration::from_secs(30), || Some("vrserver"), String::new).unwrap_err();
        assert!(err.to_string().contains("failed: vrserver exited") && began.elapsed() < Duration::from_secs(2), "{err}");
        let began = Instant::now();
        let failed = || format!("{COMPOSITOR_REQUESTED}{COMPOSITOR_FAILED}");
        let err = read_when_stable(&dir.join("vr-1-2.png"), Duration::from_secs(30), || None, failed).unwrap_err().to_string();
        assert!(began.elapsed() < Duration::from_secs(2), "bails at the failure line, not the deadline: {err}");
        assert_eq!(
            err,
            "failed: vrcompositor could not capture the scene app's textures (Screenshot: Failed to get the correct textures.); \
             OpenXR or Proton scene apps may not support compositor screenshots; use source headset"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// vrcompositor.txt after a TakeStereoScreenshot (stem vr-1-2) with an OpenXR scene app under Proton.
    const COMPOSITOR_REQUESTED: &str = "Tue Oct 27 2026 21:14:02.100 [Info] - Screenshot: Take stereo screenshot request dest filename=\"/h/.local/state/SteamVrMcp/screenshots/vr-1-2-preview\"\n\
        Tue Oct 27 2026 21:14:02.104 [Info] - Screenshot: Marked submission of screenshot textures for screenshot type 2, flags 3\n\
        Tue Oct 27 2026 21:14:02.110 [Info] - Screenshot: Capturing Stereo2D texture\n";
    const COMPOSITOR_FAILED: &str = "Tue Oct 27 2026 21:14:02.116 [Info] - Screenshot: Failed to get the correct textures.\n";

    #[test]
    fn compositor_log_shows_the_request_and_its_failure() {
        assert_eq!(compositor_shot("", "vr-1-2"), (false, None));
        assert_eq!(compositor_shot(COMPOSITOR_REQUESTED, "vr-1-2"), (true, None));
        let failed = format!("{COMPOSITOR_REQUESTED}{COMPOSITOR_FAILED}");
        assert_eq!(compositor_shot(&failed, "vr-1-2"), (true, Some("Screenshot: Failed to get the correct textures.")));
        assert_eq!(compositor_shot(&failed, "vr-1-22"), (false, None), "another request's lines");
    }

    #[test]
    fn compositor_log_counts_only_this_requests_failure() {
        // calls run concurrently: an earlier request (vr-1-1) fails after ours is logged, then ours (vr-1-2) fails
        let other = COMPOSITOR_REQUESTED.replace("vr-1-2", "vr-1-1");
        let log = format!("{other}{COMPOSITOR_FAILED}{COMPOSITOR_REQUESTED}");
        assert_eq!(compositor_shot(&log, "vr-1-2"), (true, None), "the earlier request's failure is not ours");
        assert_eq!(compositor_shot(&log, "vr-1-1"), (true, Some("Screenshot: Failed to get the correct textures.")));
        let log = format!("{COMPOSITOR_REQUESTED}{other}{COMPOSITOR_FAILED}");
        assert_eq!(compositor_shot(&log, "vr-1-2"), (true, None), "a failure after the next request belongs to that one");
        assert_eq!(compositor_shot(&log, "vr-1-1"), (true, Some("Screenshot: Failed to get the correct textures.")));
    }

    fn pairs(path: &str, value: serde_json::Value) -> Vec<PathValuePair> {
        vec![PathValuePair { path: parse_path(path).unwrap(), value }]
    }

    #[test]
    fn verify_applied_detects_mismatch() {
        let session = json!({ "session_settings": { "video": { "preferred_fps": 72.0 } } });
        let e = verify_applied(&session, &pairs("session_settings.video.preferred_fps", json!(90.0))).unwrap_err();
        assert!(e.contains("session_settings.video.preferred_fps") && e.contains("72"), "{e}");
    }

    #[test]
    fn verify_applied_accepts_match() {
        let mut session = serde_json::to_value(SessionConfig::default()).unwrap();
        // ALVR stores f32, so a requested 0.9 comes back as 0.8999999761581421
        session["session_settings"]["video"]["preferred_fps"] = json!(0.9f32);
        assert_eq!(verify_applied(&session, &pairs("session_settings.video.preferred_fps", json!(0.9))), Ok(()));
        let index = "session_settings.headset.controllers.content.hand_skeleton.content.steamvr_input_2_0";
        session["session_settings"]["headset"]["controllers"]["content"]["hand_skeleton"]["content"]["steamvr_input_2_0"] = json!(true);
        assert_eq!(verify_applied(&session, &pairs(index, json!(true))), Ok(()));
        let e = verify_applied(&session, &pairs("session_settings.video.nope", json!(1))).unwrap_err();
        assert!(e.contains("session_settings.video.nope"), "{e}");
    }

    #[test]
    fn settings_refuse_network_exposure() {
        let parse = |p: &str| parse_values(vec![SettingValue { path: p.into(), value: json!(true) }]);
        for p in [
            "client_connections",
            "client_connections.x.manual_ips",
            "session_settings",
            "session_settings.connection",
            "session_settings.connection.allow_remote_dashboard",
            "session_settings.connection.client_discovery.enabled",
            "session_settings.connection.stream_port",
            "session_settings.connection.enable_on_connect_script",
            "session_settings.headset",
            "session_settings.headset.vmc.content.host",
        ] {
            let e = format!("{:#}", parse(p).unwrap_err());
            assert!(e.contains("refused") && e.contains("session_settings.connection"), "{p}: {e}");
        }
        assert!(parse("session_settings.video.preferred_fps").is_ok());
        assert!(parse("session_settings.headset.controllers.content.hand_skeleton.content.steamvr_input_2_0").is_ok());
    }

    fn session_event(session: &SessionConfig) -> Event {
        Event { timestamp: String::new(), event_type: EventType::Session(Box::new(session.clone())) }
    }

    #[test]
    fn wait_applied_takes_a_match_that_lands_at_the_deadline() {
        let mut session = SessionConfig::default();
        session.session_settings.video.preferred_fps = 90.0;
        let values = pairs("session_settings.video.preferred_fps", json!(90.0));
        let deadline = Instant::now() + Duration::from_millis(50);
        // the read blocks past the deadline, then hands over the matching session
        let late = |_| {
            std::thread::sleep(deadline.saturating_duration_since(Instant::now()) + Duration::from_millis(20));
            Ok(Some(session_event(&session)))
        };
        assert!(wait_applied(late, &values, deadline).is_ok());
        let none = |t| {
            std::thread::sleep(t);
            Ok(None)
        };
        let e = wait_applied(none, &values, Instant::now() + Duration::from_millis(50)).unwrap_err();
        assert!(e.to_string().contains("ALVR did not apply the values: ALVR sent no session"), "{e}");
        let mismatch = |_| Ok(Some(session_event(&SessionConfig::default())));
        let e = wait_applied(mismatch, &values, Instant::now() + Duration::from_millis(50)).unwrap_err();
        assert!(e.to_string().contains("preferred_fps is"), "{e}");
    }

    #[test]
    fn settings_reject_bad_path() {
        let e = parse_values(vec![SettingValue { path: "video..fps".into(), value: json!(90.0) }]).unwrap_err();
        assert!(format!("{e:#}").contains("invalid path"), "{e:#}");
    }

    #[test]
    fn real_time_follows_alvr_schema() {
        let rt = |p: &str| is_real_time(&parse_path(p).unwrap());
        assert!(!rt("session_settings.video.preferred_fps"), "flagged steamvr-restart");
        assert!(!rt("session_settings.headset.controllers.content.hand_skeleton.content.steamvr_input_2_0"));
        assert!(rt("session_settings.extra.logging.log_haptics"), "flagged leaf");
        assert!(rt("session_settings.headset.controllers.content.haptics.enabled"), "under a flagged entry");
        assert!(!rt("session_settings.video.nope"));
        assert!(!rt("client_connections"));
    }

    fn line(t: &str, source: &str, level: &str, text: &str) -> LogLine {
        LogLine { t: t.into(), t_s: 0.0, at: String::new(), source: source.into(), level: level.into(), text: text.into() }
    }

    fn stamped(day: &str, time: &str) -> String {
        format!("{day} {time} [Info] - x")
    }

    #[test]
    fn steamvr_lines_carry_session_time() {
        let lines = steamvr_log_lines(
            "vrserver",
            ["Wed Oct 21 2026 05:00:36.212461 [Info] - a", "cont", "Wed Oct 21 2026 05:00:37.212461 [Info] - b"].map(String::from).to_vec(),
        );
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[1].t_s, lines[0].t_s);
        assert!((lines[2].t_s - lines[0].t_s - 1.0).abs() < 0.001);
        assert!(lines.iter().all(|l| l.at.ends_with('Z')));
        assert_eq!(lines[0].t, "05:00:36.212");
        assert!(steamvr_log_lines("vrserver", vec!["orphan".into()]).is_empty(), "no stamp yet: dropped");
    }

    #[test]
    fn logs_sort_across_midnight() {
        let steam = steamvr_log_lines(
            "vrserver",
            vec![stamped("Wed Oct 21 2026", "00:00:01.000000"), stamped("Tue Oct 20 2026", "23:59:59.000000")],
        );
        let sorted = filter_logs(steam, &LogQuery::default()).unwrap();
        assert_eq!(sorted.iter().map(|l| l.t.as_str()).collect::<Vec<_>>(), ["23:59:59.000", "00:00:01.000"]);
    }

    #[test]
    fn status_reports_session_clock() {
        let s = test_stack().status();
        assert!(s.now_t_s >= 0.0);
        assert!(s.t0_at.ends_with('Z') && s.now_at.ends_with('Z'));
        assert!(s.now_at >= s.t0_at);
    }

    #[test]
    fn wait_plan_rejects_bad_args() {
        let args = |app_id: &str, pattern: &str, fail: Option<&str>, timeout_s: Option<f64>| WaitForLogArgs {
            app_id: app_id.into(),
            pattern: pattern.into(),
            fail_pattern: fail.map(String::from),
            since_seq: None,
            timeout_s,
        };
        let plan = |a: WaitForLogArgs| wait_plan(&a, |id| id == 7);
        assert!(plan(args("7", "(", None, None)).is_err());
        assert!(plan(args("7", "ok", Some("["), None)).is_err());
        for bad in [0.0, 601.0, f64::NAN, -1.0] {
            assert!(plan(args("7", "ok", None, Some(bad))).is_err(), "{bad}");
        }
        assert!(plan(args("7", "ok", None, Some(1.0))).is_ok() && plan(args("7", "ok", None, Some(600.0))).is_ok());
        assert!(plan(args("8", "ok", None, None)).unwrap_err().contains("[app_logs]"));
        assert!(plan(args("x", "ok", None, None)).is_err());
        let (app, pattern, fail, timeout) = plan(args(" 7 ", "ok", Some("bad"), None)).unwrap();
        assert_eq!((app, pattern.as_str(), fail.unwrap().as_str(), timeout), (7, "ok", "bad", Duration::from_secs(60)));
    }

    #[test]
    fn watch_renews_only_our_lease_while_managing() {
        let (me, ttl, now) = (10, Duration::from_secs(1800), SystemTime::now());
        let ours = |left_s| Lease { owner: "a".into(), pid: me, expires_at: clock::rfc3339(now + Duration::from_secs(left_s)) };
        assert!(watch_renews(true, true, Some(&ours(100)), me, ttl, now), "half the ttl is gone");
        assert!(!watch_renews(true, true, Some(&ours(1700)), me, ttl, now), "fresh");
        assert!(watch_renews(true, true, Some(&Lease { expires_at: "garbage".into(), ..ours(0) }), me, ttl, now), "expired or unreadable");
        assert!(!watch_renews(false, true, Some(&ours(100)), me, ttl, now), "no session to keep");
        assert!(!watch_renews(true, false, Some(&ours(100)), me, ttl, now), "SteamVR is gone");
        assert!(!watch_renews(true, true, None, me, ttl, now), "vr_release gave it up: do not take it back");
        assert!(!watch_renews(true, true, Some(&Lease { pid: 11, ..ours(100) }), me, ttl, now), "another MCP's lease");
    }

    #[test]
    fn a_failed_restart_asks_for_vr_start() {
        let stack = test_stack();
        stack.state.lock().last_restart_error = Some((Instant::now(), "SteamVR restart failed: boom".into()));
        assert_eq!(stack.status().warning, None, "no session: nothing to rerun");
        stack.state.lock().managing = true;
        assert!(stack.status().warning.is_some_and(|w| w.contains("run vr_start")), "the supervisor does not retry");
        let starting = stack.starting.lock();
        assert_eq!(stack.status().warning, None, "vr_start runs");
        drop(starting);
        stack.state.lock().last_restart_error = None;
        assert_eq!(stack.status().warning, None);
    }

    #[test]
    fn vr_status_joins_every_warning() {
        let stack = test_stack();
        {
            let mut state = stack.state.lock();
            state.leaked = true;
            state.ended = Some(SESSION_LOST);
            state.managing = true;
            state.last_restart_error = Some((Instant::now(), "boom".into()));
        }
        stack.openvr_calls.fetch_add(1, SeqCst);
        assert_eq!(stack.status().warning.unwrap(), [LEAKED, WEDGED, SESSION_LOST, RESTART_FAILED].join("; "));
        // the watchdog's two: stayed_quiet only when neither a failed restart nor gave_up says more
        {
            let mut state = stack.state.lock();
            (state.watchdog.gave_up, state.watchdog.stayed_quiet) = (true, true);
        }
        assert_eq!(stack.status().warning.unwrap(), [LEAKED, WEDGED, SESSION_LOST, RESTART_FAILED, WATCHDOG_GAVE_UP].join("; "));
        {
            let mut state = stack.state.lock();
            state.watchdog.gave_up = false;
            state.last_restart_error = None;
        }
        assert_eq!(stack.status().warning.unwrap(), [LEAKED, WEDGED, SESSION_LOST, WATCHDOG_STAYED_QUIET].join("; "));
        stack.openvr_calls.fetch_sub(1, SeqCst);
        stack.state.lock().managing = false;
    }

    #[test]
    fn a_session_whose_steamvr_died_ends() {
        let (stack, root) = blocked_stack("lost", false);
        stack.state.lock().managing = true;
        let starting = stack.starting.lock();
        assert!(!stack.end_lost_session(None), "vr_start brings SteamVR up");
        drop(starting);
        let restarting = stack.restarting.lock();
        assert!(!stack.end_lost_session(None), "a restart takes SteamVR down on purpose");
        drop(restarting);
        assert!(stack.state.lock().managing && stack.status().warning.is_none());
        assert!(stack.end_lost_session(Some(Instant::now())));
        assert!(!stack.state.lock().managing, "no more lease renewal, supervisor restarts or stop_guard bypass");
        assert_eq!(stack.status().warning.as_deref(), Some(SESSION_LOST));
        assert!(!stack.end_lost_session(None), "already ended");
        stack.stop().unwrap();
        assert_eq!(stack.status().warning, None, "vr_stop clears it");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_session_ends_only_once_steamvr_stays_gone() {
        let t0 = Instant::now();
        let at = |s| t0 + Duration::from_secs(s);
        let mut g = VrGone::default();
        assert!(!g.lost(true, &["vrserver", "vrmonitor"], at(0)));
        // SteamVR's own restart: vrmonitor and vrstartup live on while vrserver is gone for seconds
        assert!(!g.lost(true, &["vrmonitor", "vrstartup"], at(1)));
        assert!(!g.lost(true, &["vrmonitor", "vrstartup"], at(15)), "a relaunching vrserver may take a while");
        assert!(g.lost(true, &["vrmonitor"], at(21)), "but not past LAUNCH_SETTLE");
        assert_eq!(g.seen, Some(at(0)));
        let mut g = VrGone::default();
        assert!(!g.lost(true, &[], at(0)));
        assert!(!g.lost(true, &[], at(2)));
        assert!(g.lost(true, &[], at(3)), "nothing of SteamVR for VR_EXIT_GRACE");
        let mut g = VrGone::default();
        assert!(!g.lost(false, &[], at(0)), "our own restart holds `restarting`");
        assert!(!g.lost(false, &[], at(30)));
        assert!(!g.lost(true, &[], at(31)), "the clock starts when the restart lets go");
        assert!(g.lost(true, &[], at(34)));
    }

    #[test]
    fn a_session_lost_to_a_failed_restart_says_so() {
        let stack = test_stack();
        let seen = Instant::now();
        let session = |error_at: Instant| {
            let mut state = stack.state.lock();
            state.managing = true;
            state.last_restart_error = Some((error_at, "SteamVR restart failed: boom".into()));
        };
        session(seen + Duration::from_secs(1));
        assert!(stack.end_lost_session(Some(seen)));
        assert_eq!(stack.status().warning.as_deref(), Some(RESTART_FAILED), "the restart took SteamVR down and failed");
        session(seen - Duration::from_secs(60));
        assert!(stack.end_lost_session(Some(seen)));
        assert_eq!(stack.status().warning.as_deref(), Some(SESSION_LOST), "an older failure; SteamVR ran after it");
    }

    #[test]
    fn a_lost_session_ends_its_dashboard_so_vr_release_works() {
        let (stack, root) = blocked_stack("lost-release", false);
        stack.claim().unwrap();
        let child = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        let pid = child.id();
        {
            let mut state = stack.state.lock();
            (state.managing, state.started_steam, state.dashboard) = (true, true, Some(child));
        }
        assert!(stack.end_lost_session(None));
        assert!(steamvr::stat(pid).is_none(), "the dashboard child is killed and reaped");
        assert!(stack.state.lock().started_steam, "kept: vr_stop closes the Steam it started");
        assert!(stack.release().unwrap(), "no lease left blocking other MCPs until its ttl");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn app_exit_ends_a_log_wait() {
        use std::os::unix::process::ExitStatusExt;
        let status = std::process::ExitStatus::from_raw(256);
        assert_eq!(app_exited(None, None, |_| true), None);
        assert_eq!(app_exited(None, Some(5), |_| true), None, "the scene still runs");
        assert!(app_exited(Some(status), Some(5), |_| true).unwrap().contains("Proton launcher exited"));
        assert!(app_exited(None, Some(5), |_| false).unwrap().contains("scene pid 5"));
    }

    /// A stack whose app 42 logs to the returned game.log, primed before the file exists, and whose Proton launch already exited.
    fn exited_game(name: &str) -> (Arc<Stack>, PathBuf, PathBuf) {
        use std::os::unix::process::ExitStatusExt;
        let (stack, root) = blocked_stack(name, false);
        let (steam, cfg) = (stack.paths().steam_dir.unwrap(), Config::parse("[app_logs]\n\"42\" = [\"${install}/game.log\"]").unwrap());
        std::fs::create_dir_all(steam.join("steamapps/common/Game")).unwrap();
        std::fs::write(steam.join("steamapps/appmanifest_42.acf"), "\"AppState\"\n{\n\t\"installdir\"\t\t\"Game\"\n}\n").unwrap();
        *stack.cfg.lock() = cfg.clone();
        *stack.app_logs.lock() = AppLogs::new(&cfg.app_logs);
        stack.read_app_log(ReadAppLogArgs { app_id: "42".into(), since_seq: None, contains: None, limit: None }).unwrap();
        let exited = Arc::new(Mutex::new(Some(std::process::ExitStatus::from_raw(256))));
        stack.state.lock().launched = Some(proton::Launched { pgid: 0, compatdata: root.clone(), app_id: 42, exited });
        (stack, root, steam.join("steamapps/common/Game/game.log"))
    }

    fn wait_ready(stack: &Stack, timeout_s: f64) -> Result<Value> {
        stack.wait_for_log(WaitForLogArgs { app_id: "42".into(), pattern: "READY".into(), fail_pattern: None, since_seq: None, timeout_s: Some(timeout_s) })
    }

    #[test]
    fn wait_for_log_fails_once_the_launched_game_exits() {
        let (stack, root, _) = exited_game("wait-exit");
        let started = Instant::now();
        let e = wait_ready(&stack, 5.0).unwrap_err().to_string();
        assert!(e.contains("failed: app exited"), "{e}");
        assert!(started.elapsed() < Duration::from_secs(2));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn wait_for_log_counts_the_unfinished_last_line_of_an_exited_game() {
        let (stack, root, log) = exited_game("wait-partial");
        std::fs::write(&log, "boot\nREADY").unwrap();
        assert_eq!(wait_ready(&stack, 5.0).unwrap()["result"], "match");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn wait_for_log_reads_a_backlog_over_the_read_cap_before_failing() {
        let (stack, root, log) = exited_game("wait-backlog");
        let filler = format!("{}\n", "x".repeat(1023));
        std::fs::write(&log, filler.repeat(5 << 10) + "READY\n").unwrap();
        assert_eq!(wait_ready(&stack, 5.0).unwrap()["result"], "match");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn wait_for_log_joins_a_last_line_written_in_two_parts() {
        let (stack, root, log) = exited_game("wait-split");
        std::fs::write(&log, "boot\n").unwrap();
        // a byte every 30 ms keeps each pair of 50 ms reads busy, while the line takes 150 ms: past two reads, so a count of finished lines splits it
        let writer = thread::spawn(move || {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
            for part in ["R", "E", "A", "D", "Y\n"] {
                thread::sleep(Duration::from_millis(30));
                f.write_all(part.as_bytes()).unwrap();
            }
        });
        assert_eq!(wait_ready(&stack, 5.0).unwrap()["result"], "match");
        writer.join().unwrap();
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn wait_for_log_counts_an_unfinished_line_still_growing_at_the_drain_limit() {
        let (stack, root, log) = exited_game("wait-growing");
        let writing = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let writer = {
            let (writing, log) = (Arc::clone(&writing), log.clone());
            thread::spawn(move || {
                use std::io::Write;
                let mut f = std::fs::OpenOptions::new().append(true).create(true).open(&log).unwrap();
                f.write_all(b"READY").unwrap();
                while writing.load(SeqCst) {
                    f.write_all(b".").unwrap();
                    thread::sleep(Duration::from_millis(5));
                }
            })
        };
        let started = Instant::now();
        let found = wait_ready(&stack, 30.0);
        writing.store(false, SeqCst);
        writer.join().unwrap();
        assert_eq!(found.unwrap()["result"], "match");
        assert!(started.elapsed() < LOG_DRAIN + Duration::from_secs(2), "{:?}", started.elapsed());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn wait_for_log_drain_is_bounded() {
        // a writer that never stops: the drain gives up at LOG_DRAIN, however its polls see the file grow
        let (stack, root, log) = exited_game("wait-drain");
        let writing = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let writer = {
            let (writing, log) = (Arc::clone(&writing), log.clone());
            thread::spawn(move || {
                use std::io::Write;
                let mut f = std::fs::OpenOptions::new().append(true).create(true).open(&log).unwrap();
                while writing.load(SeqCst) {
                    f.write_all(b"still logging\n").unwrap();
                    thread::sleep(Duration::from_millis(1));
                }
            })
        };
        let started = Instant::now();
        let e = wait_ready(&stack, 30.0).unwrap_err().to_string();
        assert!(e.contains("failed: app exited"), "{e}");
        assert!(started.elapsed() < LOG_DRAIN + Duration::from_secs(2), "{:?}", started.elapsed());
        writing.store(false, SeqCst);
        writer.join().unwrap();
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn vr_stop_ends_the_launched_game_first() {
        let (stack, root) = blocked_stack("stop-game", false);
        let dir = root.join("prefix");
        std::fs::create_dir_all(&dir).unwrap();
        let env = vec![
            ("PATH".into(), std::env::var_os("PATH").unwrap()),
            ("STEAM_COMPAT_DATA_PATH".into(), dir.clone().into()),
            ("STEAM_COMPAT_APP_ID".into(), "9".into()),
        ];
        let l = proton::ProtonLaunch { argv: vec!["sleep".into(), "30".into()], env, cwd: dir.clone(), compatdata: dir.clone(), app_id: 9 };
        let launched = proton::spawn(&l, &dir.join("run.log")).unwrap();
        stack.state.lock().launched = Some(launched.clone());
        assert_eq!(stack.stop_launched(), [launched.pgid]);
        assert!(launched.exited.lock().is_some(), "the game's group got SIGTERM and was reaped");
        assert!(stack.state.lock().launched.is_none());
        assert_eq!(stack.stop_launched(), Vec::<u32>::new(), "nothing left to stop");
        launched.signal(libc::SIGKILL);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_proton_launch_racing_vr_stop_is_refused_or_seen_by_it() {
        let (stack, root) = blocked_stack("launch-race", false);
        let dir = root.join("prefix");
        std::fs::create_dir_all(&dir).unwrap();
        let env = vec![("PATH".into(), std::env::var_os("PATH").unwrap())];
        let l = proton::ProtonLaunch { argv: vec!["sleep".into(), "30".into()], env, cwd: dir.clone(), compatdata: dir.clone(), app_id: 9 };
        // no session, or vr_stop already cleared managing and found nothing to stop
        let e = stack.spawn_launched(&l, &dir.join("run.log")).map(|l| l.signal(libc::SIGKILL)).unwrap_err().to_string();
        assert_eq!(e, "no vr_start session is being managed; run vr_start", "no stop ran here: the error must not blame one");
        assert!(stack.state.lock().launched.is_none(), "nothing spawned that vr_stop would miss");
        stack.state.lock().managing = true;
        let launched = stack.spawn_launched(&l, &dir.join("run.log")).unwrap();
        assert!(stack.state.lock().launched.is_some(), "kept for vr_stop");
        launched.signal(libc::SIGKILL);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn wait_for_log_sees_lines_logged_before_the_call() {
        let root = std::env::temp_dir().join(format!("SteamVrMcp-stack-wait-log-{}", std::process::id()));
        std::fs::remove_dir_all(&root).ok();
        let steam = root.join("steam");
        std::fs::create_dir_all(steam.join("steamapps")).unwrap();
        std::fs::write(steam.join("steamapps/appmanifest_42.acf"), "\"AppState\"\n{\n\t\"installdir\"\t\t\"Game\"\n}\n").unwrap();
        std::fs::create_dir_all(steam.join("steamapps/common/Game")).unwrap();
        let paths = Paths {
            steam_dir: Some(steam.clone()),
            flatpak_steam: false,
            steamvr_dir: None,
            streamer_dir: None,
            state_dir: root.join("state"),
            runtime_dir: root.join("run"),
        };
        let cfg = Config::parse("[app_logs]\n\"42\" = [\"${install}/game.log\"]").unwrap();
        let stack = Stack::new(cfg, paths);
        assert!(stack.readiness().app_logs);
        let read = |since| stack.read_app_log(ReadAppLogArgs { app_id: "42".into(), since_seq: since, contains: None, limit: None }).unwrap();
        assert!(read(None).lines.is_empty());
        for limit in [0, applog::PAGE + 1] {
            let e = stack.read_app_log(ReadAppLogArgs { app_id: "42".into(), since_seq: None, contains: None, limit: Some(limit) }).unwrap_err();
            assert_eq!(e.to_string(), "limit must be 1 to 500");
        }
        std::fs::write(steam.join("steamapps/common/Game/game.log"), "boot\nERROR x\nREADY\n").unwrap();
        let wait = |pattern: &str, fail: Option<&str>, since| {
            let a = WaitForLogArgs { app_id: "42".into(), pattern: pattern.into(), fail_pattern: fail.map(String::from), since_seq: since, timeout_s: Some(1.0) };
            stack.wait_for_log(a).unwrap()
        };
        let hit = wait("READY", None, None);
        assert_eq!((hit["result"].as_str(), hit["line"]["text"].as_str(), hit["next_seq"].as_u64()), (Some("match"), Some("READY"), Some(4)));
        assert_eq!(wait("READY", Some("ERROR"), None)["result"], "fail");
        let late = wait("READY", None, Some(4));
        assert_eq!((late["result"].as_str(), late["next_seq"].as_u64()), (Some("timeout"), Some(4)));
        std::fs::remove_dir_all(&root).ok();
    }

    fn query(source: &str, contains: Option<&str>, min_level: Option<&str>, limit: Option<usize>) -> LogQuery {
        LogQuery { source: Some(source.into()), contains: contains.map(Into::into), min_level: min_level.map(Into::into), limit }
    }

    #[test]
    fn logs_filter_by_level_and_text() {
        let ring = vec![
            line("10:00:00.000", "alvr", "info", "Connected"),
            line("10:00:01.000", "alvr", "error", "Encoder FAILED"),
            line("10:00:02.000", "alvr", "error", "second error"),
            line("10:00:03.000", "vrserver", "info", "failed to load"),
        ];
        let texts = |q| filter_logs(ring.clone(), &q).unwrap().into_iter().map(|l| l.text).collect::<Vec<_>>();
        assert_eq!(texts(query("alvr", None, Some("error"), None)), ["Encoder FAILED", "second error"]);
        assert_eq!(texts(query("all", None, Some("error"), None)), ["Encoder FAILED", "second error", "failed to load"], "min_level is for ALVR only");
        assert_eq!(texts(query("all", Some("fAiLeD"), None, None)), ["Encoder FAILED", "failed to load"]);
        assert_eq!(texts(query("all", None, None, Some(2))), ["second error", "failed to load"], "limit keeps the newest");
        assert_eq!(texts(LogQuery::default()).len(), 4, "all sources by default");
        for bad in [query("steamvr", None, None, None), query("all", None, Some("fatal"), None), query("all", None, None, Some(0))] {
            assert!(filter_logs(ring.clone(), &bad).is_err());
        }
    }

    #[test]
    fn logs_merge_sources_in_time_order() {
        let steam = steamvr_log_lines(
            "vrserver",
            ["Wed Oct 21 2026 05:00:36.212461 [Info] - b", "continuation", "Wed Oct 21 2026 05:00:38.000001 [Error] - d"].map(String::from).to_vec(),
        );
        assert_eq!((steam[0].t.as_str(), steam[0].level.as_str(), steam[0].text.as_str()), ("05:00:36.212", "info", "b"));
        assert_eq!((steam[1].t.as_str(), steam[1].level.as_str(), steam[1].text.as_str()), ("05:00:36.212", "info", "continuation"), "belongs to the line before");
        let alvr_at = |t_s, level: &str, text: &str| LogLine { t_s, ..line("", "alvr", level, text) };
        let base = steam[0].t_s;
        let alvr = vec![alvr_at(base - 1.0, "info", "a"), alvr_at(base + 1.0, "warning", "c")];
        let merged = filter_logs([alvr, steam].concat(), &LogQuery::default()).unwrap();
        assert_eq!(merged.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(), ["a", "b", "continuation", "c", "d"]);
    }

    // shortcut: pins the decision, not start_inner's wiring of it (that needs SteamVR); add a launch seam if the order regresses again
    #[test]
    fn cold_start_brings_the_headset_up_after_alvr_answers() {
        assert_eq!(headset_up(false), HeadsetUp::AfterAlvrAnswers, "a client listening while vrserver starts races Hmd::activate");
        assert_eq!(headset_up(true), HeadsetUp::BeforeLaunch, "an adopted SteamVR activated its HMD long ago");
    }

    #[test]
    fn hmd_activation_needs_the_active_hmd_then_the_added_device() {
        let active = "Wed Oct 21 2026 20:11:52.675 - Active HMD set to alvr_server.1WMHH000X00000\n";
        let added = "Wed Oct 21 2026 20:11:52.816 - Driver 'alvr_server' finished adding tracked device with serial number '1WMHH000X00000'\n";
        assert!(hmd_activated(&format!("x\n{active}y\n{added}")), "activated");
        assert!(!hmd_activated(""), "nothing logged yet");
        assert!(!hmd_activated(active), "active HMD set, device not added yet");
        assert!(!hmd_activated(&format!("{added}{active}")), "the device line must follow the active HMD");
    }

    #[test]
    fn headset_comes_up_only_while_managing() {
        assert_eq!(headset_step(true, true), HeadsetStep::Resume, "the paused client");
        assert_eq!(headset_step(true, false), HeadsetStep::StartNew, "its pause leaked it, or none was kept");
        assert_eq!(headset_step(false, true), HeadsetStep::Skip, "vr_stop ended the session; it disposes the client");
        assert_eq!(headset_step(false, false), HeadsetStep::Skip, "vr_stop took the client; a new one would outlive it");
    }

    #[test]
    fn headset_is_not_quiet_during_a_restart() {
        assert!(counts_as_quiet(false, true, false), "attached, not streaming, no restart");
        assert!(!counts_as_quiet(true, true, false), "streaming");
        assert!(!counts_as_quiet(false, false, false), "no events stream");
        assert!(!counts_as_quiet(false, true, true), "a restart holds the headset out of state");
    }

    #[test]
    fn the_watchdog_rearms_only_after_a_stream_held_and_gives_up() {
        let mut w = Watchdog::default();
        let (quiet, s) = (Some(WATCHDOG + Duration::from_secs(1)), Duration::from_secs);
        assert!(!w.fires(None, Some(s(10))), "not quiet long enough");
        assert!(w.fires(None, quiet), "armed by vr_start");
        assert!(!w.fires(None, quiet), "once per arming");
        assert!(!w.fires(Some(s(1)), None), "a stream that came up for a tick");
        assert!(!w.fires(None, quiet), "does not re-arm it");
        assert!(!w.fires(Some(WATCHDOG_REARM), None));
        assert!(w.fires(None, quiet), "a stream that held does");
        assert!(!w.fires(Some(WATCHDOG_REARM), None));
        assert!(w.fires(None, quiet));
        assert!(!w.fires(Some(WATCHDOG_REARM), None) && !w.gave_up);
        assert!(!w.fires(None, quiet), "WATCHDOG_MAX restarts per vr_start");
        assert!(w.gave_up && w.restarts == WATCHDOG_MAX);
    }

    #[test]
    fn the_watchdog_flags_a_headset_that_stays_quiet_after_a_fire() {
        let mut w = Watchdog::default();
        let s = Duration::from_secs;
        assert!(w.fires(None, Some(WATCHDOG + s(1))));
        assert!(!w.fires(None, Some(2 * WATCHDOG)) && !w.stayed_quiet, "not quiet long enough yet");
        assert!(!w.fires(None, Some(2 * WATCHDOG + s(1))) && w.stayed_quiet, "never came back");
        assert!(!w.fires(Some(WATCHDOG_REARM), None) && !w.stayed_quiet, "a stream that held clears it");
        // came back for less than WATCHDOG_REARM, then dropped
        assert!(w.fires(None, Some(WATCHDOG + s(1))));
        assert!(!w.fires(Some(s(5)), None));
        assert!(!w.fires(None, Some(2 * WATCHDOG + s(1))) && w.stayed_quiet);
    }

    #[test]
    fn vr_status_warns_when_the_headset_stayed_quiet_after_a_watchdog_restart() {
        let stack = test_stack();
        stack.state.lock().watchdog.stayed_quiet = true;
        assert_eq!(stack.status().warning, None, "no session");
        stack.state.lock().managing = true;
        assert_eq!(stack.status().warning.as_deref(), Some(WATCHDOG_STAYED_QUIET));
        stack.state.lock().managing = false;
    }

    #[test]
    fn vr_status_warns_once_the_watchdog_gave_up() {
        let stack = test_stack();
        stack.state.lock().watchdog.gave_up = true;
        assert_eq!(stack.status().warning, None, "no session");
        stack.state.lock().managing = true;
        assert_eq!(stack.status().warning.as_deref(), Some(WATCHDOG_GAVE_UP));
        stack.state.lock().managing = false;
    }

    #[test]
    fn restart_hud_matches_client_core_format() {
        // client_core's set_hud_message prefixes every message with version, hostname and IP
        let hud = |m: &str| format!("ALVR v21.0.0-dev14\nhostname: 1234.client.local.\nIP: 192.0.2.1\n\n{m}");
        assert!(is_restart_hud(&hud(SERVER_RESTART_MESSAGE)));
        assert!(!is_restart_hud(&hud("Searching for streamer...")));
    }
}
