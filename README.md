# SteamVR-MCP

An MCP server that gives an AI agent a virtual SteamVR headset, so it can test VR apps on Linux with nobody wearing one.

The server joins a stock ALVR 21 streamer as a headset client over loopback. SteamVR then sees a headset with two Valve Index controllers and optional hand tracking. The agent can start and stop SteamVR, launch a VR game, move the head and hands, press buttons, and then check what happened: screenshots of what the headset receives, SteamVR's tracked poses and skeletons, haptic pulses, frame timing, audio levels and log lines. Input enters SteamVR at the driver level, so OpenXR and OpenVR games both work. ALVR is not patched and no OpenXR layer is installed.

## Requirements

- Linux with a logged-in desktop session (X11 or Wayland). Headless and CI machines are not supported.
- Native Steam. Flatpak Steam is refused.
- SteamVR, installed and started once.
- The ALVR streamer at the version the crates are pinned to: 21.0.0-dev14, a nightly. Nightlies must match exactly; once a stable 21.x ships, any 21.x release works. ALVR publishes nightlies as releases of `alvr-org/ALVR-nightly` on GitHub. Pick the one whose version is 21.0.0-dev14 (ALVR commit `ebec107b`), download `alvr_streamer_linux` and unpack it anywhere. Do not run ALVR's own dashboard while the MCP works: the MCP plays that role.
- An AMD or Intel GPU for SteamVR. ALVR 21 direct mode is broken on NVIDIA upstream (ALVR #3421), so a machine with only NVIDIA GPUs is refused. A hybrid laptop works if SteamVR runs on the AMD or Intel GPU (see [Hybrid GPUs](#hybrid-gpus)).
- VA-API H.264 encoding on that GPU (Mesa's `radeonsi` or Intel's media driver).
- A default audio input. ALVR's client opens it at every connection.
- Optional: PipeWire for `record_audio` and `audio_level`.

`vr_status` lists every startup check with its result and the fix.

## Build

You need Rust 1.98 or newer, clang (for bindgen), pkg-config, and the ALSA and PipeWire development packages: `alsa-lib-devel pipewire-devel clang` on Fedora, `libasound2-dev libpipewire-0.3-dev clang pkg-config` on Debian and Ubuntu. Then:

```sh
cargo build --release
```

The binary is `target/release/SteamVR-MCP`. `cargo test` runs the unit and integration tests and needs no Steam.

If the build cannot find `alsa.pc` or `libpipewire-0.3.pc`, point `PKG_CONFIG_PATH` at the directory holding them. If bindgen cannot find `stddef.h`, pass your compiler's include directory through `BINDGEN_EXTRA_CLANG_ARGS`, for example `-I/usr/lib/gcc/x86_64-linux-gnu/14/include`.

## Register it with an MCP host

The server speaks MCP over stdio and takes no arguments. Registering it starts nothing: the startup checks only read state.

Claude Code:

```sh
claude mcp add --scope user SteamVR-MCP -- /path/to/SteamVR-MCP/target/release/SteamVR-MCP
```

Other hosts take the same command in their usual `mcpServers` entry:

```json
{
  "mcpServers": {
    "SteamVR-MCP": { "command": "/path/to/SteamVR-MCP/target/release/SteamVR-MCP" }
  }
}
```

Stdout carries only the protocol. Logs go to stderr (see [Logging](#logging)).

### Tool gating

Most hosts see a tool only while its prerequisites hold, and the server sends `notifications/tools/list_changed` when that changes. `vr_status`, `get_logs`, `vr_acquire` and `vr_release` are always listed. Before any session a healthy machine also lists `vr_start`, `record_audio` and `audio_level`, plus `vr_restore` when `restore.json` exists and `read_app_log` and `wait_for_log` when `app_logs` is set. The input tools appear once the headset streams.

Claude Code is the exception: it gets every tool listed from the start. It does act on `tools/list_changed` (for MCP protocol 2026-07-28 through `subscriptions/listen`, which the server serves), but its subagents keep the tool list they were spawned with, and a refresh in the middle of a turn removes tools the model already planned to call. Hosts like this are listed in `HOSTS_WITHOUT_LIST_CHANGED` in `src/server.rs`.

Whether listed or not, a tool whose prerequisites fail answers `not ready: <reason>`.

## Configuration

Paths are detected, so most machines need no config. An optional `$XDG_CONFIG_HOME/SteamVR-MCP/config.toml` (usually `~/.config/SteamVR-MCP/config.toml`) overrides the detection. Unknown keys are rejected.

| Key | Default | Meaning |
| --- | --- | --- |
| `steam_dir` | `~/.steam/root`, then `~/.local/share/Steam` | Steam root. |
| `steamvr_dir` | found through ALVR, then `<steam_dir>/steamapps/common/SteamVR` | SteamVR install. |
| `streamer_dir` | the ALVR driver already registered with SteamVR | The unpacked `alvr_streamer_linux`. |
| `steam.env` | empty | Environment variables for Steam, a table of strings. |
| `steam.args` | empty | Extra arguments for Steam, a list of strings. |
| `alvr_settings` | empty | ALVR session path to value, applied after the built-in defaults on every `vr_start`. |
| `timeouts.streaming_s` | 240 | How long `vr_start` waits for the headset to stream, restarts included. |
| `timeouts.app_s` | 180 | How long `launch_app` waits for the app's scene. |
| `app_logs` | empty | Steam app id to glob patterns of the game's own log files, for `read_app_log` and `wait_for_log`. |

A minimal file:

```toml
streamer_dir = "/path/to/alvr_streamer_linux"
```

The server reads the file at startup, at each `vr_start`, and at `vr_status` when the file changed or a core check fails. It applies a new version only while nothing runs, so after you fix a failing check, the next `vr_status` shows it without restarting the MCP. If SteamVR or a session is up, run `vr_stop` first. A malformed file does not stop the server: the `config` check fails, and `vr_start` stays unavailable until the file parses. While `restore.json` exists, changing `streamer_dir`, `steam_dir` or `steamvr_dir` makes `vr_start` refuse until you run `vr_restore` (see [What it changes on your system](#what-it-changes-on-your-system)).

### Steam environment

`steam.env` and `steam.args` apply only when the MCP starts Steam. A Steam that is already running is used as it is, since its environment cannot change. If it runs without the `steam.env` values, `vr_start` fails at once and asks you to close Steam first.

Steam gets only the desktop session's variables (`HOME`, `PATH`, `DISPLAY`, `WAYLAND_DISPLAY`, `XDG_*`, locale, D-Bus and Pulse) plus `steam.env`. The rest of the agent's environment, API keys included, never reaches Steam or its games.

If the new Steam client lacks SteamVR's launcher service, set `STEAM_FORCE_CLIENT` in `steam.env` to the directory name of the older 32-bit Steam client under the Steam root.

### Hybrid GPUs

SteamVR's compositor runs on the GPU Steam uses, and a Proton or OpenXR game has to render on the same GPU. A game whose DXVK picks the other GPU crashes inside `vrclient`. Select the SteamVR GPU for Steam through `steam.env`:

```toml
[steam]
env = { MESA_VK_DEVICE_SELECT = "<vendor:device>!" }
```

Find `<vendor:device>` as the `[vvvv:dddd]` pair on the GPU's line in `lspci -nn`, or the vendorID and deviceID in `vulkaninfo --summary`. Then give the game's Steam launch options the Vulkan ICD of that GPU and the same device selection:

```
VK_DRIVER_FILES=<icd json of the SteamVR GPU> MESA_VK_DEVICE_SELECT=<vendor:device>! %command%
```

For `launch_app` with `via: "proton"`, pass `MESA_VK_DEVICE_SELECT` in its `env`. Its env allowlist refuses `VK_DRIVER_FILES` and `VK_ICD_FILENAMES`, which load drivers, so there the device selection alone picks the GPU.

The MCP never edits your desktop launchers or a game's launch options.

### Game logs

`app_logs` maps a Steam app id (decimal, as a string) to glob patterns. A pattern may use:

- `${compatdata}`: `<library>/steamapps/compatdata/<id>`
- `${prefix}`: `${compatdata}/pfx`
- `${install}`: the install dir from the app manifest. It is empty for a non-Steam shortcut, so those patterns match nothing.

`**` is refused, because Proton prefixes link `z:` to `/`. Write one `*` per directory level. An unknown variable, a bad pattern or an id that is not a plain decimal fails the `config` check. This example reads a BepInEx log and a Unity player log of a Proton game:

```toml
[app_logs]
"<app_id>" = [
  "${install}/BepInEx/LogOutput.log",
  "${prefix}/drive_c/users/steamuser/AppData/LocalLow/*/*/Player.log",
]
```

### ALVR settings

`alvr_settings` takes ALVR's path syntax from the session JSON root:

```toml
[alvr_settings]
"session_settings.video.preferred_fps" = 90.0
```

The built-in defaults the MCP applies first: H.264 Baseline, foveated encoding off, transcoding and emulated view resolution scale 1.0, 72 fps, game audio and microphone streaming off, `Stage` recentering, tracked Index controllers with hand skeletons and gesture input, a manual button mapping that passes every Index input through, client discovery off and the remote dashboard off. They set no keyframe or IDR interval, so ALVR's own encoder defaults apply. `set_alvr_settings` changes last until the next `vr_start`, which applies the defaults and `alvr_settings` again.

`alvr_settings` in config.toml is not filtered the way `set_alvr_settings` is, so it can turn client discovery or the remote dashboard back on and change the connection settings. Leave those alone unless you mean to expose ALVR on the network.

## Tools

Coordinates are SteamVR standing space: +Y up, -Z forward. Arguments use meters, degrees and seconds. The default `Stage` recentering means the poses you send are absolute. A controller pose is the Index controller's raw pose, which ALVR moves by its default controller offset of 11 cm along the controller's own -Z, so `get_tracked_state` reports the controller 11 cm along its own -Z from the pose you set. Games aim and grip from poses SteamVR derives from the raw one (see [Observed behavior](#observed-behavior)).

### Session

| Tool | What it does |
| --- | --- |
| `vr_status` | Startup checks and the state of Steam, SteamVR, ALVR, the headset, the lease and the session clock. |
| `vr_acquire` | Take or renew the SteamVR lease under a name you choose. |
| `vr_release` | Give the lease up so another agent can take it. Refuses while this MCP runs a session; `vr_stop` already releases it. |
| `vr_start` | Start Steam if needed, then SteamVR with ALVR and the headset, and return once it streams. Once past its preflight checks it resets the rig (poses, buttons, hands, playspace, proximity) to its defaults, so set poses after it. A start refused by those checks (no ALVR streamer, or a running Steam without `steam.env`) keeps the rig, and so does a start while the headset already streams. |
| `vr_stop` | Shut SteamVR down and disconnect the headset. Ask the Steam that `vr_start` started to exit, with SIGTERM to its client if it ignores that. |
| `vr_restore` | Put back the SteamVR and ALVR setup from before the first `vr_start`, and report whether all of it is back. |
| `set_alvr_settings` | Set ALVR session values by path in a `vr_start` session, restarting SteamVR when ALVR needs it. |
| `set_hand_tracking_flavour` | Bare hands as the Index controller (`index`) or as separate skeletal hand trackers (`skeletal_tracker`). |

### Apps

| Tool | What it does |
| --- | --- |
| `launch_app` | Launch a Steam app or shortcut through Steam, or a Steam app straight through its Proton (`via: "proton"`), and wait for its scene. |
| `stop_app` | Stop the scene app, and for a Proton launch its whole process tree. Works without a streaming headset. |
| `close_dashboard` | Click the right controller's system button once if SteamVR's dashboard is open, so the game gets focus. |

### Head and playspace

| Tool | What it does |
| --- | --- |
| `set_head_pose` | Set the head pose. |
| `move_head` | Move the head relative to its pose. |
| `animate_head` | Move the head through waypoints over time. |
| `set_proximity` | Tell SteamVR the headset is worn or taken off. |
| `set_playspace` | Set the play area size. |
| `recenter` | Send the play area to ALVR again, which recenters. |

### Controllers

| Tool | What it does |
| --- | --- |
| `set_controller_pose` | Set a controller's pose, in the playspace or relative to the head. |
| `move_controller` | Move a controller relative to its pose. |
| `animate_controller` | Move a controller through waypoints over time. |
| `set_controller_state` | Connect or disconnect a controller. |
| `press_button` | Press and hold an Index button. |
| `release_button` | Release a button. |
| `touch_button` | Touch or untouch a capacitive sensor. |
| `set_trigger` | Set the trigger value. |
| `set_grip` | Set the grip value and force. |
| `set_thumbstick` | Set the thumbstick position, click and touch. |
| `set_trackpad` | Set the trackpad position, force and touch. |

### Hands

| Tool | What it does |
| --- | --- |
| `set_finger_curl` | Curl and splay each finger. |
| `set_hand_mode` | Per hand, hold a controller or be a bare tracked hand. |
| `set_hand_skeleton` | Send raw 26-joint hand data. |
| `set_hand_gesture` | Pose a hand with a named gesture (open, fist, point, pinch, thumbs_up) or custom curls. |
| `run_sequence` | Run timed steps of the head, controller and hand tools, all checked before any runs. |

### Observation

| Tool | What it does |
| --- | --- |
| `screenshot` | PNG from `headset` (what the headset receives, about 18 frames after a fresh keyframe; the default) or `compositor` (SteamVR's uncompressed scene textures). There is no desktop or window source. |
| `get_tracked_state` | What SteamVR tracks: devices, poses, velocities, skeletons and curls, the scene app. |
| `get_haptics` | Haptic events the app sent. |
| `get_frame_timing` | SteamVR compositor frame timings, frame counts and ALVR latency statistics over a window. |
| `get_logs` | ALVR events plus the tails of SteamVR's vrserver, vrcompositor and vrmonitor logs. |
| `read_app_log` | Lines of a game's own log files, as `app_logs` configures, filtered by text and capped by `limit`. |
| `wait_for_log` | Wait for a game log line matching a regex, a `fail_pattern`, or the timeout. |
| `record_audio` | Record the default audio output to a WAV file. |
| `audio_level` | RMS and peak level of the default audio output over a short window. |

Each tool's full description, with every argument and limit, is in its MCP schema (from `src/server.rs`).

### Behavior notes

- `vr_start`, `vr_stop`, `set_alvr_settings`, `set_hand_tracking_flavour`, `launch_app`, `run_sequence` and `wait_for_log` return only when they finish, which can take minutes.
- Waits end with an error that says why: `failed: <signal>` for a known failure, such as vrserver exiting, or `deadline N s`. `wait_for_log` answers `result: "match"`, `"fail"` or `"timeout"`, and fails with `failed: app exited` once the game it watched is gone. Before it fails it reads the game's log files to their end, an unfinished last line included. That read lasts until the files stop growing, at most 2 s, so it can run past `timeout_s`.
- Times come in two forms. `t_s` is wall-clock seconds since the MCP started, and `at` is the same instant in UTC (RFC 3339). `vr_status` reports `now_t_s`, `now_at` and `t0_at`, so lines from different tools can be put in order. `t_s` follows the system clock, so it matches `at` and SteamVR's log times, and it jumps if the clock is stepped (a suspend, an NTP correction). Do not compute durations across such a step. After a backward step, call `get_haptics` without `since_s`, and expect `get_logs` with a `limit` to drop some newer lines.
- A `headset` screenshot waits for about 18 frames after a fresh keyframe, so the image is sharper and a pose set just before it has usually reached the game. That makes it slower than the keyframe alone, and a slow game can still show the old aim. If the stream stalls or a frame fails to decode (a lost frame usually does), it returns an earlier frame, at worst the keyframe. Corrupt data that still decodes is not detected.
- The first connection against a fresh ALVR session always restarts SteamVR once. `vr_start` handles it and reports `restarts`. One `timeouts.streaming_s` deadline covers the whole `vr_start`, restarts included.
- SteamVR opens its dashboard about 10 s after no scene app runs, and a game started under it stays unfocused (OpenXR `VISIBLE`, not `FOCUSED`). `vr_start` reports `dashboard_visible`. `launch_app` closes the dashboard by itself; for a game started another way, call `close_dashboard`.
- SteamVR reserves the Index system click for its dashboard. System touch reaches apps.
- SteamVR gives background OpenVR readers no input until a scene app (SteamVR Home counts) runs, so `get_tracked_state` skeletons read inactive right after `vr_start`.
- `set_alvr_settings` needs a `vr_start` session. It refuses `client_connections`, `session_settings.connection` and `session_settings.headset.vmc`, anything below them and their parents, because those would let ALVR stream to or take tracking from another host. Every `vr_start` also sets `allow_remote_dashboard` back to `false`.
- `launch_app` with `via: "proton"` needs a `vr_start` session and a streaming headset, like the Steam mode. The game's Steam launch options do not apply and Steam shows no dialogs. The launcher's output goes to the `run_log` it returns. Its arguments are limited so a call cannot run a program of its choosing:
  - `exe` is relative to the install dir and must stay inside it. It must be a real Windows `.exe` (an `MZ` header), and not a shell, script interpreter or loader: `cmd`, `powershell`, `pwsh`, `wscript`, `cscript`, `mshta`, `rundll32`, `regsvr32`, `node`, `start`, `explorer`, or a name starting with `python`, `java` or `wine`.
  - `args` may not hold the Chromium/CEF switches that name a program to run: `renderer-cmd-prefix`, `gpu-launcher`, `utility-cmd-prefix` and `browser-subprocess-path`.
  - `env` takes upper-case `[A-Z0-9_]` names starting with `MESA_`, `RADV_`, `VK_`, `DXVK_`, `VKD3D_`, `PROTON_`, `SDL_` or `__GL_`, except names that hold `PATH`, `FILENAMES`, `_FILES`, `LIBRARY`, `DYNAMIC_API` or `GL_DRIVER` or end in `_CMD`. It also takes `WINEDEBUG`, `WINEDLLOVERRIDES`, `DRI_PRIME`, `__NV_PRIME_RENDER_OFFLOAD`, `__VK_LAYER_NV_optimus`, `MANGOHUD`, `WINE_FULLSCREEN_FSR`, `WINEFSYNC`, `WINEESYNC`, `PULSE_LATENCY_MSEC`, `AMD_VULKAN_ICD`, and `__GLX_VENDOR_LIBRARY_NAME` with a plain `[a-z0-9_]` value. No value may hold `..`.
  - Compatibility tools and runtimes (an install dir with a `toolmanifest.vdf`, such as Proton or the Steam Linux Runtime), non-Steam shortcuts and SteamVR itself are refused as apps.
- `stop_app` works without a streaming headset, and stops a Proton launch even after SteamVR is gone.
- `screenshot {source: "compositor"}` fails at once when vrcompositor logs `Screenshot: Failed to get the correct textures.`, which happens with some OpenXR and Proton scene apps; use the `headset` source then. Its 10 s timeout says whether vrcompositor logged the request at all (`vrcompositor logged the request but wrote no file`, or `vrcompositor logged no screenshot request; vrserver may not see this directory`).
- `get_frame_timing` takes ALVR's statistics from the events stream the MCP holds during its session. Its `compositor` block summarizes only the frames still in SteamVR's frame history. The MCP asks for 1024, but SteamVR has been seen to return only about the last 128, whatever `window_s` is, and since the compositor can run far above the nominal refresh rate, those may span well under a second. The `counts` (presents, dropped and reprojected frames, from SteamVR's cumulative counters) cover the whole window, so for the frame rate over a long window use `counts.presents / window_s`.
- The MCP talks only to the ALVR that this user's vrserver hosts. Any other listener on ALVR's port gets `not ours: the ALVR on port N does not belong to this user's SteamVR`. Responses are capped at 8 MiB, and a request ALVR leaves unanswered fails with its method and route, for example `GET /api/version: ALVR accepted the connection but did not answer within 10 s`.
- An OpenVR call that never returns (a wedged vrserver) stays counted. Until it returns, OpenVR reads and `launch_app` fail at once, and `vr_status` warns `OpenVR client wedged (an OpenVR call never returned); restart the MCP`.
- `record_audio` and `audio_level` hear the whole default output, other apps included. WAV files stay in the MCP's private runtime dir until logout.

### Session watcher

While the MCP manages a session, a watcher checks SteamVR every second.

- If SteamVR exits on its own (it crashed or you quit it), the session ends once no SteamVR process has run for 3 s, or vrserver has been gone for 20 s. SteamVR's own restart, an update or vrmonitor relaunching vrserver stays within that. The MCP closes its headset and its ALVR dashboard, and `vr_status` warns `SteamVR exited without vr_stop (it crashed or was quit), so the session ended; run vr_start`, or `the last SteamVR restart failed and the headset is not streaming; run vr_start` when a restart failed. The lease is no longer renewed and expires; `vr_release` frees it at once.
- If the headset stops streaming for 30 s, the watchdog restarts SteamVR with a fresh headset. It re-arms only after a stream has held for 60 s, and fires at most 3 times per `vr_start`, so a stream that keeps dropping cannot loop restarts and kill the game under test. After that `vr_status` warns `the headset went quiet again after 3 watchdog SteamVR restarts, which the watchdog no longer repeats; run vr_start`. When the headset stays quiet after a watchdog restart it warns `the watchdog restarted SteamVR but the headset stayed quiet; run vr_start`.

`vr_status` joins every warning that applies into one `warning` string, separated by `; `.

### Observed behavior

These come from testing, and no test checks them. They may differ by game, GPU or SteamVR version.

- With a static head pose, SteamVR goes into standby after about 5 minutes.
- With Index controller emulation, one game's aim ray pointed about 30 to 35 degrees above the pose `set_controller_pose` set. SteamVR derives the aim and grip poses from the Index raw pose through fixed rotations in the Index render model, so expect a tilt like this, and check its size in each game.
- An app's pause menu could be reached only through the system button, which opens SteamVR's dashboard, followed by `close_dashboard`.
- During performance runs on an integrated GPU, a GPU stall of about 70 ms came every 20 s or so. The cause is unknown. The MCP's ALVR defaults set no keyframe or IDR interval; a `headset` screenshot does ask ALVR for one IDR frame, and decodes it together with the frames that follow.

## What it changes on your system

The first `vr_start` saves a snapshot to `$XDG_STATE_HOME/SteamVR-MCP/restore.json` (usually `~/.local/state/SteamVR-MCP/`). The snapshot holds:

- the absolute paths of `steamvr.vrsettings`, the ALVR session file, `vrcompositor` and the ALVR driver, as found at that start;
- the `external_drivers` list from `openvrpaths.vrpath`;
- the whole `steamvr.vrsettings`;
- the ALVR session file and the `session_old.json` and `session_invalid.json` next to it, which ALVR overwrites, plus the names of files that ALVR creates in that directory while the MCP writes its session;
- the `vrcompositor` wrapper symlink an older ALVR left, if SteamVR had one.

Later starts keep the snapshot, with one exception. When a cold `vr_start` finds that SteamVR carries none of the MCP's changes (`forcedDriver` is not `alvr_server`, the recorded driver is not registered, and no restore is under way), it takes that to mean you put SteamVR back by hand, and retakes only the driver list and `steamvr.vrsettings`, keeps the session files and the compositor entry as first recorded, and notes `restore.json predated a SteamVR setup without the MCP's changes; took a fresh snapshot`.

`vr_start` refuses before it changes anything when:

- `restore.json` does not parse: `<file> does not parse; inspect it, put back by hand what you need from it, then delete it`;
- `restore.json` is partial (see [vr_restore](#vr_restore));
- a recorded path differs from what the config gives now, for example after changing `streamer_dir`, `steam_dir` or `steamvr_dir`: `restore.json has <what> at <then>, but the config now gives <now>; run vr_restore first (it restores to the recorded paths)`.

A cold `vr_start` then:

- registers the pinned ALVR driver in `openvrpaths.vrpath`, dropping any other driver whose path contains `alvr`. The other drivers keep their order, and ALVR's goes last;
- sets `forcedDriver` to `alvr_server` in `steamvr.vrsettings`;
- clears `blocked_by_safe_mode` for the ALVR driver, and SteamVR's safe-mode blocks on other drivers (SteamVR 2.18.4 otherwise stops the launch on a desktop dialog; `vr_status` lists the ones present as `blocked_drivers`);
- unwraps the `vrcompositor` wrapper an older ALVR left, if there is one;
- writes the MCP's ALVR settings, with the headset as a trusted `127.0.0.1` client;
- starts Steam with `steam.env` if it was not running.

When SteamVR already runs with ALVR, `vr_start` adopts it. It still takes the snapshot, with the refusals above, but it changes no files; the settings and the client go to ALVR through its API. If ALVR's session file shows the previous client still connected, ALVR is stuck on a dead client and would not answer, so `vr_start` restarts SteamVR first.

The MCP writes `restore.json`, `steamvr.vrsettings`, `openvrpaths.vrpath` and the files `vr_restore` puts back through a temp file, fsync and rename, so a crash leaves the old file or the new one, never a torn one. The new file keeps the old file's permissions. A file without the owner write bit is refused: some users make `openvrpaths.vrpath` read-only so SteamVR cannot reset the OpenXR runtime, and the MCP will not silently undo that. The error reads `<file> is read-only, perhaps to keep SteamVR from changing it; chmod u+w it, run this again, then make it read-only again`.

It also keeps its own files:

- `$XDG_STATE_HOME/SteamVR-MCP/`: `restore.json`, the OpenVR action manifest (`openvr/`), compositor screenshots (`screenshots/`) and Proton launch logs (`runs/`).
- `$XDG_RUNTIME_DIR/SteamVR-MCP/`: the lease, the `alvr_dashboard` link and audio recordings (`audio/`). This is a tmpfs that goes away at logout.

### vr_stop

`vr_stop` ends a game that `launch_app via proton` started and disconnects the headset. It then shuts SteamVR down through ALVR, and if SteamVR is still up after 30 s it sends SIGTERM and then SIGKILL. Each signal reaches `vrmonitor` last, after the other SteamVR processes are gone or 5 s passed, because killing `vrmonitor` first makes SteamVR's safe mode block the ALVR driver. Once every SteamVR process is gone it clears the safe-mode blocks SteamVR may have set and releases the lease. It stops only a SteamVR that runs ALVR; close one you started without ALVR yourself. A SteamVR that a failed `vr_start` launched counts as ALVR's while that start's dashboard runs and ALVR's driver is blocked, so `vr_stop` can clear the block.

If `vr_start` started Steam, `vr_stop` then asks Steam to exit. After an online session Steam can ignore that for 40 s or more, so if the Steam client that `vr_start` started still runs after 60 s, `vr_stop` sends SIGTERM to that one process and waits up to 15 s more. If that client still runs after that, the next `vr_stop` tries again. These promises hold only when the MCP identified the Steam client it started. Without that record, `vr_stop` forwards one `-shutdown` at that stop, waits, sends no signal and keeps nothing for a later stop. Once the recorded client is gone (Steam may restart its client after a self-update), the Steam running then is left alone. It never sends SIGKILL to Steam and never signals any other Steam process. It never asks a Steam you opened yourself to exit when it recorded the client it started; without that record, `vr_stop` asks whatever Steam is running then to exit, once, and never signals it. In both cases `-shutdown` is forwarded only if a process named `steam` still runs.

The result says what happened. When `vr_start` started Steam or a client record was kept, `closed_steam` is true when no Steam process is left (also when Steam had already exited), and `remaining_steam` lists the names of the Steam processes still alive at the end; otherwise `vr_stop` does not touch Steam and they stay `false` and empty. `signalled_steam` says whether the SIGTERM went out. `steam_note` says why Steam may still run: the forwarded `-shutdown` failed (with the error), the Steam client the MCP started had exited and was left alone, or Steam was left running with no record, so no later `vr_stop` will close it.

Known limit: if a failed `vr_start` leaves its dashboard and ALVR's safe-mode block, and you then close and reopen SteamVR yourself while the block is still in `steamvr.vrsettings`, `vr_stop` counts that SteamVR as the MCP's.

Tool calls run concurrently. A `vr_stop` that arrives while a `vr_start` or a SteamVR restart runs waits for it to give up. A start that already launched SteamVR first waits up to 20 s for Steam to hand the launch over, so `vr_stop` finds that SteamVR and stops it too. Meanwhile `vr_start` answers `not ready: vr_stop is running`.

It does not put your setup back. After `vr_stop`, SteamVR still has the ALVR driver registered and forced, and ALVR keeps the MCP's settings, so a real headset will not work until you run `vr_restore`.

### vr_restore

`vr_restore` writes back everything in `restore.json` to the paths recorded there: the driver list, the whole `steamvr.vrsettings`, the ALVR session files and the `vrcompositor` wrapper. It removes the files ALVR created. Anything you changed in SteamVR's settings, the driver list or the ALVR session between the first `vr_start` and `vr_restore` is lost. It returns `restored` and `notes`. When everything is back it deletes `restore.json`, releases the lease and reports `restored: true`, and the next `vr_start` takes a fresh snapshot.

It reports `restored: false` and keeps both `restore.json` and the lease when:

- `openvrpaths.vrpath` cannot be read or written. The other steps still run, and a note says what failed. Fix the file and run `vr_restore` again.
- A recorded file's directory is gone, for example a SteamVR library or the streamer on a drive that is not mounted. It puts back the rest, notes `left <file> unrestored: its directory is gone`, and rewrites `restore.json` as partial, holding only the skipped entries. `vr_start` refuses while it exists. Mount the drive and run `vr_restore` again, which retries only those entries. If you removed the install on purpose, run `vr_restore {forget_missing: true}` instead: it restores what it can, drops each entry whose directory is gone with a note, deletes `restore.json` and reports `restored: true`.
- A recorded file's directory cannot be checked, for example after a permission error or on a dead network mount. The entry is kept, `forget_missing` never drops it, and the note names the directory. Restore access to it and run `vr_restore` again.
- `restore.json` comes from an older SteamVR-MCP that recorded no paths. `vr_restore` detects the paths, stores them in `restore.json` and names them in the note. `forget_missing` stays off for that file: if the paths are right, mount or reinstall what is gone and run `vr_restore` again; if they are wrong, correct them in `restore.json` by hand.

It refuses while any SteamVR process runs (`vrserver`, `vrcompositor`, `vrmonitor`, `vrdashboard`, `vrwebhelper`, `vrstartup`, `steamtours`), because they can still rewrite `steamvr.vrsettings`; the message names them. It also refuses while a `vr_start`, `vr_stop` or SteamVR restart runs, or while this MCP runs a session. Run `vr_stop` first.

`restore.json` is marked as restoring before anything changes, and every step can run again, so a restore that crashed or failed halfway finishes on the next `vr_restore`. Because the state is a file, a crashed MCP can still restore.

## Sharing SteamVR between agents

Several MCP processes (several agent sessions) on one machine share one SteamVR through a lease in `$XDG_RUNTIME_DIR/SteamVR-MCP/`.

- Any tool that changes something takes the lease by itself, for 30 minutes unless `vr_acquire` sets `ttl_s` (600 to 14400 s), and renews it once half is left. `vr_acquire` is only needed to choose the name or the ttl. The MCP forgets that name and ttl when the lease is released, so the next lease is under its own name again.
- While another live MCP holds the lease, the mutating tools disappear from the list and answer `not ready: SteamVR is leased to ...`.
- Read-only tools stay open: `vr_status`, `get_logs`, `get_haptics`, `get_tracked_state`, `screenshot`, `read_app_log`, `wait_for_log`, `get_frame_timing`, `record_audio`, `audio_level`, `vr_acquire` and `vr_release`. An observer has no headset of its own, so for it `get_haptics` and `get_frame_timing` are unavailable and `screenshot` takes only the `compositor` source.
- The holder's MCP keeps renewing the lease while it manages a session and SteamVR runs. The lease ends at `vr_release`, at a `vr_stop` that leaves no SteamVR process behind, at a `vr_restore` that reports `restored: true`, or when the holder exits. A lease whose MCP died is free at once. One whose MCP no longer manages a session, a lost session included, can be taken over once it expires.
- `vr_release` refuses while this MCP runs a session (`run vr_stop first`) or while a `vr_start`, `vr_stop` or SteamVR restart runs. It works after a lost session. Since `vr_stop` releases the lease itself, a `vr_release` after a clean `vr_stop` returns `released: false`.
- `vr_status` shows the owner and whether it is this MCP.

## Logging

The server logs through `env_logger` to stderr, and by default only errors are shown. Set `RUST_LOG` in the server's environment for more:

```sh
claude mcp add --scope user -e RUST_LOG=info SteamVR-MCP -- /path/to/target/release/SteamVR-MCP
```

`warn` adds recoverable problems (skipped events, a reader left running), `info` adds ALVR's client messages, and `debug` is verbose. A one-line note per MCP client connection is always printed.

The logs that matter for a test are available without this: `get_logs` returns ALVR's events and SteamVR's own logs, and `launch_app via proton` writes the launcher's output to its `run_log`.

## Troubleshooting

Start with `vr_status`: each failed check names its reason and the fix.

| Check | Fails when | Fix |
| --- | --- | --- |
| `config` | config.toml does not parse | Fix the file; it is read again at the next `vr_status` or `vr_start` while nothing runs. |
| `steam_native` | No Steam, or Flatpak Steam | Install native Steam or set `steam_dir`. |
| `steamvr_installed` | No `bin/linux64/vrserver` | Install SteamVR (app 250820) and run it once, or set `steamvr_dir`. |
| `alvr_streamer` | No ALVR driver, or another ALVR version | Install the 21.0.0-dev14 streamer and set `streamer_dir`. |
| `alvr_dashboard_absent` | ALVR's dashboard or another SteamVR-MCP runs | Close it. |
| `audio_input` | No default audio input | Enable one. |
| `gpu_direct_mode` | Only NVIDIA, or no AMD or Intel GPU | Run SteamVR on AMD or Intel; on hybrids set `steam.env`. |
| `h264_encoder` | No VA-API H.264 encoder on that GPU | Install VA-API drivers with H.264 encoding. |
| `openvr_client` | SteamVR's `libopenvr_api.so` does not load | Reinstall SteamVR. |
| `screenshot_decoder` | OpenH264 fails to start | Rebuild. |
| `audio_capture` | No PipeWire | Run PipeWire for the session. |

Common errors at run time:

- `Steam is running without steam.env (...)`: close Steam, and `vr_start` starts it with `steam.env`.
- `SteamVR is running without the ALVR driver; close SteamVR and retry` or `SteamVR is running without ALVR; close it yourself`: close the SteamVR you started by hand, then call `vr_start`.
- `failed: SteamVR's safe mode blocked the ALVR driver (driver_alvr_server in steamvr.vrsettings), so ALVR will not load`: SteamVR's safe mode blocked ALVR's driver during the launch, so ALVR would never answer. Run `vr_stop`, which clears the block once SteamVR is gone, then `vr_start`. A SteamVR restart fails the same way and is not retried.
- `ALVR's API does not answer on port N (deadline 10 s); run vr_stop`: SteamVR runs with ALVR's driver, but ALVR is still starting or hangs. Run `vr_stop`, then `vr_start`.
- `not ours: the ALVR on port N does not belong to this user's SteamVR`: another process listens on ALVR's port. Close it.
- `<METHOD> <route>: ALVR accepted the connection but did not answer within N s`: ALVR hangs. Run `vr_stop`.
- `port N answers, but no SteamVR of this user runs`: another user's SteamVR or ALVR holds the port. Close it.
- `failed: Steam is waiting for a dialog on the desktop (...)`: Steam stopped a launch on a dialog (`ShowInterstitials` or `ShowLaunchOption`, for example "VR Theater Screen" for an app Steam treats as non-VR, or `SynchronizingCloud` when a Steam Cloud sync fails). Dismiss it on the desktop and retry.
- `Steam did not take ...`, within 20 s of `vr_start` or `launch_app`: a second Steam client (Flatpak Steam, for example) took over Steam's runtime launch service. Close the other Steam, then restart this one. Two Steam clients under one user also cannot both run a game: the game's steamclient dies about 28 s in with "fatal stalled cross-thread pipe".
- `failed: vrserver exited`: SteamVR crashed during the wait. `get_logs` with `source: "vrserver"` shows why. On a hybrid GPU, check that Steam and the game use the SteamVR GPU.
- `failed: vrcompositor could not capture the scene app's textures (...)`: the scene app does not support compositor screenshots, which is common for OpenXR and Proton apps. Use `source: "headset"`.
- `SteamVR wrote no screenshot to ... (deadline 10 s); vrcompositor logged no screenshot request; vrserver may not see this directory`: vrserver runs in the Steam Linux Runtime container and cannot write that directory. Keep `$XDG_STATE_HOME` under your home directory. The variant `vrcompositor logged the request but wrote no file` means the compositor took the request and failed silently.
- `not ready: SteamVR is leased to ...`: another agent holds SteamVR. Wait for it to call `vr_stop` or `vr_release`, or for its lease to expire once it stops managing a session.
- `not ready: vr_stop is running` or `not ready: vr_start or vr_stop is running`: wait for the other call to return.
- `set_alvr_settings changes only the session vr_start set up; run vr_start first`: call `vr_start` first.
- `OpenVR client wedged (an OpenVR call never returned); restart the MCP`: vrserver stopped answering mid-call. Restart the MCP; `vr_stop` first if SteamVR hangs.
- `vr_restore` refuses with `SteamVR processes still run (...)`, `a vr_start session runs` or `vr_start is running`: call `vr_stop` first.
- `vr_start` refuses over `restore.json` (it does not parse, is partial, or records other paths), or a file `is read-only`: see [What it changes on your system](#what-it-changes-on-your-system) and [vr_restore](#vr_restore).
- `vr_status` warns that the session ended, a restart failed, or the watchdog gave up or the headset stayed quiet: see [Session watcher](#session-watcher). Run `vr_start`.

## Limitations

- Restarts are slow. ALVR's TCP stream socket spins at end of stream on loopback (`sockets/src/stream_socket/tcp.rs:137-147`), and the server's receive thread never finishes after the headset drops. So a headset reconnect needs a SteamVR restart, a settings change that needs a restart takes about 80 s, and `vr_stop` usually ends SteamVR with signals. If the MCP exits while the headset streams, vrserver keeps one CPU core busy until SteamVR stops.
- The virtual headset lives inside the MCP process. When the MCP or the agent session running it restarts, a running OpenXR app drops from `FOCUSED` to `SYNCHRONIZED`. The next `vr_start` adopts SteamVR and starts a new headset.
- ALVR 21 registers the left hand tracker's buttons from the right-hand map (`Controller.cpp:112-117`).
- ALVR 21 has one flag per controller for connected and tracked, so a disconnected controller is also untracked.
- Audio tools capture the whole default output, not one game.
- Compositor screenshots fail for some OpenXR and Proton scene apps; the `headset` source works for all of them.
- With an adopted SteamVR, a button held by an earlier stream can stay pressed even after `vr_start` resets the rig: SteamVR's driver keeps the last value, and ALVR's new stream drops a release it never saw pressed. Only `vr_stop` clears it.
- The watchdog restarts SteamVR at most 3 times per `vr_start`. After that, a headset that keeps dropping needs a new `vr_start`.
- Not covered: Monado, WiVRn, an OpenXR API layer, Windows, Flatpak Steam, headless or CI use, Steam Frame controllers, desktop or window capture, eye, face and body tracking, and more than one virtual headset.

## Development

CI runs clippy and the tests that need no Steam or desktop. To run the same checks locally: `cargo clippy --all-targets -- -D warnings && cargo test --lib --test config_reload --test setup_roundtrip`.

### Bumping the ALVR pin

Change `rev` for every `alvr_*` dependency in `Cargo.toml`, install the matching streamer, and run `cargo test`. Then run the live tests.

### Live tests

The live tests in `tests/live.rs` drive the real Steam, SteamVR and ALVR, so they need a desktop session, a working config and no other agent using SteamVR:

```sh
VR_TEST_GAME_ID=<app id> VR_TEST_REQUIRE_ALL=1 cargo test --test live -- --ignored --test-threads=1
```

`--test-threads=1` runs the tests one at a time in name order, so `live_zz_stop_and_restore` runs last. `VR_TEST_GAME_ID` is the Steam app id or non-Steam shortcut game id of a Windows VR game; the menu tests were written against Beat Saber.

A live test that cannot run on the machine as it is prints `skipped (N live tests so far): <why>` to stderr and passes, and `live_zz_stop_and_restore` prints `live run: N live tests skipped`. With `VR_TEST_REQUIRE_ALL=1` a skip fails the test instead, so a run that exercised nothing is not green. One exception: `live_start_streams_and_stop_cleans` needs Steam closed and `live_start_streams_and_stop_cleans_with_steam_running` needs it running, so every run skips one of them, and that skip never fails.

The streaming tests share one SteamVR and leave it running for the next test. The tests that need SteamVR closed (`live_prepare_restore_round_trip` and both `live_start_streams` tests) stop it and restore the setup first, and `live_zz_stop_and_restore` does the same at the end. After running a single live test, run `live_zz_stop_and_restore` too, or call `vr_stop` and `vr_restore`.

The `xr_probe` example is a native OpenXR app that the live tests use to check inputs and haptics. The tests build it themselves with `cargo build --example xr_probe` (debug) and run it from `$CARGO_TARGET_DIR/debug/examples/`, or `target/debug/examples/` when that is unset.

### Manual checks through the MCP

`cargo test` does not cover these. Run them through the MCP from a desktop session while no other agent uses SteamVR. Each line gives the call and what passing looks like.

1. Clocks: `vr_status` shows `now_t_s`, `now_at` and `t0_at`. After `vr_start`, a `get_logs` vrserver line and an ALVR line from the same second have `t_s` within 1 s. A `get_haptics` event fired by `xr_probe` has `t_s` within 0.2 s of the probe's own log line.
2. Cold start: a cold `vr_start`, and a `set_alvr_settings` that restarts SteamVR, never fail with `failed: vrserver exited`.
3. Lease: start two MCP sessions. Session A calls `set_head_pose`. Session B's tool list drops the mutating tools within 2 s, `vr_status.state.owner.mine` is false in B, and B's `vr_start` answers `not ready: SteamVR is leased to mcp-<A pid>`. A runs `vr_stop`, which releases the lease, and B's mutating tools come back within 2 s. Kill A with SIGKILL while it holds the lease, and B can claim at once. While A runs a session and calls only read-only tools for over 30 minutes, A's lease stays renewed. While A restarts SteamVR, B's `get_tracked_state` fails or answers within 5 s and B keeps working afterwards. (In Claude Code every tool stays listed, so check B's answers instead of its tool list.)
4. Safe mode: `vr_start`, then kill vrmonitor with SIGKILL. Call `vr_status` until `blocked_drivers` lists the blocked drivers. `vr_stop`, and `blocked_drivers` is back to the snapshot's state. `vr_start` streams.
5. App logs: add `[app_logs]` for a Unity Proton game and `launch_app` it. `read_app_log` returns lines from the new `Player.log` only. `wait_for_log {pattern: "<a line the game prints at its menu>", fail_pattern: "Exception"}` answers `match` within the game's load time.
6. Proximity: the headset reports worn at each stream start (SteamVR 2.18 needs that to focus OpenXR apps). With an OpenXR app running, `set_proximity {on_head: false}` makes `get_tracked_state` show the HMD's `head_proximity: false`, and the app reports focus loss or idle. `on_head: true` reverses it. Call `get_tracked_state` right after `set_proximity`, both after `vr_start` and after `launch_app`: it shows the new value.
7. Frame timing: with a game in VR, `get_frame_timing {window_s: 5}` returns a non-null `compositor` with `fps` near the stream fps, a non-null `alvr`, and empty `notes`.
8. Compositor screenshot: `screenshot {source: "compositor", eye: "left"}` returns a PNG at the app's eye render size, sharper than `source: "headset"`.
9. Proton launch: `launch_app {app_id, via: "proton", exe: "<game exe>", args: ["<arg>"], env: {"PROTON_LOG": "1"}}` reaches a running scene without a Steam dialog. The game sees the arg, and `run_log` has Proton's output. `stop_app` returns, and afterwards `/proc` has no process with that `STEAM_COMPAT_DATA_PATH` and app id. Launch again, then `vr_stop`: its `stopped_app` names the game, and a second Proton launch of the same app reaches its scene. Run it once with Proton Experimental and once with a custom compatibility tool.
10. Audio: play a test tone or the game's menu music. `audio_level {window_s: 1}` reads above -60 dBFS on both channels. Turn the head 90 degrees with `move_head {yaw: 90}`, and the L/R balance of a spatialized sound shifts. `record_audio {seconds: 3}` writes a WAV that plays back the same sound. Repeat on a mono and a 5.1 default sink.

## Third-party files

- `vendor/openvr/openvr_capi.h` is Valve's OpenVR SDK 2.15.6 C header, as ALVR's `openvr` submodule pins it, under the BSD 3-clause license in `vendor/openvr/LICENSE`.
- `examples/xr_probe.rs` and its shaders in `examples/xr_probe/` are adapted from the Vulkan example of [openxrs](https://github.com/Ralith/openxrs) 0.22, by Benjamin Saunders and the openxrs contributors, Copyright (c) 2020 The openxrs Developers, used under MIT OR Apache-2.0. The MIT permission notice that covers it is the same text as this repository's [LICENSE-MIT](LICENSE-MIT); the changes are listed at the top of `examples/xr_probe.rs`.
- `tests/fixtures/red_blue.h264` is a synthetic one-frame x264 encode; the command that makes it is next to the test in `src/headset.rs`.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
