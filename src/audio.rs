//! `record_audio` and `audio_level`: the default sink's monitor, captured in-process through PipeWire.
use crate::stack::within;
use alvr_common::anyhow::{Result, anyhow, bail};
use pipewire::{
    context::ContextRc,
    keys,
    main_loop::MainLoopRc,
    properties,
    spa::{
        param::audio::{AudioFormat, AudioInfoRaw},
        pod::{self, Pod, Value, serialize::PodSerializer},
        utils::Direction,
    },
    stream::{StreamFlags, StreamRc, StreamState},
};
use serde::Serialize;
use std::{cell::RefCell, io, rc::Rc, time::Duration};

pub const RATE: u32 = 48000;
pub const CHANNELS: u16 = 2;
pub const SILENCE_DBFS: f32 = -120.0;
pub const CHECK_TIMEOUT: Duration = Duration::from_secs(2);
/// How long past the capture itself PipeWire may take to deliver the first frames.
const SLACK: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Level {
    pub rms_dbfs: f32,
    pub peak_dbfs: f32,
}

/// Whether a PipeWire context connects. Only connects: it creates no stream and reads no audio.
pub fn check() -> Result<String, String> {
    if cfg!(test) {
        return Err("not probed in unit tests".to_owned()); // unit tests never touch PipeWire
    }
    let probe = || -> Result<(), pipewire::Error> {
        let mainloop = MainLoopRc::new(None)?;
        ContextRc::new(&mainloop, None)?.connect(None)?;
        Ok(())
    };
    match within(CHECK_TIMEOUT, probe) {
        Some(Ok(())) => Ok("PipeWire connects".to_owned()),
        Some(Err(e)) => Err(format!("PipeWire: {e}")),
        None => Err(format!("no answer from PipeWire (deadline {} s)", CHECK_TIMEOUT.as_secs())),
    }
}

/// Interleaved L, R samples of the default sink's monitor, `seconds` long.
// shortcut: captures the whole default sink, other apps included, target the game's node by application.process.id when a test needs isolation
pub fn capture(seconds: f64) -> Result<Vec<i16>> {
    let want = (seconds * f64::from(RATE)).ceil() as usize * usize::from(CHANNELS);
    let mainloop = MainLoopRc::new(None)?;
    let core = ContextRc::new(&mainloop, None)?.connect_rc(None)?;
    // (samples so far, why it stopped early); every callback runs on this thread
    let state: Rc<RefCell<(Vec<i16>, Option<String>)>> = Rc::default();
    let fail = {
        let (state, mainloop) = (state.clone(), mainloop.clone());
        move |why: String| {
            state.borrow_mut().1.get_or_insert(why);
            mainloop.quit();
        }
    };
    let _core = core
        .add_listener_local()
        .error({
            let fail = fail.clone();
            // id 0 is the connection itself: PipeWire went away
            move |id, _, res, msg| if id == 0 { fail(format!("PipeWire disconnected: {msg} ({res})")) }
        })
        .register();
    let deadline = Duration::from_secs_f64(seconds) + SLACK;
    let timer = mainloop.loop_().add_timer({
        let fail = fail.clone();
        move |_| fail(format!("no audio from the default sink's monitor (deadline {} s); is a default sink running?", deadline.as_secs_f64()))
    });
    timer.update_timer(Some(deadline), None).into_result()?;
    let stream = StreamRc::new(
        core.clone(),
        "SteamVR-MCP-capture",
        properties::properties! {
            *keys::MEDIA_TYPE => "Audio",
            *keys::MEDIA_CATEGORY => "Capture",
            *keys::MEDIA_ROLE => "Music",
            *keys::STREAM_CAPTURE_SINK => "true",
        },
    )?;
    let _listener = stream
        .add_local_listener::<()>()
        .state_changed({
            let fail = fail.clone();
            move |_, _, _, new| if let StreamState::Error(e) = new { fail(format!("capture stream: {e}")) }
        })
        .process({
            let (state, mainloop) = (state.clone(), mainloop.clone());
            move |stream, _| {
                let Some(mut buffer) = stream.dequeue_buffer() else { return };
                let Some(data) = buffer.datas_mut().first_mut() else { return };
                let (offset, size) = (data.chunk().offset() as usize, data.chunk().size() as usize);
                let Some(bytes) = data.data().and_then(|d| d.get(offset..offset + size)) else { return };
                let mut state = state.borrow_mut();
                state.0.extend(bytes.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])));
                if state.0.len() >= want {
                    state.0.truncate(want);
                    mainloop.quit();
                }
            }
        })
        .register()?;
    let mut info = AudioInfoRaw::new();
    info.set_format(AudioFormat::S16LE);
    info.set_rate(RATE);
    info.set_channels(u32::from(CHANNELS));
    let format = PodSerializer::serialize(
        io::Cursor::new(Vec::new()),
        &Value::Object(pod::Object { type_: pipewire::spa::sys::SPA_TYPE_OBJECT_Format, id: pipewire::spa::sys::SPA_PARAM_EnumFormat, properties: info.into() }),
    )
    .map_err(|e| anyhow!("{e:?}"))?
    .0
    .into_inner();
    stream.connect(
        Direction::Input,
        None,
        StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
        &mut [Pod::from_bytes(&format).ok_or_else(|| anyhow!("bad format pod"))?],
    )?;
    mainloop.run();
    let (samples, why) = state.take();
    match why {
        Some(why) => bail!(why),
        None if samples.len() < want => bail!("the capture ended after {} of {want} samples", samples.len()),
        None => Ok(samples),
    }
}

fn dbfs(x: f64) -> f32 {
    if x <= 0.0 { SILENCE_DBFS } else { ((20.0 * (x / 32768.0).log10()) as f32).max(SILENCE_DBFS) }
}

/// RMS and peak per channel of interleaved L, R samples.
pub fn levels(samples: &[i16]) -> [Level; 2] {
    [0, 1].map(|c| {
        let ch = samples.iter().skip(c).step_by(2).map(|&s| f64::from(s));
        let (n, sum, peak) = ch.fold((0usize, 0.0, 0.0f64), |(n, sum, peak), s| (n + 1, sum + s * s, peak.max(s.abs())));
        Level { rms_dbfs: dbfs(if n == 0 { 0.0 } else { (sum / n as f64).sqrt() }), peak_dbfs: dbfs(peak) }
    })
}

/// A canonical 44-byte PCM header, then the samples little-endian.
pub fn wav(samples: &[i16]) -> Vec<u8> {
    let data = samples.len() as u32 * 2;
    let block = CHANNELS * 2;
    let mut out = Vec::with_capacity(44 + data as usize);
    out.extend(b"RIFF");
    out.extend((36 + data).to_le_bytes());
    out.extend(b"WAVEfmt ");
    out.extend(16u32.to_le_bytes());
    out.extend(1u16.to_le_bytes());
    out.extend(CHANNELS.to_le_bytes());
    out.extend(RATE.to_le_bytes());
    out.extend((RATE * u32::from(block)).to_le_bytes());
    out.extend(block.to_le_bytes());
    out.extend(16u16.to_le_bytes());
    out.extend(b"data");
    out.extend(data.to_le_bytes());
    out.extend(samples.iter().flat_map(|s| s.to_le_bytes()));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square(left: i16, right: i16) -> Vec<i16> {
        (0..1000).flat_map(|i| if i % 2 == 0 { [left, right] } else { [-left, -right] }).collect()
    }

    fn near(a: f32, b: f32) -> bool {
        (a - b).abs() < 0.01
    }

    #[test]
    fn levels_of_full_and_half_scale() {
        let [l, r] = levels(&square(32767, 16384));
        assert!(near(l.rms_dbfs, 0.0) && near(l.peak_dbfs, 0.0), "{l:?}");
        assert!(near(r.rms_dbfs, -6.02) && near(r.peak_dbfs, -6.02), "{r:?}");
    }

    #[test]
    fn levels_of_a_sine() {
        let sine: Vec<i16> = (0..4800).flat_map(|i| { let v = ((i as f32 / 48.0 * std::f32::consts::TAU).sin() * 32767.0) as i16; [v, v] }).collect();
        let [l, _] = levels(&sine);
        assert!(near(l.peak_dbfs, 0.0) && near(l.rms_dbfs, -3.01), "{l:?}"); // rms of a sine is peak / sqrt 2
    }

    #[test]
    fn silence_is_floor() {
        let floor = Level { rms_dbfs: -120.0, peak_dbfs: -120.0 };
        assert_eq!(levels(&[0; 400]), [floor, floor]);
        assert_eq!(levels(&[]), [floor, floor]);
    }

    #[test]
    fn left_right_are_separate() {
        let samples: Vec<i16> = (0..100).flat_map(|_| [0, 32767]).collect();
        let [l, r] = levels(&samples);
        assert_eq!((l.rms_dbfs, l.peak_dbfs), (-120.0, -120.0));
        assert!(near(r.rms_dbfs, 0.0) && near(r.peak_dbfs, 0.0), "{r:?}");
    }

    #[test]
    fn wav_header_is_canonical() {
        let w = wav(&[1, -1, 2, -2]);
        let u32_at = |i: usize| u32::from_le_bytes(w[i..i + 4].try_into().unwrap());
        let u16_at = |i: usize| u16::from_le_bytes(w[i..i + 2].try_into().unwrap());
        assert_eq!(w.len(), 52);
        assert_eq!((&w[0..4], u32_at(4), &w[8..12], &w[12..16], u32_at(16)), (&b"RIFF"[..], 44, &b"WAVE"[..], &b"fmt "[..], 16));
        assert_eq!((u16_at(20), u16_at(22), u32_at(24), u32_at(28), u16_at(32), u16_at(34)), (1, 2, 48000, 192000, 4, 16));
        assert_eq!((&w[36..40], u32_at(40)), (&b"data"[..], 8));
        assert_eq!(&w[44..], [1, 0, 255, 255, 2, 0, 254, 255]);
    }
}
