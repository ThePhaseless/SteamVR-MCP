//! Game log tailing for `[app_logs]`: patterns expand per app, files are followed across truncation and rotation, lines land in a per-app ring.
use crate::{clock, steamvr};
use alvr_common::warn;
use regex::Regex;
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

/// Lines kept per app, and lines per `page`.
pub const LINES_KEPT: usize = 5000;
pub const PAGE: usize = 500;
const MAX_READ: u64 = 4 << 20;
/// The longest unfinished line held back; a log with no newline (binary, or `\r` only) would otherwise grow it by MAX_READ per call.
const MAX_PARTIAL: usize = 64 << 10;

/// Fills `${compatdata}`, `${prefix}` and `${install}` in a glob pattern. Ok(None): `${install}` has no
/// value (a shortcut), so the pattern matches nothing. Values are escaped, so their glob characters stay literal.
pub fn expand(pattern: &str, compatdata: &Path, install: Option<&Path>) -> Result<Option<String>, String> {
    let (mut out, mut rest, mut missing) = (String::new(), pattern, false);
    while let Some(start) = rest.find("${") {
        let end = rest[start..].find('}').ok_or_else(|| format!("unterminated ${{ in {pattern:?}"))? + start;
        let value = match &rest[start + 2..end] {
            "compatdata" => Some(compatdata.to_path_buf()),
            "prefix" => Some(compatdata.join("pfx")),
            "install" => install.map(Path::to_path_buf),
            other => return Err(format!("unknown variable ${{{other}}} in {pattern:?}")),
        };
        out.push_str(&rest[..start]);
        match value {
            Some(v) => out.push_str(&glob::Pattern::escape(&v.to_string_lossy())),
            None => missing = true,
        }
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    Ok((!missing).then_some(out))
}

/// Where reading a file stopped: its identity, the offset after the last byte read, and the bytes of an unfinished line.
#[derive(Default)]
pub struct Tail {
    pub id: (u64, u64),
    pub offset: u64,
    pub partial: Vec<u8>,
}

/// The whole lines the file gained since `tail`. A different file (dev, inode) or a shorter one is read from the start.
// shortcut: truncate-then-regrow past the old offset between two polls loses lines, compare a head hash when a game does this
// shortcut: at most 4 MiB per call, raise if a game logs faster
pub fn read_new(path: &Path, tail: &mut Tail) -> io::Result<Vec<String>> {
    let mut file = File::open(path)?;
    let meta = file.metadata()?;
    let id = (meta.dev(), meta.ino());
    if id != tail.id || meta.len() < tail.offset {
        *tail = Tail { id, ..Tail::default() };
    }
    file.seek(SeekFrom::Start(tail.offset))?;
    let mut bytes = std::mem::take(&mut tail.partial);
    tail.offset += file.take(MAX_READ).read_to_end(&mut bytes)? as u64;
    let mut lines = vec![];
    if let Some(last) = bytes.iter().rposition(|&b| b == b'\n') {
        tail.partial = bytes.split_off(last + 1);
        bytes.pop();
        lines = bytes.split(|&b| b == b'\n').map(decode_line).collect();
    } else {
        tail.partial = bytes;
    }
    if tail.partial.len() > MAX_PARTIAL {
        tail.partial.truncate(MAX_PARTIAL);
        lines.push(decode_line(&std::mem::take(&mut tail.partial)));
    }
    Ok(lines)
}

fn decode_line(l: &[u8]) -> String {
    String::from_utf8_lossy(l.strip_suffix(b"\r").unwrap_or(l)).into_owned()
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AppLine {
    pub seq: u64,
    /// When the line was read: session seconds, and the same instant as RFC 3339 UTC.
    pub t_s: f64,
    pub at: String,
    pub file: String,
    pub text: String,
}

#[derive(Debug, Serialize)]
pub struct Page {
    pub app_id: u32,
    /// The files the patterns matched at the last poll.
    pub files: Vec<String>,
    pub lines: Vec<AppLine>,
    /// The `since_seq` that continues after these lines.
    pub next_seq: u64,
}

struct App {
    patterns: Vec<String>,
    /// Resolved through `find_app` until it succeeds.
    info: Option<steamvr::AppInfo>,
    tails: BTreeMap<PathBuf, Tail>,
    files: Vec<PathBuf>,
    lines: VecDeque<AppLine>,
    next_seq: u64,
    /// False until the first scan, which only moves each existing file's tail to its end.
    primed: bool,
}

impl App {
    /// The files the patterns match now, in path order.
    fn globbed(&mut self, steam_dir: &Path, app_id: u32) -> Vec<PathBuf> {
        if self.info.is_none() {
            self.info = steamvr::find_app(steam_dir, app_id.into()).ok();
        }
        let Some(info) = &self.info else { return vec![] };
        let mut files: Vec<PathBuf> = self
            .patterns
            .iter()
            .filter_map(|p| expand(p, &info.compatdata, info.install_dir.as_deref()).ok().flatten())
            .filter_map(|p| glob::glob(&p).inspect_err(|e| warn!("app {app_id} log pattern {p:?}: {e}")).ok())
            .flatten()
            .flatten()
            .filter(|p| p.is_file())
            .collect();
        files.sort();
        // one file reached by two paths (a symlinked dir in a prefix) is tailed once, under its first path
        let mut seen = BTreeSet::new();
        files.retain(|p| p.metadata().is_ok_and(|m| seen.insert((m.dev(), m.ino()))));
        files
    }

    /// `start_at_end`: skip what the files hold now.
    fn scan(&mut self, steam_dir: &Path, app_id: u32, start_at_end: bool) {
        let files = self.globbed(steam_dir, app_id);
        let ids: Vec<_> = files.iter().map(|p| p.metadata().map(|m| (m.dev(), m.ino()))).collect();
        // shortcut: a deleted log's inode reused by a new matched file passes on its old offset, compare a head hash if a game does this
        // A rotated file (Player.log -> Player-prev.log) keeps its tail under its new name, so it is not read twice.
        for (path, id) in files.iter().zip(&ids) {
            if let (false, Ok(id)) = (self.tails.contains_key(path), id)
                && let Some(old) = self.tails.iter().find(|(_, t)| t.id == *id).map(|(k, _)| k.clone())
            {
                let tail = self.tails.remove(&old).unwrap();
                self.tails.insert(path.clone(), tail);
            }
        }
        let (t_s, mut new) = (clock::now_s(), vec![]);
        for (path, id) in files.iter().zip(&ids) {
            let (Ok(id), Ok(meta)) = (id, path.metadata()) else { continue };
            if start_at_end && !self.tails.contains_key(path) {
                self.tails.insert(path.clone(), Tail { id: *id, offset: meta.len(), partial: vec![] });
            } else if let Ok(lines) = read_new(path, self.tails.entry(path.clone()).or_default()) {
                new.extend(lines.into_iter().map(|text| (path.display().to_string(), text)));
            }
        }
        for (file, text) in new {
            self.keep(t_s, file, text);
        }
        self.files = files;
        // An unresolved app has no files yet; priming now would read them from the start once it resolves.
        self.primed = self.info.is_some();
    }

    fn keep(&mut self, t_s: f64, file: String, text: String) {
        if self.lines.len() == LINES_KEPT {
            self.lines.pop_front();
        }
        self.lines.push_back(AppLine { seq: self.next_seq, t_s, at: clock::at(t_s), file, text });
        self.next_seq += 1;
    }
}

/// The ring of every configured app's log lines.
pub struct AppLogs {
    apps: BTreeMap<u32, App>,
}

impl AppLogs {
    /// Keys that are not decimal are skipped: `Config::parse` refuses them.
    pub fn new(patterns: &BTreeMap<String, Vec<String>>) -> AppLogs {
        let apps = patterns
            .iter()
            .filter_map(|(id, p)| {
                let app = App { patterns: p.clone(), info: None, tails: BTreeMap::new(), files: vec![], lines: VecDeque::new(), next_seq: 1, primed: false };
                Some((id.parse().ok()?, app))
            })
            .collect();
        AppLogs { apps }
    }

    pub fn has_app(&self, app_id: u32) -> bool {
        self.apps.contains_key(&app_id)
    }

    pub fn poll(&mut self, steam_dir: Option<&Path>) {
        for id in self.apps.keys().copied().collect::<Vec<_>>() {
            self.poll_app(id, steam_dir);
        }
    }

    pub fn poll_app(&mut self, app_id: u32, steam_dir: Option<&Path>) {
        if let (Some(app), Some(dir)) = (self.apps.get_mut(&app_id), steam_dir) {
            let first = !app.primed;
            app.scan(dir, app_id, first);
        }
    }

    /// The seq the app's next line gets; 0 for an app not in `[app_logs]`.
    pub fn next_seq(&self, app_id: u32) -> u64 {
        self.apps.get(&app_id).map_or(0, |a| a.next_seq)
    }

    /// Bytes read so far from the app's files, an unfinished last line included: it moves with every byte a writer adds.
    pub fn bytes_read(&self, app_id: u32) -> u64 {
        self.apps.get(&app_id).map_or(0, |a| a.tails.values().map(|t| t.offset).sum())
    }

    /// Commits each file's unfinished last line, for a game that exited without its newline. The offset stays,
    /// so bytes a still-running writer adds later start a new line: such a line shows split in two.
    pub fn flush_partial(&mut self, app_id: u32) {
        let Some(app) = self.apps.get_mut(&app_id) else { return };
        let t_s = clock::now_s();
        let partials: Vec<_> = app
            .tails
            .iter_mut()
            .filter(|(_, t)| !t.partial.is_empty())
            .map(|(path, t)| (path.display().to_string(), decode_line(&std::mem::take(&mut t.partial))))
            .collect();
        for (file, text) in partials {
            app.keep(t_s, file, text);
        }
    }

    /// Forgets the app's lines (seq keeps growing) and skips what its files hold now.
    pub fn reset(&mut self, app_id: u32, steam_dir: Option<&Path>) {
        if let Some(app) = self.apps.get_mut(&app_id) {
            app.lines.clear();
            app.tails.clear();
            app.info = None; // the game may have moved to another library
            app.primed = false;
        }
        self.poll_app(app_id, steam_dir);
    }

    /// Up to `limit` lines with `seq >= since_seq` that contain `contains` (ignoring case), oldest first.
    pub fn page(&self, app_id: u32, since_seq: u64, contains: Option<&str>, limit: usize) -> Result<Page, String> {
        let app = self.apps.get(&app_id).ok_or_else(|| format!("app {app_id} has no entry in [app_logs]"))?;
        let contains = contains.map(str::to_lowercase);
        let kept = |l: &&AppLine| l.seq >= since_seq && contains.as_ref().is_none_or(|c| l.text.to_lowercase().contains(c));
        let lines: Vec<AppLine> = app.lines.iter().filter(kept).take(limit).cloned().collect();
        // a short page scanned every line kept, so the next read starts after the newest
        let next_seq = match lines.last() {
            Some(l) if lines.len() == limit => l.seq + 1,
            _ => app.next_seq.max(since_seq),
        };
        Ok(Page { app_id, files: app.files.iter().map(|p| p.display().to_string()).collect(), lines, next_seq })
    }
}

/// The first line `fail` or `pattern` matches; `fail` is checked first on each line.
pub fn find_match<'a>(lines: &'a [AppLine], pattern: &Regex, fail: Option<&Regex>) -> Option<(&'static str, &'a AppLine)> {
    lines.iter().find_map(|l| match fail {
        Some(f) if f.is_match(&l.text) => Some(("fail", l)),
        _ => pattern.is_match(&l.text).then_some(("match", l)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::BTreeMap, fs, io::Write, path::PathBuf};

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("SteamVR-MCP-applog-{name}-{}", std::process::id()));
        fs::remove_dir_all(&dir).ok();
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn append(path: &Path, text: impl AsRef<[u8]>) {
        fs::OpenOptions::new().append(true).create(true).open(path).unwrap().write_all(text.as_ref()).unwrap();
    }

    /// A fake Steam tree with app 42 installed as "Game", and `AppLogs` for the given patterns.
    fn game(name: &str, patterns: &[&str]) -> (PathBuf, PathBuf, AppLogs) {
        let steam = tmp(name);
        fs::create_dir_all(steam.join("steamapps")).unwrap();
        fs::write(steam.join("steamapps/appmanifest_42.acf"), "\"AppState\"\n{\n\t\"name\"\t\t\"G\"\n\t\"installdir\"\t\t\"Game\"\n}\n").unwrap();
        let install = steam.join("steamapps/common/Game");
        fs::create_dir_all(&install).unwrap();
        let logs = AppLogs::new(&BTreeMap::from([("42".to_owned(), patterns.iter().map(|p| p.to_string()).collect())]));
        (steam, install, logs)
    }

    fn texts(logs: &AppLogs, since: u64) -> Vec<String> {
        logs.page(42, since, None, PAGE).unwrap().lines.into_iter().map(|l| l.text).collect()
    }

    #[test]
    fn expand_fills_vars_and_rejects_unknown() {
        let (c, i) = (Path::new("/c"), Path::new("/i"));
        assert_eq!(expand("${prefix}/x", c, None), Ok(Some("/c/pfx/x".to_owned())));
        assert_eq!(expand("${compatdata}/x", c, None), Ok(Some("/c/x".to_owned())));
        assert_eq!(expand("${install}/y", c, Some(i)), Ok(Some("/i/y".to_owned())));
        assert_eq!(expand("${install}/y", c, None), Ok(None));
        assert!(expand("${nope}", c, None).unwrap_err().contains("nope"));
        assert!(expand("${install}${nope}", c, None).is_err(), "an unknown variable fails even when install is empty");
        assert!(expand("${install", c, None).is_err());
        assert_eq!(expand("${install}/*.log", c, Some(Path::new("/g[1]"))), Ok(Some("/g[[]1[]]/*.log".to_owned())), "glob characters in a value are literal");
    }

    #[test]
    fn read_new_follows_truncate_rotate_and_partial_lines() {
        let dir = tmp("read-new");
        let path = dir.join("Player.log");
        let mut tail = Tail::default();
        fs::write(&path, "a\r\nb").unwrap();
        assert_eq!(read_new(&path, &mut tail).unwrap(), ["a"]);
        append(&path, "c\n");
        assert_eq!(read_new(&path, &mut tail).unwrap(), ["bc"]);
        assert!(read_new(&path, &mut tail).unwrap().is_empty());
        fs::write(&path, "d\n").unwrap(); // BepInEx truncates in place
        assert_eq!(read_new(&path, &mut tail).unwrap(), ["d"]);
        fs::rename(&path, dir.join("Player-prev.log")).unwrap(); // Unity rotates
        fs::write(&path, "e\n").unwrap();
        assert_eq!(read_new(&path, &mut tail).unwrap(), ["e"]);
        append(&path, b"f\xffg\n");
        assert_eq!(read_new(&path, &mut tail).unwrap(), ["f\u{fffd}g"]);
        assert!(read_new(&dir.join("missing.log"), &mut tail).is_err());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_endless_line_is_flushed_truncated() {
        let dir = tmp("endless");
        let path = dir.join("bin.log");
        let mut tail = Tail::default();
        fs::write(&path, vec![b'x'; MAX_PARTIAL]).unwrap();
        assert!(read_new(&path, &mut tail).unwrap().is_empty(), "held until it passes the cap");
        append(&path, vec![b'y'; 10]);
        let lines = read_new(&path, &mut tail).unwrap();
        assert_eq!((lines.len(), lines[0].len(), tail.partial.len()), (1, MAX_PARTIAL, 0));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn first_poll_starts_at_end_later_files_from_start() {
        let (steam, install, mut logs) = game("first-poll", &["${install}/*.log"]);
        fs::write(install.join("old.log"), "x\n").unwrap();
        logs.poll(Some(&steam));
        assert!(texts(&logs, 0).is_empty());
        append(&install.join("old.log"), "y\n");
        fs::write(install.join("z-new.log"), "z\n").unwrap();
        logs.poll(Some(&steam));
        let page = logs.page(42, 0, None, PAGE).unwrap();
        assert_eq!(page.lines.iter().map(|l| (l.seq, l.text.as_str())).collect::<Vec<_>>(), [(1, "y"), (2, "z")]);
        assert_eq!(page.files, [install.join("old.log"), install.join("z-new.log")].map(|p| p.display().to_string()));
        assert_eq!(page.lines[1].file, install.join("z-new.log").display().to_string());
        assert!(page.lines[0].t_s >= 0.0 && page.lines[0].at.ends_with('Z'));
        fs::remove_dir_all(&steam).ok();
    }

    #[test]
    fn a_rotated_file_under_a_new_name_is_not_read_twice() {
        let (steam, install, mut logs) = game("rotate-glob", &["${install}/Player*.log"]);
        fs::write(install.join("Player.log"), "1\n").unwrap();
        logs.poll(Some(&steam));
        append(&install.join("Player.log"), "2\n");
        fs::rename(install.join("Player.log"), install.join("Player-prev.log")).unwrap();
        fs::write(install.join("Player.log"), "3\n").unwrap();
        logs.poll(Some(&steam));
        assert_eq!(texts(&logs, 0), ["2", "3"]);
        fs::remove_dir_all(&steam).ok();
    }

    #[test]
    fn a_file_reached_by_two_paths_is_tailed_once() {
        let (steam, install, mut logs) = game("symlink", &["${install}/*/*.log"]);
        fs::create_dir_all(install.join("logs")).unwrap();
        std::os::unix::fs::symlink(install.join("logs"), install.join("z-link")).unwrap(); // as in a Proton prefix
        fs::write(install.join("logs/a.log"), "").unwrap();
        logs.poll(Some(&steam));
        append(&install.join("logs/a.log"), "1\n");
        logs.poll(Some(&steam));
        let page = logs.page(42, 0, None, PAGE).unwrap();
        assert_eq!(page.lines.iter().map(|l| (l.text.as_str(), l.file.clone())).collect::<Vec<_>>(), [("1", install.join("logs/a.log").display().to_string())]);
        assert_eq!(page.files, [install.join("logs/a.log").display().to_string()], "the first path in path order");
        fs::remove_dir_all(&steam).ok();
    }

    #[test]
    fn reset_drops_old_lines_and_keeps_seq_growing() {
        let (steam, install, mut logs) = game("reset", &["${install}/a.log"]);
        logs.poll(Some(&steam));
        fs::write(install.join("a.log"), "1\n2\n").unwrap();
        logs.poll(Some(&steam));
        assert_eq!(logs.page(42, 0, None, PAGE).unwrap().lines.iter().map(|l| l.seq).collect::<Vec<_>>(), [1, 2]);
        append(&install.join("a.log"), "unread\n");
        logs.reset(42, Some(&steam));
        assert!(texts(&logs, 0).is_empty());
        append(&install.join("a.log"), "3\n");
        logs.poll(Some(&steam));
        let page = logs.page(42, 0, None, PAGE).unwrap();
        assert_eq!(page.lines.iter().map(|l| (l.seq, l.text.as_str())).collect::<Vec<_>>(), [(3, "3")]);
        assert!(logs.page(7, 0, None, PAGE).is_err());
        fs::remove_dir_all(&steam).ok();
    }

    #[test]
    fn page_by_seq_caps_at_page() {
        let (steam, install, mut logs) = game("page", &["${install}/a.log"]);
        logs.poll(Some(&steam));
        let write = |range: std::ops::Range<u32>| range.map(|n| format!("l{n}\n")).collect::<String>();
        append(&install.join("a.log"), &write(1..1201));
        logs.poll(Some(&steam));
        let first = logs.page(42, 0, None, PAGE).unwrap();
        assert_eq!((first.lines.len(), first.next_seq), (PAGE, 501));
        let second = logs.page(42, 501, None, PAGE).unwrap();
        assert_eq!((second.lines[0].seq, second.lines[0].text.as_str(), second.next_seq), (501, "l501", 1001));
        assert_eq!(logs.page(42, 1201, None, PAGE).unwrap().next_seq, 1201);
        assert_eq!(logs.page(42, 9000, None, PAGE).unwrap().next_seq, 9000, "a future cursor stays where it is");
        append(&install.join("a.log"), &write(1201..5101));
        logs.poll(Some(&steam));
        let kept = logs.page(42, 0, None, PAGE).unwrap();
        assert_eq!((kept.lines[0].seq, kept.next_seq), (5100 - LINES_KEPT as u64 + 1, 5100 - LINES_KEPT as u64 + 1 + PAGE as u64));
        assert_eq!(logs.page(42, 5100, None, PAGE).unwrap().lines[0].text, "l5100");
        fs::remove_dir_all(&steam).ok();
    }

    #[test]
    fn page_filters_by_contains_and_limit() {
        let (steam, install, mut logs) = game("filter", &["${install}/a.log"]);
        logs.poll(Some(&steam));
        append(&install.join("a.log"), "a\n[input] x\nb\n[INPUT] y\nc\n[input] z\nd\n");
        logs.poll(Some(&steam));
        let texts = |p: &Page| p.lines.iter().map(|l| l.text.clone()).collect::<Vec<_>>();
        let first = logs.page(42, 0, Some("[input]"), 2).unwrap();
        assert_eq!((texts(&first), first.next_seq), (vec!["[input] x".into(), "[INPUT] y".into()], 5), "ignores case, stops at the limit");
        let rest = logs.page(42, first.next_seq, Some("[input]"), 2).unwrap();
        assert_eq!((texts(&rest), rest.next_seq), (vec!["[input] z".to_string()], 8), "next_seq passes the unmatched tail");
        assert_eq!(logs.page(42, 0, Some("nothing"), 2).unwrap().next_seq, 8);
        assert_eq!(logs.page(42, 0, None, 3).unwrap().next_seq, 4);
        fs::remove_dir_all(&steam).ok();
    }

    #[test]
    fn flush_partial_commits_the_unfinished_line_once() {
        let (steam, install, mut logs) = game("flush", &["${install}/a.log"]);
        logs.poll(Some(&steam));
        append(&install.join("a.log"), "done\nhalf");
        logs.poll(Some(&steam));
        assert_eq!(texts(&logs, 0), ["done"]);
        logs.flush_partial(42);
        assert_eq!((texts(&logs, 0), logs.next_seq(42)), (vec!["done".to_string(), "half".into()], 3));
        logs.flush_partial(42);
        assert_eq!((texts(&logs, 0).len(), logs.next_seq(42)), (2, 3), "nothing left to commit");
        append(&install.join("a.log"), "-rest\n");
        logs.poll(Some(&steam));
        assert_eq!(texts(&logs, 0), ["done", "half", "-rest"], "bytes after the flush start a new line");
        assert_eq!(logs.next_seq(7), 0);
        fs::remove_dir_all(&steam).ok();
    }

    #[test]
    fn find_match_reports_fail_first() {
        let line = |seq, text: &str| AppLine { seq, t_s: 0.0, at: String::new(), file: String::new(), text: text.into() };
        let lines = [line(1, "ok"), line(2, "Exception: x PASS")];
        let (pass, fail) = (Regex::new("PASS").unwrap(), Regex::new("Exception").unwrap());
        assert_eq!(find_match(&lines, &pass, Some(&fail)), Some(("fail", &lines[1])));
        assert_eq!(find_match(&lines, &pass, None), Some(("match", &lines[1])));
        assert_eq!(find_match(&lines[..1], &pass, Some(&fail)), None);
    }
}
