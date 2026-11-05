use crate::applog;
use alvr_common::anyhow::{Context, Error, Result, bail};
use serde::Deserialize;
use std::{collections::BTreeMap, fs, path::{Path, PathBuf}};

#[derive(Deserialize, Default, Clone, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub steam_dir: Option<PathBuf>,
    pub steamvr_dir: Option<PathBuf>,
    pub streamer_dir: Option<PathBuf>,
    pub steam: SteamLaunch,
    /// ALVR session path -> value, applied after the built-in defaults.
    pub alvr_settings: BTreeMap<String, serde_json::Value>,
    pub timeouts: Timeouts,
    /// Steam app id -> glob patterns of its log files; see `applog::expand` for the variables.
    pub app_logs: BTreeMap<String, Vec<String>>,
}

#[derive(Deserialize, Default, Clone, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct SteamLaunch {
    pub env: BTreeMap<String, String>,
    pub args: Vec<String>,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct Timeouts {
    pub streaming_s: u64,
    pub app_s: u64,
}

impl Default for Timeouts {
    fn default() -> Self {
        // one budget for the whole vr_start: a cold start (60-90 s) plus 1-2 supervisor restarts of about 80 s each
        Self { streaming_s: 240, app_s: 180 }
    }
}

impl Config {
    /// `$XDG_CONFIG_HOME/SteamVR-MCP/config.toml`.
    pub fn file() -> Result<PathBuf> {
        Ok(dirs::config_dir().context("no config dir")?.join("SteamVR-MCP/config.toml"))
    }

    /// The file's text now; None when it is absent or unreadable.
    pub fn text() -> Option<String> {
        fs::read_to_string(Config::file().ok()?).ok()
    }

    /// The config in `text`, with `app_logs` ids and patterns checked.
    pub fn parse(text: &str) -> Result<Config> {
        let cfg: Config = toml::from_str(text)?;
        for (id, patterns) in &cfg.app_logs {
            let n = id.parse::<u32>().with_context(|| format!("app_logs key {id:?} is not a decimal app id"))?;
            if n.to_string() != *id {
                bail!("app_logs key {id:?} must be written as {n}, with no sign or leading zeros");
            }
            for p in patterns {
                let context = || format!("app_logs {id:?}");
                // Proton prefixes hold `dosdevices/z: -> /`, and glob follows directory symlinks, so `**` would walk the whole disk.
                if p.contains("**") {
                    bail!("app_logs {id:?}: {p:?} uses `**`; use one `*` per directory level");
                }
                // A dummy install dir stands in so the pattern is checked even where `${install}` would be empty.
                let expanded = applog::expand(p, Path::new(""), Some(Path::new("/x"))).map_err(Error::msg).with_context(context)?;
                glob::Pattern::new(&expanded.unwrap_or_default()).with_context(|| format!("app_logs {id:?}: {p:?}"))?;
            }
        }
        Ok(cfg)
    }

    /// Reads `$XDG_CONFIG_HOME/SteamVR-MCP/config.toml`; a missing file gives the defaults.
    pub fn load() -> Result<Config> {
        let path = Config::file()?;
        match fs::read_to_string(&path) {
            Ok(text) => Config::parse(&text).with_context(|| format!("{}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e).with_context(|| format!("{}", path.display())),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Paths {
    pub steam_dir: Option<PathBuf>,
    pub flatpak_steam: bool,
    pub steamvr_dir: Option<PathBuf>,
    pub streamer_dir: Option<PathBuf>,
    pub state_dir: PathBuf,
    pub runtime_dir: PathBuf,
}

/// True when Steam runs from the Flatpak data dir, or no native Steam was found and Flatpak Steam exists.
fn is_flatpak_steam(home: &Path, steam_dir: Option<&Path>) -> bool {
    let flatpak = home.join(".var/app/com.valvesoftware.Steam");
    match steam_dir {
        Some(dir) => dir.starts_with(&flatpak),
        None => flatpak.exists(),
    }
}

impl Paths {
    pub fn detect(cfg: &Config) -> Paths {
        let home = dirs::home_dir().unwrap_or_default();
        let steam_dir = cfg.steam_dir.clone().or_else(|| {
            fs::canonicalize(home.join(".steam/root"))
                .ok()
                .or_else(|| Some(home.join(".local/share/Steam")).filter(|p| p.exists()))
        });
        let steamvr_dir = cfg.steamvr_dir.clone().or_else(|| {
            alvr_server_io::steamvr_root_dir().ok().or_else(|| {
                Some(steam_dir.as_ref()?.join("steamapps/common/SteamVR")).filter(|p| p.exists())
            })
        });
        let streamer_dir = cfg.streamer_dir.clone().or_else(|| {
            let driver = alvr_server_io::get_driver_dir_from_registered().ok()?;
            Some(driver.parent()?.parent()?.to_owned()) // streamer/lib64/alvr -> streamer
        });
        // shortcut: no HOME makes state_dir relative to cwd, upgrade if the MCP must run without HOME
        Paths {
            flatpak_steam: is_flatpak_steam(&home, steam_dir.as_deref()),
            steam_dir,
            steamvr_dir,
            streamer_dir,
            state_dir: dirs::state_dir().unwrap_or_default().join("SteamVR-MCP"),
            runtime_dir: dirs::runtime_dir().unwrap_or_else(std::env::temp_dir).join("SteamVR-MCP"),
        }
    }

    pub fn driver_dir(&self) -> Option<PathBuf> {
        Some(self.streamer_dir.as_ref()?.join("lib64/alvr"))
    }

    pub fn session_file(&self) -> Option<PathBuf> {
        Some(alvr_filesystem::filesystem_layout_from_openvr_driver_root_dir(&self.driver_dir()?)?.session())
    }

    // Not alvr_server_io::steamvr_settings_file_path: it hard-codes ~/.local/share/Steam.
    pub fn vrsettings_file(&self) -> Option<PathBuf> {
        Some(self.steam_dir.as_ref()?.join("config/steamvr.vrsettings"))
    }

    pub fn vrcompositor(&self) -> Option<PathBuf> {
        Some(self.steamvr_dir.as_ref()?.join("bin/linux64/vrcompositor"))
    }

    pub fn restore_file(&self) -> PathBuf {
        self.state_dir.join("restore.json")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_when_file_absent() {
        let c = Config::default();
        assert_eq!((c.timeouts.streaming_s, c.timeouts.app_s), (240, 180));
        assert!(c.steam.env.is_empty() && c.steam.args.is_empty() && c.alvr_settings.is_empty());
    }

    #[test]
    fn flatpak_steam_only_when_steam_dir_is_flatpak_or_missing() {
        let home = std::env::temp_dir().join(format!("SteamVR-MCP-flatpak-{}", std::process::id()));
        let flatpak = home.join(".var/app/com.valvesoftware.Steam");
        std::fs::create_dir_all(&flatpak).unwrap();
        let native = home.join(".local/share/Steam");
        assert!(!is_flatpak_steam(&home, Some(&native)));
        assert!(is_flatpak_steam(&home, Some(&flatpak.join("data/Steam"))));
        assert!(is_flatpak_steam(&home, None));
        std::fs::remove_dir_all(&flatpak).unwrap();
        assert!(!is_flatpak_steam(&home, None));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn config_rejects_unknown_key() {
        assert!(toml::from_str::<Config>("bogus = 1").is_err());
    }

    #[test]
    fn config_reads_steam_env() {
        let c: Config = toml::from_str("[steam]\nenv = { A = \"b\" }").unwrap();
        assert_eq!(c.steam.env["A"], "b");
    }

    #[test]
    fn config_reads_app_logs() {
        let c = Config::parse("[app_logs]\n\"620980\" = [\"${install}/BepInEx/LogOutput.log\"]").unwrap();
        assert_eq!(c.app_logs["620980"], ["${install}/BepInEx/LogOutput.log"]);
    }

    #[test]
    fn config_rejects_unsafe_or_malformed_app_log_patterns() {
        for (case, want) in [
            ("\"7\" = [\"${prefix}/**/Player.log\"]", "**"),
            ("\"7\" = [\"${install}/[abc.log\"]", "[abc"),
            ("\"07\" = []", "07"),
            ("\"+7\" = []", "+7"),
        ] {
            let e = format!("{:#}", Config::parse(&format!("[app_logs]\n{case}")).unwrap_err());
            assert!(e.contains(want), "{case}: {e}");
        }
        assert!(Config::parse("[app_logs]\n\"7\" = [\"${prefix}/a/*/*/Player.log\", \"${install}/*.log\"]").is_ok());
    }

    #[test]
    fn config_rejects_bad_app_logs() {
        let key = format!("{:#}", Config::parse("[app_logs]\nabc = []").unwrap_err());
        assert!(key.contains("abc"), "{key}");
        let var = format!("{:#}", Config::parse("[app_logs]\n\"7\" = [\"${home}/x\"]").unwrap_err());
        assert!(var.contains("home"), "{var}");
    }
}
