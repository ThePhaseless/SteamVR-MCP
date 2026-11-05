//! A lease on the machine's SteamVR, so two MCP processes (two agent sessions) do not step on each other.
use crate::{clock, steamvr::private_dir};
use alvr_common::anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io,
    ops::RangeInclusive,
    os::fd::AsRawFd,
    path::Path,
    time::{Duration, SystemTime},
};

pub const DEFAULT_TTL: Duration = Duration::from_secs(1800);
pub const TTL_S: RangeInclusive<u64> = 600..=14400;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Lease {
    pub owner: String,
    pub pid: u32,
    /// RFC 3339 UTC.
    pub expires_at: String,
}

/// What vr_status shows: the live lease, and whether this MCP holds it.
#[derive(Debug, Serialize)]
pub struct Owner {
    #[serde(flatten)]
    pub lease: Lease,
    pub mine: bool,
}

// shortcut: pid liveness assumes all MCPs share one pid namespace, use a lock fd held for the lease's life if a sandboxed MCP appears
pub fn alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

/// The lease when another live process holds it and it has not expired.
pub fn blocking(lease: Option<&Lease>, me: u32, now: SystemTime, alive: impl Fn(u32) -> bool) -> Option<&Lease> {
    // shortcut: a reused pid keeps a dead lease alive until expires_at, compare process start time when it bites
    // an expires_at that will not parse counts as expired
    lease.filter(|l| l.pid != me && clock::parse_rfc3339(&l.expires_at).is_some_and(|t| t > now) && alive(l.pid))
}

/// No lock: writes are a temp file plus rename, so a reader sees a whole file.
pub fn read(dir: &Path) -> Option<Lease> {
    serde_json::from_slice(&fs::read(dir.join("owner.json")).ok()?).ok()
}

pub fn holder_text(l: &Lease) -> String {
    format!(
        "SteamVR is leased to {} (pid {}) until {}; read-only tools stay available until it runs vr_release or vr_stop, or exits",
        l.owner, l.pid, l.expires_at
    )
}

/// The open lock file; the lock ends when it drops.
fn lock(dir: &Path) -> Result<File> {
    private_dir(dir)?;
    let path = dir.join("owner.lock");
    let file = OpenOptions::new().create(true).write(true).truncate(false).open(&path).with_context(|| path.display().to_string())?;
    // shortcut: flock EINTR surfaces as an error, retry on Interrupted if it ever shows
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(io::Error::last_os_error()).with_context(|| path.display().to_string());
    }
    Ok(file)
}

/// Takes the lease, or renews ours. Refuses while another live process holds it.
pub fn acquire(dir: &Path, owner: &str, ttl: Duration) -> Result<Lease> {
    let _lock = lock(dir)?;
    let pid = std::process::id();
    let now = SystemTime::now();
    if let Some(holder) = blocking(read(dir).as_ref(), pid, now, alive) {
        bail!("{}", holder_text(holder));
    }
    write(dir, owner, ttl)
}

/// Renews the lease only while it is still ours, checked under the lock: a vr_release since the caller looked gave it up for good. Whether it renewed.
pub fn renew(dir: &Path, owner: &str, ttl: Duration) -> Result<bool> {
    let _lock = lock(dir)?;
    if read(dir).is_none_or(|l| l.pid != std::process::id()) {
        return Ok(false);
    }
    write(dir, owner, ttl).map(|_| true)
}

/// Writes our lease; the caller holds the lock.
fn write(dir: &Path, owner: &str, ttl: Duration) -> Result<Lease> {
    let lease = Lease { owner: owner.to_owned(), pid: std::process::id(), expires_at: clock::rfc3339(SystemTime::now() + ttl) };
    let (file, tmp) = (dir.join("owner.json"), dir.join("owner.json.tmp"));
    fs::write(&tmp, serde_json::to_vec(&lease)?).with_context(|| tmp.display().to_string())?;
    fs::rename(&tmp, &file).with_context(|| file.display().to_string())?;
    Ok(lease)
}

/// Whether it removed our lease; another process's lease stays.
pub fn release(dir: &Path) -> Result<bool> {
    let _lock = lock(dir)?;
    if read(dir).is_none_or(|l| l.pid != std::process::id()) {
        return Ok(false);
    }
    let file = dir.join("owner.json");
    fs::remove_file(&file).with_context(|| file.display().to_string())?;
    Ok(true)
}

/// Less than half the ttl is left (or the expiry is unreadable).
pub fn renew_due(lease: &Lease, ttl: Duration, now: SystemTime) -> bool {
    clock::parse_rfc3339(&lease.expires_at).and_then(|t| t.duration_since(now).ok()).is_none_or(|left| left < ttl / 2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::rfc3339;
    use std::{
        fs,
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    fn dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("SteamVR-MCP-lease-{name}-{}", std::process::id()));
        fs::remove_dir_all(&dir).ok();
        dir
    }

    fn lease(pid: u32, expires: SystemTime) -> Lease {
        Lease { owner: "runner".into(), pid, expires_at: rfc3339(expires) }
    }

    /// Writes `l` as another MCP would have.
    fn plant(dir: &std::path::Path, l: &Lease) {
        crate::steamvr::private_dir(dir).unwrap();
        fs::write(dir.join("owner.json"), serde_json::to_vec(l).unwrap()).unwrap();
    }

    #[test]
    fn blocking_ignores_free_mine_expired_or_dead() {
        let now = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let later = now + Duration::from_secs(60);
        let live = |_| true;
        assert_eq!(blocking(None, 5, now, live), None);
        assert_eq!(blocking(Some(&lease(5, later)), 5, now, live), None, "ours");
        assert_eq!(blocking(Some(&lease(9, now - Duration::from_secs(1))), 5, now, live), None, "expired");
        assert_eq!(blocking(Some(&lease(9, later)), 5, now, |_| false), None, "dead");
        let unparsable = Lease { expires_at: "soon".into(), ..lease(9, later) };
        assert_eq!(blocking(Some(&unparsable), 5, now, live), None, "counts as expired");
        let other = lease(9, later);
        assert_eq!(blocking(Some(&other), 5, now, live), Some(&other));
    }

    #[test]
    fn second_owner_is_refused_while_first_lives() {
        let dir = dir("refused");
        plant(&dir, &lease(1, SystemTime::now() + Duration::from_secs(600)));
        let before = fs::read(dir.join("owner.json")).unwrap();
        let err = acquire(&dir, "driver", Duration::from_secs(60)).unwrap_err().to_string();
        assert!(err.contains("runner") && err.contains("pid 1"), "{err}");
        assert_eq!(fs::read(dir.join("owner.json")).unwrap(), before);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn acquire_renew_release_round_trip() {
        let dir = dir("round-trip");
        let first = acquire(&dir, "a", Duration::from_secs(60)).unwrap();
        assert_eq!((first.owner.as_str(), first.pid), ("a", std::process::id()));
        assert_eq!(read(&dir), Some(first));
        assert_eq!(acquire(&dir, "b", Duration::from_secs(60)).unwrap().owner, "b", "ours, so renewed");
        assert!(release(&dir).unwrap());
        assert!(!dir.join("owner.json").exists());
        assert!(!release(&dir).unwrap());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn release_leaves_another_owners_lease() {
        let dir = dir("release-other");
        plant(&dir, &lease(1, SystemTime::now() + Duration::from_secs(600)));
        assert!(!release(&dir).unwrap());
        assert!(dir.join("owner.json").exists());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn expired_or_dead_lease_is_taken() {
        let dir = dir("taken");
        plant(&dir, &lease(1, SystemTime::now() - Duration::from_secs(5)));
        assert_eq!(acquire(&dir, "driver", Duration::from_secs(60)).unwrap().owner, "driver");
        plant(&dir, &lease(u32::MAX, SystemTime::now() + Duration::from_secs(600)));
        assert_eq!(acquire(&dir, "again", Duration::from_secs(60)).unwrap().owner, "again");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn renew_rewrites_only_our_own_lease() {
        let dir = dir("renew");
        assert!(!renew(&dir, "a", Duration::from_secs(60)).unwrap(), "vr_release gave it up: not taken back");
        assert_eq!(read(&dir), None);
        let first = acquire(&dir, "a", Duration::from_secs(60)).unwrap();
        assert!(renew(&dir, "a", Duration::from_secs(600)).unwrap());
        let renewed = read(&dir).unwrap();
        assert!(renewed.pid == first.pid && renewed.expires_at > first.expires_at, "{renewed:?}");
        let other = lease(1, SystemTime::now() + Duration::from_secs(600));
        plant(&dir, &other);
        assert!(!renew(&dir, "a", Duration::from_secs(60)).unwrap());
        assert_eq!(read(&dir), Some(other), "another MCP's lease stays");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn renew_due_after_half_ttl() {
        let now = SystemTime::now();
        let ttl = Duration::from_secs(100);
        assert!(!renew_due(&lease(1, now + Duration::from_secs(60)), ttl, now));
        assert!(renew_due(&lease(1, now + Duration::from_secs(40)), ttl, now));
    }
}
