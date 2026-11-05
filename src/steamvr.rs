use crate::{alvr_api::Api, config::{Config, Paths}, setup};
use alvr_common::{
    anyhow::{Context, Result, bail},
    error, warn,
};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

pub const STEAMVR_URL: &str = "steam://rungameid/250820";
pub const STEAMVR_APP_ID: u32 = 250820;

/// SteamVR's processes, by comm or argv0 as is_named matches them (pinned from a live SteamVR 2.x run).
/// vrmonitor is last: killing it first makes SteamVR's safe mode block the ALVR driver.
pub const VR_PROCESS_NAMES: &[&str] = &[
    "vrserver", "vrcompositor", "vrdashboard", "vrwebhelper", "vrstartup", "vrstartup.sh", "steamtours", "steamtours.sh",
    "vrmonitor",
];

/// Steam's processes, by comm or argv0, which vr_stop waits on after asking the Steam that vr_start started to exit.
/// The client comes first: vr_stop forwards -shutdown only while it runs.
pub const STEAM_PROCESS_NAMES: &[&str] = &["steam", "steam.sh", "steamwebhelper"];

/// Every pid in /proc whose real uid is this user's: another user's Steam or SteamVR (a second account) is not ours to see or stop.
/// The /proc dir owner would hide our non-dumpable file-capability helpers (vrcompositor-launcher), which show as root.
pub fn all_pids() -> Vec<u32> {
    proc_pids().filter(|&pid| ours(pid)).collect()
}

/// Every pid in /proc, of any user.
fn proc_pids() -> impl Iterator<Item = u32> {
    fs::read_dir("/proc").into_iter().flatten().flatten().filter_map(|e| e.file_name().to_str()?.parse().ok())
}

/// Whether `pid`'s real uid is this user's; an unreadable status is not ours.
fn ours(pid: u32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/status")).ok().and_then(|s| real_uid(&s)) == Some(unsafe { libc::getuid() })
}

/// The real uid: the first field of the `Uid:` line of `/proc/<pid>/status`.
fn real_uid(status: &str) -> Option<u32> {
    status.lines().find_map(|l| l.strip_prefix("Uid:"))?.split_whitespace().next()?.parse().ok()
}

// name before uid: the status read is the costly part, and few pids match the name
// shortcut: reads every non-matching pid's cmdline (one extra small read per pid per poll), filter on cheaper /proc data if polls show up in profiles
pub fn pids_named(name: &str) -> Vec<u32> {
    proc_pids().filter(|&pid| name_of(pid, &[name]).is_some() && ours(pid)).collect()
}

/// The `names` (in their order) with a process of this user, from one /proc pass.
pub fn running<'a>(names: &[&'a str]) -> Vec<&'a str> {
    let up: Vec<&str> = proc_pids().filter_map(|pid| name_of(pid, names).filter(|_| ours(pid))).collect();
    names.iter().copied().filter(|n| up.contains(n)).collect()
}

/// Whether a process with this comm and NUL-separated cmdline is `name`: its comm, or argv0's basename
/// (vrserver renames its main thread; a script's argv0 is its shell, so comm stays a key too).
pub fn is_named(name: &str, comm: &str, cmdline: &[u8]) -> bool {
    let argv0 = cmdline.split(|&b| b == 0).next().unwrap_or_default();
    comm == name || argv0.rsplit(|&b| b == b'/').next() == Some(name.as_bytes())
}

/// The first of `names` that `pid` is (see is_named); the cmdline is read only when its comm is none of them.
pub fn name_of<'a>(pid: u32, names: &[&'a str]) -> Option<&'a str> {
    let comm = comm(pid)?;
    names.iter().copied().find(|&n| n == comm).or_else(|| {
        let cmdline = fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
        names.iter().copied().find(|n| is_named(n, &comm, &cmdline))
    })
}

/// The VR_PROCESS_NAMES entry `pid` is, else its comm.
pub fn process_name(pid: u32) -> Option<String> {
    name_of(pid, VR_PROCESS_NAMES).map(str::to_owned).or_else(|| comm(pid))
}

/// `/proc/<pid>/comm`: the process name.
pub fn comm(pid: u32) -> Option<String> {
    Some(fs::read_to_string(format!("/proc/{pid}/comm")).ok()?.trim_end().to_owned())
}

/// The state letter and start time of `pid`, from `/proc/<pid>/stat`.
pub fn stat(pid: u32) -> Option<(char, u64)> {
    parse_stat(&fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

/// The state letter (field 3) and start time in clock ticks (field 22); the comm before them may hold spaces and parens.
fn parse_stat(stat: &str) -> Option<(char, u64)> {
    let mut fields = stat.get(stat.rfind(')')? + 1..)?.split_whitespace();
    let state = fields.next()?.chars().next()?;
    Some((state, fields.nth(18)?.parse().ok()?))
}

/// The `snapshot` pids still the same process (start time) and not a zombie, whatever their name now.
pub fn survivors(snapshot: &[(u32, u64)], stat: impl Fn(u32) -> Option<(char, u64)>) -> Vec<u32> {
    snapshot.iter().filter(|&&(pid, start)| stat(pid).is_some_and(|(state, t)| t == start && state != 'Z')).map(|&(pid, _)| pid).collect()
}

pub fn exe_of(pid: u32) -> Option<PathBuf> {
    fs::read_link(format!("/proc/{pid}/exe")).ok()
}

pub fn is_steamvr_running() -> bool {
    !pids_named("vrserver").is_empty()
}

/// Whether `/proc/<pid>/maps` lists a file whose path contains `file`.
pub fn has_mapped(pid: u32, file: &str) -> bool {
    fs::read_to_string(format!("/proc/{pid}/maps")).is_ok_and(|maps| maps.contains(file))
}

/// Whether a running vrserver loaded ALVR's driver, even one whose API no longer answers.
pub fn vrserver_has_alvr() -> bool {
    pids_named("vrserver").into_iter().any(|pid| has_mapped(pid, "driver_alvr_server.so"))
}

pub fn is_steam_running() -> bool {
    !pids_named("steam").is_empty()
}

/// The `steam.env` entries that a Steam process's NUL-separated environ lacks or has with another value,
/// as `KEY=<want> but has <got|missing>`.
// shortcut: steam.sh rewrites LD_* and STEAM_RUNTIME* keys, so such a key in steam.env always mismatches; skip those keys if a user sets one
pub fn env_mismatches(environ: &[u8], want: &BTreeMap<String, String>) -> Vec<String> {
    let have: BTreeMap<&[u8], &[u8]> = environ.split(|&b| b == 0).filter_map(|e| {
        let i = e.iter().position(|&b| b == b'=')?;
        Some((&e[..i], &e[i + 1..]))
    }).collect();
    want.iter().filter_map(|(k, v)| match have.get(k.as_bytes()) {
        Some(got) if *got == v.as_bytes() => None,
        Some(got) => Some(format!("{k}={v} but has {}", String::from_utf8_lossy(got))),
        None => Some(format!("{k}={v} but has missing")),
    }).collect()
}

/// Whether `pid` is the Steam client, not the bootstrap script, whose exe is a shell.
fn is_steam_client(pid: u32) -> bool {
    exe_of(pid).is_some_and(|e| e.file_name().is_some_and(|n| n == "steam"))
}

/// The environ of the running Steam client (not the bootstrap script, whose exe is a shell); none if unreadable.
pub fn running_steam_environ() -> Option<Vec<u8>> {
    let pids = pids_named("steam");
    let client = pids.iter().find(|&&p| is_steam_client(p));
    fs::read(format!("/proc/{}/environ", client.or(pids.first())?)).ok()
}

/// The Steam client as (pid, start time) when exactly one runs, so vr_stop can tell it from a Steam started later.
/// None under test: no Steam there is ever the MCP's.
pub fn steam_client() -> Option<(u32, u64)> {
    if cfg!(test) {
        return None;
    }
    match pids_named("steam").into_iter().filter(|&p| is_steam_client(p)).collect::<Vec<_>>()[..] {
        [pid] => Some((pid, stat(pid)?.1)),
        _ => None,
    }
}

/// Polls `done` every 100 ms until it holds or `timeout` passes.
pub fn wait_for(timeout: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if done() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// Whether a process named `name` runs that is not a zombie, which no wait or signal clears.
pub fn has_live(name: &str) -> bool {
    pids_named(name).into_iter().any(|p| stat(p).is_some_and(|(s, _)| s != 'Z'))
}

pub fn wait_gone(names: &[&str], timeout: Duration) -> bool {
    wait_for(timeout, || running(names).is_empty())
}

/// The first key seen on an earlier poll that has not been `alive` for `grace` up to `now`. `seen` carries
/// the keys and since when each is missing across polls; `appeared` adds this poll's.
pub fn gone_after_seen<K: Copy + PartialEq>(
    seen: &mut Vec<(K, Option<Instant>)>, appeared: impl IntoIterator<Item = K>, alive: impl Fn(K) -> bool, now: Instant, grace: Duration,
) -> Option<K> {
    for k in appeared {
        if !seen.iter().any(|&(s, _)| s == k) {
            seen.push((k, None));
        }
    }
    for (k, missing) in seen.iter_mut() {
        *missing = if alive(*k) { None } else { missing.or(Some(now)) };
    }
    seen.iter().find(|(_, m)| m.is_some_and(|t| now.duration_since(t) >= grace)).map(|&(k, _)| k)
}

/// How long vrserver or vrmonitor stays gone before it counts as exited: a relaunching vrserver leaves a ~0.2 s gap.
pub const VR_EXIT_GRACE: Duration = Duration::from_secs(3);

/// vrserver or vrmonitor once it ran during this wait (`seen`) and stayed gone for VR_EXIT_GRACE: SteamVR quit or crashed.
pub fn vr_exited(seen: &mut Vec<(&'static str, Option<Instant>)>) -> Option<&'static str> {
    // one /proc pass for both names: this runs on every poll of a minutes-long wait
    let up = running(&["vrserver", "vrmonitor"]);
    let gone = gone_after_seen(seen, up.iter().copied(), |n| up.contains(&n), Instant::now(), VR_EXIT_GRACE);
    if let Some(name) = gone {
        // names the mechanism next time: what of SteamVR still runs, under which comm and state
        let left: Vec<_> = proc_pids()
            .filter(|p| fs::read(format!("/proc/{p}/cmdline")).is_ok_and(|c| String::from_utf8_lossy(&c).contains("SteamVR/bin")))
            .map(|p| (p, comm(p).unwrap_or_default(), stat(p).map(|(state, _)| state)))
            .collect();
        warn!("{name} gone for {} s; SteamVR/bin processes (pid, comm, state): {left:?}", VR_EXIT_GRACE.as_secs());
    }
    gone
}

/// `pid` as kill() takes it, when that names one process the MCP may signal: 0 and anything above
/// i32::MAX would signal a process group or every process, and init, this MCP and the leader of its
/// process group (often the agent that started it) are never signalled.
pub fn signalable(pid: u32) -> Option<i32> {
    i32::try_from(pid).ok().filter(|&p| p > 1 && pid != std::process::id() && p != unsafe { libc::getpgrp() })
}

/// Sends `sig` to `pid`; does nothing for a pid that is not a single process.
pub fn terminate(pid: u32, sig: i32) {
    if let Some(pid) = signalable(pid) {
        unsafe { libc::kill(pid, sig) };
    }
}

/// How long VrProcs::kill waits after each signal, once for the earlier names and once for the last.
pub const KILL_WAIT: Duration = Duration::from_secs(5);

/// How long after a launch Steam may take to start SteamVR's first process.
pub const LAUNCH_SETTLE: Duration = URL_TAKEN_WAIT;

/// The processes vr_stop and a SteamVR restart end, and how long they wait on them: SteamVR's, or a unit test's fakes.
#[derive(Clone, Copy, Debug)]
pub struct VrProcs {
    pub names: &'static [&'static str],
    /// How long they get to exit after ALVR's shutdown request, before signals.
    pub grace: Duration,
    /// How long each of kill()'s waits lasts: four at most.
    pub kill_wait: Duration,
    /// Steam's processes, which vr_stop waits on (and never signals by name), and how long.
    pub steam: &'static [&'static str],
    pub steam_wait: Duration,
    /// How long vr_stop waits on them after it sent SIGTERM to the Steam client vr_start started.
    pub steam_term_wait: Duration,
}

/// What VrProcs::end did: whether every process is gone, and whether it took signals.
#[derive(Debug, PartialEq)]
pub struct Ended {
    pub gone: bool,
    pub used_signals: bool,
}

impl VrProcs {
    pub const STEAMVR: VrProcs =
        VrProcs { names: VR_PROCESS_NAMES, grace: Duration::from_secs(30), kill_wait: KILL_WAIT, steam: STEAM_PROCESS_NAMES, steam_wait: Duration::from_secs(60), steam_term_wait: Duration::from_secs(15) };
    /// Matches no process: what unit tests get, so none can signal the user's SteamVR or wait on their Steam.
    pub const NONE: VrProcs = VrProcs { names: &[], steam: &[], steam_term_wait: Duration::ZERO, ..Self::STEAMVR };

    /// Every such process as (pid, start time), so a later check finds it whatever its comm becomes.
    pub fn snapshot(&self) -> Vec<(u32, u64)> {
        self.names.iter().flat_map(|n| pids_named(n)).filter_map(|pid| Some((pid, stat(pid)?.1))).collect()
    }

    /// is_steamvr_running, when these processes include vrserver: never for NONE.
    pub fn vrserver_running(&self) -> bool {
        self.names.contains(&"vrserver") && is_steamvr_running()
    }

    /// The names with a running process.
    pub fn alive(&self) -> Vec<&'static str> {
        running(self.names)
    }

    /// SIGTERM, then SIGKILL, by name. Each signal reaches the last name (vrmonitor) only once the earlier ones are gone or
    /// kill_wait passed, so vrmonitor never goes before the rest. True when none remain.
    pub fn kill(&self) -> bool {
        let Some((last, earlier)) = self.names.split_last() else { return true };
        let signal = |names: &[&str], sig| names.iter().flat_map(|n| pids_named(n)).for_each(|pid| terminate(pid, sig));
        for sig in [libc::SIGTERM, libc::SIGKILL] {
            signal(earlier, sig);
            wait_for(self.kill_wait, || !earlier.iter().any(|n| has_live(n)));
            signal(&[last], sig);
            if wait_gone(self.names, self.kill_wait) {
                return true;
            }
        }
        false
    }

    /// The `steam` names with a live process; a zombie does not count.
    pub fn live_steam(&self) -> Vec<&'static str> {
        self.steam.iter().copied().filter(|n| has_live(n)).collect()
    }

    /// After ALVR was asked to shut SteamVR down: waits `grace`, then kills by name, then SIGKILLs the
    /// `snapshot` pids still alive under another name (a vrserver that renamed itself lived on 30 s).
    pub fn end(&self, snapshot: &[(u32, u64)]) -> Ended {
        let mut used_signals = false;
        let mut gone = wait_gone(self.names, self.grace) || {
            used_signals = true;
            self.kill()
        };
        let survivors = || survivors(snapshot, stat);
        if !survivors().is_empty() {
            used_signals = true;
            survivors().into_iter().for_each(|pid| terminate(pid, libc::SIGKILL));
            gone = wait_for(self.kill_wait, || survivors().is_empty()) && gone;
        }
        Ended { gone, used_signals }
    }

    /// The names still running, then the `snapshot` pids still alive under another name.
    pub fn remaining(&self, snapshot: &[(u32, u64)]) -> Vec<String> {
        let named = self.alive().into_iter().map(str::to_owned);
        named.chain(survivors(snapshot, stat).into_iter().map(|pid| format!("pid {pid} ({})", process_name(pid).unwrap_or_default()))).collect()
    }

    /// After a launch the caller gave up on (vr_stop): waits until one of these processes runs, or
    /// LAUNCH_SETTLE after `launched`, so the stop that follows finds the SteamVR Steam was handed.
    // shortcut: a Steam still starting up (login, self-update) can start SteamVR after the settle, track Steam's console log for AppID 250820 if that shows up
    pub fn settle(&self, launched: Instant) {
        wait_for(LAUNCH_SETTLE.saturating_sub(launched.elapsed()), || !self.alive().is_empty());
    }
}

/// True when `now` holds a vrserver pid that `old` does not: SteamVR really relaunched.
pub fn is_new_vrserver(old: &[u32], now: &[u32]) -> bool {
    now.iter().any(|pid| !old.contains(pid))
}

/// True when SteamVR processes outlive vrserver: Steam then refuses to launch SteamVR again
/// (console_log: WaitingPrevProcess, AppError_16), so start() kills them first.
pub fn has_leftovers(alive: &[&str]) -> bool {
    !alive.is_empty() && !alive.contains(&"vrserver")
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AppKind {
    Steam,
    Shortcut,
}

#[derive(Clone, Debug, Serialize)]
pub struct AppInfo {
    pub app_id: u32,
    /// What steam://rungameid/ takes.
    pub game_id: u64,
    pub kind: AppKind,
    pub name: String,
    /// Steam's CompatToolMapping entry, e.g. "proton_experimental"; none for native games and Steam's default.
    pub compat_tool: Option<String>,
    /// `<library>/steamapps/common/<installdir>`; none for a shortcut.
    #[serde(skip)]
    pub install_dir: Option<PathBuf>,
    /// `<library>/steamapps/compatdata/<app_id>`; a shortcut's library is the Steam dir.
    #[serde(skip)]
    pub compatdata: PathBuf,
}

/// Steam's game id for a non-Steam shortcut.
pub fn shortcut_game_id(appid: u32) -> u64 {
    (appid as u64) << 32 | 0x0200_0000
}

// shortcut: line-oriented VDF scan without escape handling; switch to a parser if Valve changes the format or a value holds a quote
/// A text VDF line up to its `//` comment; a `//` inside quotes is part of a token.
fn code(line: &str) -> &str {
    let mut at = 0;
    for (i, part) in line.split('"').enumerate() {
        if i % 2 == 0
            && let Some(c) = part.find("//")
        {
            return &line[..at + c];
        }
        at += part.len() + 1;
    }
    line
}

/// The quoted tokens of a text VDF line, comment excluded.
fn tokens(line: &str) -> Vec<&str> {
    code(line).split('"').skip(1).step_by(2).collect()
}

/// The first `"key" "value"` pair's value with this key.
pub(crate) fn vdf_value<'a>(vdf: &'a str, key: &str) -> Option<&'a str> {
    vdf.lines().find_map(|l| match tokens(l)[..] {
        [k, v] if k == key => Some(v),
        _ => None,
    })
}

/// Every library's `path` in libraryfolders.vdf.
pub fn library_dirs(libraryfolders_vdf: &str) -> Vec<PathBuf> {
    libraryfolders_vdf
        .lines()
        .filter_map(|l| match tokens(l)[..] {
            ["path", v] => Some(PathBuf::from(v)),
            _ => None,
        })
        .collect()
}

/// The Steam dir, then every library in libraryfolders.vdf, without repeats.
pub fn libraries(steam_dir: &Path) -> Vec<PathBuf> {
    let vdf = fs::read_to_string(steam_dir.join("steamapps/libraryfolders.vdf")).unwrap_or_default();
    let mut libs = vec![steam_dir.to_path_buf()];
    for lib in library_dirs(&vdf) {
        if !libs.contains(&lib) {
            libs.push(lib);
        }
    }
    libs
}

/// The `key` value in `"<section>" { "<entry>" { "<key>" "value" } }` of a text VDF.
pub(crate) fn entry_value<'a>(vdf: &'a str, section: &str, entry: &str, key: &str) -> Option<&'a str> {
    let lines = vdf.lines().skip_while(|l| tokens(l) != [section]).skip(1);
    let (mut depth, mut in_entry) = (0, false);
    for line in lines {
        // in_entry changes only at the next depth-1 key, so a block nested in the entry keeps it
        match (code(line).trim(), &tokens(line)[..]) {
            ("{", _) => depth += 1,
            ("}", _) if depth == 1 => return None,
            ("}", _) => depth -= 1,
            (_, [k]) if depth == 1 => in_entry = *k == entry,
            (_, [k, v]) if depth == 2 && in_entry && *k == key => return Some(v),
            _ => (),
        }
    }
    None
}

/// The tool name under CompatToolMapping / `<app_id>` in config.vdf; app id 0 is Steam's default.
pub fn compat_tool(config_vdf: &str, app_id: u32) -> Option<String> {
    entry_value(config_vdf, "CompatToolMapping", &app_id.to_string(), "name").map(str::to_owned)
}

/// The (appid, AppName) of each entry in Steam's binary shortcuts.vdf; a truncated file yields what came before.
pub fn parse_shortcuts(bytes: &[u8]) -> Vec<(u32, String)> {
    fn walk(b: &[u8], out: &mut Vec<(u32, String)>) -> Option<()> {
        let (mut i, mut id, mut name) = (0, None, None);
        let cstr = |i: &mut usize| {
            let len = b.get(*i..)?.iter().position(|&c| c == 0)?;
            *i += len + 1;
            Some(String::from_utf8_lossy(&b[*i - len - 1..*i - 1]).into_owned())
        };
        while let Some(&kind) = b.get(i) {
            i += 1;
            if kind == 0x08 {
                // the end of a map; an entry has both fields once its first nested map (tags) ends
                if let (Some(id), Some(name)) = (id.take(), name.take()) {
                    out.push((id, name));
                }
                continue;
            }
            let key = cstr(&mut i)?;
            match kind {
                0x00 => (),
                0x01 => name = key.eq_ignore_ascii_case("appname").then_some(cstr(&mut i)?).or(name),
                0x02 => {
                    let v = i32::from_le_bytes(b.get(i..i + 4)?.try_into().ok()?);
                    i += 4;
                    if key.eq_ignore_ascii_case("appid") {
                        id = Some(v as u32);
                    }
                }
                _ => return None, // shortcuts.vdf uses only maps, strings and int32
            }
        }
        Some(())
    }
    let mut out = vec![];
    walk(bytes, &mut out);
    out
}

/// `id`: a Steam app id, a 32-bit shortcut app id or a 64-bit shortcut game id.
pub fn find_app(steam_dir: &Path, id: u64) -> Result<AppInfo> {
    let app_id = if id > u32::MAX as u64 { (id >> 32) as u32 } else { id as u32 };
    let compat_tool = || compat_tool(&fs::read_to_string(steam_dir.join("config/config.vdf")).unwrap_or_default(), app_id);
    if id <= u32::MAX as u64 {
        for lib in libraries(steam_dir) {
            if let Ok(manifest) = fs::read_to_string(lib.join(format!("steamapps/appmanifest_{app_id}.acf"))) {
                let name = vdf_value(&manifest, "name").unwrap_or_default().to_owned();
                let install_dir = vdf_value(&manifest, "installdir").map(|d| lib.join("steamapps/common").join(d));
                let compatdata = lib.join("steamapps/compatdata").join(app_id.to_string());
                return Ok(AppInfo { app_id, game_id: id, kind: AppKind::Steam, name, compat_tool: compat_tool(), install_dir, compatdata });
            }
        }
    }
    let game_id = shortcut_game_id(app_id);
    if id == app_id as u64 || id == game_id {
        for user in fs::read_dir(steam_dir.join("userdata")).into_iter().flatten().flatten() {
            let shortcuts = fs::read(user.path().join("config/shortcuts.vdf")).unwrap_or_default();
            if let Some((_, name)) = parse_shortcuts(&shortcuts).into_iter().find(|(a, _)| *a == app_id) {
                let compatdata = steam_dir.join("steamapps/compatdata").join(app_id.to_string());
                return Ok(AppInfo { app_id, game_id, kind: AppKind::Shortcut, name, compat_tool: compat_tool(), install_dir: None, compatdata });
            }
        }
    }
    bail!("no Steam app or non-Steam shortcut with id {id}")
}

fn log_file(steam_dir: &Path, name: &str) -> PathBuf {
    steam_dir.join("logs").join(format!("{name}.txt"))
}

/// Where Steam's console log ends now; read from here after a launch.
pub fn console_log_len(steam_dir: &Path) -> u64 {
    log_mark(steam_dir, "console_log").0
}

/// What Steam wrote to its console log after `from`; all of it when the log is now shorter (rotated).
pub fn console_log_since(steam_dir: &Path, from: u64) -> String {
    log_since(steam_dir, "console_log", (from, None))
}

/// A log's length and, when known, its file identity (dev, inode).
pub type LogMark = (u64, Option<(u64, u64)>);

fn mark_of(meta: &fs::Metadata) -> LogMark {
    use std::os::unix::fs::MetadataExt;
    (meta.len(), Some((meta.dev(), meta.ino())))
}

/// Where `<steam>/logs/<name>.txt` ends now, and which file that is; no identity when it is missing.
pub fn log_mark(steam_dir: &Path, name: &str) -> LogMark {
    fs::metadata(log_file(steam_dir, name)).map_or((0, None), |m| mark_of(&m))
}

/// The offset to read a log from, given `mark` taken earlier and the file's mark `now`: 0 when it is
/// another file (SteamVR rotates vrserver.txt to vrserver.previous.txt on some starts) or shorter (truncated).
fn read_from(mark: LogMark, now: LogMark) -> u64 {
    let ((from, id), (len, now_id)) = (mark, now);
    if (id.is_some() && id != now_id) || len < from { 0 } else { from }
}

/// What `<steam>/logs/<name>.txt` got after `mark`; all of it when the log was rotated or truncated.
pub fn log_since(steam_dir: &Path, name: &str, mark: LogMark) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let mut bytes = vec![];
    if let Ok(mut file) = fs::File::open(log_file(steam_dir, name)) {
        let from = file.metadata().map_or(0, |m| read_from(mark, mark_of(&m)));
        file.seek(SeekFrom::Start(from)).and_then(|_| file.read_to_end(&mut bytes)).ok();
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Launch steps at which Steam can wait on a desktop dialog.
const DIALOG_STEPS: [&str; 3] = ["ShowInterstitials", "ShowLaunchOption", "SynchronizingCloud"];

/// The AppID and step of a launch Steam holds at a desktop dialog: a wait at a DIALOG_STEPS step with
/// no "continues" after it. Steam logs a ShowInterstitials wait for every launch and continues at once when no dialog shows.
pub fn pending_interstitial(console_log: &str) -> Option<(u32, &'static str)> {
    let mut pending: Vec<(&str, &'static str)> = vec![];
    for line in console_log.lines() {
        let Some((action, rest)) = line.split_once("GameAction [").and_then(|(_, r)| r.split_once(']')) else { continue };
        for step in DIALOG_STEPS {
            if rest.contains(&format!("LaunchApp waiting for user response to {step} ")) {
                pending.push((action, step));
            } else if rest.contains(&format!("LaunchApp continues with user response \"{step}\"")) {
                pending.retain(|p| *p != (action, step));
            }
        }
    }
    let (action, step) = pending.last()?;
    Some((action.strip_prefix("AppID ")?.split(',').next()?.parse().ok()?, step))
}

/// How long Steam may take to log `ExecuteSteamURL` for a URL handed to it while it runs.
const URL_TAKEN_WAIT: Duration = Duration::from_secs(20);

/// None when `since` (the console log after the hand-off) shows Steam took `url`; else the failure
/// message, naming the cause when `tail` (the log's last lines) shows the runtime launch service failing.
// shortcut: fails fast instead of recovering; restart Steam automatically only if a later report shows the user wants that
pub fn url_not_taken(since: &str, tail: &str, url: &str) -> Option<String> {
    if since.contains(&format!("ExecuteSteamURL: \"{url}\"")) {
        return None;
    }
    let mut msg = format!("Steam did not take {url} (no ExecuteSteamURL in console_log.txt within {} s); restart Steam and retry", URL_TAKEN_WAIT.as_secs());
    // seen live with a Flatpak Steam running: both compete for the service's D-Bus name, and this Steam stops taking URLs
    let exits = tail.lines().filter(|l| l.contains("steam-runtime-launcher-service pid") && l.ends_with(" exited")).count();
    if exits >= 3 || tail.contains("steam-runtime-launcher-service process exited too many times, disabling") {
        msg += "; Steam's runtime launch service keeps failing (another Steam, e.g. Flatpak, may hold it: close that one, or start this Steam first)";
    }
    Some(msg)
}

/// None while Steam may still take `url`, or once `since` showed it took it: `taken` latches that, so
/// later polls skip the scan. Once the wait for it `expired` without it, url_not_taken's message; only then is `tail` read.
fn url_check(taken: &mut bool, since: &str, expired: bool, url: &str, tail: impl FnOnce() -> String) -> Option<String> {
    if *taken {
        return None;
    }
    *taken = since.contains(&format!("ExecuteSteamURL: \"{url}\""));
    if *taken || !expired {
        return None;
    }
    url_not_taken(since, &tail(), url)
}

/// How long a dialog-step wait may last before it counts as a dialog: seen live, Steam continued after up to 5 s.
const DIALOG_GRACE: Duration = Duration::from_secs(10);

/// wait_for for a launch through Steam, which fails fast once Steam waits on a desktop dialog
/// (console log lines after `log_from`). `handed_url` is the URL launch() gave an already running Steam:
/// it also fails fast when Steam logs no ExecuteSteamURL for it within URL_TAKEN_WAIT.
pub fn wait_launched(steam_dir: Option<&Path>, log_from: u64, handed_url: Option<&str>, timeout: Duration, done: impl FnMut() -> bool) -> Result<bool> {
    wait_launched_within((URL_TAKEN_WAIT, DIALOG_GRACE), steam_dir, log_from, handed_url, timeout, done)
}

/// What a wait for ALVR fails with once SteamVR's safe mode blocked its driver; each caller adds its advice.
pub const ALVR_BLOCKED: &str = "failed: SteamVR's safe mode blocked the ALVR driver (driver_alvr_server in steamvr.vrsettings), so ALVR will not load";

/// wait_launched that also fails, with ALVR_BLOCKED and `advice`, once `vrsettings` shows ALVR's driver blocked: SteamVR then never loads it.
/// The file is read only on polls where `answered` is false, so a stale block never fails an ALVR that answers.
pub fn wait_alvr(
    vrsettings: Option<&Path>, steam_dir: Option<&Path>, log_from: u64, handed_url: Option<&str>, timeout: Duration, advice: &str,
    mut answered: impl FnMut() -> bool,
) -> Result<bool> {
    let mut blocked = false;
    let done = wait_launched(steam_dir, log_from, handed_url, timeout, || {
        if answered() {
            return true;
        }
        blocked = vrsettings.is_some_and(setup::alvr_blocked);
        blocked
    })?;
    if blocked {
        bail!("{ALVR_BLOCKED}{advice}");
    }
    Ok(done)
}

/// wait_launched with its (URL_TAKEN_WAIT, DIALOG_GRACE), which tests shorten.
fn wait_launched_within(
    (url_wait, grace): (Duration, Duration),
    steam_dir: Option<&Path>,
    log_from: u64,
    handed_url: Option<&str>,
    timeout: Duration,
    mut done: impl FnMut() -> bool,
) -> Result<bool> {
    let (mut since, mut blocked) = (None, None);
    let (began, mut not_taken, mut taken) = (Instant::now(), None, false);
    let finished = wait_for(timeout, || {
        if done() {
            return true;
        }
        // one read per poll serves both the URL check and the dialog check
        let log = steam_dir.map(|d| console_log_since(d, log_from));
        if let (Some(d), Some(log), Some(url)) = (steam_dir, &log, handed_url) {
            // shortcut: counts from the hand-off, so a Steam still starting up (login, self-update) can miss it; count from its startup line if that shows up in reports
            not_taken = url_check(&mut taken, log, began.elapsed() >= url_wait, url, || log_tail(d, "console_log", 200).join("\n"));
            if not_taken.is_some() {
                return true;
            }
        }
        let pending = log.as_deref().and_then(pending_interstitial);
        let started = *since.get_or_insert_with(Instant::now);
        if pending.is_none() {
            since = None;
        }
        blocked = pending.filter(|_| started.elapsed() >= grace);
        blocked.is_some()
    });
    if let Some(msg) = not_taken {
        bail!("failed: {msg}");
    }
    if let Some((id, step)) = blocked {
        bail!("failed: Steam is waiting for a dialog on the desktop ({step} for AppID {id}); dismiss it and retry");
    }
    Ok(finished)
}

/// Host variables a desktop Steam needs; the rest of the MCP's environment (API keys and other
/// secrets of the agent host) never reaches Steam or the games it starts.
const DESKTOP_ENV: [&str; 12] = [
    "HOME", "USER", "LOGNAME", "SHELL", "PATH", "LANG", "LANGUAGE", "DISPLAY", "WAYLAND_DISPLAY", "XAUTHORITY",
    "DBUS_SESSION_BUS_ADDRESS", "PULSE_SERVER",
];

/// Steam's environment: the desktop-session variables of `host`, then `extra` (steam.env) over them.
pub fn steam_env(host: impl IntoIterator<Item = (OsString, OsString)>, extra: &BTreeMap<String, String>) -> Vec<(OsString, OsString)> {
    let desktop = |k: &str| DESKTOP_ENV.contains(&k) || k.starts_with("LC_") || k.starts_with("XDG_");
    let mut env: BTreeMap<OsString, OsString> = host.into_iter().filter(|(k, _)| k.to_str().is_some_and(desktop)).collect();
    env.extend(extra.iter().map(|(k, v)| (k.into(), v.into())));
    env.into_iter().collect()
}

/// Runs `steam <url>` with only the desktop-session environment. Only when Steam is not running yet
/// does it get `steam.env` and `steam.args`, because a running Steam just forwards the URL. True if this started Steam.
pub fn launch(cfg: &Config, url: &str) -> Result<bool> {
    if cfg!(test) {
        bail!("unit tests never run steam"); // it would start or shut down the user's Steam and SteamVR
    }
    let start_steam = !is_steam_running();
    let no_extra = BTreeMap::new();
    let mut command = Command::new("steam");
    command.env_clear().envs(steam_env(std::env::vars_os(), if start_steam { &cfg.steam.env } else { &no_extra }));
    if start_steam {
        command.args(&cfg.steam.args);
    }
    let mut child = command
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("running steam")?;
    // port of the dashboard's spawn_and_reap: an unreaped zombie blocks vrmonitor
    thread::spawn(move || child.wait());
    Ok(start_steam)
}

/// The last `lines` lines of SteamVR's own log `<steam>/logs/<name>.txt`; none when it is unreadable.
pub fn log_tail(steam_dir: &Path, name: &str, lines: usize) -> Vec<String> {
    let Ok(bytes) = fs::read(steam_dir.join("logs").join(format!("{name}.txt"))) else { return vec![] };
    let text = String::from_utf8_lossy(&bytes);
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].iter().map(|l| l.to_string()).collect()
}

/// Creates `dir` (mode 0700) if absent, then requires a real directory owned by this user that no
/// one else can write (an older build made it 0755): the runtime dir may fall back to the shared temp dir.
pub fn private_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    fs::DirBuilder::new().recursive(true).mode(0o700).create(dir).with_context(|| dir.display().to_string())?;
    let meta = fs::symlink_metadata(dir).with_context(|| dir.display().to_string())?;
    if !meta.is_dir() || meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o022 != 0 {
        bail!("{} must be a directory of this user that only it can write, not mode {:o}; remove it", dir.display(), meta.mode() & 0o7777);
    }
    Ok(())
}

/// Starts this binary as `alvr_dashboard` through a link in `runtime_dir`. ALVR creates the
/// headset at SteamVR start only while a process of that name exists.
pub fn spawn_dashboard_child(runtime_dir: &Path) -> Result<Child> {
    private_dir(runtime_dir)?;
    let link = runtime_dir.join(alvr_filesystem::dashboard_fname());
    match fs::remove_file(&link) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e).context(link.display().to_string()),
        _ => (),
    }
    // the running image itself, even after a rebuild replaced the file on disk
    std::os::unix::fs::symlink(format!("/proc/{}/exe", std::process::id()), &link)?;
    Command::new(&link)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("spawning the alvr_dashboard child")
}

/// The dashboard child's whole life: exist until the parent process goes away. Not
/// PR_SET_PDEATHSIG, which fires when the spawning thread exits.
pub fn run_dashboard_child() -> ! {
    let parent = unsafe { libc::getppid() };
    while parent != 1 && unsafe { libc::getppid() } == parent {
        thread::sleep(Duration::from_secs(1));
    }
    std::process::exit(0)
}

/// The dashboard's restart: ALVR shuts SteamVR down cleanly, then SteamVR is launched again. The
/// wait for the relaunch takes up to `timeout` and ends early once `stopped`; `started_steam` is set when the relaunch started Steam.
pub fn restart_steamvr(
    cfg: &Config, paths: &Paths, api: &Api, procs: &VrProcs, timeout: Duration, stopped: impl Fn() -> bool, started_steam: &mut bool,
) -> Result<()> {
    let vrsettings = paths.vrsettings_file().context("Steam not found; set steam_dir in config.toml")?;
    let old = pids_named("vrserver");
    let snapshot = procs.snapshot();
    // on failure (also when Api refuses a foreign ALVR) the wait and signals below still stop our SteamVR, so it is always relaunched
    if let Err(e) = api.post("/api/steamvr/restart") {
        error!("{e:#}");
    }
    // as in Stack::stop(): ALVR's shutdown hangs on a stream thread spinning on its client's EOF
    // (sockets/src/stream_socket/tcp.rs:137-147), so escalate to signals. Every SteamVR process
    // must be gone: a steamtours that outlives vrserver keeps Steam's AppID 250820 running, and
    // Steam then refuses the relaunch (console_log: WaitingPrevProcess, AppError_16)
    if !procs.end(&snapshot).gone {
        bail!("SteamVR processes still running after SIGKILL: {:?}", procs.remaining(&snapshot));
    }
    // SteamVR has exited, so it no longer rewrites the file; a failure only leaves other drivers
    // blocked, so log it and relaunch anyway
    setup::undo_safe_mode_blocks(paths).map_err(|e| error!("{e:#}")).ok();
    setup::unblock_safe_mode(&vrsettings)?;
    if stopped() {
        bail!("failed: vr_stop ended the SteamVR restart");
    }
    let log_from = paths.steam_dir.as_deref().map_or(0, console_log_len);
    *started_steam = launch(cfg, STEAMVR_URL)?;
    let launched = Instant::now();
    let handed = (!*started_steam).then_some(STEAMVR_URL);
    // the old server can still answer ping while it hangs in shutdown
    let (mut seen, mut exited) = (Vec::new(), None);
    let relaunched = || {
        exited = vr_exited(&mut seen);
        stopped() || exited.is_some() || (is_new_vrserver(&old, &pids_named("vrserver")) && api.ping())
    };
    let advice = "; run vr_stop, which clears the block once SteamVR is gone, then vr_start";
    if !wait_alvr(Some(&vrsettings), paths.steam_dir.as_deref(), log_from, handed, timeout, advice, relaunched)? {
        bail!("ALVR did not come back after the SteamVR restart (deadline {} s)", timeout.as_secs());
    }
    if stopped() {
        procs.settle(launched); // the stop waiting for this restart then finds the SteamVR Steam starts
        bail!("failed: vr_stop ended the SteamVR restart");
    }
    if let Some(name) = exited {
        bail!("failed: {name} exited before ALVR came back after the SteamVR restart");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Binary VDF as Steam writes shortcuts.vdf: one entry per (appid, AppName), plus a nested tags map.
    fn shortcuts_vdf(entries: &[(u32, &str)]) -> Vec<u8> {
        let mut b = b"\x00shortcuts\x00".to_vec();
        for (i, (id, name)) in entries.iter().enumerate() {
            b.extend([&[0x00][..], i.to_string().as_bytes(), b"\x00\x02appid\x00"].concat());
            b.extend((*id as i32).to_le_bytes());
            b.extend([b"\x01AppName\x00", name.as_bytes(), b"\x00\x01Exe\x00\"/x.exe\"\x00\x00tags\x00\x01", b"0\x00VR\x00\x08\x08"].concat());
        }
        b.extend(b"\x08\x08");
        b
    }

    #[test]
    fn all_pids_lists_only_this_users_processes() {
        let pids = all_pids();
        assert!(pids.contains(&std::process::id()));
        let me = unsafe { libc::getuid() };
        for pid in pids {
            // a pid that exited since the scan has no status left
            if let Ok(status) = fs::read_to_string(format!("/proc/{pid}/status")) {
                assert_eq!(real_uid(&status), Some(me), "pid {pid}: another user's Steam or SteamVR must not count");
            }
        }
    }

    #[test]
    fn running_scans_proc_once_for_all_names() {
        let me = comm(std::process::id()).unwrap();
        assert_eq!(running(&["no-such-process", me.as_str()]), vec![me.as_str()]);
        assert!(running(&["no-such-process"]).is_empty());
    }

    #[test]
    fn real_uid_is_the_first_uid_field() {
        assert_eq!(real_uid("Name:\tvrserver\nPid:\t7\nUid:\t1000\t1000\t1000\t1000\nGid:\t1000\n"), Some(1000));
        // a file-capability helper (vrcompositor-launcher) is non-dumpable: /proc shows it as root, the real uid stays ours
        assert_eq!(real_uid("Name:\tvrcompositor-la\nUid:\t1000\t0\t0\t0\n"), Some(1000));
        assert_eq!(real_uid("garbage\nUid:\tx\n"), None);
        assert_eq!(real_uid(""), None);
    }

    #[test]
    fn a_process_seen_then_gone_ends_the_wait() {
        let (now, no_grace) = (Instant::now(), Duration::ZERO);
        let mut seen = Vec::new();
        assert_eq!(gone_after_seen(&mut seen, [], |_: &str| false, now, no_grace), None, "never seen: still starting");
        assert_eq!(gone_after_seen(&mut seen, ["vrmonitor"], |n| n == "vrmonitor", now, no_grace), None);
        assert_eq!(gone_after_seen(&mut seen, ["vrmonitor", "vrserver"], |_| true, now, no_grace), None);
        assert_eq!(gone_after_seen(&mut seen, ["vrmonitor"], |n| n == "vrmonitor", now, no_grace), Some("vrserver"));
        let mut pids = Vec::new();
        assert_eq!(gone_after_seen(&mut pids, Some(42), |_| true, now, no_grace), None);
        assert_eq!(gone_after_seen(&mut pids, None, |p| p != 42, now, no_grace), Some(42), "the scene pid went away while the scene showed none");
    }

    #[test]
    fn a_brief_gap_does_not_end_the_wait() {
        let t0 = Instant::now();
        let at = |ms| t0 + Duration::from_millis(ms);
        let mut seen = Vec::new();
        assert_eq!(gone_after_seen(&mut seen, ["vrserver"], |_| true, at(0), VR_EXIT_GRACE), None);
        assert_eq!(gone_after_seen(&mut seen, [], |_| false, at(100), VR_EXIT_GRACE), None, "vrserver relaunching: a new pid follows within 0.2 s");
        assert_eq!(gone_after_seen(&mut seen, ["vrserver"], |_| true, at(300), VR_EXIT_GRACE), None);
        assert_eq!(gone_after_seen(&mut seen, [], |_| false, at(3200), VR_EXIT_GRACE), None, "the gap starts over after vrserver came back");
        assert_eq!(gone_after_seen(&mut seen, [], |_| false, at(6100), VR_EXIT_GRACE), None);
        assert_eq!(gone_after_seen(&mut seen, [], |_| false, at(6200), VR_EXIT_GRACE), Some("vrserver"), "gone for the whole grace");
    }

    #[test]
    fn stat_reads_state_and_start_time_past_the_comm() {
        let stat = "4242 (vr server) (x)) S 1 4242 4242 0 -1 4194560 100 0 0 0 5 3 0 0 20 0 12 0 987654 123456789 2000 18446744073709551615";
        assert_eq!(parse_stat(stat), Some(('S', 987654)));
        assert_eq!(parse_stat("7 (vrserver) Z 1 7"), None, "truncated");
        assert_eq!(parse_stat(""), None);
    }

    #[test]
    fn survivors_are_the_same_live_processes_whatever_their_name() {
        let stat = |pid| match pid {
            1 => Some(('S', 100)), // renamed itself, still running: pids_named misses it
            2 => Some(('Z', 200)), // exited, not yet reaped
            3 => Some(('R', 999)), // the pid now names another process
            _ => None,             // gone
        };
        assert_eq!(survivors(&[(1, 100), (2, 200), (3, 300), (4, 400)], stat), [1]);
        assert!(survivors(&[], stat).is_empty());
    }

    #[test]
    fn url_not_taken_scans_log() {
        let url = "steam://rungameid/250820";
        let taken = format!("[x] ExecCommandLine: \"{url}\"\n[x] ExecuteSteamURL: \"{url}\"\n");
        assert_eq!(url_not_taken(&taken, "", url), None);
        let msg = url_not_taken("[x] something else\nExecuteSteamURL: \"steam://rungameid/1\"\n", "", url).unwrap();
        assert_eq!(msg, format!("Steam did not take {url} (no ExecuteSteamURL in console_log.txt within 20 s); restart Steam and retry"));
        let cause = "; Steam's runtime launch service keeps failing (another Steam, e.g. Flatpak, may hold it: close that one, or start this Steam first)";
        let off = "Steam Runtime Launch Service: steam-runtime-launcher-service process exited too many times, disabling\n";
        assert!(url_not_taken("", off, url).unwrap().ends_with(cause));
        // seen live: the loop ran before the hand-off and Steam then stopped logging, so only the tail shows it
        let looping = "Steam Runtime Launch Service: steam-runtime-launcher-service pid 7 exited\n".repeat(3);
        assert!(url_not_taken("", &looping, url).unwrap().ends_with(cause));
        let once = "Steam Runtime Launch Service: steam-runtime-launcher-service pid 7 exited\n";
        assert!(!url_not_taken("", once, url).unwrap().contains("keeps failing"), "one exit is a normal SteamVR stop");
    }

    #[test]
    fn a_taken_url_stays_taken() {
        let url = "steam://rungameid/250820";
        let mut taken = false;
        assert_eq!(url_check(&mut taken, "", false, url, || panic!("tail read before the wait ran out")), None);
        assert_eq!(url_check(&mut taken, &format!("ExecuteSteamURL: \"{url}\"\n"), false, url, || panic!("tail read")), None);
        assert!(taken);
        assert_eq!(url_check(&mut taken, "", true, url, || panic!("tail read after the URL was taken")), None, "a rotated log does not undo it");
        let mut taken = false;
        assert!(url_check(&mut taken, "", true, url, String::new).unwrap().starts_with("Steam did not take"));
    }

    #[test]
    fn shortcut_game_id_matches_steam() {
        assert_eq!(shortcut_game_id(3000000000), 12884901888033554432);
        assert_eq!(shortcut_game_id(3_000_000_000), (3_000_000_000u64 << 32) | 0x0200_0000);
    }

    #[test]
    fn parse_shortcuts_reads_appid_and_name() {
        assert_eq!(parse_shortcuts(&shortcuts_vdf(&[(3000000000, "Probe")])), [(3000000000, "Probe".to_owned())]);
        let two = parse_shortcuts(&shortcuts_vdf(&[(1, "A"), (2, "B")]));
        assert_eq!(two, [(1, "A".to_owned()), (2, "B".to_owned())]);
        assert!(parse_shortcuts(b"\x00shortcuts").is_empty(), "truncated");
    }

    #[test]
    fn library_dirs_from_vdf() {
        let vdf = "\"libraryfolders\"\n{\n\t\"0\"\n\t{\n\t\t\"path\"\t\t\"/a\"\n\t\t\"label\"\t\t\"\"\n\t}\n\t\"1\"\n\t{\n\t\t\"path\"\t\t\"/b\"\n\t}\n}\n";
        assert_eq!(library_dirs(vdf), [PathBuf::from("/a"), PathBuf::from("/b")]);
    }

    #[test]
    fn libraries_list_steam_dir_first_without_duplicates() {
        let steam = fake_steam("libraries");
        let vdf = format!("\"libraryfolders\"\n{{\n\t\"0\"\n\t{{\n\t\t\"path\"\t\t\"{0}\"\n\t}}\n\t\"1\"\n\t{{\n\t\t\"path\"\t\t\"/b\"\n\t}}\n}}\n", steam.display());
        fs::write(steam.join("steamapps/libraryfolders.vdf"), vdf).unwrap();
        assert_eq!(libraries(&steam), [steam.clone(), PathBuf::from("/b")]);
        fs::remove_dir_all(&steam).ok();
    }

    const CONFIG_VDF: &str = "\"InstallConfigStore\"\n{\n\t\"Software\"\n\t{\n\t\t\"CompatToolMapping\"\n\t\t{\n\t\t\t\"620980\"\n\t\t\t{\n\t\t\t\t\"name\"\t\t\"proton_experimental\"\n\t\t\t\t\"config\"\t\t\"\"\n\t\t\t}\n\t\t\t\"0\"\n\t\t\t{\n\t\t\t\t\"name\"\t\t\"proton_9\"\n\t\t\t}\n\t\t}\n\t\t\"Other\"\n\t\t{\n\t\t\t\"7\"\n\t\t\t{\n\t\t\t\t\"name\"\t\t\"not_a_tool\"\n\t\t\t}\n\t\t}\n\t}\n}\n";

    #[test]
    fn compat_tool_lookup() {
        assert_eq!(compat_tool(CONFIG_VDF, 620980).as_deref(), Some("proton_experimental"));
        assert_eq!(compat_tool(CONFIG_VDF, 1), None);
        assert_eq!(compat_tool(CONFIG_VDF, 7), None, "outside CompatToolMapping");
        assert_eq!(compat_tool(CONFIG_VDF, 0).as_deref(), Some("proton_9"), "Steam's default");
        let nested = "\"CompatToolMapping\"\n{\n\t\"620980\"\n\t{\n\t\t\"sub\"\n\t\t{\n\t\t\t\"x\"\t\"y\"\n\t\t}\n\t\t\"name\"\t\"proton_x\"\n\t}\n}\n";
        assert_eq!(compat_tool(nested, 620980).as_deref(), Some("proton_x"), "a key after a nested block");
    }

    fn fake_steam(name: &str) -> PathBuf {
        let steam = std::env::temp_dir().join(format!("SteamVR-MCP-{name}-{}", std::process::id()));
        fs::remove_dir_all(&steam).ok();
        fs::create_dir_all(steam.join("steamapps")).unwrap();
        steam
    }

    #[test]
    fn unknown_app_is_rejected() {
        let steam = fake_steam("unknown-app");
        let e = find_app(&steam, 1).unwrap_err();
        assert_eq!(e.to_string(), "no Steam app or non-Steam shortcut with id 1");
        fs::remove_dir_all(&steam).ok();
    }

    #[test]
    fn find_app_happy_paths() {
        let steam = fake_steam("find-app");
        let lib = steam.join("lib2");
        fs::create_dir_all(lib.join("steamapps")).unwrap();
        fs::create_dir_all(steam.join("config")).unwrap();
        fs::create_dir_all(steam.join("userdata/42/config")).unwrap();
        let libraries = format!("\"libraryfolders\"\n{{\n\t\"0\"\n\t{{\n\t\t\"path\"\t\t\"{}\"\n\t}}\n}}\n", lib.display());
        fs::write(steam.join("steamapps/libraryfolders.vdf"), libraries).unwrap();
        fs::write(lib.join("steamapps/appmanifest_620980.acf"), "\"AppState\"\n{\n\t\"appid\"\t\t\"620980\"\n\t\"name\"\t\t\"Beat Saber\"\n\t\"installdir\"\t\t\"Beat Saber\"\n}\n").unwrap();
        fs::write(steam.join("config/config.vdf"), CONFIG_VDF).unwrap();
        fs::write(steam.join("userdata/42/config/shortcuts.vdf"), shortcuts_vdf(&[(3000000000, "Probe")])).unwrap();

        let steam_app = find_app(&steam, 620980).unwrap();
        assert_eq!((steam_app.app_id, steam_app.game_id, steam_app.kind), (620980, 620980, AppKind::Steam));
        assert_eq!((steam_app.name.as_str(), steam_app.compat_tool.as_deref()), ("Beat Saber", Some("proton_experimental")));
        assert_eq!(steam_app.install_dir, Some(lib.join("steamapps/common/Beat Saber")));
        assert_eq!(steam_app.compatdata, lib.join("steamapps/compatdata/620980"));
        for id in [3000000000, 12884901888033554432] {
            let shortcut = find_app(&steam, id).unwrap();
            assert_eq!((shortcut.app_id, shortcut.game_id, shortcut.kind), (3000000000, 12884901888033554432, AppKind::Shortcut), "{id}");
            assert_eq!((shortcut.name.as_str(), shortcut.compat_tool.as_deref()), ("Probe", None));
            assert_eq!((shortcut.install_dir, shortcut.compatdata), (None, steam.join("steamapps/compatdata/3000000000")));
        }
        assert!(find_app(&steam, (620980u64 << 32) | 1).is_err(), "a 64-bit id must be a shortcut game id");
        fs::remove_dir_all(&steam).ok();
    }

    #[test]
    fn interstitial_pending_until_steam_continues() {
        let waiting = "[2026-10-21 07:04:37] GameAction [AppID 620980, ActionID 70] : LaunchApp waiting for user response to ShowInterstitials \"\"\n";
        let continues = "[2026-10-21 07:04:37] GameAction [AppID 620980, ActionID 70] : LaunchApp continues with user response \"ShowInterstitials\"\n";
        let other = "[2026-10-21 07:04:38] GameAction [AppID 250820, ActionID 71] : LaunchApp waiting for user response to CreatingProcess \"\"\n";
        assert_eq!(pending_interstitial(waiting), Some((620980, "ShowInterstitials")));
        let option = "[2026-10-17 09:17:28] GameAction [AppID 1574820, ActionID 4] : LaunchApp waiting for user response to ShowLaunchOption \"\"\n";
        assert_eq!(pending_interstitial(&[waiting, continues, option].concat()), Some((1574820, "ShowLaunchOption")));
        assert_eq!(pending_interstitial(&[waiting, continues].concat()), None, "Steam logs every launch's interstitial step, and continues at once");
        let sync = "[2026-11-05 00:06:06] GameAction [AppID 250820, ActionID 1] : LaunchApp waiting for user response to SynchronizingCloud \"syncfailed\"\n";
        assert_eq!(pending_interstitial(sync), Some((250820, "SynchronizingCloud")), "Steam's cloud sync failed dialog");
        assert_eq!(pending_interstitial(other), None);
        assert_eq!(pending_interstitial(""), None);
    }

    #[test]
    fn console_log_since_reads_only_new_lines() {
        let steam = fake_steam("console-log");
        fs::create_dir_all(steam.join("logs")).unwrap();
        fs::write(steam.join("logs/console_log.txt"), "old\n").unwrap();
        let from = console_log_len(&steam);
        fs::write(steam.join("logs/console_log.txt"), "old\nnew\n").unwrap();
        assert_eq!(console_log_since(&steam, from), "new\n");
        fs::write(steam.join("logs/console_log.txt"), "x\n").unwrap();
        assert_eq!(console_log_since(&steam, from), "x\n", "a rotated log is read from its start");
        fs::remove_dir_all(&steam).ok();
    }

    /// A fake Steam with a console log, its length before the launch, and a ShowInterstitials line of `action`.
    fn launch_log(name: &str) -> (PathBuf, u64, impl Fn(u32, &str)) {
        let steam = fake_steam(name);
        fs::create_dir_all(steam.join("logs")).unwrap();
        let log = steam.join("logs/console_log.txt");
        fs::write(&log, "before the launch\n").unwrap();
        let append = move |action: u32, what: &str| {
            use std::io::Write;
            let line = format!("[x] GameAction [AppID 620980, ActionID {action}] : LaunchApp {what}\n");
            fs::OpenOptions::new().append(true).open(&log).unwrap().write_all(line.as_bytes()).unwrap();
        };
        (steam.clone(), console_log_len(&steam), append)
    }
    const WAITING: &str = "waiting for user response to ShowInterstitials \"\"";
    const CONTINUES: &str = "continues with user response \"ShowInterstitials\"";

    #[test]
    fn a_dialog_fails_the_launch_after_a_grace_that_restarts_when_it_clears() {
        let (steam, from, append) = launch_log("wait-launched-grace");
        let polls = std::cell::Cell::new(0);
        // done() runs before each poll's log read, so the log changes at known polls whatever the load
        let done = || {
            polls.set(polls.get() + 1);
            match polls.get() {
                1 => append(1, WAITING),
                2 => append(1, CONTINUES),
                3 => append(2, WAITING),
                _ => (),
            }
            false
        };
        // polls are >= 100 ms apart: poll 3 is >= 200 ms after the first wait, under the grace after the second
        let grace = Duration::from_millis(150);
        let e = wait_launched_within((URL_TAKEN_WAIT, grace), Some(&steam), from, None, Duration::from_secs(10), done).unwrap_err();
        assert!(e.to_string().contains("dialog on the desktop (ShowInterstitials for AppID 620980)"), "{e:#}");
        assert!((4..=5).contains(&polls.get()), "the grace counts from the second wait, at poll 3: failed at poll {}", polls.get());
        fs::remove_dir_all(&steam).ok();
    }

    #[test]
    fn launch_wait_outcomes_rank_done_then_url_then_dialog() {
        let (steam, from, append) = launch_log("wait-launched-rank");
        append(3, WAITING);
        let (now, timeout) = ((Duration::ZERO, Duration::ZERO), Duration::from_secs(10));
        assert!(wait_launched_within(now, Some(&steam), from, None, timeout, || true).unwrap(), "done wins over a dialog");
        let e = wait_launched_within(now, Some(&steam), from, Some("steam://rungameid/1"), timeout, || false).unwrap_err();
        assert!(e.to_string().contains("Steam did not take steam://rungameid/1"), "an untaken URL wins over the dialog: {e:#}");
        let e = wait_launched_within(now, Some(&steam), from, None, timeout, || false).unwrap_err();
        assert!(e.to_string().contains("waiting for a dialog"), "{e:#}");
        append(3, CONTINUES);
        assert!(!wait_launched_within(now, Some(&steam), from, None, Duration::from_millis(250), || false).unwrap(), "a cleared dialog only times out");
        fs::remove_dir_all(&steam).ok();
    }

    #[test]
    fn leftover_vr_processes_need_cleanup() {
        assert!(has_leftovers(&["steamtours", "vrmonitor"]), "Steam refuses the relaunch: WaitingPrevProcess");
        assert!(!has_leftovers(&[]));
        assert!(!has_leftovers(&["vrserver", "steamtours"]), "a running SteamVR is adopted, not killed");
    }

    #[test]
    fn read_from_starts_over_on_another_file() {
        let old = (100, Some((1, 7)));
        assert_eq!(read_from(old, (150, Some((1, 7)))), 100, "same file, grown");
        assert_eq!(read_from(old, (150, Some((1, 8)))), 0, "rotated, and the new file already outgrew the old");
        assert_eq!(read_from(old, (50, Some((1, 7)))), 0, "truncated");
        assert_eq!(read_from((0, None), (150, Some((1, 8)))), 0, "missing before the launch");
        assert_eq!(read_from((100, None), (150, Some((1, 8)))), 100, "no identity taken: length only");
    }

    #[test]
    fn log_tail_keeps_the_last_lines() {
        let steam = std::env::temp_dir().join(format!("SteamVR-MCP-log-tail-{}", std::process::id()));
        fs::create_dir_all(steam.join("logs")).unwrap();
        fs::write(steam.join("logs/vrserver.txt"), "a\nb\nc\n").unwrap();
        assert_eq!(log_tail(&steam, "vrserver", 2), ["b", "c"]);
        assert_eq!(log_tail(&steam, "vrserver", 9), ["a", "b", "c"]);
        assert!(log_tail(&steam, "vrmonitor", 2).is_empty(), "missing file");
        fs::remove_dir_all(&steam).ok();
    }

    #[test]
    fn relaunch_needs_a_new_vrserver() {
        assert!(!is_new_vrserver(&[10], &[10]), "the old, wedged server still answers");
        assert!(!is_new_vrserver(&[10], &[]));
        assert!(is_new_vrserver(&[10], &[11]));
        assert!(is_new_vrserver(&[], &[11]));
    }

    #[test]
    fn steam_gets_only_desktop_session_env() {
        let host = [("HOME", "/h"), ("LC_ALL", "C"), ("XDG_RUNTIME_DIR", "/r"), ("DISPLAY", ":0"), ("ANTHROPIC_API_KEY", "secret"), ("GH_TOKEN", "t"), ("PATH", "/bin")]
            .map(|(k, v)| (k.into(), v.into()));
        let extra = [("MESA_VK_DEVICE_SELECT".to_owned(), "x!".to_owned()), ("PATH".to_owned(), "/steam".to_owned())].into();
        let env: BTreeMap<_, _> = steam_env(host, &extra).into_iter().collect();
        let want = [("DISPLAY", ":0"), ("HOME", "/h"), ("LC_ALL", "C"), ("MESA_VK_DEVICE_SELECT", "x!"), ("PATH", "/steam"), ("XDG_RUNTIME_DIR", "/r")];
        assert_eq!(env, want.map(|(k, v)| (k.into(), v.into())).into(), "steam.env wins over the host");
    }

    #[test]
    fn only_single_processes_are_signalled() {
        assert_eq!(signalable(0), None, "0 is our own process group");
        assert_eq!(signalable(i32::MAX as u32 + 1), None, "negative as i32: a process group");
        assert_eq!(signalable(u32::MAX), None, "-1: every process");
        let (me, group) = (std::process::id(), unsafe { libc::getpgrp() } as u32);
        let other = (4242..).find(|p| ![me, group].contains(p)).unwrap();
        assert_eq!(signalable(other), Some(other as i32));
        assert_eq!(signalable(1), None, "init");
        assert_eq!(signalable(std::process::id()), None, "this MCP");
        assert_eq!(signalable(unsafe { libc::getpgrp() } as u32), None, "our group's leader, e.g. the agent that started the MCP");
    }

    #[test]
    fn runtime_dir_must_be_private() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let root = std::env::temp_dir().join(format!("SteamVR-MCP-runtime-{}", std::process::id()));
        let dir = root.join("SteamVR-MCP");
        private_dir(&dir).unwrap();
        assert_eq!(fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        private_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        private_dir(&dir).expect("an older build made it 0755: ours, and no one else can write it");
        for mode in [0o775, 0o757] {
            fs::set_permissions(&dir, fs::Permissions::from_mode(mode)).unwrap();
            assert!(private_dir(&dir).unwrap_err().to_string().contains(&format!("{mode:o}")), "others could swap the link");
        }
        symlink(&root, root.join("link")).unwrap();
        assert!(private_dir(&root.join("link")).is_err(), "a symlink is not ours to trust");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn mapped_files_come_from_proc_maps() {
        assert!(has_mapped(std::process::id(), "libc.so"));
        assert!(!has_mapped(std::process::id(), "driver_alvr_server.so"));
        assert!(!has_mapped(0, "libc.so"), "no such process");
    }

    #[test]
    fn a_process_is_named_by_comm_or_argv0() {
        let vrserver = b"/s/SteamVR/bin/linux64/vrserver\0-waitformonitor\0-ipccontrolfilepath\0/tmp/SteamVR-IPCControlFile-1153238\0";
        assert!(is_named("vrserver", "1153238: vrstar", vrserver), "vrserver renames its main thread; argv0 still names it");
        assert!(is_named("vrstartup.sh", "vrstartup.sh", b"/bin/bash\0/s/SteamVR/bin/vrstartup.sh\0"), "a script: comm names it, argv0 is the shell");
        assert!(!is_named("vrserver", "tail", b"/usr/bin/tail\0-f\0/s/SteamVR/bin/linux64/vrserver\0"), "vrserver only in a later argument");
        assert!(!is_named("vrserver", "vrserver-x", b"/opt/vrserver-x\0"));
        assert!(is_named("kworker/0:1", "kworker/0:1", b""), "a kernel thread or zombie has an empty cmdline: comm alone");
        assert!(!is_named("vrserver", "kworker/0:1", b""));
    }

    #[test]
    fn pids_named_finds_a_process_that_renamed_itself() {
        let mut child = Command::new("sh").args(["-c", "printf renamed > /proc/self/comm; read x"]).stdin(Stdio::piped()).spawn().unwrap();
        let pid = child.id();
        assert!(wait_for(Duration::from_secs(2), || comm(pid).as_deref() == Some("renamed")));
        let found = pids_named("sh").contains(&pid);
        child.kill().ok();
        child.wait().ok();
        assert!(found, "argv0 still names it");
    }

    #[test]
    fn pids_named_finds_self() {
        let comm = std::fs::read_to_string("/proc/self/comm").unwrap();
        assert!(pids_named(comm.trim()).contains(&std::process::id()));
    }

    #[test]
    fn env_mismatches_name_missing_and_changed_keys() {
        let want: BTreeMap<String, String> = [("A", "1"), ("B", "2"), ("C", "3")].map(|(k, v)| (k.into(), v.into())).into();
        assert!(env_mismatches(b"A=1\0B=2\0C=3\0X=y", &want).is_empty());
        assert_eq!(env_mismatches(b"A=1\0B=2", &want), ["C=3 but has missing"]);
        assert_eq!(env_mismatches(b"A=1\0B=9\0C=3", &want), ["B=2 but has 9"]);
        // a non-UTF-8 entry elsewhere must not hide a match or a mismatch
        assert_eq!(env_mismatches(b"JUNK=\xff\xfe\0A=1\0B=2\0C=0", &want), ["C=3 but has 0"]);
    }

    /// A temp steamvr.vrsettings holding `json`.
    fn vrsettings(name: &str, json: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("SteamVR-MCP-steamvr-{name}-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("steamvr.vrsettings"), json).unwrap();
        dir.join("steamvr.vrsettings")
    }

    #[test]
    fn a_block_written_during_the_wait_fails_at_once() {
        let file = vrsettings("block-wait", "{}");
        let mut polls = 0;
        let answered = || {
            polls += 1;
            if polls == 3 {
                fs::write(&file, r#"{"driver_alvr_server":{"blocked_by_safe_mode":true}}"#).unwrap();
            }
            false
        };
        let began = Instant::now();
        let e = wait_alvr(Some(&file), None, 0, None, Duration::from_secs(30), "; then retry", answered).unwrap_err();
        assert_eq!(e.to_string(), format!("{ALVR_BLOCKED}; then retry"));
        assert!(began.elapsed() < Duration::from_secs(2), "{:?}", began.elapsed());
        fs::remove_dir_all(file.parent().unwrap()).ok();
    }

    #[test]
    fn an_answering_alvr_beats_a_stale_block() {
        let file = vrsettings("block-stale", r#"{"driver_alvr_server":{"blocked_by_safe_mode":true}}"#);
        assert!(wait_alvr(Some(&file), None, 0, None, Duration::from_secs(5), "", || true).unwrap());
        fs::remove_dir_all(file.parent().unwrap()).ok();
    }

    #[test]
    fn another_drivers_block_does_not_fail_the_wait() {
        let file = vrsettings("block-other", r#"{"driver_lighthouse":{"blocked_by_safe_mode":true}}"#);
        assert!(!wait_alvr(Some(&file), None, 0, None, Duration::from_millis(300), "", || false).unwrap());
        fs::remove_dir_all(file.parent().unwrap()).ok();
    }

    #[test]
    fn a_restart_that_vr_stop_ended_never_relaunches() {
        let root = std::env::temp_dir().join(format!("SteamVR-MCP-steamvr-restart-{}", std::process::id()));
        fs::remove_dir_all(&root).ok();
        fs::create_dir_all(root.join("steam/config")).unwrap();
        fs::write(root.join("steam/config/steamvr.vrsettings"), "{}").unwrap();
        let paths = Paths { steam_dir: Some(root.join("steam")), flatpak_steam: false, steamvr_dir: None, streamer_dir: None, state_dir: root.join("state"), runtime_dir: root.join("run") };
        let e = restart_steamvr(&Config::default(), &paths, &Api { port: 1 }, &VrProcs::NONE, Duration::from_secs(1), || true, &mut false).unwrap_err();
        assert_eq!(e.to_string(), "failed: vr_stop ended the SteamVR restart", "stopped before launch(), which unit tests refuse");
        fs::remove_dir_all(&root).ok();
    }
}
