//! A config.toml fix applies at the next vr_status, without vr_start (hidden while a core check
//! fails) or an MCP restart. The only test in this binary: it sets XDG_CONFIG_HOME before any thread runs.

use std::fs;
use steamvr_mcp::{
    checks::Check,
    config::{Config, Paths},
    stack::Stack,
    steamvr,
};

fn check(stack: &Stack, name: &str) -> Check {
    stack.checks().into_iter().find(|c| c.name == name).unwrap()
}

#[test]
fn vr_status_applies_a_fixed_config() {
    if steamvr::is_steamvr_running() {
        eprintln!("skipped: SteamVR runs, and the config is applied only while nothing runs");
        return;
    }
    let root = std::env::temp_dir().join(format!("SteamVR-MCP-config-reload-{}", std::process::id()));
    fs::remove_dir_all(&root).ok();
    let file = root.join("xdg/SteamVR-MCP/config.toml");
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    unsafe { std::env::set_var("XDG_CONFIG_HOME", root.join("xdg")) };
    let streamer = |name: &str| format!("streamer_dir = {:?}\n", root.join(name).display().to_string());

    fs::write(&file, streamer("a")).unwrap();
    let cfg = Config::load().unwrap();
    let stack = Stack::new(cfg.clone(), Paths::detect(&cfg));
    let a = check(&stack, "alvr_streamer");
    assert!(!a.ok && a.reason.contains(&root.join("a").display().to_string()), "{a:?}");

    fs::write(&file, "bogus = 1").unwrap();
    assert!(!check(&stack, "config").ok, "a malformed edit shows at once");

    fs::write(&file, streamer("b")).unwrap();
    let b = check(&stack, "alvr_streamer");
    assert!(b.reason.contains(&root.join("b").display().to_string()), "the fix applied: {b:?}");
    assert!(check(&stack, "config").ok);
    fs::remove_dir_all(&root).ok();
}
