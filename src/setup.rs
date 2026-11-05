use crate::config::{Config, Paths};
use alvr_common::anyhow::{Context, Result, bail};
use alvr_packets::{ClientConnectionsAction, PathValuePair, parse_path};
use alvr_server_io::ServerSessionManager;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{self, Write},
    net::{IpAddr, Ipv4Addr},
    path::{Path, PathBuf},
};

/// (section, key) pairs in steamvr.vrsettings that prepare changes and restore puts back.
pub const VRSETTINGS_KEYS: [(&str, &str); 2] =
    [("steamvr", "forcedDriver"), ("driver_alvr_server", "blocked_by_safe_mode")];

const NO_STREAMER: &str = "ALVR streamer not found; set streamer_dir in config.toml";
const NO_STEAM: &str = "Steam not found; set steam_dir in config.toml";
const NO_STEAMVR: &str = "SteamVR not found; set steamvr_dir in config.toml";
/// What ALVR overwrites next to session.json when it loads one it cannot use (server_io/src/lib.rs load_session).
const SESSION_SIDE_FILES: [&str; 2] = ["session_old.json", "session_invalid.json"];

#[derive(Serialize, Deserialize, Default, Debug, PartialEq)]
#[serde(default)]
pub struct RestoreState {
    /// Where the files were at snapshot time; restore writes there. None in an old restore.json.
    pub paths: Option<SnapshotPaths>,
    /// Set once a restore began, so a half-restored setup never counts as stale.
    pub restoring: bool,
    /// Set when a restore put back all it could: only what it skipped is left, the driver list is back already.
    pub partial: bool,
    /// SESSION_SIDE_FILES as they were. Empty in an old restore.json.
    pub session_backups: BTreeMap<PathBuf, FileState>,
    /// The original `external_drivers` value, written back verbatim (null included).
    pub external_drivers: Value,
    /// "section/key" -> original (None = absent). Only an old restore.json has it.
    pub vrsettings: BTreeMap<String, Option<Value>>,
    /// The whole original steamvr.vrsettings. None in an old restore.json.
    pub vrsettings_file: Option<FileState>,
    pub session: Option<FileState>,
    pub created_files: Vec<PathBuf>,
    /// Some only if it was wrapped
    pub vrcompositor: Option<VrcompositorWrap>,
    /// Set when `paths` were detected for an old restore.json, not recorded at snapshot time: they may be wrong (a
    /// stale config.toml override), so forget_missing stays off.
    pub paths_detected: bool,
    /// Directories a partial restore could not check (EACCES, EIO, a dead mount): forget_missing never drops their entries.
    pub unreachable: BTreeSet<PathBuf>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub enum FileState {
    Absent,
    Contents(String),
}

/// Absolute paths, as Paths gave them at snapshot time.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct SnapshotPaths {
    pub vrsettings: PathBuf,
    pub session: PathBuf,
    /// None only when an old restore.json had no wrap to put back and SteamVR was not found.
    pub vrcompositor: Option<PathBuf>,
    /// None only when an old restore.json was restored without the streamer found.
    pub driver_dir: Option<PathBuf>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct VrcompositorWrap {
    pub link_target: PathBuf,
    pub real_path: PathBuf,
}

#[derive(Debug)]
pub struct PrepareReport {
    pub snapshot_written: bool,
    pub unblocked_safe_mode: bool,
    pub unwrapped_vrcompositor: bool,
}

/// Built-in defaults, then `cfg.alvr_settings` in key order. Paths start at the session JSON root.
pub fn session_values(cfg: &Config) -> Result<Vec<PathValuePair>> {
    let session = serde_json::to_value(alvr_session::SessionConfig::default())?;
    let template = &session["session_settings"]["headset"]["controllers"]["content"]["button_mappings"];
    let defaults = [
        ("session_settings.video.preferred_codec.variant", json!("H264")),
        ("session_settings.video.encoder_config.h264_profile.variant", json!("Baseline")),
        ("session_settings.video.foveated_encoding.enabled", json!(false)),
        ("session_settings.video.transcoding_view_resolution.variant", json!("Scale")),
        ("session_settings.video.transcoding_view_resolution.Scale", json!(1.0)),
        ("session_settings.video.emulated_headset_view_resolution.variant", json!("Scale")),
        ("session_settings.video.emulated_headset_view_resolution.Scale", json!(1.0)),
        ("session_settings.video.preferred_fps", json!(72.0)),
        ("session_settings.audio.game_audio.enabled", json!(false)),
        ("session_settings.audio.microphone.enabled", json!(false)),
        ("session_settings.headset.recentering_mode.variant", json!("Stage")),
        ("session_settings.headset.controllers.enabled", json!(true)),
        ("session_settings.headset.controllers.content.tracked", json!(true)),
        ("session_settings.headset.controllers.content.emulation_mode.variant", json!("ValveIndex")),
        ("session_settings.headset.controllers.content.hand_skeleton.enabled", json!(true)),
        ("session_settings.headset.controllers.content.hand_skeleton.content.steamvr_input_2_0", json!(false)),
        ("session_settings.headset.controllers.content.hand_tracking_interaction.enabled", json!(true)),
        ("session_settings.connection.client_discovery.enabled", json!(false)),
        ("session_settings.connection.allow_remote_dashboard", json!(false)),
        ("session_settings.headset.controllers.content.button_mappings", index_button_mappings(&template)),
    ];
    defaults
        .into_iter()
        .chain(cfg.alvr_settings.iter().map(|(k, v)| (k.as_str(), v.clone())))
        .map(|(key, value)| Ok(PathValuePair { path: parse_path(key)?, value }))
        .collect()
}

/// ALVR's manual `button_mappings` passing every Index input straight through, so the ones its
/// automatic mapping has no source for (trackpad, left A/B; server_core/src/input_mapping.rs:243-590)
/// reach SteamVR too. `template` is the default session's `button_mappings` value.
pub fn index_button_mappings(template: &Value) -> Value {
    let index = &alvr_common::CONTROLLER_PROFILE_INFO[&*alvr_common::INDEX_CONTROLLER_PROFILE_ID].button_set;
    let mut paths: Vec<_> = index.iter().map(|id| alvr_common::BUTTON_INFO[id].path).collect();
    paths.sort();
    let value = &template["content"]["value"];
    let pairs = paths
        .into_iter()
        .map(|path| {
            let mut target = value["element"].clone();
            target["destination"] = json!(path);
            let mut targets = value.clone();
            targets["content"] = json!([target]);
            json!([path, targets])
        })
        .collect::<Vec<_>>();
    let mut mappings = template.clone();
    mappings["set"] = json!(true);
    mappings["content"]["content"] = json!(pairs);
    mappings
}

fn read_json(path: &Path) -> Result<Value> {
    serde_json::from_str(&fs::read_to_string(path).with_context(|| path.display().to_string())?)
        .with_context(|| path.display().to_string())
}

fn write_json(path: &Path, value: &Value) -> Result<()> {
    write_atomic(path, serde_json::to_string_pretty(value)?.as_bytes())
}

/// Temp file in the same dir, fsync, rename (as lease.rs does): a crash leaves the old file or the new one, never a torn one.
/// The new file keeps the old one's mode; a read-only one is refused, as a plain write would fail on it.
fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    // a symlinked file keeps its link: the target is replaced
    let path = fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
    let perms = fs::metadata(&path).ok().map(|m| m.permissions());
    if perms.as_ref().is_some_and(|p| p.mode() & 0o200 == 0) {
        // users chmod 444 openvrpaths.vrpath so SteamVR cannot reset `runtime`
        bail!("{} is read-only, perhaps to keep SteamVR from changing it; chmod u+w it, run this again, then make it read-only again", path.display());
    }
    let name = path.file_name().with_context(|| path.display().to_string())?.to_string_lossy();
    let tmp = path.with_file_name(format!("{name}.tmp"));
    let mut file = fs::File::create(&tmp).with_context(|| tmp.display().to_string())?;
    if let Some(perms) = perms {
        file.set_permissions(perms).with_context(|| tmp.display().to_string())?;
    }
    file.write_all(contents).and_then(|()| file.sync_all()).with_context(|| tmp.display().to_string())?;
    fs::rename(&tmp, &path).with_context(|| path.display().to_string())?;
    // so the rename itself survives a power loss
    fs::File::open(path.parent().unwrap_or(Path::new("."))).and_then(|d| d.sync_all()).ok();
    Ok(())
}

/// Writes `state` back to `path`: its contents, or no file.
fn put_file(path: &Path, state: &FileState) -> Result<()> {
    match state {
        FileState::Contents(text) => write_atomic(path, text.as_bytes()),
        FileState::Absent => remove_file(path),
    }
}

/// One restore run: whether it may forget, its notes, and the directories it could not check.
#[derive(Default)]
struct Pass {
    forget: bool,
    notes: Vec<String>,
    unreachable: BTreeSet<PathBuf>,
}

impl Pass {
    /// None when `path`'s directory is there, so its entry can be put back. Otherwise Some, with a note: Some(true)
    /// drops the entry, only with `forget` and the directory known gone (its drive not mounted, or the install moved
    /// or was removed); Some(false) keeps it for a retry.
    fn skipped(&mut self, path: &Path) -> Option<bool> {
        let dir = path.parent()?;
        let (dropped, why) = match dir.try_exists() {
            Ok(true) => return None,
            Ok(false) => (self.forget, "its directory is gone".to_owned()),
            // EACCES, EIO, a dead FUSE or NFS mount: maybe still there, so never forgotten
            Err(e) => {
                self.unreachable.insert(dir.to_owned());
                (false, format!("cannot check its directory: {e}"))
            }
        };
        let what = if dropped { "forgot" } else { "left unrestored" };
        self.notes.push(format!("{what} {}: {why}", path.display()));
        Some(dropped)
    }

    /// put_file, unless `skipped`. True when done with the entry (put back, or dropped); false keeps it for a retry.
    fn put_back(&mut self, path: &Path, state: &FileState) -> Result<bool> {
        if let Some(dropped) = self.skipped(path) {
            return Ok(dropped);
        }
        put_file(path, state)?;
        Ok(true)
    }
}

/// openvrpaths.vrpath, where ALVR's server_io keeps it on Linux.
fn openvr_paths_file() -> Result<PathBuf> {
    Ok(dirs::config_dir().context("no config dir")?.join("openvr/openvrpaths.vrpath"))
}

/// openvrpaths.vrpath parsed, as ALVR's server_io loads it. Saved here with write_json: ALVR's fs::write can tear it.
fn load_openvr_paths() -> Result<(PathBuf, Value)> {
    let file = openvr_paths_file()?;
    let text = fs::read_to_string(&file).with_context(|| file.display().to_string())?;
    // shortcut: a UTF-8 BOM is skipped, UTF-16 (which ALVR decodes) is not; upgrade if SteamVR on Linux ever writes it
    let value = serde_json::from_str(text.trim_start_matches('\u{feff}')).with_context(|| file.display().to_string())?;
    Ok((file, value))
}

fn external_drivers() -> Result<Value> {
    Ok(load_openvr_paths()?.1.get("external_drivers").cloned().unwrap_or(Value::Null))
}

/// Unregisters every other ALVR driver and registers `driver_dir`, as the dashboard's launch_steamvr does.
fn register_driver(driver_dir: &Path) -> Result<()> {
    let (file, mut json) = load_openvr_paths()?;
    let drivers = json.get_mut("external_drivers").with_context(|| format!("{} has no external_drivers", file.display()))?;
    let mut list: Vec<_> = alvr_server_io::from_openvr_paths(drivers)
        .into_iter()
        .filter(|p| p != driver_dir && !p.to_string_lossy().to_lowercase().contains("alvr"))
        .collect();
    list.push(driver_dir.to_owned());
    *drivers = alvr_server_io::to_openvr_paths(&list);
    write_json(&file, &json)
}

/// Writes `external_drivers` back verbatim, keeping the file's other fields.
fn put_external_drivers(external_drivers: &Value) -> Result<()> {
    let (file, mut json) = load_openvr_paths()?;
    json.as_object_mut()
        .with_context(|| format!("{} is not an object", file.display()))?
        .insert("external_drivers".into(), external_drivers.clone());
    write_json(&file, &json)
}

fn file_state(path: &Path) -> Result<FileState> {
    match path.exists() {
        true => Ok(FileState::Contents(fs::read_to_string(path).with_context(|| path.display().to_string())?)),
        false => Ok(FileState::Absent),
    }
}

fn abs(p: Option<PathBuf>, missing: &'static str) -> Result<PathBuf> {
    Ok(std::path::absolute(p.context(missing)?)?)
}

/// The paths a snapshot records, from the current Paths.
fn current_paths(paths: &Paths) -> Result<SnapshotPaths> {
    Ok(SnapshotPaths {
        vrsettings: abs(paths.vrsettings_file(), NO_STEAM)?,
        session: abs(paths.session_file(), NO_STREAMER)?,
        vrcompositor: Some(abs(paths.vrcompositor(), NO_STEAMVR)?),
        driver_dir: Some(abs(paths.driver_dir(), NO_STREAMER)?),
    })
}

/// Where restore writes: the recorded paths, or for an old restore.json the current ones, of which only those a
/// restore step uses must be found.
fn restore_paths(paths: &Paths, state: &RestoreState) -> Result<SnapshotPaths> {
    if let Some(at) = &state.paths {
        return Ok(at.clone());
    }
    let opt = |p: Option<PathBuf>| p.map(std::path::absolute).transpose();
    Ok(SnapshotPaths {
        vrsettings: abs(paths.vrsettings_file(), NO_STEAM)?,
        session: abs(paths.session_file(), NO_STREAMER)?,
        vrcompositor: match state.vrcompositor {
            Some(_) => Some(abs(paths.vrcompositor(), NO_STEAMVR)?),
            None => opt(paths.vrcompositor())?,
        },
        driver_dir: opt(paths.driver_dir())?,
    })
}

/// Sets `section.key`, or removes it (and its section once empty) for None.
fn put(settings: &mut Value, section: &str, key: &str, value: Option<Value>) {
    match value {
        Some(v) => settings[section][key] = v,
        None => {
            let Some(obj) = settings.get_mut(section).and_then(Value::as_object_mut) else { return };
            obj.remove(key);
            if obj.is_empty() {
                settings.as_object_mut().unwrap().remove(section);
            }
        }
    }
}

/// Port of the ALVR dashboard's `unblock_alvr_driver_within_vrsettings`. True if it was blocked.
pub fn unblock_safe_mode(vrsettings: &Path) -> Result<bool> {
    let (section, key) = VRSETTINGS_KEYS[1];
    let mut settings = read_json(vrsettings)?;
    if settings[section][key] != true {
        return Ok(false);
    }
    put(&mut settings, section, key, Some(json!(false)));
    write_json(vrsettings, &settings)?;
    Ok(true)
}

/// Removes `blocked_by_safe_mode` from every `driver_*` section, and a section left empty. SteamVR
/// 2.18.4 stops a launch on a desktop dialog while any driver is blocked, whatever the snapshot held.
/// Returns the sorted names it unblocked.
pub fn clear_safe_mode_blocks(current: &mut Value) -> Vec<String> {
    let key = VRSETTINGS_KEYS[1].1;
    let mut names: Vec<String> = current
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(name, s)| name.starts_with("driver_") && s.get(key) == Some(&json!(true)))
        .map(|(name, _)| name.clone())
        .collect();
    names.sort();
    for name in &names {
        put(current, name, key, None);
    }
    names
}

/// `driver_*` sections SteamVR blocked in safe mode, sorted; empty when the file is unreadable.
pub fn blocked_drivers(vrsettings: &Path) -> Vec<String> {
    let Ok(Value::Object(sections)) = read_json(vrsettings) else { return vec![] };
    let mut names: Vec<_> = sections
        .iter()
        .filter(|(name, s)| name.starts_with("driver_") && s.get("blocked_by_safe_mode") == Some(&json!(true)))
        .map(|(name, _)| name.clone())
        .collect();
    names.sort();
    names
}

/// Whether SteamVR's safe mode blocked ALVR's driver; false when the file is unreadable.
pub fn alvr_blocked(vrsettings: &Path) -> bool {
    blocked_drivers(vrsettings).iter().any(|d| d == VRSETTINGS_KEYS[1].0)
}

/// After SteamVR has exited (never while a `VR_PROCESS_NAMES` process lives, which rewrites the file):
/// clears every safe-mode block and returns the driver names. `vr_restore` still puts the snapshot back.
pub fn undo_safe_mode_blocks(paths: &Paths) -> Result<Vec<String>> {
    // without a snapshot vr_restore could not put the blocks back
    if !paths.restore_file().exists() {
        return Ok(vec![]);
    }
    let path = paths.vrsettings_file().context("Steam not found; set steam_dir in config.toml")?;
    let mut settings = read_json(&path)?;
    let names = clear_safe_mode_blocks(&mut settings);
    if !names.is_empty() {
        write_json(&path, &settings)?;
    }
    Ok(names)
}

fn remove_file(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e).with_context(|| path.display().to_string()),
        _ => Ok(()),
    }
}

/// Port of the dashboard's `maybe_unwrap_vrcompositor_launcher` check: Some if vrcompositor is
/// ALVR's wrapper symlink and Valve's binary is found.
fn wrapped_vrcompositor(compositor: &Path) -> Result<Option<VrcompositorWrap>> {
    let Ok(link_target) = fs::read_link(compositor) else { return Ok(None) };
    let bin_dir = compositor.parent().context("vrcompositor has no parent")?;
    let candidates = [alvr_filesystem::original_vrcompositor_path(bin_dir), bin_dir.join("vrcompositor.real")];
    let Some(real_path) = candidates.into_iter().find(|p| p.is_file()) else {
        bail!("vrcompositor is a symlink but Valve's binary was not found next to it; verify SteamVR files in Steam");
    };
    Ok(Some(VrcompositorWrap { link_target, real_path }))
}

fn read_state(paths: &Paths) -> Result<RestoreState> {
    let file = paths.restore_file();
    let text = fs::read_to_string(&file).context("no restore.json: nothing to restore")?;
    serde_json::from_str(&text).with_context(|| {
        format!("{} does not parse; inspect it, put back by hand what you need from it, then delete it", file.display())
    })
}

fn write_state(paths: &Paths, state: &RestoreState) -> Result<()> {
    fs::create_dir_all(&paths.state_dir)?;
    write_atomic(&paths.restore_file(), serde_json::to_string_pretty(state)?.as_bytes()).context("writing restore.json")
}

fn list_dir(dir: &Path) -> BTreeSet<PathBuf> {
    fs::read_dir(dir).map(|d| d.flatten().map(|e| e.path()).collect()).unwrap_or_default()
}

/// The current files, as restore.json records them.
fn take_snapshot(paths: &Paths) -> Result<RestoreState> {
    let at = current_paths(paths)?;
    let compositor = at.vrcompositor.as_deref().context(NO_STEAMVR)?;
    let session_backups = SESSION_SIDE_FILES
        .iter()
        .map(|name| {
            let path = at.session.with_file_name(name);
            Ok((path.clone(), file_state(&path)?))
        })
        .collect::<Result<_>>()?;
    Ok(RestoreState {
        external_drivers: external_drivers()?,
        vrsettings: BTreeMap::new(),
        vrsettings_file: Some(FileState::Contents(fs::read_to_string(&at.vrsettings).with_context(|| at.vrsettings.display().to_string())?)),
        session: Some(file_state(&at.session)?),
        session_backups,
        created_files: vec![],
        vrcompositor: wrapped_vrcompositor(compositor)?,
        paths: Some(at),
        restoring: false,
        partial: false,
        paths_detected: false,
        unreachable: BTreeSet::new(),
    })
}

/// Writes restore.json from the current files, only if absent. Some(state) when written. An existing one must
/// parse and match the current paths, so prepare never changes files a restore would not put back.
pub fn snapshot(paths: &Paths) -> Result<Option<RestoreState>> {
    if !paths.restore_file().exists() {
        let state = take_snapshot(paths)?;
        write_state(paths, &state)?;
        return Ok(Some(state));
    }
    let state = read_state(paths)?;
    // its other changes are undone already, so a prepare on top would leave them for no restore to undo
    if state.partial {
        bail!("{}", left_message(&paths.restore_file(), &state));
    }
    let now = current_paths(paths)?;
    let Some(then) = state.paths else { return Ok(None) };
    // a path the restore.json lacks is one its restore does not use
    let pairs = [
        ("steamvr.vrsettings", Some(&then.vrsettings), now.vrsettings),
        ("the ALVR session", Some(&then.session), now.session),
        ("vrcompositor", then.vrcompositor.as_ref(), now.vrcompositor.unwrap_or_default()),
        ("the ALVR driver", then.driver_dir.as_ref(), now.driver_dir.unwrap_or_default()),
    ];
    if let Some((what, Some(then), now)) = pairs.into_iter().find(|(_, then, now)| then.is_some_and(|then| *then != *now)) {
        bail!(
            "restore.json has {what} at {}, but the config now gives {}; run vr_restore first (it restores to the recorded paths)",
            then.display(),
            now.display()
        );
    }
    Ok(None)
}

/// Whether `a` and `b` name the same directory.
fn same_dir(a: &Path, b: &Path) -> bool {
    a == b || fs::canonicalize(a).is_ok_and(|a| fs::canonicalize(b).is_ok_and(|b| a == b))
}

/// For a cold vr_start: a restore.json whose setup carries none of the MCP's changes (forcedDriver is not ALVR's and
/// its driver is not registered: the user put SteamVR back by hand since) would revert everything done since, so a
/// fresh driver list and vrsettings replace the recorded ones. The rest (session, its side files, the compositor
/// wrap) stays as first recorded, with its paths: the check says nothing about them. A half-done restore is never
/// stale. True when refreshed.
pub fn refresh_stale_snapshot(paths: &Paths) -> Result<bool> {
    if !paths.restore_file().exists() {
        return Ok(false);
    }
    let mut state = read_state(paths)?;
    if state.restoring {
        return Ok(false);
    }
    let at = restore_paths(paths, &state)?;
    let (section, key) = VRSETTINGS_KEYS[0];
    // unreadable counts as changed: keep the snapshot when unsure
    let forced = read_json(&at.vrsettings).map_or(true, |s| s[section][key] == "alvr_server");
    let registered = load_openvr_paths().map_or(true, |(_, json)| {
        let drivers = alvr_server_io::from_openvr_paths(&json["external_drivers"]);
        at.driver_dir.as_ref().is_none_or(|dir| drivers.iter().any(|d| same_dir(d, dir)))
    });
    if forced || registered {
        return Ok(false);
    }
    let vrsettings = abs(paths.vrsettings_file(), NO_STEAM)?;
    state.external_drivers = external_drivers()?;
    state.vrsettings_file = Some(FileState::Contents(fs::read_to_string(&vrsettings).with_context(|| vrsettings.display().to_string())?));
    // an old restore.json's other paths were detected just now
    state.paths_detected |= state.paths.is_none();
    state.paths = Some(SnapshotPaths { vrsettings, ..at });
    write_state(paths, &state)?;
    Ok(true)
}

/// Registers the headset as a trusted client reached at 127.0.0.1, without mDNS or the trust prompt.
pub fn client_actions() -> [ClientConnectionsAction; 3] {
    let loopback = vec![IpAddr::V4(Ipv4Addr::LOCALHOST)];
    [
        ClientConnectionsAction::AddIfMissing { trusted: true, manual_ips: loopback.clone() },
        ClientConnectionsAction::Trust,
        ClientConnectionsAction::SetManualIps(loopback),
    ]
}

/// Switches SteamVR to this streamer's ALVR driver. Writes restore.json first, only if absent.
pub fn prepare(paths: &Paths, values: &[PathValuePair], hostname: &str) -> Result<PrepareReport> {
    let driver_dir = paths.driver_dir().context(NO_STREAMER)?;
    let session_file = paths.session_file().context(NO_STREAMER)?;
    let vrsettings = paths.vrsettings_file().context(NO_STEAM)?;
    let compositor = paths.vrcompositor().context(NO_STEAMVR)?;
    let config_dir = session_file.parent().context("session file has no parent")?;

    // 1. snapshot
    let snapshot_written = snapshot(paths)?.is_some();

    // 2. driver registration
    register_driver(&driver_dir)?;

    // 3. vrcompositor wrapper
    let wrap = wrapped_vrcompositor(&compositor)?;
    if let Some(wrap) = &wrap {
        // one rename replaces the symlink, so a crash never leaves SteamVR without a vrcompositor
        fs::rename(&wrap.real_path, &compositor).with_context(|| format!("{} -> {}", wrap.real_path.display(), compositor.display()))?;
    }

    // 4. vrsettings
    let (section, key) = VRSETTINGS_KEYS[0];
    let mut settings = read_json(&vrsettings)?;
    put(&mut settings, section, key, Some(json!("alvr_server")));
    write_json(&vrsettings, &settings)?;
    let unblocked_safe_mode = unblock_safe_mode(&vrsettings)?;

    // 5. session
    let before = list_dir(config_dir);
    let mut session = ServerSessionManager::new(Some(session_file.clone()));
    session.set_session_values(values.to_vec())?;
    for action in client_actions() {
        session.update_client_connections(hostname.into(), action);
    }
    // ALVR writes session_old.json when it extrapolates an older session; the snapshot may predate this prepare
    let created: Vec<_> = list_dir(config_dir).difference(&before).cloned().collect();
    if !created.is_empty() {
        let mut state = read_state(paths)?;
        state.created_files.extend(created);
        write_state(paths, &state)?;
    }

    Ok(PrepareReport { snapshot_written, unblocked_safe_mode, unwrapped_vrcompositor: wrap.is_some() })
}

/// Puts back everything restore.json recorded, at the paths it recorded, then deletes it. Every step can run
/// again, so a retry after a failure finishes the job. A file whose directory is gone (an unmounted drive) is
/// skipped and restore.json rewritten as partial, holding only those for a retry, or with `forget_missing` dropped
/// (an install removed on purpose); an unreadable openvrpaths.vrpath is skipped and restore.json kept whole for a
/// retry. Returns notes on what it left as it was.
pub fn restore(paths: &Paths, forget_missing: bool) -> Result<Vec<String>> {
    let mut state = read_state(paths)?;
    let at = restore_paths(paths, &state)?;
    if !state.restoring || state.paths.is_none() {
        state.restoring = true;
        if state.paths.is_none() {
            // an old restore.json keeps the paths found now, so a retry never depends on detecting them again
            state.paths = Some(at.clone());
            state.paths_detected = true;
        }
        write_state(paths, &state)?;
    }

    let mut pass = Pass::default();
    // a partial restore.json put the driver list back already; the user may have changed it since
    let drivers_back = if state.partial { Ok(()) } else { put_external_drivers(&state.external_drivers) };
    if let Err(e) = &drivers_back {
        pass.notes.push(format!("left openvrpaths.vrpath as it is and kept restore.json; fix the file, then run vr_restore again: {e:#}"));
    }
    // a torn openvrpaths.vrpath keeps restore.json whole, so nothing would be forgotten
    pass.forget = forget_missing && drivers_back.is_ok() && !state.paths_detected;

    // each step clears what it put back, so a partial restore.json holds only what was skipped
    if let Some(original) = &state.vrsettings_file {
        if pass.put_back(&at.vrsettings, original)? {
            state.vrsettings_file = None;
        }
    } else if !state.vrsettings.is_empty() {
        let skip = pass.skipped(&at.vrsettings);
        if skip.is_none() {
            let mut settings = read_json(&at.vrsettings)?;
            for (name, original) in &state.vrsettings {
                let (section, key) = name.split_once('/').context("bad vrsettings key in restore.json")?;
                put(&mut settings, section, key, original.clone());
            }
            write_json(&at.vrsettings, &settings)?;
        }
        if skip != Some(false) {
            state.vrsettings.clear();
        }
    }

    if let Some(original) = &state.session
        && pass.put_back(&at.session, original)?
    {
        state.session = None;
    }
    for file in std::mem::take(&mut state.created_files) {
        match pass.skipped(&file) {
            None => remove_file(&file)?,
            Some(true) => (),
            Some(false) => state.created_files.push(file),
        }
    }
    for (file, original) in std::mem::take(&mut state.session_backups) {
        if !pass.put_back(&file, &original)? {
            state.session_backups.insert(file, original);
        }
    }

    // restore_paths finds the compositor whenever a wrap was recorded
    let mut rewrap_left = false;
    if let Some(wrap) = &state.vrcompositor
        && let Some(compositor) = &at.vrcompositor
        && fs::read_link(compositor).is_err()
    {
        let bin_dir = compositor.parent().context("vrcompositor has no parent")?;
        let target = bin_dir.join(&wrap.link_target);
        let neither = format!("neither {} nor {} exists", compositor.display(), wrap.real_path.display());
        // try_exists: a stat error is not "gone"
        match (compositor.try_exists(), wrap.real_path.try_exists()) {
            // nothing to rewrap, whether or not bin/linux64 is there
            (Ok(false), Ok(false)) if pass.forget => {
                pass.notes.push(format!("forgot the vrcompositor wrap: Valve's binary is gone ({neither})"))
            }
            (Ok(false), Ok(false)) => {
                rewrap_left = true;
                let why = match (bin_dir.exists(), state.paths_detected) {
                    (true, false) => "verify SteamVR's files in Steam, then run vr_restore again (or with forget_missing true)",
                    (true, true) => "verify SteamVR's files in Steam, then run vr_restore again",
                    (false, _) => "SteamVR's drive is not mounted, or SteamVR moved or was removed",
                };
                pass.notes.push(format!("left vrcompositor unwrapped: {neither}; {why}"));
            }
            (Err(e), _) | (_, Err(e)) => {
                rewrap_left = true;
                pass.unreachable.insert(bin_dir.to_owned());
                pass.notes.push(format!("left vrcompositor unwrapped: cannot check {}: {e}", compositor.display()));
            }
            _ if !target.exists() => {
                pass.notes.push(format!("left Valve's vrcompositor in place: the wrapper it linked to, {}, is gone", target.display()))
            }
            _ => {
                // a retry after a crash finds Valve's binary already moved back
                if !wrap.real_path.exists() {
                    fs::rename(compositor, &wrap.real_path).with_context(|| format!("{} -> {}", compositor.display(), wrap.real_path.display()))?;
                }
                let tmp = compositor.with_file_name("vrcompositor.tmp");
                remove_file(&tmp)?;
                std::os::unix::fs::symlink(&wrap.link_target, &tmp).with_context(|| tmp.display().to_string())?;
                fs::rename(&tmp, compositor).with_context(|| compositor.display().to_string())?;
            }
        }
    }
    if !rewrap_left {
        state.vrcompositor = None;
    }

    let mut notes = pass.notes;
    if drivers_back.is_err() {
        return Ok(notes); // restore.json kept whole, so the retry redoes every step
    }
    let left = state.vrsettings_file.is_some()
        || !state.vrsettings.is_empty()
        || state.session.is_some()
        || !state.created_files.is_empty()
        || !state.session_backups.is_empty()
        || state.vrcompositor.is_some();
    if !left {
        remove_file(&paths.restore_file())?;
        return Ok(notes);
    }
    state.partial = true;
    state.unreachable = pass.unreachable;
    write_state(paths, &state)?;
    notes.push(format!("kept restore.json: {}", left_message(&paths.restore_file(), &state)));
    Ok(notes)
}

/// The way out of a partial restore.json, for vr_restore's notes and vr_start's refusal. Offers forget_missing only
/// where it would drop what is left, so an agent following it never loops.
fn left_message(file: &Path, state: &RestoreState) -> String {
    let file = file.display();
    if !state.unreachable.is_empty() {
        let dirs = state.unreachable.iter().map(|d| d.display().to_string()).collect::<Vec<_>>().join(", ");
        return format!(
            "{file} holds what vr_restore could not put back; it cannot check {dirs} (see vr_restore's notes for the \
             error), and forget_missing never drops what it cannot check: a human has to restore access to {dirs}, then \
             run vr_restore again"
        );
    }
    if state.paths_detected {
        let opt = |p: &Option<PathBuf>| p.as_ref().map_or("none".to_owned(), |p| p.display().to_string());
        let at = state.paths.as_ref().map_or("none".to_owned(), |p| {
            format!(
                "steamvr.vrsettings {}, ALVR session {}, vrcompositor {}, ALVR driver {}",
                p.vrsettings.display(),
                p.session.display(),
                opt(&p.vrcompositor),
                opt(&p.driver_dir)
            )
        });
        return format!(
            "this restore.json recorded no paths, so vr_restore detected them and stored them in {file}: {at}; \
             forget_missing stays off for this file. If those paths are right, mount or reinstall what is gone and run \
             vr_restore again; if they are wrong, a human must correct the paths in {file}"
        );
    }
    format!(
        "{file} holds only what vr_restore could not put back, a directory being gone; mount the drive and run vr_restore \
         again, or, if that install was removed on purpose, run vr_restore {{forget_missing: true}} to drop those files"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use alvr_server_io::ServerSessionManager;
    use alvr_session::{
        CodecType, ControllersEmulationMode, H264Profile, RecenteringMode, settings_schema::Switch,
    };

    #[test]
    fn blocked_drivers_lists_true_blocks() {
        let dir = std::env::temp_dir().join(format!("SteamVR-MCP-blocked-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("steamvr.vrsettings");
        fs::write(&file, r#"{"driver_a":{"blocked_by_safe_mode":true},"driver_b":{"blocked_by_safe_mode":false},"steamvr":{"blocked_by_safe_mode":true}}"#).unwrap();
        assert_eq!(blocked_drivers(&file), ["driver_a"]);
        assert!(blocked_drivers(&dir.join("missing")).is_empty());
        fs::remove_dir_all(&dir).ok();
    }

    fn applied(cfg: &Config) -> alvr_common::anyhow::Result<ServerSessionManager> {
        let mut m = ServerSessionManager::new(None);
        m.set_session_values(session_values(cfg)?)?;
        Ok(m)
    }

    #[test]
    fn default_session_values_apply_with_alvr_code() {
        let m = applied(&Config::default()).unwrap();
        let s = m.settings();
        assert!(matches!(s.video.preferred_codec, CodecType::H264));
        assert!(matches!(s.video.encoder_config.h264_profile, H264Profile::Baseline));
        let Switch::Enabled(c) = &s.headset.controllers else { panic!("controllers disabled") };
        assert!(matches!(c.emulation_mode, ControllersEmulationMode::ValveIndex));
        let Switch::Enabled(h) = &c.hand_skeleton else { panic!("hand_skeleton disabled") };
        assert!(!h.steamvr_input_2_0);
        assert!(matches!(c.hand_tracking_interaction, Switch::Enabled(_)));
        assert!(matches!(s.headset.recentering_mode, RecenteringMode::Stage));
        assert!(matches!(s.video.foveated_encoding, Switch::Disabled));
        assert!(matches!(s.audio.game_audio, Switch::Disabled));
        assert!(matches!(s.audio.microphone, Switch::Disabled));
        assert!(matches!(s.connection.client_discovery, Switch::Disabled));
        assert_eq!(s.video.preferred_fps, 72.0);
    }

    #[test]
    fn index_mappings_cover_every_index_input() {
        use alvr_common::{BUTTON_INFO, CONTROLLER_PROFILE_INFO, INDEX_CONTROLLER_PROFILE_ID};
        use alvr_session::ButtonMappingType;
        let m = applied(&Config::default()).unwrap();
        let Switch::Enabled(c) = &m.settings().headset.controllers else { panic!("controllers disabled") };
        let mappings = c.button_mappings.as_ref().expect("manual button mappings");
        let index = &CONTROLLER_PROFILE_INFO[&*INDEX_CONTROLLER_PROFILE_ID].button_set;
        assert_eq!((mappings.len(), index.len()), (38, 38));
        for (key, targets) in mappings {
            assert!(index.iter().any(|id| BUTTON_INFO[id].path == key), "{key} is not an Index input");
            assert_eq!(targets.len(), 1, "{key}");
            assert_eq!(targets[0].destination, *key);
            assert!(matches!(targets[0].mapping_type, ButtonMappingType::Passthrough), "{key}");
            assert!(targets[0].binary_conditions.is_empty(), "{key}");
        }
        let keys: Vec<_> = mappings.iter().map(|(k, _)| k.as_str()).collect();
        assert!(keys.contains(&"/user/hand/left/input/a/click") && keys.contains(&"/user/hand/right/input/trackpad/force"));
    }

    #[test]
    fn session_values_close_the_dashboard_to_the_lan() {
        let mut m = ServerSessionManager::new(None);
        let open = [("session_settings.connection.allow_remote_dashboard", json!(true)), ("session_settings.connection.client_discovery.enabled", json!(true))];
        m.set_session_values(open.map(|(p, value)| PathValuePair { path: parse_path(p).unwrap(), value }).to_vec()).unwrap();
        m.set_session_values(session_values(&Config::default()).unwrap()).unwrap();
        let c = &m.settings().connection;
        assert!(!c.allow_remote_dashboard, "a vr_start resets what an earlier session opened");
        assert!(matches!(c.client_discovery, Switch::Disabled));
    }

    #[test]
    fn overrides_follow_defaults() {
        let mut cfg = Config::default();
        cfg.alvr_settings.insert("session_settings.video.preferred_fps".into(), 90.0.into());
        assert_eq!(applied(&cfg).unwrap().settings().video.preferred_fps, 90.0);
    }

    #[test]
    fn overrides_reject_bad_path() {
        let mut cfg = Config::default();
        cfg.alvr_settings.insert("a..b".into(), 1.into());
        let err = session_values(&cfg).unwrap_err();
        assert!(err.to_string().contains("invalid path"), "{err}");
    }

    #[test]
    fn unblock_safe_mode_sets_false_and_reports() {
        let path = std::env::temp_dir().join(format!("SteamVR-MCP-vrsettings-{}", std::process::id()));
        fs::write(&path, r#"{"driver_alvr_server":{"blocked_by_safe_mode":true}}"#).unwrap();
        assert!(unblock_safe_mode(&path).unwrap());
        let v: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["driver_alvr_server"]["blocked_by_safe_mode"], false);
        assert!(!unblock_safe_mode(&path).unwrap());
        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn write_atomic_refuses_a_read_only_target_and_keeps_the_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("SteamVR-MCP-atomic-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        // chmod 444 keeps SteamVR from resetting `runtime` in openvrpaths.vrpath
        let locked = dir.join("locked");
        fs::write(&locked, "mine").unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o444)).unwrap();
        let e = format!("{:#}", write_atomic(&locked, b"new").unwrap_err());
        assert!(e.contains(&locked.display().to_string()) && e.contains("read-only"), "{e}");
        assert_eq!((fs::read_to_string(&locked).unwrap(), mode(&locked)), ("mine".into(), 0o444));
        let private = dir.join("private");
        fs::write(&private, "mine").unwrap();
        fs::set_permissions(&private, fs::Permissions::from_mode(0o600)).unwrap();
        write_atomic(&private, b"new").unwrap();
        assert_eq!((fs::read_to_string(&private).unwrap(), mode(&private)), ("new".into(), 0o600));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn clear_safe_mode_blocks_removes_every_block() {
        let mut current = json!({
            "driver_alvr_server": {"blocked_by_safe_mode": true},
            "driver_gamepad": {"blocked_by_safe_mode": true},
            "driver_prism": {"blocked_by_safe_mode": true, "keep": 1},
            "driver_ok": {"blocked_by_safe_mode": false},
            "steamvr": {"blocked_by_safe_mode": true}
        });
        let names = clear_safe_mode_blocks(&mut current);
        assert_eq!(names, ["driver_alvr_server", "driver_gamepad", "driver_prism"]);
        assert_eq!(current, json!({"driver_prism": {"keep": 1}, "driver_ok": {"blocked_by_safe_mode": false}, "steamvr": {"blocked_by_safe_mode": true}}));
    }
}
