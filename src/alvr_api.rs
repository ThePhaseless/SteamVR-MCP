use alvr_common::{
    anyhow::{Context, Result, anyhow, bail},
    semver::Version,
    parking_lot::Mutex,
    warn,
};
use alvr_events::Event;
use alvr_session::SessionConfig;
use serde::Serialize;
use std::{
    collections::{BTreeSet, HashSet},
    fs,
    io::{ErrorKind, Read, Write},
    net::{Ipv4Addr, SocketAddr, TcpStream},
    path::Path,
    time::Duration,
};
use tungstenite::{Message, WebSocket, client::IntoClientRequest, protocol::WebSocketConfig};

const TIMEOUT: Duration = Duration::from_secs(10);
/// For probes that must answer at once even when ALVR stalls (readiness, vr_status).
const QUICK: Duration = Duration::from_millis(500);
/// The events websocket handshake; ALVR answers it at once on loopback.
const HANDSHAKE: Duration = Duration::from_secs(3);
/// The most a response or an event may hold; a whole session is far below it.
const MAX_MESSAGE: usize = 8 << 20;

/// The ALVR server's REST and events API on 127.0.0.1.
#[derive(Clone, Copy)]
pub struct Api {
    pub port: u16,
}

impl Api {
    /// The session's `connection.web_server_port`; ALVR's default (8082) when the file is unreadable.
    pub fn from_session_file(path: &Path) -> Api {
        let mut session = SessionConfig::default();
        if let Some(json) = std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok()) {
            session.merge_from_json(&json).ok();
        }
        Api { port: session.to_settings().connection.web_server_port }
    }

    // shortcut: no chunked decoding; axum answers these routes with Content-Length, switch to a client crate if a route streams
    fn request(&self, method: &str, path: &str, body: Option<&str>, timeout: Duration) -> Result<String> {
        refuse_foreign(self.port, || crate::steamvr::pids_named("vrserver"))?;
        self.send(method, path, body, timeout)
    }

    fn send(&self, method: &str, path: &str, body: Option<&str>, timeout: Duration) -> Result<String> {
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, self.port));
        let mut stream = TcpStream::connect_timeout(&addr, timeout.min(Duration::from_secs(1)))
            .with_context(|| format!("ALVR API not reachable on port {}", self.port))?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        stream.write_all(build_request(method, path, body).as_bytes()).with_context(|| format!("{method} {path}: sending the request to ALVR"))?;
        let mut response = String::new();
        if let Err(e) = stream.take(MAX_MESSAGE as u64 + 1).read_to_string(&mut response) {
            let why = match e.kind() {
                ErrorKind::WouldBlock | ErrorKind::TimedOut => format!("ALVR accepted the connection but did not answer within {} s", timeout.as_secs_f64()),
                _ => "reading ALVR's response".into(),
            };
            return Err(anyhow!(e).context(format!("{method} {path}: {why}")));
        }
        if response.len() > MAX_MESSAGE {
            bail!("{method} {path}: response over {MAX_MESSAGE} bytes");
        }
        let (head, body) = response.split_once("\r\n\r\n").context("malformed HTTP response")?;
        let status = head.lines().next().unwrap_or_default();
        if !status.split(' ').nth(1).is_some_and(|code| code.starts_with('2')) {
            bail!("{method} {path}: {status}: {body}");
        }
        Ok(body.to_owned())
    }

    pub fn get(&self, path: &str) -> Result<String> {
        self.request("GET", path, None, TIMEOUT)
    }

    pub fn post(&self, path: &str) -> Result<()> {
        self.request("POST", path, None, TIMEOUT).map(drop)
    }

    pub fn post_json<T: Serialize>(&self, path: &str, body: &T) -> Result<()> {
        self.request("POST", path, Some(&serde_json::to_string(body)?), TIMEOUT).map(drop)
    }

    pub fn ping(&self) -> bool {
        self.request("GET", "/api/ping", None, QUICK).is_ok()
    }

    pub fn version(&self) -> Result<String> {
        self.get("/api/version")
    }

    /// The version within QUICK, for vr_status.
    pub fn quick_version(&self) -> Result<String> {
        self.request("GET", "/api/version", None, QUICK)
    }

    pub fn events(&self) -> Result<EventStream> {
        refuse_foreign(self.port, || crate::steamvr::pids_named("vrserver"))?;
        self.connect_events()
    }

    fn connect_events(&self) -> Result<EventStream> {
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, self.port));
        let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(1))
            .with_context(|| format!("ALVR API not reachable on port {}", self.port))?;
        stream.set_read_timeout(Some(HANDSHAKE))?;
        stream.set_write_timeout(Some(HANDSHAKE))?;
        let mut request = format!("ws://127.0.0.1:{}/api/events", self.port).into_client_request()?;
        request.headers_mut().insert("X-ALVR", "1".parse()?);
        let config = WebSocketConfig::default().max_message_size(Some(MAX_MESSAGE)).max_frame_size(Some(MAX_MESSAGE));
        let (socket, _) = tungstenite::client::client_with_config(request, stream, Some(config)).map_err(|e| anyhow!("ALVR events handshake: {e}"))?;
        Ok(EventStream(socket))
    }
}

pub struct EventStream(WebSocket<TcpStream>);

impl EventStream {
    /// The next event, or None when none arrives within `timeout`.
    pub fn next(&mut self, timeout: Duration) -> Result<Option<Event>> {
        self.0.get_ref().set_read_timeout(Some(timeout))?;
        loop {
            match self.0.read() {
                // skip what does not parse, as ALVR's dashboard does (dashboard/src/data_sources.rs)
                Ok(Message::Text(text)) => match serde_json::from_str(&text) {
                    Ok(event) => return Ok(Some(event)),
                    Err(e) => warn!("skipping ALVR event that does not parse: {e}"),
                },
                Ok(Message::Close(_)) => bail!("ALVR closed the events stream"),
                Ok(_) => (),
                Err(tungstenite::Error::Io(e)) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    return Ok(None);
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
}

/// Refuses a call unless every socket listening on `port` is held by one of `vrservers` (this
/// user's): another user or program can hold the port, and would take our POSTs and feed us forged
/// sessions, restart requests and log lines. Verified sockets are remembered: an inode names one socket while it lives.
// shortcut: checked before connecting, so a listener swapped in between is missed; compare the accepted socket's inode if that matters
fn refuse_foreign(port: u16, vrservers: impl FnOnce() -> Vec<u32>) -> Result<()> {
    static VERIFIED: Mutex<BTreeSet<u64>> = Mutex::new(BTreeSet::new());
    let listening: Vec<u64> = ["/proc/net/tcp", "/proc/net/tcp6"]
        .iter()
        .flat_map(|t| listening_inodes(&fs::read_to_string(t).unwrap_or_default(), port))
        .collect();
    if listening.is_empty() {
        bail!("ALVR API not reachable on port {port}");
    }
    let mut verified = VERIFIED.lock();
    if listening.iter().all(|i| verified.contains(i)) {
        return Ok(());
    }
    let held: HashSet<u64> = vrservers().into_iter().flat_map(socket_inodes).collect();
    if !listening.iter().all(|i| held.contains(i)) {
        bail!("not ours: the ALVR on port {port} does not belong to this user's SteamVR");
    }
    verified.extend(listening);
    Ok(())
}

/// The inodes of the sockets in LISTEN state on `port` in a /proc/net/tcp or tcp6 table, at an
/// address a connect to 127.0.0.1 reaches: 127.0.0.1, 0.0.0.0, [::] or [::ffff:127.0.0.1].
/// Others (127.0.0.2, a LAN address, [::1]) never take our requests, so they are not in the way.
fn listening_inodes(table: &str, port: u16) -> Vec<u64> {
    const REACHES_LOOPBACK: [&str; 4] = ["0100007F", "00000000", "00000000000000000000000000000000", "0000000000000000FFFF00000100007F"];
    table
        .lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            let (local, state, inode) = (f.get(1)?, f.get(3)?, f.get(9)?);
            let (addr, p) = local.split_once(':')?;
            (u16::from_str_radix(p, 16) == Ok(port) && REACHES_LOOPBACK.contains(&addr) && *state == "0A").then(|| inode.parse().ok()).flatten()
        })
        .collect()
}

/// The inodes of the sockets `pid` holds open.
fn socket_inodes(pid: u32) -> Vec<u64> {
    fs::read_dir(format!("/proc/{pid}/fd"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|fd| fs::read_link(fd.path()).ok()?.to_str()?.strip_prefix("socket:[")?.strip_suffix(']')?.parse().ok())
        .collect()
}

pub fn build_request(method: &str, path: &str, body: Option<&str>) -> String {
    let body = body.unwrap_or_default();
    format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nX-ALVR: 1\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// ALVR's `protocol_id` rule (common/src/version.rs) applied to another version.
pub fn protocol_id_of(version: &str) -> Result<String> {
    let v = Version::parse(version.trim())?;
    Ok(if v.pre.is_empty() { v.major.to_string() } else { format!("{}-{}", v.major, v.pre) })
}

/// True when a server of this version speaks our protocol: same major for stable, same pre-release otherwise.
pub fn compatible(server_version: &str) -> bool {
    protocol_id_of(server_version).is_ok_and(|id| id == alvr_common::protocol_id())
}

/// The mean of ALVR's 500 ms statistics summaries.
#[derive(Debug, Serialize)]
pub struct AlvrSummary {
    pub samples: usize,
    pub total_latency_ms: f32,
    pub network_latency_ms: f32,
    pub encode_latency_ms: f32,
    pub decode_latency_ms: f32,
    pub client_fps: f32,
    pub server_fps: f32,
    pub video_mbits_per_sec: f32,
}

pub fn average(samples: &[alvr_events::StatisticsSummary]) -> Option<AlvrSummary> {
    if samples.is_empty() {
        return None;
    }
    let mean = |f: fn(&alvr_events::StatisticsSummary) -> f32| samples.iter().map(f).sum::<f32>() / samples.len() as f32;
    Some(AlvrSummary {
        samples: samples.len(),
        total_latency_ms: mean(|s| s.total_latency_ms),
        network_latency_ms: mean(|s| s.network_latency_ms),
        encode_latency_ms: mean(|s| s.encode_latency_ms),
        decode_latency_ms: mean(|s| s.decode_latency_ms),
        client_fps: mean(|s| s.client_fps as f32),
        server_fps: mean(|s| s.server_fps as f32),
        video_mbits_per_sec: mean(|s| s.video_mbits_per_sec),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn average_means_samples() {
        let at = |ms| alvr_events::StatisticsSummary { total_latency_ms: ms, ..Default::default() };
        let a = average(&[at(10.0), at(20.0)]).unwrap();
        assert_eq!((a.samples, a.total_latency_ms), (2, 15.0));
        assert!(average(&[]).is_none());
    }

    #[test]
    fn compatible_follows_protocol_id() {
        assert!(compatible(&alvr_common::ALVR_VERSION.to_string()));
        assert!(compatible("21.0.0-dev14+nightly.2026.10.19")); // build metadata ignored
        assert!(!compatible("21.0.0-dev13"));
        assert!(!compatible("20.14.1"));
    }

    #[test]
    fn stable_versions_match_on_major() {
        assert_eq!(protocol_id_of("21.3.0").unwrap(), "21");
        assert_eq!(protocol_id_of("21.0.0-dev14").unwrap(), "21-dev14");
    }

    #[test]
    fn ping_gives_up_fast_on_a_stalled_server() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap(); // accepts, never answers
        let api = Api { port: listener.local_addr().unwrap().port() };
        let started = std::time::Instant::now();
        assert!(api.send("GET", "/api/ping", None, QUICK).is_err());
        // QUICK, not TIMEOUT: a margin of several seconds keeps a loaded machine from failing it
        assert!(started.elapsed() < TIMEOUT / 2, "{:?}", started.elapsed());
    }

    #[test]
    fn a_silent_server_errors_with_the_route_and_wait() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap(); // the kernel accepts, nobody answers
        let api = Api { port: listener.local_addr().unwrap().port() };
        let e = api.send("GET", "/api/ping", None, Duration::from_millis(200)).unwrap_err();
        assert_eq!(e.to_string(), "GET /api/ping: ALVR accepted the connection but did not answer within 0.2 s");
        // stack::timed_out reads the io::Error under the context to restart a stuck adopted ALVR
        let io = e.chain().find_map(|c| c.downcast_ref::<std::io::Error>()).expect("the io::Error stays the source");
        assert!(matches!(io.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut), "{io:?}");
    }

    #[test]
    fn version_probe_and_events_handshake_give_up_on_a_stalled_server() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap(); // accepts, never answers
        let api = Api { port: listener.local_addr().unwrap().port() };
        let started = std::time::Instant::now();
        assert!(api.send("GET", "/api/version", None, QUICK).is_err());
        assert!(started.elapsed() < TIMEOUT / 2, "{:?}", started.elapsed());
        let started = std::time::Instant::now();
        assert!(api.connect_events().is_err());
        assert!(started.elapsed() < HANDSHAKE * 2, "{:?}", started.elapsed());
    }

    #[test]
    fn only_the_listener_our_vrserver_holds_is_ours() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let e = refuse_foreign(port, Vec::new).unwrap_err().to_string();
        assert_eq!(e, format!("not ours: the ALVR on port {port} does not belong to this user's SteamVR"));
        // every call goes through the check, reads and the events stream included; no vrserver holds this listener
        let api = Api { port };
        for e in [api.get("/api/session").unwrap_err(), api.post("/api/x").unwrap_err(), api.events().err().unwrap()] {
            assert!(e.to_string().starts_with("not ours"), "{e}");
        }
        assert!(refuse_foreign(port, || vec![std::process::id()]).is_ok(), "this process holds it");
        assert!(refuse_foreign(port, || panic!("verified once")).is_ok());
        drop(listener);
        // a child another test forks meanwhile holds the fd until its exec, so give the close a moment
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let closed = loop {
            match refuse_foreign(port, Vec::new) {
                Err(e) => break e.to_string(),
                Ok(()) if std::time::Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
                Ok(()) => panic!("port {port} still listens 2 s after its listener closed"),
            }
        };
        assert!(closed.contains("not reachable"), "{closed}");
    }

    #[test]
    fn listening_inodes_reads_proc_net_tables() {
        let tcp = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:1F92 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 4242 1 0000000000000000 100 0 0 10 0
   1: 0100007F:1F92 0100007F:D0A2 01 00000000:00000000 00:00000000 00000000  1000        0 4343 1 0000000000000000 20 4 30 10 -1
   2: 0100007F:1F93 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 4444 1 0000000000000000 100 0 0 10 0";
        assert_eq!(listening_inodes(tcp, 8082), [4242], "LISTEN on the port only");
        // only listeners a connect to 127.0.0.1 can reach: another user's on 127.0.0.2 or a LAN address is not in the way
        let other = "   0: 0200007F:1F92 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1001        0 11 1 0000000000000000 100 0 0 10 0
   1: 0A00A8C0:1F92 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1001        0 12 1 0000000000000000 100 0 0 10 0
   2: 00000000:1F92 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1001        0 13 1 0000000000000000 100 0 0 10 0";
        assert_eq!(listening_inodes(other, 8082), [13], "0.0.0.0 takes 127.0.0.1, 127.0.0.2 and 192.168.0.10 do not");
        let tcp6 = "  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000000000000000000000000000:1F92 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1001        0 77 1 0000000000000000 100 0 0 10 0
   1: 00000000000000000000000001000000:1F92 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1001        0 78 1 0000000000000000 100 0 0 10 0
   2: 0000000000000000FFFF00000100007F:1F92 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1001        0 79 1 0000000000000000 100 0 0 10 0";
        assert_eq!(listening_inodes(tcp6, 8082), [77, 79], "[::] and [::ffff:127.0.0.1], not [::1]");
    }

    #[test]
    fn responses_are_capped() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let api = Api { port: listener.local_addr().unwrap().port() };
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let _ = s.read(&mut [0; 4096]); // the request; unread, closing would reset the connection
            s.write_all(b"HTTP/1.1 200 OK\r\n\r\n").ok();
            let chunk = vec![b'a'; 1 << 20];
            while s.write_all(&chunk).is_ok() {} // endless: only the client's cap ends it
        });
        let e = api.send("GET", "/api/session", None, TIMEOUT).unwrap_err();
        assert!(e.to_string().contains("response over"), "{e}");
        server.join().unwrap();
    }

    #[test]
    fn request_bytes_carry_x_alvr() {
        let req = build_request("POST", "/api/session/values", Some("[]"));
        for part in ["POST /api/session/values HTTP/1.1\r\n", "X-ALVR: 1\r\n", "Content-Length: 2\r\n", "Connection: close\r\n"] {
            assert!(req.contains(part), "{part:?} missing in {req:?}");
        }
        assert!(req.ends_with("\r\n\r\n[]"));
    }
}
