use alvr_client_core::{ClientCapabilities, ClientCoreContext, ClientCoreEvent};
use alvr_common::{
    Fov, HAND_LEFT_ID, HAND_RIGHT_ID, Pose, RelaxedAtomic, ViewParams,
    anyhow::{Context, Result, bail},
    debug,
    glam::{Quat, UVec2, Vec3},
    parking_lot::Mutex,
    warn,
};
use crate::{alvr_api::Api, clock, rig::Rig};
use openh264::{decoder::Decoder, formats::YUVSource};
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Default, Serialize)]
pub struct HeadsetStatus {
    pub streaming: bool,
    pub fps: f32,
    pub view_resolution: [u32; 2],
    pub codec: Option<String>,
    pub frames_received: u64,
    pub hud_message: String,
    /// Streams started by any headset of this MCP, as of this headset's latest; grows on every
    /// StreamingStarted, so a caller sees a new stream even when it missed the gap between.
    pub stream_generation: u64,
}

/// Which part of the side-by-side frame a screenshot shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(crate = "rmcp::schemars")]
pub enum Eye {
    #[default]
    Both,
    Left,
    Right,
}

/// Where a screenshot comes from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Deserialize, rmcp::schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(crate = "rmcp::schemars")]
pub enum Source {
    /// The H264 stream ALVR sends to the virtual headset.
    #[default]
    Headset,
    /// SteamVR's compositor: the scene app's uncompressed textures, side by side.
    Compositor,
}

impl Source {
    pub fn name(self) -> &'static str {
        match self {
            Source::Headset => "headset",
            Source::Compositor => "compositor",
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Screenshot {
    #[serde(skip)]
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub source: &'static str,
    /// The stream's timestamp of the frame; only for the headset.
    pub frame_timestamp_ms: Option<u64>,
    /// When the returned frame arrived (headset) or the compositor was asked (compositor), on the session clock.
    pub t_s: f64,
    pub at: String,
}

/// Shared by every headset, so a recreated headset never repeats a generation.
static STREAMS_STARTED: AtomicU64 = AtomicU64::new(0);

const HAPTICS_KEPT: usize = 1024;

/// A haptic pulse the app sent. `t_s` is seconds on the session clock; `at` is the same instant as RFC 3339.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct HapticEvent {
    pub t_s: f64,
    pub at: String,
    /// "left", "right" or "other".
    pub hand: String,
    pub duration_s: f32,
    pub frequency: f32,
    pub amplitude: f32,
}

/// Frames a screenshot decodes after the fresh IDR: 250 ms at 72 fps, time for a pose just set to reach the game and for P-frames to sharpen the image.
const FRAMES_AFTER_IDR: usize = 18;
/// How long a screenshot waits for the IDR it asked for.
const IDR_WAIT: Duration = Duration::from_secs(2);
/// How long it waits for the frames after the IDR.
const FRAMES_WAIT: Duration = Duration::from_secs(1);

/// A video frame as it arrived; `t_s` on the session clock.
struct Frame {
    timestamp: Duration,
    nal: Vec<u8>,
    t_s: f64,
}

#[derive(Default)]
struct Shared {
    status: HeadsetStatus,
    config_nal: Vec<u8>,
    /// Set by a screenshot in progress: an IDR frame and the frames after it land in `frames`.
    armed: bool,
    /// At most FRAMES_AFTER_IDR + 1, starting at an IDR.
    frames: Vec<Frame>,
    /// Timestamp of the newest video frame, for ALVR's client statistics.
    last_frame: Duration,
    /// The newest HAPTICS_KEPT haptic events, oldest first.
    // shortcut: lost when the headset is recreated (watchdog or reconnect); move to Stack if tests read across one
    haptics: VecDeque<HapticEvent>,
}

impl Shared {
    /// The decoder callback. Accepts a frame only once DecoderConfig has arrived, as a real client
    /// does: the server sends DecoderConfig only in reply to RequestIdr (server_core
    /// connection.rs:1287-1294), and the client sends one for every frame not accepted.
    fn on_frame(&mut self, timestamp: Duration, nal: &[u8]) -> bool {
        self.status.frames_received += 1;
        self.last_frame = timestamp;
        // a copy only: decoding waits for the screenshot's own thread
        if self.armed && self.frames.len() <= FRAMES_AFTER_IDR {
            let idr = has_idr(nal);
            if idr {
                self.frames.clear(); // a later IDR (the client's RequestIdr after a loss, or ALVR's) starts the chain again
            }
            if idr || !self.frames.is_empty() {
                self.frames.push(Frame { timestamp, nal: nal.to_vec(), t_s: clock::now_s() });
            }
        }
        self.status.codec.is_some()
    }

    fn on_decoder_config(&mut self, codec: String, config_nal: Vec<u8>) {
        self.status.codec = Some(codec);
        self.config_nal = config_nal;
    }

    fn on_haptics(&mut self, event: HapticEvent) {
        if self.haptics.len() == HAPTICS_KEPT {
            self.haptics.pop_front();
        }
        self.haptics.push_back(event);
    }

    /// Events after `since_s`, oldest first; `clear` empties the ring.
    fn haptics(&mut self, since_s: Option<f64>, clear: bool) -> Vec<HapticEvent> {
        let events = self.haptics.iter().filter(|e| since_s.is_none_or(|s| e.t_s > s)).cloned().collect();
        if clear {
            self.haptics.clear();
        }
        events
    }

    fn on_streaming_started(&mut self, fps: f32, view_resolution: [u32; 2]) {
        let status = &mut self.status;
        (status.streaming, status.fps, status.view_resolution) = (true, fps, view_resolution);
        // The handshake's "stream will begin soon" text would otherwise outlive the stream start.
        status.hud_message.clear();
        status.stream_generation = STREAMS_STARTED.fetch_add(1, Ordering::Relaxed) + 1;
    }

    fn on_streaming_stopped(&mut self) {
        let status = &mut self.status;
        (status.streaming, status.fps, status.view_resolution, status.codec) = (false, 0.0, [0, 0], None);
    }
}

/// The virtual headset: a stock `ClientCoreContext` driven like `alvr_client_mock`, without its GUI.
pub struct Headset {
    context: Arc<ClientCoreContext>,
    shared: Arc<Mutex<Shared>>,
    stop: Arc<RelaxedAtomic>,
    thread: Option<JoinHandle<()>>,
}

pub fn capabilities() -> ClientCapabilities {
    let resolution = UVec2::new(1088, 1216);
    ClientCapabilities {
        platform: alvr_system_info::platform(None, None),
        default_view_resolution: resolution,
        max_view_resolution: resolution,
        refresh_rates: vec![72.0, 90.0],
        foveated_encoding: false,
        encoder_high_profile: false,
        encoder_10_bits: false,
        encoder_av1: false,
        prefer_10bit: false,
        preferred_encoding_gamma: 1.0,
        prefer_hdr: false,
    }
}

/// The hostname `ClientCoreContext::new` stored; read it after `Headset::start`, which may reset it.
pub fn client_hostname() -> Result<String> {
    let path = dirs::config_dir().context("no config dir")?.join("ALVR Client/session.json");
    let json: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).context("reading ALVR client session")?)?;
    json["hostname"].as_str().map(str::to_owned).context("no hostname in ALVR client session")
}

/// Whether an Annex-B stream holds an IDR slice (NAL type 5).
pub fn has_idr(annexb: &[u8]) -> bool {
    annexb.windows(4).any(|w| w[..3] == [0, 0, 1] && w[3] & 0x1f == 5)
}

/// Decodes one IDR frame, after the codec's config NALs, into `(width, height, RGB8)`.
pub fn decode_idr(config_nal: &[u8], frame: &[u8]) -> Result<(u32, u32, Vec<u8>)> {
    decode_chain(config_nal, &[frame]).map(|(_, w, h, rgb)| (w, h, rgb))
}

/// Decodes `frames`, an IDR and the frames after it, into the index of the last one decoded and its `(width, height, RGB8)`.
/// The IDR must decode. A later frame that fails ends the chain at the last good picture: past a lost or broken frame
/// the picture would be corrupt (OpenH264 runs without error concealment, so a frame_num gap is an error).
// shortcut: converts every decoded frame to RGB; convert only the last and re-decode on the rare error if the rig shows this slow
fn decode_chain(config_nal: &[u8], frames: &[&[u8]]) -> Result<(usize, u32, u32, Vec<u8>)> {
    let Some((first, rest)) = frames.split_first() else { bail!("no frame to decode") };
    let mut decoder = Decoder::new()?;
    let mut rgb = vec![];
    let to_rgb = |yuv: openh264::decoder::DecodedYUV<'_>, rgb: &mut Vec<u8>| {
        let (w, h) = yuv.dimensions();
        rgb.resize(w * h * 3, 0);
        yuv.write_rgb8(rgb);
        (w as u32, h as u32)
    };
    let yuv = decoder.decode(&[config_nal, first].concat())?.context("the decoder produced no picture")?;
    let (mut index, (mut w, mut h)) = (0, to_rgb(yuv, &mut rgb));
    for (i, frame) in rest.iter().enumerate() {
        match decoder.decode(frame) {
            Ok(Some(yuv)) => (index, (w, h)) = (i + 1, to_rgb(yuv, &mut rgb)),
            Ok(None) => (),
            Err(e) => {
                warn!("screenshot: frame {} of {} after the IDR failed to decode ({e}); returning frame {index}", i + 1, rest.len());
                break;
            }
        }
    }
    Ok((index, w, h, rgb))
}

/// `eye` of the last frame of `frames` that decodes, stamped with that frame's timestamp and arrival.
fn screenshot_of(config_nal: &[u8], frames: &[Frame], eye: Eye) -> Result<Screenshot> {
    let (index, width, height, rgb) = decode_chain(config_nal, &frames.iter().map(|f| f.nal.as_slice()).collect::<Vec<_>>())?;
    let (width, rgb) = crop(eye, width, height, &rgb);
    let png = encode_rgb(width, height, &rgb)?;
    let frame = &frames[index];
    let (source, frame_timestamp_ms) = (Source::Headset.name(), Some(frame.timestamp.as_millis() as u64));
    Ok(Screenshot { png, width, height, source, frame_timestamp_ms, t_s: frame.t_s, at: clock::at(frame.t_s) })
}

/// The part of a side-by-side RGB8 frame `eye` shows: ALVR packs the left eye on the left half.
fn crop(eye: Eye, width: u32, height: u32, rgb: &[u8]) -> (u32, Vec<u8>) {
    let (width, half) = (width as usize, width as usize / 2);
    let (from, to) = match eye {
        Eye::Both => return (width as u32, rgb.to_vec()),
        Eye::Left => (0, half),
        Eye::Right => (half, width),
    };
    let rows = (0..height as usize).flat_map(|y| &rgb[(y * width + from) * 3..(y * width + to) * 3]);
    ((to - from) as u32, rows.copied().collect())
}

pub fn encode_rgb(width: u32, height: u32, rgb: &[u8]) -> Result<Vec<u8>> {
    let mut png = vec![];
    let mut encoder = png::Encoder::new(&mut png, width, height);
    (encoder.set_color(png::ColorType::Rgb), encoder.set_depth(png::BitDepth::Eight));
    encoder.write_header()?.write_image_data(rgb)?;
    Ok(png)
}

/// `eye` of a side-by-side PNG of any color type, as width, height and an 8-bit RGB PNG.
pub fn split_png(png: &[u8], eye: Eye) -> Result<(u32, u32, Vec<u8>)> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(png));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info()?;
    let mut buf = vec![0; reader.output_buffer_size().context("PNG too large")?];
    let info = reader.next_frame(&mut buf)?;
    let samples = info.color_type.samples();
    let rgb: Vec<u8> = buf[..info.buffer_size()]
        .chunks(samples)
        .flat_map(|p| if samples < 3 { [p[0]; 3] } else { [p[0], p[1], p[2]] })
        .collect();
    let (width, rgb) = crop(eye, info.width, info.height, &rgb);
    Ok((width, info.height, encode_rgb(width, info.height, &rgb)?))
}

fn hand_name(device_id: u64) -> &'static str {
    match device_id {
        id if id == *HAND_LEFT_ID => "left",
        id if id == *HAND_RIGHT_ID => "right",
        _ => "other",
    }
}

fn view_params() -> [ViewParams; 2] {
    let eye = |x: f32, fov| ViewParams { pose: Pose { orientation: Quat::IDENTITY, position: Vec3::new(x, 0.0, 0.0) }, fov };
    [
        eye(-0.0315, Fov { left: -0.96, right: 0.87, up: 0.96, down: -0.96 }),
        eye(0.0315, Fov { left: -0.87, right: 0.96, up: 0.96, down: -0.96 }),
    ]
}

fn tracking_thread(context: Arc<ClientCoreContext>, rig: Arc<Mutex<Rig>>, streaming: Arc<RelaxedAtomic>, fps: f32) {
    let origin = Instant::now();
    let mut deadline = Instant::now();
    // shortcut: resends held inputs once, 1 s in, when the controller poses have surely reached
    // SteamVR (it drops buttons before, Controller.cpp:145-147); resend on pose validity if 1 s proves short
    let mut resend_at = Some(origin + Duration::from_secs(1));
    rig.lock().recenter(); // a new stream gets the current play area
    while streaming.value() {
        let (data, buttons, area) = {
            let mut rig = rig.lock();
            let mut buttons = rig.take_buttons();
            if resend_at.is_some_and(|t| Instant::now() >= t) {
                (resend_at, buttons) = (None, rig.all_buttons());
            }
            (rig.tracking(Instant::now(), origin.elapsed()), buttons, rig.take_playspace())
        };
        if area.is_some() {
            context.send_playspace(area);
        }
        context.send_tracking(data);
        if !buttons.is_empty() {
            context.send_buttons(buttons);
        }
        deadline += Duration::from_secs_f32(1.0 / fps / 3.0);
        thread::sleep(deadline.saturating_duration_since(Instant::now()));
    }
}

/// Port of the mock's `client_thread` loop: events, then one emulated frame per frame period.
fn event_thread(context: Arc<ClientCoreContext>, shared: Arc<Mutex<Shared>>, rig: Arc<Mutex<Rig>>, stop: Arc<RelaxedAtomic>) {
    let streaming = Arc::new(RelaxedAtomic::new(false));
    let mut tracking = None;
    let mut reported_frame = Duration::ZERO;
    let mut deadline = Instant::now();
    while !stop.value() {
        while let Some(event) = context.poll_event() {
            match event {
                ClientCoreEvent::UpdateHudMessage(message) => shared.lock().status.hud_message = message,
                ClientCoreEvent::StreamingStarted(config) => {
                    let negotiated = &config.negotiated_config;
                    let fps = negotiated.refresh_rate_hint;
                    shared.lock().on_streaming_started(fps, negotiated.view_resolution.to_array());
                    // SteamVR 2.18 keeps an OpenXR session at VISIBLE until it has a proximity state.
                    let order = rig.lock().proximity_send.clone();
                    let _order = order.lock();
                    let on_head = rig.lock().on_head;
                    context.send_proximity_state(on_head);
                    context.send_view_params(view_params());
                    streaming.set(true);
                    let (context, rig, streaming) = (Arc::clone(&context), Arc::clone(&rig), Arc::clone(&streaming));
                    tracking = Some(thread::spawn(move || tracking_thread(context, rig, streaming, fps)));
                }
                ClientCoreEvent::StreamingStopped => {
                    streaming.set(false);
                    if let Some(t) = tracking.take() {
                        t.join().ok();
                    }
                    shared.lock().on_streaming_stopped();
                }
                ClientCoreEvent::DecoderConfig { codec, config_nal } => {
                    shared.lock().on_decoder_config(format!("{codec:?}"), config_nal);
                }
                ClientCoreEvent::Haptics { device_id, duration, frequency, amplitude } => {
                    let t_s = clock::now_s();
                    let hand = hand_name(device_id).to_owned();
                    shared.lock().on_haptics(HapticEvent { t_s, at: clock::at(t_s), hand, duration_s: duration.as_secs_f32(), frequency, amplitude });
                }
                ClientCoreEvent::RealTimeConfig(_) => (),
            }
        }

        // The mock reports a constant zero timestamp, which matches no frame and spams
        // "Statistics summary not ready!"; report the newest received frame instead.
        let frame = shared.lock().last_frame;
        if frame != reported_frame {
            reported_frame = frame;
            context.report_compositor_start(frame);
            context.report_submit(frame, Duration::ZERO);
        }

        let fps = shared.lock().status.fps;
        deadline += Duration::from_secs_f32(1.0 / if fps > 0.0 { fps } else { 72.0 });
        thread::sleep(deadline.saturating_duration_since(Instant::now()).max(Duration::from_millis(3)));
    }
    streaming.set(false);
    if let Some(t) = tracking {
        t.join().ok();
    }
}

impl Headset {
    /// Starts the client; its tracking thread sends what `rig` holds.
    pub fn start(rig: Arc<Mutex<Rig>>) -> Headset {
        let context = Arc::new(ClientCoreContext::new(capabilities(), vec![]));
        let shared = Arc::new(Mutex::new(Shared::default()));
        let stop = Arc::new(RelaxedAtomic::new(false));
        let frames = Arc::clone(&shared);
        context.set_decoder_input_callback(Box::new(move |timestamp, nal| frames.lock().on_frame(timestamp, nal)));
        context.resume();
        let thread = thread::spawn({
            let (context, shared, stop) = (Arc::clone(&context), Arc::clone(&shared), Arc::clone(&stop));
            move || event_thread(context, shared, rig, stop)
        });
        Headset { context, shared, stop, thread: Some(thread) }
    }

    pub fn status(&self) -> HeadsetStatus {
        self.shared.lock().status.clone()
    }

    /// Haptic events with `t_s` after `since_s`, oldest first; `clear` empties the log.
    pub fn haptics(&self, since_s: Option<f64>, clear: bool) -> Vec<HapticEvent> {
        self.shared.lock().haptics(since_s, clear)
    }

    /// A handle that takes screenshots without the headset itself, so callers need not hold a lock around the wait.
    pub fn grabber(&self) -> Grabber {
        Grabber(Arc::clone(&self.shared))
    }

    /// Disconnects and stays disconnected until `resume`.
    pub fn pause(&self) {
        self.context.pause();
    }

    pub fn resume(&self) {
        self.context.resume();
    }

    pub fn context(&self) -> Arc<ClientCoreContext> {
        Arc::clone(&self.context)
    }
}

pub struct Grabber(Arc<Mutex<Shared>>);

impl Grabber {
    /// Asks ALVR for an IDR frame, waits up to IDR_WAIT for it and FRAMES_WAIT for the FRAMES_AFTER_IDR frames after it,
    /// and returns `eye` of the last frame that decodes as a PNG: sharper than the IDR alone, and a pose just set has usually reached the game.
    // shortcut: one screenshot at a time (a second call's arming cuts the first short, now likelier over the longer wait), enough for one MCP client
    pub fn screenshot(&self, api: &Api, eye: Eye) -> Result<Screenshot> {
        let codec = self.0.lock().status.codec.clone().context("the headset has not received ALVR's decoder config yet")?;
        if codec != "H264" {
            bail!("screenshots need H264; ALVR codec is {codec}");
        }
        {
            let mut shared = self.0.lock();
            shared.armed = true;
            shared.frames.clear();
        }
        if let Err(e) = api.post("/api/insert-idr") {
            self.0.lock().armed = false;
            return Err(e);
        }
        let (frames, config_nal) = self.wait_frames(IDR_WAIT, FRAMES_WAIT);
        if frames.is_empty() {
            bail!("no IDR frame arrived (deadline {} s)", IDR_WAIT.as_secs());
        }
        if frames.len() <= FRAMES_AFTER_IDR {
            debug!("screenshot: only {} of {} frames after the IDR arrived; returning the last that decodes", frames.len() - 1, FRAMES_AFTER_IDR);
        }
        screenshot_of(&config_nal, &frames, eye)
    }

    /// Polls every 10 ms, without holding the lock between polls, until FRAMES_AFTER_IDR frames followed the IDR, `tail` passed
    /// since the IDR (`idr_wait` before it), or the stream stopped after the IDR. Disarms, and returns the frames and the config NALs.
    fn wait_frames(&self, idr_wait: Duration, tail: Duration) -> (Vec<Frame>, Vec<u8>) {
        let (mut deadline, mut seen) = (Instant::now() + idr_wait, false);
        loop {
            let mut shared = self.0.lock();
            if !seen && !shared.frames.is_empty() {
                (deadline, seen) = (Instant::now() + tail, true); // a later IDR does not extend it
            }
            let stopped = seen && !shared.status.streaming;
            if shared.frames.len() > FRAMES_AFTER_IDR || stopped || Instant::now() >= deadline {
                shared.armed = false;
                return (std::mem::take(&mut shared.frames), shared.config_nal.clone());
            }
            drop(shared);
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Headset {
    fn drop(&mut self) {
        self.stop.set(true);
        if let Some(t) = self.thread.take() {
            t.join().ok();
        }
        // the context's own Drop joins its connection thread once this last Arc goes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alvr_common::glam::UVec2;

    #[test]
    fn sources_are_headset_and_compositor_only() {
        let parse = |s: &str| serde_json::from_value::<Source>(serde_json::json!(s)).map(Source::name).map_err(|e| e.to_string());
        assert_eq!(parse("headset"), Ok("headset"));
        assert_eq!(parse("compositor"), Ok("compositor"));
        assert!(parse("desktop").is_err(), "the desktop source showed the user's whole screen");
    }

    #[test]
    fn capabilities_are_modest() {
        let c = capabilities();
        assert_eq!(c.default_view_resolution, UVec2::new(1088, 1216));
        assert_eq!(c.max_view_resolution, c.default_view_resolution);
        assert_eq!(c.default_view_resolution % 32, UVec2::ZERO);
        assert_eq!(c.refresh_rates, vec![72.0, 90.0]);
        assert!(!c.foveated_encoding);
    }

    #[test]
    fn frames_are_accepted_only_after_decoder_config() {
        let mut s = Shared::default();
        // not accepted: the client then sends RequestIdr, the only trigger for DecoderConfig
        assert!(!s.on_frame(Duration::from_millis(1), &[]));
        s.on_decoder_config("H264".into(), vec![0, 0, 1]);
        assert!(s.on_frame(Duration::from_millis(2), &[]));
        s.on_streaming_stopped();
        assert!(!s.on_frame(Duration::from_millis(3), &[]), "a new stream needs a new DecoderConfig");
        assert_eq!((s.status.frames_received, s.last_frame), (3, Duration::from_millis(3)));
        assert_eq!(s.status.codec, None);
    }

    #[test]
    fn stream_generation_grows_across_headsets() {
        let (mut a, mut b) = (Shared::default(), Shared::default());
        a.on_streaming_started(72.0, [1088, 1216]);
        b.on_streaming_started(90.0, [1088, 1216]);
        assert!(a.status.streaming && a.status.fps == 72.0 && a.status.view_resolution == [1088, 1216]);
        assert!(a.status.stream_generation > 0 && b.status.stream_generation > a.status.stream_generation);
        b.on_streaming_stopped();
        assert!(b.status.stream_generation > a.status.stream_generation, "kept after the stream stops");
    }

    #[test]
    fn streaming_clears_the_handshake_hud() {
        let mut s = Shared::default();
        s.status.hud_message = "ALVR v21\nThe stream will begin soon\nPlease wait...".into();
        s.on_streaming_started(72.0, [1088, 1216]);
        assert_eq!(s.status.hud_message, "");
    }

    // 128x64, red left, blue right; regenerate with: ffmpeg -f lavfi -i "color=red:s=64x64,format=yuv420p" -f lavfi -i "color=blue:s=64x64,format=yuv420p" -filter_complex hstack -frames:v 1 -c:v libx264 -profile:v baseline -f h264 red_blue.h264
    const FIXTURE: &[u8] = include_bytes!("../tests/fixtures/red_blue.h264");

    #[test]
    fn has_idr_finds_type_5() {
        assert!(has_idr(FIXTURE));
        assert!(!has_idr(&[0, 0, 0, 1, 0x41, 0x9a]));
    }

    #[test]
    fn decodes_fixture_into_red_left_blue_right() {
        let (w, h, rgb) = decode_idr(&[], FIXTURE).unwrap();
        assert_eq!((w, h), (128, 64));
        let px = |x: usize, y: usize| &rgb[(y * w as usize + x) * 3..][..3];
        let (red, blue) = (px(10, 10), px(100, 10));
        assert!(red[0] > 200 && red[1] < 60 && red[2] < 60, "{red:?}");
        assert!(blue[2] > 200 && blue[0] < 60 && blue[1] < 60, "{blue:?}");
    }

    #[test]
    fn left_eye_crop_is_red_half() {
        let (w, h, rgb) = decode_idr(&[], FIXTURE).unwrap();
        let (cw, left) = crop(Eye::Left, w, h, &rgb);
        assert_eq!((cw, left.len()), (64, 64 * 64 * 3));
        assert!(left.chunks(3).all(|p| p[0] > 200 && p[2] < 60));
        let (cw, right) = crop(Eye::Right, w, h, &rgb);
        assert_eq!(cw, 64);
        assert!(right.chunks(3).all(|p| p[2] > 200 && p[0] < 60));
        assert_eq!(crop(Eye::Both, w, h, &rgb), (128, rgb));
    }

    #[test]
    fn split_png_crops_rgba_halves() {
        let mut rgba = vec![];
        for _ in 0..2 {
            rgba.extend([[255, 0, 0, 255]; 2].concat());
            rgba.extend([[0, 0, 255, 255]; 2].concat());
        }
        let mut png = vec![];
        let mut encoder = png::Encoder::new(&mut png, 4, 2);
        (encoder.set_color(png::ColorType::Rgba), encoder.set_depth(png::BitDepth::Eight));
        encoder.write_header().unwrap().write_image_data(&rgba).unwrap();
        let pixels = |png: &[u8]| {
            let mut reader = png::Decoder::new(std::io::Cursor::new(png)).read_info().unwrap();
            let mut out = vec![0; reader.output_buffer_size().unwrap()];
            let info = reader.next_frame(&mut out).unwrap();
            assert_eq!(info.color_type, png::ColorType::Rgb);
            out
        };
        let (w, h, left) = split_png(&png, Eye::Left).unwrap();
        assert_eq!((w, h), (2, 2));
        assert!(pixels(&left).chunks(3).all(|p| p == [255, 0, 0]));
        let (_, _, right) = split_png(&png, Eye::Right).unwrap();
        assert!(pixels(&right).chunks(3).all(|p| p == [0, 0, 255]));
        let (w, h, both) = split_png(&png, Eye::Both).unwrap();
        assert_eq!((w, h, pixels(&both).len()), (4, 2, 4 * 2 * 3));
    }

    #[test]
    fn haptics_ring_filters_and_clears() {
        let mut s = Shared::default();
        let event = |t_s| HapticEvent { t_s, at: clock::at(t_s), hand: "left".into(), duration_s: 0.1, frequency: 160.0, amplitude: 0.8 };
        for i in 0..1030 {
            s.on_haptics(event(i as f64));
        }
        let all = s.haptics(None, false);
        assert_eq!((all.len(), all[0].t_s, all[1023].t_s), (1024, 6.0, 1029.0), "a ring of 1024");
        assert_eq!(s.haptics(Some(1027.0), false), vec![event(1028.0), event(1029.0)], "strictly after since_s");
        assert_eq!(s.haptics(Some(1028.0), true), vec![event(1029.0)]);
        assert!(s.haptics(None, false).is_empty(), "cleared");
        assert_eq!(hand_name(*alvr_common::HAND_RIGHT_ID), "right");
        assert_eq!(hand_name(*alvr_common::HEAD_ID), "other");
    }

    const P_FRAME: &[u8] = &[0, 0, 0, 1, 0x41, 0x9a];

    #[test]
    fn armed_slot_collects_the_idr_and_the_next_frames() {
        let mut s = Shared::default();
        let timestamps = |s: &Shared| s.frames.iter().map(|f| f.timestamp.as_millis()).collect::<Vec<_>>();
        s.on_frame(Duration::from_millis(1), FIXTURE);
        assert!(s.frames.is_empty(), "not armed");
        s.armed = true;
        s.on_frame(Duration::from_millis(2), P_FRAME);
        assert!(s.frames.is_empty(), "nothing before the IDR");
        s.on_frame(Duration::from_millis(3), FIXTURE);
        s.on_frame(Duration::from_millis(4), P_FRAME);
        assert_eq!(timestamps(&s), [3, 4]);
        s.on_frame(Duration::from_millis(5), FIXTURE);
        assert_eq!(timestamps(&s), [5], "a later IDR restarts the chain");
        for t in 6..40 {
            s.on_frame(Duration::from_millis(t), P_FRAME);
        }
        assert_eq!(s.frames.len(), FRAMES_AFTER_IDR + 1);
        assert_eq!(s.frames.last().unwrap().timestamp, Duration::from_millis(5 + FRAMES_AFTER_IDR as u64));
    }

    /// A grabber whose shared state streams and is armed, and a feeder that runs `feed` on it.
    fn armed_grabber(feed: impl FnOnce(&Mutex<Shared>) + Send + 'static) -> (Grabber, JoinHandle<()>) {
        let shared = Arc::new(Mutex::new(Shared::default()));
        {
            let mut s = shared.lock();
            s.on_streaming_started(72.0, [1088, 1216]);
            (s.armed, s.config_nal) = (true, vec![7]);
        }
        let feeder = Arc::clone(&shared);
        (Grabber(shared), thread::spawn(move || feed(&feeder)))
    }

    fn feed_frames(shared: &Mutex<Shared>, n: u64) {
        for t in 0..n {
            thread::sleep(Duration::from_millis(5));
            shared.lock().on_frame(Duration::from_millis(t), if t == 0 { FIXTURE } else { P_FRAME });
        }
    }

    #[test]
    fn wait_frames_returns_at_the_cap() {
        let (grabber, feeder) = armed_grabber(|s| feed_frames(s, 40));
        let began = Instant::now();
        let (frames, config_nal) = grabber.wait_frames(Duration::from_secs(2), Duration::from_secs(5));
        assert_eq!((frames.len(), config_nal), (FRAMES_AFTER_IDR + 1, vec![7]));
        assert!(began.elapsed() < Duration::from_secs(2), "{:?}", began.elapsed());
        assert!(!grabber.0.lock().armed);
        feeder.join().unwrap();
    }

    #[test]
    fn wait_frames_returns_the_best_frames_at_the_tail() {
        let (grabber, feeder) = armed_grabber(|s| feed_frames(s, 4));
        let began = Instant::now();
        let (frames, _) = grabber.wait_frames(Duration::from_secs(2), Duration::from_millis(300));
        assert_eq!(frames.len(), 4);
        assert!(began.elapsed() >= Duration::from_millis(300) && began.elapsed() < Duration::from_secs(1), "{:?}", began.elapsed());
        feeder.join().unwrap();
    }

    #[test]
    fn wait_frames_returns_at_once_when_the_stream_stops() {
        let (grabber, feeder) = armed_grabber(|s| {
            feed_frames(s, 2);
            s.lock().on_streaming_stopped();
        });
        let began = Instant::now();
        let (frames, _) = grabber.wait_frames(Duration::from_secs(2), Duration::from_secs(5));
        assert_eq!(frames.len(), 2);
        assert!(began.elapsed() < Duration::from_secs(1), "{:?}", began.elapsed());
        feeder.join().unwrap();
    }

    #[test]
    fn wait_frames_without_an_idr_returns_nothing_disarmed() {
        let (grabber, feeder) = armed_grabber(|s| {
            s.lock().on_frame(Duration::ZERO, P_FRAME);
        });
        let (frames, _) = grabber.wait_frames(Duration::from_millis(200), Duration::from_secs(5));
        assert!(frames.is_empty() && !grabber.0.lock().armed);
        feeder.join().unwrap();
    }

    /// `n` 64x64 frames from OpenH264's own encoder: the first dark and an IDR, the rest bright P-frames.
    fn encoded(n: usize) -> Vec<Vec<u8>> {
        use openh264::{OpenH264API, encoder::{BitRate, Encoder, EncoderConfig, FrameType}, formats::YUVBuffer};
        let config = EncoderConfig::new().skip_frames(false).scene_change_detect(false).bitrate(BitRate::from_bps(8_000_000));
        let mut encoder = Encoder::with_api_config(OpenH264API::from_source(), config).unwrap();
        (0..n)
            .map(|i| {
                let mut yuv = vec![128; 64 * 64 * 3 / 2];
                yuv[..64 * 64].fill(if i == 0 { 20 } else { 200 });
                let bits = encoder.encode(&YUVBuffer::from_vec(yuv, 64, 64)).unwrap();
                assert_eq!(bits.frame_type() == FrameType::IDR, i == 0, "frame {i}");
                let nal = bits.to_vec();
                assert!(!nal.is_empty(), "frame {i}");
                nal
            })
            .collect()
    }

    fn mean(rgb: &[u8]) -> f64 {
        rgb.iter().map(|&v| v as f64).sum::<f64>() / rgb.len() as f64
    }

    #[test]
    fn decode_chain_returns_the_last_frame() {
        let stream = encoded(4);
        let (index, w, h, rgb) = decode_chain(&[], &stream.iter().map(Vec::as_slice).collect::<Vec<_>>()).unwrap();
        assert_eq!((index, w, h), (3, 64, 64));
        assert!(mean(&rgb) > 150.0, "{}", mean(&rgb));
        let (_, _, first) = decode_idr(&[], &stream[0]).unwrap();
        assert!(mean(&first) < 60.0, "{}", mean(&first));
    }

    #[test]
    fn a_frame_that_fails_to_decode_ends_the_chain() {
        let stream = encoded(3);
        let garbage: &[u8] = &[0xff; 64]; // no Annex-B start code
        assert_eq!(decode_chain(&[], &[&stream[0], &stream[1], garbage, &stream[2]]).unwrap().0, 1);
    }

    #[test]
    fn a_lost_frame_ends_the_chain() {
        let stream = encoded(3);
        assert_eq!(decode_chain(&[], &[stream[0].as_slice(), &stream[2]]).unwrap().0, 0, "OpenH264 reports the frame_num gap");
    }

    #[test]
    fn screenshot_stamps_describe_the_returned_frame() {
        let frames = [
            Frame { timestamp: Duration::from_millis(5), nal: FIXTURE.to_vec(), t_s: 1.0 },
            Frame { timestamp: Duration::from_millis(6), nal: vec![0xff; 64], t_s: 2.0 },
        ];
        let shot = screenshot_of(&[], &frames, Eye::Both).unwrap();
        assert_eq!((shot.frame_timestamp_ms, shot.t_s, shot.at), (Some(5), 1.0, clock::at(1.0)));
        assert_eq!((shot.width, shot.height), (128, 64));
    }
}
