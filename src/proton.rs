//! `launch_app via proton`: run a Steam game's compatibility tool the way Steam does, and find
//! every process of that run again to stop it.
use crate::steamvr::{self, AppInfo, STEAMVR_APP_ID, VR_PROCESS_NAMES, compat_tool, entry_value, find_app, libraries, signalable, vdf_value};
use alvr_common::{
    anyhow::{Context, Result, bail},
    parking_lot::Mutex,
};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs,
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    sync::Arc,
    thread,
    time::Duration,
};

const VERB: &str = "waitforexitandrun";

/// Whether Steam's internal name of an official Proton (`proton_9`) names this `steamapps/common` dir
/// (`Proton 9.0`): equal once lowercased and kept to letters and digits, or the dir with one more `0`.
pub fn official_tool_matches(name: &str, installdir: &str) -> bool {
    let norm = |s: &str| s.to_lowercase().chars().filter(char::is_ascii_alphanumeric).collect::<String>();
    let (name, dir) = (norm(name), norm(installdir));
    dir == name || dir == name + "0"
}

/// The `install_path` of the tool with internal name `name` in a compatibilitytool.vdf.
pub fn custom_tool_path(compatibilitytool_vdf: &str, name: &str) -> Option<String> {
    entry_value(compatibilitytool_vdf, "compat_tools", name, "install_path").map(str::to_owned)
}

/// The dir of the compatibility tool Steam's config.vdf calls `name`.
// shortcut: only tools in a subdir of <steam>/compatibilitytools.d, add /usr/share/steam/compatibilitytools.d and top-level vdfs when a user has one
pub fn tool_dir(steam_dir: &Path, name: &str) -> Result<PathBuf> {
    for dir in fs::read_dir(steam_dir.join("compatibilitytools.d")).into_iter().flatten().flatten() {
        let Ok(vdf) = fs::read_to_string(dir.path().join("compatibilitytool.vdf")) else { continue };
        if let Some(path) = custom_tool_path(&vdf, name) {
            // components() drops the "." of `install_path "."`
            return Ok(dir.path().join(path).components().collect());
        }
    }
    for lib in libraries(steam_dir) {
        for dir in fs::read_dir(lib.join("steamapps/common")).into_iter().flatten().flatten() {
            let path = dir.path();
            if path.join("proton").is_file() && path.join("toolmanifest.vdf").is_file() && dir.file_name().to_str().is_some_and(|d| official_tool_matches(name, d)) {
                return Ok(path);
            }
        }
    }
    bail!("compatibility tool {name:?} is in neither <steam>/compatibilitytools.d nor any library's steamapps/common")
}

/// The tool's command as Steam runs it: toolmanifest.vdf's `commandline`, inside the Steam Linux
/// Runtime's `_v2-entry-point` when the manifest requires an installed runtime.
pub fn tool_argv(steam_dir: &Path, tool_dir: &Path) -> Result<Vec<OsString>> {
    let path = tool_dir.join("toolmanifest.vdf");
    let manifest = fs::read_to_string(&path).with_context(|| path.display().to_string())?;
    let line = vdf_value(&manifest, "commandline").with_context(|| format!("{} has no commandline", path.display()))?;
    let mut words = line.split_whitespace().map(|w| if w == "%verb%" { VERB } else { w });
    let program = words.next().with_context(|| format!("{} has an empty commandline", path.display()))?;
    let mut argv: Vec<OsString> = vec![tool_dir.join(program.trim_start_matches('/')).into()];
    argv.extend(words.map(OsString::from));
    let entry_point = vdf_value(&manifest, "require_tool_appid")
        .and_then(|id| find_app(steam_dir, id.parse().ok()?).ok()?.install_dir)
        .map(|runtime| runtime.join("_v2-entry-point"))
        .filter(|p| p.is_file());
    if let Some(entry_point) = entry_point {
        argv.splice(0..0, [entry_point.into(), format!("--verb={VERB}").into(), "--".into()]);
    }
    Ok(argv)
}

/// `exe` (relative, `\` allowed) as a file inside `install_dir` once symlinks are resolved.
pub fn resolve_exe(install_dir: &Path, exe: &str) -> Result<PathBuf> {
    let relative = exe.replace('\\', "/");
    if Path::new(&relative).is_absolute() {
        bail!("exe {exe:?} must be relative to the install dir");
    }
    let root = install_dir.canonicalize().with_context(|| install_dir.display().to_string())?;
    let path = root.join(&relative).canonicalize().with_context(|| format!("exe {exe:?} not found under {}", root.display()))?;
    if !path.starts_with(&root) {
        bail!("exe {exe:?} resolves to {}, outside the install dir {}", path.display(), root.display());
    }
    if !path.is_file() {
        bail!("exe {exe:?} is not a file");
    }
    // Wine runs any file it is handed: a .cmd through cmd.exe /c, a Linux ELF or script by fork/exec
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_ascii_lowercase();
    let Some(stem) = name.strip_suffix(".exe") else { bail!("exe {exe:?} must end in .exe: Wine runs other files as scripts or Linux programs") };
    let mut magic = [0; 2];
    if fs::File::open(&path).and_then(|mut f| std::io::Read::read_exact(&mut f, &mut magic)).is_err() || &magic != b"MZ" {
        bail!("exe {exe:?} does not start with MZ, so it is not a Windows executable");
    }
    if INTERPRETERS.contains(&stem) || ["python", "java", "wine"].iter().any(|p| stem.starts_with(p)) {
        bail!("exe {exe:?} is a shell, script interpreter or loader ({stem}.exe), which runs whatever its args name");
    }
    Ok(path)
}

/// Exe stems that run a command, script or other program named in their args.
// shortcut: a game exe that itself interprets its args (a console, a mod loader) still runs what they name; add a per-app exe allowlist in config.toml if one shows up
const INTERPRETERS: [&str; 11] = ["cmd", "powershell", "pwsh", "wscript", "cscript", "mshta", "rundll32", "regsvr32", "node", "start", "explorer"];

#[derive(Debug)]
pub struct ProtonLaunch {
    pub argv: Vec<OsString>,
    pub env: Vec<(OsString, OsString)>,
    pub cwd: PathBuf,
    pub compatdata: PathBuf,
    pub app_id: u32,
}

/// How to run `exe` of a Steam app through its compatibility tool: the app's CompatToolMapping
/// entry, else Steam's default ("0"). Env: `base_env`, `caller_env`, then the app ids and STEAM_COMPAT_*
/// paths, which the caller cannot change since stop_app finds the run by them.
pub fn plan(steam_dir: &Path, app: &AppInfo, exe: &str, args: &[String], base_env: Vec<(OsString, OsString)>, caller_env: &BTreeMap<String, String>) -> Result<ProtonLaunch> {
    let install = app.install_dir.as_deref().with_context(|| format!("{} has no installdir in its appmanifest", app.name))?;
    if install.join("toolmanifest.vdf").exists() {
        bail!("{} is a compatibility tool or runtime (its dir has a toolmanifest.vdf), not a game", app.name);
    }
    let exe = resolve_exe(install, exe)?;
    let config = fs::read_to_string(steam_dir.join("config/config.vdf")).unwrap_or_default();
    let tool = app.compat_tool.clone().or_else(|| compat_tool(&config, 0));
    let tool = tool.with_context(|| format!("{} has no compatibility tool in Steam's config.vdf, nor does Steam's default", app.name))?;
    let tool = tool_dir(steam_dir, &tool)?;
    let mut argv = tool_argv(steam_dir, &tool)?;
    let mut tool_paths = tool.into_os_string();
    let program = Path::new(&argv[0]);
    if let Some(runtime) = program.parent().filter(|_| program.ends_with("_v2-entry-point")) {
        tool_paths.push(":");
        tool_paths.push(runtime);
    }
    let cwd = exe.parent().context("exe has no dir")?.to_path_buf();
    argv.push(exe.into());
    argv.extend(args.iter().map(OsString::from));
    let mut libs = OsString::new();
    for (i, lib) in libraries(steam_dir).into_iter().enumerate() {
        libs.push(if i == 0 { "" } else { ":" });
        libs.push(lib.join("steamapps"));
    }
    let id = OsString::from(app.app_id.to_string());
    let mut env: BTreeMap<OsString, OsString> = base_env.into_iter().collect();
    env.extend(caller_env.iter().map(|(k, v)| (k.into(), v.into())));
    env.extend([
        ("SteamAppId".into(), id.clone()),
        ("SteamGameId".into(), app.game_id.to_string().into()),
        ("STEAM_COMPAT_DATA_PATH".into(), app.compatdata.clone().into()),
        ("STEAM_COMPAT_CLIENT_INSTALL_PATH".into(), steam_dir.into()),
        ("STEAM_COMPAT_INSTALL_PATH".into(), install.into()),
        ("STEAM_COMPAT_LIBRARY_PATHS".into(), libs),
        ("STEAM_COMPAT_TOOL_PATHS".into(), tool_paths),
        ("STEAM_COMPAT_APP_ID".into(), id),
    ]);
    Ok(ProtonLaunch { argv, env: env.into_iter().collect(), cwd, compatdata: app.compatdata.clone(), app_id: app.app_id })
}

/// A spawned launcher: the leader of its own process group.
#[derive(Clone, Debug)]
pub struct Launched {
    pub pgid: u32,
    pub compatdata: PathBuf,
    pub app_id: u32,
    /// The leader's exit status once reaped. While None the leader is unreaped, so its pid, the
    /// pgid, cannot name another group yet.
    pub exited: Arc<Mutex<Option<ExitStatus>>>,
}

impl Launched {
    /// signal_group while the leader is unreaped; once reaped the pgid may name someone else's group.
    // shortcut: group members that outlive the leader are reached only by the STEAM_COMPAT_DATA_PATH sweep, compare start times if one without it lingers
    pub fn signal(&self, sig: i32) -> bool {
        let exited = self.exited.lock(); // held across kill(), so the reaper cannot reap in between
        exited.is_none() && signal_group(self.pgid, sig)
    }
}

/// Starts `l` in a new process group with only its env, stdout and stderr to `log` (truncated).
pub fn spawn(l: &ProtonLaunch, log: &Path) -> Result<Launched> {
    let out = fs::File::create(log).with_context(|| log.display().to_string())?;
    let (program, args) = l.argv.split_first().context("empty argv")?;
    let mut child = Command::new(program)
        .args(args)
        .env_clear()
        .envs(l.env.iter().cloned())
        .current_dir(&l.cwd)
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(out.try_clone()?)
        .stderr(out)
        .spawn()
        .with_context(|| format!("running {}", Path::new(program).display()))?;
    let exited = Arc::new(Mutex::new(None));
    let (pid, status) = (child.id(), exited.clone());
    thread::spawn(move || {
        // wait without reaping, then reap under the lock: Launched::signal never races a reused pgid
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        while unsafe { libc::waitid(libc::P_PID, pid, &mut info, libc::WEXITED | libc::WNOWAIT) } == -1
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR)
        {}
        let mut status = status.lock();
        *status = child.wait().ok();
    });
    Ok(Launched { pgid: pid, compatdata: l.compatdata.clone(), app_id: l.app_id, exited })
}

const COMPAT_DATA: &str = "STEAM_COMPAT_DATA_PATH";
const APP_ID_KEYS: [&str; 2] = ["STEAM_COMPAT_APP_ID", "SteamGameId"];

/// SIGTERM to the scene pid, to `launched`'s group, and to every process of the game's trees
/// (game_pids of `launched` and of the scene pid's own env): wine processes leave the group.
/// SIGKILL after 15 s. The pids and group signalled, sorted; none when nothing ran.
pub fn stop(scene: Option<u32>, launched: Option<&Launched>) -> Result<Vec<u32>> {
    let scene = scene.filter(|&pid| !is_steamvr(pid, &fs::read(format!("/proc/{pid}/environ")).unwrap_or_default()));
    let mut games: Vec<(String, String)> = launched.and_then(|l| Some((l.compatdata.to_str()?.to_owned(), l.app_id.to_string()))).into_iter().collect();
    let scene_game = scene.and_then(|pid| Some((env_of(pid, COMPAT_DATA)?, APP_ID_KEYS.iter().find_map(|k| env_of(pid, k))?)));
    games.extend(scene_game);
    games.retain(|(dir, id)| !dir.is_empty() && !id.is_empty());
    games.dedup();
    let scene_comm = scene.and_then(steamvr::comm);
    // single pids proven to be the game's: the scene app (same comm, so not a reused pid), and its trees
    let live = || {
        let mut pids: Vec<u32> = scene.filter(|&pid| scene_comm.is_some() && steamvr::comm(pid) == scene_comm).into_iter().collect();
        games.iter().for_each(|(dir, id)| pids.extend(game_pids(dir, id)));
        pids
    };
    let signal = |sig| {
        let mut pids = live();
        pids.iter().for_each(|&pid| steamvr::terminate(pid, sig));
        pids.extend(launched.filter(|l| l.signal(sig)).map(|l| l.pgid));
        pids
    };
    let gone = || live().is_empty() && !launched.is_some_and(|l| l.signal(0));
    let mut stopped = signal(libc::SIGTERM);
    if !stopped.is_empty() && !steamvr::wait_for(Duration::from_secs(15), gone) {
        stopped.extend(signal(libc::SIGKILL));
        if !steamvr::wait_for(Duration::from_secs(5), gone) {
            bail!("still running after SIGKILL: {:?} (deadline 5 s)", live());
        }
    }
    stopped.sort_unstable();
    stopped.dedup();
    Ok(stopped)
}

/// Whether a NUL-separated environ holds exactly `key=value`.
pub fn environ_has(environ: &[u8], key: &str, value: &str) -> bool {
    let entry = format!("{key}={value}");
    environ.split(|&b| b == 0).any(|e| e == entry.as_bytes())
}

/// `key`'s value in the environ of `pid`.
pub fn env_of(pid: u32, key: &str) -> Option<String> {
    let environ = fs::read(format!("/proc/{pid}/environ")).ok()?;
    let prefix = format!("{key}=");
    let value = environ.split(|&b| b == 0).find_map(|e| e.strip_prefix(prefix.as_bytes()))?;
    String::from_utf8(value.to_vec()).ok()
}

/// This MCP and its ancestors up to (not including) init.
pub fn ancestors() -> Vec<u32> {
    std::iter::successors(Some(std::process::id()), |&pid| parent_of(pid).filter(|&p| p > 1)).collect()
}

/// The ppid field of `/proc/<pid>/stat`, read after the last `)` since comm may hold one.
pub(crate) fn parent_of(pid: u32) -> Option<u32> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(')')?.1.split_whitespace().nth(1)?.parse().ok()
}

/// Every process of a game's tree: its environ has the game's STEAM_COMPAT_DATA_PATH and its app
/// id (STEAM_COMPAT_APP_ID or SteamGameId), which a shell that only exported the path lacks. Never
/// pid 1, this MCP, its ancestors, its process group or SteamVR's processes; other users' processes (unreadable environ) are skipped.
// shortcut: skips the MCP's group, which holds a Steam the MCP started and maybe its games' processes; narrow to pids if a via-steam game's tree outlives stop_app
pub fn game_pids(compatdata: &str, app_id: &str) -> Vec<u32> {
    if app_id == STEAMVR_APP_ID.to_string() {
        return vec![];
    }
    let (group, mine) = (unsafe { libc::getpgrp() }, ancestors());
    steamvr::all_pids()
        .into_iter()
        .filter(|pid| !mine.contains(pid) && signalable(*pid).is_some_and(|p| unsafe { libc::getpgid(p) } != group))
        .filter(|pid| {
            fs::read(format!("/proc/{pid}/environ"))
                .is_ok_and(|env| environ_has(&env, COMPAT_DATA, compatdata) && APP_ID_KEYS.iter().any(|k| environ_has(&env, k, app_id)) && !is_steamvr(*pid, &env))
        })
        .collect()
}

/// SteamVR's own processes, which all carry SteamVR's compat env (app 250820): never a game's.
fn is_steamvr(pid: u32, environ: &[u8]) -> bool {
    let id = STEAMVR_APP_ID.to_string();
    APP_ID_KEYS.iter().any(|k| environ_has(environ, k, &id)) || steamvr::name_of(pid, VR_PROCESS_NAMES).is_some()
}

/// kill(-pgid, sig); false, and nothing sent, for 0, 1 and this MCP's own group.
pub fn signal_group(pgid: u32, sig: i32) -> bool {
    signalable(pgid).is_some_and(|g| unsafe { libc::kill(-g, sig) } == 0)
}

pub fn group_alive(pgid: u32) -> bool {
    signal_group(pgid, 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::steamvr::AppKind;
    use std::os::unix::process::ExitStatusExt;

    fn root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("SteamVR-MCP-proton-{name}-{}", std::process::id()));
        fs::remove_dir_all(&root).ok();
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    const MANIFEST: &str = "\"manifest\"\n{\n  \"version\" \"2\"\n  \"commandline\" \"/proton %verb%\"\n  \"require_tool_appid\" \"4183110\"\n}\n";

    /// A Steam dir with an official "Proton - Experimental" in steamapps/common.
    fn fake_steam(name: &str) -> (PathBuf, PathBuf) {
        let steam = root(name);
        let tool = steam.join("steamapps/common/Proton - Experimental");
        write(&tool.join("proton"), "");
        write(&tool.join("toolmanifest.vdf"), MANIFEST);
        (steam, tool)
    }

    /// Installs Steam Linux Runtime 4.0 (app 4183110), which MANIFEST requires.
    fn add_runtime(steam: &Path) -> PathBuf {
        let acf = "\"AppState\"\n{\n\t\"appid\"\t\t\"4183110\"\n\t\"name\"\t\t\"Steam Linux Runtime 4.0\"\n\t\"installdir\"\t\t\"SteamLinuxRuntime_4\"\n}\n";
        write(&steam.join("steamapps/appmanifest_4183110.acf"), acf);
        let runtime = steam.join("steamapps/common/SteamLinuxRuntime_4");
        write(&runtime.join("_v2-entry-point"), "");
        runtime
    }

    #[test]
    fn official_tool_name_matches_installdir() {
        let pairs = [("proton_experimental", "Proton - Experimental"), ("proton_hotfix", "Proton Hotfix"), ("proton_9", "Proton 9.0"), ("proton_10", "Proton 10.0"), ("proton_63", "Proton 6.3")];
        for (name, dir) in pairs {
            assert!(official_tool_matches(name, dir), "{name} {dir}");
        }
        assert!(!official_tool_matches("proton_9", "Proton 8.0"));
    }

    #[test]
    fn custom_tool_found_by_internal_name() {
        let steam = root("custom");
        let ge = steam.join("compatibilitytools.d/GE");
        let vdf = "\"compatibilitytools\"\n{\n  \"compat_tools\"\n  {\n    \"GE-Proton10\" // Internal name of this tool\n    {\n      // '.' when the tool is a subdirectory\n      \"install_path\" \".\"\n\n      \"display_name\" \"x\"\n    }\n  }\n}\n";
        write(&ge.join("compatibilitytool.vdf"), vdf);
        write(&ge.join("proton"), "");
        write(&ge.join("toolmanifest.vdf"), MANIFEST);
        assert_eq!(custom_tool_path(vdf, "GE-Proton10").as_deref(), Some("."));
        assert_eq!(custom_tool_path(vdf, "x"), None, "the display name is not the internal name");
        let commented = "\"compat_tools\"\n{\n  \"T\" // \"internal\" name\n  {\n    // \"install_path\" \"wrong\"\n    \"install_path\" \"a//b\" // \".\" for a subdir\n  }\n}\n";
        assert_eq!(custom_tool_path(commented, "T").as_deref(), Some("a//b"), "// starts a comment only outside quotes");
        assert_eq!(tool_dir(&steam, "GE-Proton10").unwrap().as_os_str(), ge.as_os_str());
        assert!(tool_dir(&steam, "Nope-1").unwrap_err().to_string().contains("\"Nope-1\""));
        fs::remove_dir_all(&steam).ok();
    }

    #[test]
    fn tool_argv_uses_runtime_entry_point() {
        let (steam, tool) = fake_steam("argv");
        assert_eq!(tool_dir(&steam, "proton_experimental").unwrap(), tool);
        let bare: Vec<OsString> = vec![tool.join("proton").into(), "waitforexitandrun".into()];
        assert_eq!(tool_argv(&steam, &tool).unwrap(), bare, "without the runtime installed");
        let runtime = add_runtime(&steam);
        let want: Vec<OsString> = vec![runtime.join("_v2-entry-point").into(), "--verb=waitforexitandrun".into(), "--".into(), tool.join("proton").into(), "waitforexitandrun".into()];
        assert_eq!(tool_argv(&steam, &tool).unwrap(), want);
        fs::remove_dir_all(&steam).ok();
    }

    #[test]
    fn exe_must_stay_inside_install_dir() {
        let root = root("exe");
        let install = root.join("Game");
        for f in ["Game/Game.exe", "Game/bin/Game.exe", "outside.exe"] {
            write(&root.join(f), "MZ");
        }
        std::os::unix::fs::symlink(root.join("outside.exe"), install.join("link.exe")).unwrap();
        let canon = install.canonicalize().unwrap();
        assert_eq!(resolve_exe(&install, "Game.exe").unwrap(), canon.join("Game.exe"));
        assert_eq!(resolve_exe(&install, "bin\\Game.exe").unwrap(), canon.join("bin/Game.exe"));
        for bad in ["..\\outside.exe", "../outside.exe", "bin\\..\\..\\outside.exe", "link.exe"] {
            assert!(resolve_exe(&install, bad).unwrap_err().to_string().contains("outside the install dir"), "{bad}: an existing file outside");
        }
        for abs in ["/bin/sh", "\\bin\\sh"] {
            assert!(resolve_exe(&install, abs).unwrap_err().to_string().contains("relative"), "{abs}");
        }
        assert!(resolve_exe(&install, "Nope.exe").is_err(), "missing");
        assert!(resolve_exe(&install, "bin").is_err(), "a dir");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn exe_must_be_a_windows_exe_and_no_interpreter() {
        let install = root("pe");
        let files = [
            ("Game.EXE", "MZP"),
            ("_CommonRedist/vcredist/2022/Microsoft Visual C++ 2022 x64.cmd", "MZ"),
            ("run.sh", "#!/bin/sh"),
            ("elf.exe", "\x7fELF"),
            ("empty.exe", ""),
            ("bin/CMD.exe", "MZ"),
            ("python/python311.exe", "MZ"),
            ("pythonw.exe", "MZ"),
            ("jre/bin/javaw.exe", "MZ"),
            ("wine64.exe", "MZ"),
            ("rundll32.exe", "MZ"),
            ("PowerShell.exe", "MZ"),
        ];
        for (f, text) in files {
            write(&install.join(f), text);
        }
        std::os::unix::fs::symlink(install.join("bin/CMD.exe"), install.join("Launcher.exe")).unwrap();
        assert!(resolve_exe(&install, "Game.EXE").is_ok(), "the .exe suffix ignores case");
        for not_exe in ["_CommonRedist\\vcredist\\2022\\Microsoft Visual C++ 2022 x64.cmd", "run.sh"] {
            assert!(resolve_exe(&install, not_exe).unwrap_err().to_string().contains("must end in .exe"), "{not_exe}");
        }
        for not_pe in ["elf.exe", "empty.exe"] {
            assert!(resolve_exe(&install, not_pe).unwrap_err().to_string().contains("MZ"), "{not_pe}");
        }
        for interpreter in ["bin/CMD.exe", "Launcher.exe", "python/python311.exe", "pythonw.exe", "jre/bin/javaw.exe", "wine64.exe", "rundll32.exe", "PowerShell.exe"] {
            let e = resolve_exe(&install, interpreter).unwrap_err().to_string();
            assert!(e.contains("interpreter"), "{interpreter}: {e}");
        }
        fs::remove_dir_all(&install).ok();
    }

    #[test]
    fn plan_env_has_compat_vars_home_and_caller_last() {
        let (steam, tool) = fake_steam("plan");
        let runtime = add_runtime(&steam);
        let config = "\"InstallConfigStore\"\n{\n\t\"Software\"\n\t{\n\t\t\"CompatToolMapping\"\n\t\t{\n\t\t\t\"0\"\n\t\t\t{\n\t\t\t\t\"name\"\t\t\"proton_experimental\"\n\t\t\t}\n\t\t}\n\t}\n}\n";
        write(&steam.join("config/config.vdf"), config);
        let install = steam.join("steamapps/common/Game");
        write(&install.join("Game.exe"), "MZ");
        let mut app = steamvr::AppInfo {
            app_id: 620980,
            game_id: 620980,
            kind: AppKind::Steam,
            name: "Game".into(),
            compat_tool: None,
            install_dir: Some(install.clone()),
            compatdata: steam.join("steamapps/compatdata/620980"),
        };
        let host = [("HOME", "/h"), ("XDG_CONFIG_HOME", "/h/.config"), ("SECRET_TOKEN", "s")].map(|(k, v)| (k.into(), v.into()));
        let base = steamvr::steam_env(host, &BTreeMap::new());
        let caller = [("XDG_CONFIG_HOME", "/c"), ("SteamAppId", "1"), ("STEAM_COMPAT_DATA_PATH", "/elsewhere")].map(|(k, v)| (k.to_owned(), v.to_owned())).into();
        let l = plan(&steam, &app, "Game.exe", &["-a".into()], base.clone(), &caller).unwrap();
        let env: BTreeMap<String, String> = l.env.iter().map(|(k, v)| (k.to_str().unwrap().into(), v.to_str().unwrap().into())).collect();
        assert_eq!((env["HOME"].as_str(), env["XDG_CONFIG_HOME"].as_str()), ("/h", "/c"), "caller env over the desktop's");
        assert!(!env.contains_key("SECRET_TOKEN"));
        let s = |p: &Path| p.to_str().unwrap().to_owned();
        assert_eq!(env["STEAM_COMPAT_DATA_PATH"], s(&app.compatdata), "check_launch refuses these keys; the MCP's value wins anyway: stop_app finds the game by it");
        assert_eq!(env["STEAM_COMPAT_CLIENT_INSTALL_PATH"], s(&steam));
        assert_eq!(env["STEAM_COMPAT_INSTALL_PATH"], s(&install));
        assert_eq!(env["STEAM_COMPAT_LIBRARY_PATHS"], s(&steam.join("steamapps")));
        assert_eq!(env["STEAM_COMPAT_TOOL_PATHS"], format!("{}:{}", s(&tool), s(&runtime)));
        assert_eq!((env["SteamAppId"].as_str(), env["SteamGameId"].as_str(), env["STEAM_COMPAT_APP_ID"].as_str()), ("620980", "620980", "620980"));
        assert_eq!(l.app_id, 620980);
        let canon = install.canonicalize().unwrap();
        let tail: Vec<OsString> = vec![tool.join("proton").into(), "waitforexitandrun".into(), canon.join("Game.exe").into(), "-a".into()];
        assert_eq!((&l.argv[0], &l.argv[3..]), (&runtime.join("_v2-entry-point").into_os_string(), &tail[..]));
        assert_eq!((l.cwd, l.compatdata), (canon, app.compatdata.clone()));

        app.compat_tool = Some("proton_hotfix".into());
        let e = plan(&steam, &app, "Game.exe", &[], base, &BTreeMap::new()).unwrap_err();
        assert!(e.to_string().contains("proton_hotfix"), "the app's own mapping comes before \"0\": {e}");
        for dir in [tool, runtime] {
            let tool_app = steamvr::AppInfo { install_dir: Some(dir.clone()), compat_tool: None, ..app.clone() };
            write(&dir.join("toolmanifest.vdf"), MANIFEST);
            let e = plan(&steam, &tool_app, "proton", &[], vec![], &BTreeMap::new()).unwrap_err().to_string();
            assert!(e.contains("is a compatibility tool or runtime"), "{}: {e}", dir.display());
        }
        fs::remove_dir_all(&steam).ok();
    }

    #[test]
    fn environ_match_is_exact() {
        let environ = b"A=1\0STEAM_COMPAT_DATA_PATH=/x/123\0";
        assert!(environ_has(environ, "STEAM_COMPAT_DATA_PATH", "/x/123"));
        assert!(!environ_has(environ, "STEAM_COMPAT_DATA_PATH", "/x/12"));
        assert!(!environ_has(environ, "STEAM_COMPAT_DATA_PATH", "/x/1234"));
        assert!(!environ_has(environ, "DATA_PATH", "/x/123"));
        assert_eq!(env_of(std::process::id(), "NO_SUCH_VAR_VR_TEST_MCP"), None);
    }

    #[test]
    fn own_group_is_never_signalled() {
        let group = unsafe { libc::getpgrp() } as u32;
        for pgid in [0, 1, group, std::process::id(), u32::MAX] {
            assert!(!signal_group(pgid, 0), "{pgid}");
        }
    }

    /// SIGKILLs the launched group, while its leader is unreaped, when a test ends early.
    struct KillGroup(Launched);
    impl Drop for KillGroup {
        fn drop(&mut self) {
            self.0.signal(libc::SIGKILL);
        }
    }

    #[test]
    fn spawn_group_and_stop_kills_children() {
        let dir = root("spawn");
        let marker = dir.to_str().unwrap().to_owned();
        let env = vec![("PATH".into(), std::env::var_os("PATH").unwrap()), ("STEAM_COMPAT_DATA_PATH".into(), marker.clone().into())];
        let env = [env, vec![("STEAM_COMPAT_APP_ID".into(), "7".into())]].concat();
        let l = ProtonLaunch { argv: vec!["sleep".into(), "30".into()], env, cwd: dir.clone(), compatdata: dir.clone(), app_id: 7 };
        let launched = spawn(&l, &dir.join("run.log")).unwrap();
        let guard = KillGroup(launched.clone());
        let mut second = Command::new("sleep").arg("30").process_group(launched.pgid as i32).spawn().unwrap();
        assert!(group_alive(launched.pgid));
        assert_eq!(game_pids(&marker, "7"), [launched.pgid], "only the launched process carries the marker");
        assert!(launched.signal(libc::SIGTERM));
        assert_eq!(second.wait().unwrap().signal(), Some(libc::SIGTERM), "the group signal reached the second process");
        assert!(steamvr::wait_for(Duration::from_secs(5), || !group_alive(launched.pgid)));
        assert!(launched.exited.lock().is_some_and(|s| s.signal() == Some(libc::SIGTERM)));
        assert!(!launched.signal(0), "a reaped leader's pgid may be reused: never signalled");
        drop(guard);
        fs::remove_dir_all(&dir).ok();
    }

    /// A `sleep 30` group, spawned as launch_app would, with PATH and `env`; killed when the test ends.
    fn sleeper(dir: &Path, app_id: u32, env: &[(&str, &str)]) -> KillGroup {
        let mut all: Vec<(OsString, OsString)> = vec![("PATH".into(), std::env::var_os("PATH").unwrap())];
        all.extend(env.iter().map(|&(k, v)| (k.into(), v.into())));
        let l = ProtonLaunch { argv: vec!["sleep".into(), "30".into()], env: all, cwd: dir.into(), compatdata: dir.join("a"), app_id };
        KillGroup(spawn(&l, Path::new("/dev/null")).unwrap())
    }

    #[test]
    fn stop_kills_stale_launched_and_scene_trees_only() {
        assert_eq!(stop(None, None).unwrap(), Vec::<u32>::new(), "nothing ran");
        let dir = root("stop");
        let (a, b) = (dir.join("a"), dir.join("b"));
        let (a, b) = (a.to_str().unwrap(), b.to_str().unwrap());
        // a stale proton launch of app 1, and a scene of app 2 that Steam launched on another prefix
        let launched = sleeper(&dir, 1, &[(COMPAT_DATA, a), ("STEAM_COMPAT_APP_ID", "1")]);
        let scene = sleeper(&dir, 2, &[(COMPAT_DATA, b), ("STEAM_COMPAT_APP_ID", "2"), ("SteamGameId", "2")]);
        // as wineserver daemonizes into its own group: only its environ ties it to the game
        let wineserver = sleeper(&dir, 2, &[(COMPAT_DATA, b), ("SteamGameId", "2"), ("X", "1")]);
        let shell = sleeper(&dir, 2, &[(COMPAT_DATA, b)]);
        let other_app = sleeper(&dir, 3, &[(COMPAT_DATA, b), ("STEAM_COMPAT_APP_ID", "3"), ("SteamGameId", "3")]);
        let mut want = vec![launched.0.pgid, scene.0.pgid, wineserver.0.pgid];
        want.sort_unstable();
        assert_eq!(stop(Some(scene.0.pgid), Some(&launched.0)).unwrap(), want);
        for dead in [&launched, &scene, &wineserver] {
            assert!(dead.0.exited.lock().is_some_and(|s| s.signal() == Some(libc::SIGTERM)), "{}", dead.0.pgid);
        }
        assert!(group_alive(shell.0.pgid), "the prefix path alone does not prove a process is the game's");
        assert!(group_alive(other_app.0.pgid), "another app on the same prefix");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn steamvr_processes_are_never_swept() {
        let dir = root("steamvr");
        let vr = dir.join("compatdata/250820");
        let vr = vr.to_str().unwrap();
        // every SteamVR process (vrserver, vrmonitor, reaper, pressure-vessel) carries SteamVR's compat env
        let env = [(COMPAT_DATA, vr), ("STEAM_COMPAT_APP_ID", "250820"), ("SteamGameId", "250820")];
        let (scene, vrserver) = (sleeper(&dir, 250820, &env), sleeper(&dir, 250820, &env));
        assert_eq!(game_pids(vr, "250820"), Vec::<u32>::new());
        assert_eq!(stop(Some(scene.0.pgid), None).unwrap(), Vec::<u32>::new(), "a scene with SteamVR's app id is SteamVR's own");
        assert!(group_alive(scene.0.pgid) && group_alive(vrserver.0.pgid));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sweep_skips_this_mcp_and_its_ancestors() {
        let me = ancestors();
        assert!(me.contains(&std::process::id()) && me.contains(&std::os::unix::process::parent_id()), "{me:?}");
    }
}
