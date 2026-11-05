//! The virtual body: head and controller poses, moves and animations, sampled by the headset's
//! tracking thread. Poses are SteamVR standing space (the default session recenters in Stage).

use crate::hand::{self, Curls, Gesture};
use alvr_common::{
    DeviceMotion, HAND_LEFT_ID, HAND_RIGHT_ID, HEAD_ID, Pose,
    parking_lot::Mutex,
    glam::{EulerRot, Quat, Vec2, Vec3, Vec4},
};
use alvr_packets::{ButtonEntry, ButtonValue, FaceData, TrackingData};
use rmcp::schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(crate = "rmcp::schemars")]
pub enum Hand {
    Left,
    Right,
}

impl Hand {
    pub fn device_id(self) -> u64 {
        match self {
            Hand::Left => *HAND_LEFT_ID,
            Hand::Right => *HAND_RIGHT_ID,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Hand::Left => "left",
            Hand::Right => "right",
        }
    }
}

/// An absolute pose: meters, degrees; +Y up, -Z forward.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct PoseArgs {
    /// x, y, z in meters.
    pub position: [f32; 3],
    /// Degrees, positive turns left (about +Y). Applied yaw, then pitch, then roll.
    pub yaw: Option<f32>,
    /// Degrees, positive looks up (about +X).
    pub pitch: Option<f32>,
    /// Degrees, positive tilts left (about +Z).
    pub roll: Option<f32>,
    /// x, y, z, w; normalized. Instead of yaw/pitch/roll.
    pub quat: Option<[f32; 4]>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(crate = "rmcp::schemars")]
pub enum Frame {
    #[default]
    Playspace,
    Local,
}

/// A relative move. Rotations are always about the device's own axes.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct MoveArgs {
    /// Meters, in `frame`.
    #[serde(default)]
    pub offset: [f32; 3],
    #[serde(default)]
    pub yaw: f32,
    #[serde(default)]
    pub pitch: f32,
    #[serde(default)]
    pub roll: f32,
    /// "playspace": offset along the playspace axes; "local": along the device's own axes.
    #[serde(default)]
    pub frame: Frame,
}

/// Moves smoothly from the current pose through each waypoint in turn, evenly spaced in time.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct AnimateArgs {
    /// 1 to 64 poses.
    pub waypoints: Vec<PoseArgs>,
    /// Seconds, at least 0.01 and at most 600.
    pub duration_s: f32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(crate = "rmcp::schemars")]
pub enum RelativeTo {
    #[default]
    Playspace,
    /// The head pose at the time of the call.
    Head,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct ControllerPoseArgs {
    pub hand: Hand,
    #[serde(flatten)]
    pub pose: PoseArgs,
    #[serde(default)]
    pub relative_to: RelativeTo,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct MoveControllerArgs {
    pub hand: Hand,
    #[serde(flatten)]
    pub mv: MoveArgs,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct AnimateControllerArgs {
    pub hand: Hand,
    #[serde(flatten)]
    pub anim: AnimateArgs,
    #[serde(default)]
    pub relative_to: RelativeTo,
}

/// ALVR 21 has one flag per hand for both: a hand without a pose is also disconnected
/// (Controller.cpp:187-205). The controller is active when neither given value is false.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct ControllerStateArgs {
    pub hand: Hand,
    pub connected: Option<bool>,
    pub tracked: Option<bool>,
}

impl ControllerStateArgs {
    /// The merged flag, or None when neither value is given.
    pub fn active(&self) -> Option<bool> {
        (self.connected.is_some() || self.tracked.is_some()).then(|| self.connected != Some(false) && self.tracked != Some(false))
    }
}

/// Holding a controller, or a bare tracked hand.
#[derive(Clone, Copy, Debug, Default, PartialEq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(crate = "rmcp::schemars")]
pub enum HandMode {
    #[default]
    Controller,
    Hand,
}

/// Each 0 (straight) to 1 (curled); splay 0 (together) to 1 (spread). An unset finger keeps its value.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct FingerCurlArgs {
    pub hand: Hand,
    pub thumb: Option<f32>,
    pub index: Option<f32>,
    pub middle: Option<f32>,
    pub ring: Option<f32>,
    pub pinky: Option<f32>,
    pub splay: Option<f32>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct HandModeArgs {
    pub hand: Hand,
    pub mode: HandMode,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct HandSkeletonArgs {
    pub hand: Hand,
    /// 26 joints in OpenXR XR_EXT_hand_tracking order, in the playspace: px, py, pz, qx, qy, qz, qw.
    pub joints: Vec<[f32; 7]>,
}

/// Exactly one of `gesture` or `curls` (custom curls).
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct HandGestureArgs {
    pub hand: Hand,
    pub gesture: Option<Gesture>,
    pub curls: Option<Curls>,
}

/// A Valve Index control. "grip" is the squeeze sensor.
#[derive(Clone, Copy, Debug, PartialEq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(crate = "rmcp::schemars")]
pub enum Button {
    A,
    B,
    System,
    Trigger,
    Grip,
    Thumbstick,
    Trackpad,
}

impl Button {
    /// The OpenXR path component; the grip is ALVR's "squeeze".
    fn path(self) -> &'static str {
        match self {
            Button::A => "a",
            Button::B => "b",
            Button::System => "system",
            Button::Trigger => "trigger",
            Button::Grip => "squeeze",
            Button::Thumbstick => "thumbstick",
            Button::Trackpad => "trackpad",
        }
    }
}

/// Input path suffixes under /user/hand/<hand>/input/ and their values.
type Inputs = Vec<(String, ButtonValue)>;

/// What pressing (or releasing) `b` sets.
fn press(b: Button, down: bool) -> Inputs {
    use ButtonValue::{Binary, Scalar};
    let s = if down { 1.0 } else { 0.0 };
    let e: Vec<(&str, ButtonValue)> = match b {
        Button::Trigger => vec![("value", Scalar(s)), ("click", Binary(down)), ("touch", Binary(down))],
        Button::Grip => vec![("value", Scalar(s)), ("force", Scalar(0.0))],
        Button::Trackpad => vec![("force", Scalar(s)), ("touch", Binary(down))],
        _ => vec![("click", Binary(down)), ("touch", Binary(down))],
    };
    e.into_iter().map(|(k, v)| (format!("{}/{k}", b.path()), v)).collect()
}

fn in_range(name: &str, v: f32, min: f32) -> Result<f32, String> {
    if (min..=1.0).contains(&v) { Ok(v) } else { Err(format!("{name} must be within {min} and 1")) }
}

/// The inputs one input action sets, or None for other actions.
fn inputs(a: &RigAction) -> Result<Option<(Hand, Inputs)>, String> {
    use ButtonValue::{Binary, Scalar};
    let unit = |name, v| in_range(name, v, 0.0);
    let axis = |name, v| in_range(name, v, -1.0);
    let set = |e: &[(&str, ButtonValue)]| e.iter().map(|&(k, v)| (k.to_owned(), v)).collect::<Inputs>();
    Ok(Some(match a {
        RigAction::PressButton(b) => (b.hand, press(b.button, true)),
        RigAction::ReleaseButton(b) => (b.hand, press(b.button, false)),
        RigAction::TouchButton(t) if t.button == Button::Grip => {
            return Err("the grip has no touch sensor in ALVR's Index profile (common/src/inputs.rs:228-275)".into());
        }
        RigAction::TouchButton(t) => (t.hand, vec![(format!("{}/touch", t.button.path()), Binary(t.touched))]),
        RigAction::SetTrigger(t) => {
            let v = unit("value", t.value)?;
            (t.hand, set(&[("trigger/value", Scalar(v)), ("trigger/click", Binary(v >= 0.95)), ("trigger/touch", Binary(v > 0.0))]))
        }
        RigAction::SetGrip(g) => {
            let (v, f) = (unit("value", g.value)?, unit("force", g.force.unwrap_or(0.0))?);
            (g.hand, set(&[("squeeze/value", Scalar(v)), ("squeeze/force", Scalar(f))]))
        }
        RigAction::SetThumbstick(t) => {
            let (x, y, click) = (axis("x", t.x)?, axis("y", t.y)?, t.click.unwrap_or(false));
            let touch = t.touch.unwrap_or(x != 0.0 || y != 0.0 || click);
            let e = [("thumbstick/x", Scalar(x)), ("thumbstick/y", Scalar(y)), ("thumbstick/click", Binary(click)), ("thumbstick/touch", Binary(touch))];
            (t.hand, set(&e))
        }
        RigAction::SetTrackpad(t) => {
            let (x, y, force) = (axis("x", t.x)?, axis("y", t.y)?, unit("force", t.force.unwrap_or(0.0))?);
            let touch = t.touch.unwrap_or(true);
            let e = [("trackpad/x", Scalar(x)), ("trackpad/y", Scalar(y)), ("trackpad/force", Scalar(force)), ("trackpad/touch", Binary(touch))];
            (t.hand, set(&e))
        }
        _ => return Ok(None),
    }))
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct ButtonArgs {
    pub hand: Hand,
    pub button: Button,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct TouchArgs {
    pub hand: Hand,
    /// Any button but "grip": ALVR's Index profile has no squeeze touch.
    pub button: Button,
    pub touched: bool,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct TriggerArgs {
    pub hand: Hand,
    /// 0 to 1; click is value >= 0.95, touch is value > 0.
    pub value: f32,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct GripArgs {
    pub hand: Hand,
    /// 0 to 1.
    pub value: f32,
    /// 0 to 1, default 0.
    pub force: Option<f32>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct AxisArgs {
    pub hand: Hand,
    /// -1 (left) to 1 (right).
    pub x: f32,
    /// -1 (down) to 1 (up).
    pub y: f32,
    /// Default false.
    pub click: Option<bool>,
    /// Default: x or y is not 0, or click.
    pub touch: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct TrackpadArgs {
    pub hand: Hand,
    /// -1 (left) to 1 (right).
    pub x: f32,
    /// -1 (down) to 1 (up).
    pub y: f32,
    /// 0 to 1, default 0.
    pub force: Option<f32>,
    /// Default true.
    pub touch: Option<bool>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "action", content = "args", rename_all = "snake_case")]
pub enum RigAction {
    SetHeadPose(PoseArgs),
    MoveHead(MoveArgs),
    AnimateHead(AnimateArgs),
    SetControllerPose(ControllerPoseArgs),
    MoveController(MoveControllerArgs),
    AnimateController(AnimateControllerArgs),
    SetControllerState(ControllerStateArgs),
    SetFingerCurl(FingerCurlArgs),
    SetHandMode(HandModeArgs),
    SetHandSkeleton(HandSkeletonArgs),
    SetHandGesture(HandGestureArgs),
    PressButton(ButtonArgs),
    ReleaseButton(ButtonArgs),
    TouchButton(TouchArgs),
    SetTrigger(TriggerArgs),
    SetGrip(GripArgs),
    SetThumbstick(AxisArgs),
    SetTrackpad(TrackpadArgs),
}

/// One timed action of `run_sequence`: any rig tool, by name, with its arguments.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct SequenceStep {
    /// Seconds from the start of the sequence, 0 to 600.
    pub at_s: f32,
    /// A rig tool's name: set_head_pose, animate_controller, set_trigger, set_finger_curl, ...
    pub action: String,
    /// That tool's arguments.
    #[serde(default)]
    pub args: serde_json::Value,
}

impl SequenceStep {
    pub fn parse(&self) -> Result<RigAction, String> {
        serde_json::from_value(serde_json::json!({ "action": self.action, "args": self.args })).map_err(|e| {
            let e = e.to_string();
            if e.starts_with("unknown variant") { format!("{:?} is not allowed in a sequence", self.action) } else { e }
        })
    }
}

/// What a sequence step did and when, in seconds from the start of the sequence.
#[derive(Debug, Serialize)]
pub struct AppliedStep {
    pub at_s: f32,
    pub action: String,
    pub applied_s: f32,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SequenceReport {
    /// When the sequence began, on the session clock.
    pub started_t_s: f64,
    pub started_at: String,
    pub steps: Vec<AppliedStep>,
    /// Until the last step and the latest animation are done.
    pub total_s: f32,
}

const SEQUENCE_STEPS_MAX: usize = 256;

/// Checks every step before anything runs and orders them by `at_s` (equal times keep their order).
/// The ordered steps are also applied to a copy of `rig` at their offsets, so a value ALVR or the rig
/// would refuse (a trigger of 7, an untracked controller) fails the whole sequence with its step number.
// shortcut: the copy is `rig` as of now; a tool called while the sequence runs can still fail a step mid-run, which is then reported
pub fn plan_sequence(steps: Vec<SequenceStep>, rig: &Rig) -> Result<Vec<(SequenceStep, RigAction)>, String> {
    if steps.len() > SEQUENCE_STEPS_MAX {
        return Err(format!("at most {SEQUENCE_STEPS_MAX} steps"));
    }
    let mut plan = steps
        .into_iter()
        .enumerate()
        .map(|(i, s)| {
            if !(0.0..=600.0).contains(&s.at_s) {
                return Err(format!("step {i}: at_s must be within 0 and 600"));
            }
            let action = s.parse().map_err(|e| format!("step {i}: {e}"))?;
            Ok((i, s, action))
        })
        .collect::<Result<Vec<_>, String>>()?;
    plan.sort_by(|a, b| a.1.at_s.total_cmp(&b.1.at_s));
    let (mut dry, start) = (rig.clone(), Instant::now());
    for (i, s, action) in &plan {
        dry.apply(action.clone(), start + Duration::from_secs_f32(s.at_s)).map_err(|e| format!("step {i}: {e}"))?;
    }
    Ok(plan.into_iter().map(|(_, s, a)| (s, a)).collect())
}

impl PoseArgs {
    pub fn to_pose(&self) -> Result<Pose, String> {
        let position = Vec3::from_array(self.position);
        if !position.is_finite() {
            return Err("position must be finite".into());
        }
        let euler = [self.yaw, self.pitch, self.roll];
        let orientation = match self.quat {
            Some(_) if euler.iter().any(Option::is_some) => return Err("give yaw/pitch/roll or quat, not both".into()),
            // try_normalize also refuses a quat whose length overflows to inf
            Some(q) => Quat::from_vec4(
                Vec4::from_array(q).try_normalize().ok_or("quat must be finite, non-zero and of finite length")?,
            ),
            None => {
                let [yaw, pitch, roll] = euler.map(|a| a.unwrap_or(0.0));
                euler_quat(yaw, pitch, roll)?
            }
        };
        Ok(Pose { orientation, position })
    }
}

fn euler_quat(yaw: f32, pitch: f32, roll: f32) -> Result<Quat, String> {
    if ![yaw, pitch, roll].iter().all(|a| a.is_finite()) {
        return Err("yaw, pitch and roll must be finite".into());
    }
    Ok(Quat::from_euler(EulerRot::YXZ, yaw.to_radians(), pitch.to_radians(), roll.to_radians()))
}

/// Keys spaced `step_s` apart from `start`; the first key is the pose the animation started at.
#[derive(Clone)]
struct Animation {
    start: Instant,
    step_s: f32,
    keys: Vec<Pose>,
}

impl Animation {
    /// The motion at `now`, or None once past the last key.
    fn sample(&self, now: Instant) -> Option<DeviceMotion> {
        let (keys, dt) = (&self.keys, self.step_s);
        let s = now.saturating_duration_since(self.start).as_secs_f32() / dt;
        let i = s as usize;
        if i + 1 >= keys.len() {
            return None;
        }
        let u = s - i as f32;
        let p = |k: usize| keys[k].position;
        // Catmull-Rom tangents per second, zero at both ends
        let m = |k: usize| if k == 0 || k + 1 == keys.len() { Vec3::ZERO } else { (p(k + 1) - p(k - 1)) / (2.0 * dt) };
        let (u2, u3) = (u * u, u * u * u);
        let position = (2.0 * u3 - 3.0 * u2 + 1.0) * p(i)
            + (u3 - 2.0 * u2 + u) * dt * m(i)
            + (-2.0 * u3 + 3.0 * u2) * p(i + 1)
            + (u3 - u2) * dt * m(i + 1);
        let linear_velocity = ((6.0 * u2 - 6.0 * u) * p(i)
            + (3.0 * u2 - 4.0 * u + 1.0) * dt * m(i)
            + (-6.0 * u2 + 6.0 * u) * p(i + 1)
            + (3.0 * u2 - 2.0 * u) * dt * m(i + 1))
            / dt;
        // shortcut: piecewise slerp; use squad if apps show jerk at waypoints
        let (q0, q1) = (keys[i].orientation, keys[i + 1].orientation);
        let q1 = if q0.dot(q1) < 0.0 { -q1 } else { q1 };
        let (axis, angle) = (q1 * q0.inverse()).to_axis_angle();
        Some(DeviceMotion {
            pose: Pose { orientation: q0.slerp(q1, u), position },
            linear_velocity,
            angular_velocity: axis * angle / dt,
        })
    }
}

/// A head or controller: a resting pose, or an animation that ends in one.
#[derive(Clone)]
struct Body {
    pose: Pose,
    animation: Option<Animation>,
}

impl Body {
    fn at(position: Vec3) -> Body {
        Body { pose: Pose { orientation: Quat::IDENTITY, position }, animation: None }
    }

    /// The motion at `now`; an animation past its end settles into its last key.
    fn motion(&mut self, now: Instant) -> DeviceMotion {
        if let Some(m) = self.animation.as_ref().and_then(|a| a.sample(now)) {
            return m;
        }
        if let Some(a) = self.animation.take() {
            self.pose = *a.keys.last().unwrap();
        }
        DeviceMotion { pose: self.pose, ..Default::default() }
    }

    /// Stops any animation where it is now and returns that pose.
    fn settle(&mut self, now: Instant) -> Pose {
        self.pose = self.motion(now).pose;
        self.animation = None;
        self.pose
    }

    fn set(&mut self, pose: Pose, now: Instant) {
        self.settle(now);
        self.pose = pose;
    }

    fn move_by(&mut self, mv: &MoveArgs, now: Instant) -> Result<(), String> {
        let offset = Vec3::from_array(mv.offset);
        if !offset.is_finite() {
            return Err("offset must be finite".into());
        }
        let delta = euler_quat(mv.yaw, mv.pitch, mv.roll)?;
        let pose = self.settle(now);
        let offset = if mv.frame == Frame::Local { pose.orientation * offset } else { offset };
        self.pose = Pose { orientation: (pose.orientation * delta).normalize(), position: pose.position + offset };
        Ok(())
    }

    fn animate(&mut self, anim: &AnimateArgs, origin: Pose, now: Instant) -> Result<Duration, String> {
        // below 10 ms the velocities ALVR predicts with become absurd
        if !(anim.duration_s >= 0.01 && anim.duration_s <= 600.0) {
            return Err("duration_s must be at least 0.01 and at most 600".into());
        }
        if !(1..=64).contains(&anim.waypoints.len()) {
            return Err("give 1 to 64 waypoints".into());
        }
        let waypoints = anim.waypoints.iter().map(|w| Ok(origin * w.to_pose()?)).collect::<Result<Vec<_>, String>>()?;
        let keys = [vec![self.settle(now)], waypoints].concat();
        self.animation = Some(Animation { start: now, step_s: anim.duration_s / anim.waypoints.len() as f32, keys });
        Ok(Duration::from_secs_f32(anim.duration_s))
    }
}

/// What a hand sends besides its pose.
#[derive(Clone, Default)]
struct HandState {
    mode: HandMode,
    /// Set by the last curl or gesture call; kept under a raw skeleton for the next partial curl call.
    curls: Option<Curls>,
    /// Joints in the playspace from `set_hand_skeleton`, until the next curl or gesture call.
    raw: Option<[Pose; 26]>,
}

#[derive(Clone)]
pub struct Rig {
    head: Body,
    /// left, right; a bare hand's palm follows its controller's pose too
    controllers: [Body; 2],
    active: [bool; 2],
    hands: [HandState; 2],
    /// Per hand, every input held by full path id, for a resend to a new stream.
    buttons: [BTreeMap<u64, ButtonValue>; 2],
    /// Inputs set since the last `take_buttons`, in order.
    pending: Vec<(u64, ButtonValue)>,
    /// Half the play area, the unit ALVR draws the chaperone in; the default area is 2 x 2 m.
    playspace: Vec2,
    /// Set until the tracking thread sends it.
    playspace_pending: bool,
    /// What the headset reports as worn at every stream start; here because the rig outlives headset restarts (not a new vr_start session).
    pub on_head: bool,
    /// Orders proximity sends so the latest wins, without holding the rig lock across a blocking socket write.
    pub proximity_send: Arc<Mutex<()>>,
}

impl Default for Rig {
    fn default() -> Rig {
        Rig::new()
    }
}

impl Rig {
    pub fn new() -> Rig {
        Rig {
            head: Body::at(Vec3::new(0.0, 1.6, 0.0)),
            controllers: [Body::at(Vec3::new(-0.2, 1.2, -0.3)), Body::at(Vec3::new(0.2, 1.2, -0.3))],
            active: [true; 2],
            hands: Default::default(),
            buttons: Default::default(),
            pending: vec![],
            playspace: Vec2::ONE,
            playspace_pending: false,
            on_head: true,
            proximity_send: Arc::default(),
        }
    }

    /// A new rig's state, for a vr_start that begins a session. The same proximity_send, so a send racing the reset still orders.
    /// The default area goes out anyway: the tracking thread recenters at every stream start.
    // shortcut: an adopted SteamVR's driver keeps the last binary input value and ALVR's fresh mapping drops a lone release, so a button held by an earlier stream stays pressed until vr_stop; upgrade if that shows up in tests
    pub fn reset(&mut self) {
        *self = Rig { proximity_send: Arc::clone(&self.proximity_send), ..Rig::new() };
    }

    /// Sets the play area in meters; the tracking thread sends it. ALVR draws ±area (ChaperoneUpdater.cpp:69-76), so it gets half.
    pub fn set_playspace(&mut self, width: f32, depth: f32) -> Result<(), String> {
        // ALVR falls back to 2 x 2 when width * depth is not finite; 1000 m also keeps the product finite
        if ![width, depth].iter().all(|v| *v > 0.0 && *v <= 1000.0) {
            return Err("width and depth must be above 0 and at most 1000 meters".into());
        }
        self.playspace = Vec2::new(width, depth) / 2.0;
        self.playspace_pending = true;
        Ok(())
    }

    /// Queues the current area again: ALVR recenters on every playspace update.
    pub fn recenter(&mut self) {
        self.playspace_pending = true;
    }

    /// The area to send, once per `set_playspace` or `recenter`.
    pub fn take_playspace(&mut self) -> Option<Vec2> {
        std::mem::take(&mut self.playspace_pending).then_some(self.playspace)
    }

    /// Where a controller pose given `relative_to` lands in the playspace.
    fn origin(&mut self, relative_to: RelativeTo, now: Instant) -> Pose {
        match relative_to {
            RelativeTo::Playspace => Pose::IDENTITY,
            RelativeTo::Head => self.head.motion(now).pose,
        }
    }

    /// Applies `a` and returns how long until it is done: an animation's duration, else zero.
    pub fn apply(&mut self, a: RigAction, now: Instant) -> Result<Duration, String> {
        if let Some((hand, inputs)) = inputs(&a)? {
            let i = hand as usize;
            if !self.active[i] {
                // ALVR drops buttons of a hand without a valid pose (Controller.cpp:145-147)
                return Err(format!("{} controller is not tracked", hand.name()));
            }
            if self.hands[i].mode == HandMode::Hand {
                return Err(format!("the {} hand is a bare hand: ALVR's gestures drive its buttons; set_hand_mode controller first", hand.name()));
            }
            for (suffix, value) in inputs {
                let id = alvr_common::hash_string(&format!("/user/hand/{}/input/{suffix}", hand.name()));
                self.buttons[i].insert(id, value);
                self.pending.push((id, value));
            }
            return Ok(Duration::ZERO);
        }
        match a {
            RigAction::SetHeadPose(p) => self.head.set(p.to_pose()?, now),
            RigAction::MoveHead(mv) => self.head.move_by(&mv, now)?,
            RigAction::AnimateHead(anim) => return self.head.animate(&anim, Pose::IDENTITY, now),
            RigAction::SetControllerPose(c) => {
                let pose = self.origin(c.relative_to, now) * c.pose.to_pose()?;
                self.controllers[c.hand as usize].set(pose, now);
            }
            RigAction::MoveController(c) => self.controllers[c.hand as usize].move_by(&c.mv, now)?,
            RigAction::AnimateController(c) => {
                let origin = self.origin(c.relative_to, now);
                return self.controllers[c.hand as usize].animate(&c.anim, origin, now);
            }
            RigAction::SetControllerState(c) => {
                self.active[c.hand as usize] = c.active().ok_or("give connected or tracked")?;
            }
            RigAction::SetFingerCurl(a) => {
                let h = &mut self.hands[a.hand as usize];
                let mut c = h.curls.unwrap_or_default();
                let fields = [(&mut c.thumb, a.thumb), (&mut c.index, a.index), (&mut c.middle, a.middle)]
                    .into_iter()
                    .chain([(&mut c.ring, a.ring), (&mut c.pinky, a.pinky), (&mut c.splay, a.splay)]);
                for (v, set) in fields {
                    *v = set.unwrap_or(*v);
                }
                c.check()?;
                (h.curls, h.raw) = (Some(c), None);
            }
            RigAction::SetHandMode(a) => {
                let i = a.hand as usize;
                self.hands[i].mode = a.mode;
                if a.mode == HandMode::Hand {
                    // release refuses for a bare hand, so let go of what it held now, not after a reconnect
                    let released = std::mem::take(&mut self.buttons[i]).into_iter().map(|(id, v)| match v {
                        ButtonValue::Binary(_) => (id, ButtonValue::Binary(false)),
                        ButtonValue::Scalar(_) => (id, ButtonValue::Scalar(0.0)),
                    });
                    self.pending.extend(released);
                }
            }
            RigAction::SetHandSkeleton(a) => {
                if a.joints.len() != 26 {
                    return Err("joints must have 26 entries".into());
                }
                let pose = |(i, j): (usize, &[f32; 7])| {
                    let args = PoseArgs { position: [j[0], j[1], j[2]], quat: Some([j[3], j[4], j[5], j[6]]), ..Default::default() };
                    args.to_pose().map_err(|e| format!("joint {i}: {e}"))
                };
                let joints = a.joints.iter().enumerate().map(pose).collect::<Result<Vec<_>, _>>()?;
                self.hands[a.hand as usize].raw = Some(joints.try_into().unwrap());
            }
            RigAction::SetHandGesture(a) => {
                let c = match (a.gesture, a.curls) {
                    (Some(g), None) => hand::gesture_curls(g),
                    (None, Some(c)) => c,
                    _ => return Err("give exactly one of gesture or curls".into()),
                };
                c.check()?;
                let h = &mut self.hands[a.hand as usize];
                (h.curls, h.raw) = (Some(c), None);
            }
            _ => unreachable!("input actions return above"),
        }
        Ok(Duration::ZERO)
    }

    /// The inputs set since the last call, for the headset to send.
    pub fn take_buttons(&mut self) -> Vec<ButtonEntry> {
        self.pending.drain(..).map(|(path_id, value)| ButtonEntry { path_id, value }).collect()
    }

    /// Every input set so far. A new stream needs them again: the server's mapping state starts empty.
    pub fn all_buttons(&self) -> Vec<ButtonEntry> {
        self.buttons.iter().flatten().map(|(&path_id, &value)| ButtonEntry { path_id, value }).collect()
    }

    /// The head plus each active hand at `now`: a controller pose, its skeleton once curls or
    /// joints were set, or for a bare hand only a skeleton (ALVR picks the device from what the
    /// packet holds, server_openvr/src/lib.rs:403-434). Both go out in this one packet, so they share
    /// the poll_timestamp ALVR pairs them on (server_core/src/tracking/mod.rs:250-258).
    pub fn tracking(&mut self, now: Instant, poll_timestamp: Duration) -> TrackingData {
        let mut device_motions = vec![(*HEAD_ID, self.head.motion(now))];
        let mut hand_skeletons = [None, None];
        for hand in [Hand::Left, Hand::Right] {
            let i = hand as usize;
            if !self.active[i] {
                continue;
            }
            let (motion, h) = (self.controllers[i].motion(now), &self.hands[i]);
            if h.mode == HandMode::Controller {
                device_motions.push((hand.device_id(), motion));
            }
            // without a skeleton ALVR derives a controller's curls from trigger and grip
            let curls = h.curls.or((h.mode == HandMode::Hand).then(Curls::default));
            // shortcut: no grip-to-palm offset; add one if apps show the hand off the controller
            hand_skeletons[i] = h.raw.or_else(|| curls.map(|c| hand::skeleton(hand, motion.pose, &c)));
        }
        TrackingData { poll_timestamp, device_motions, hand_skeletons, face: FaceData::default(), body: None }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alvr_common::glam::Vec2;

    fn close(a: Vec3, b: Vec3) -> bool {
        a.distance(b) < 1e-4
    }

    fn pose_at(p: [f32; 3]) -> PoseArgs {
        PoseArgs { position: p, ..Default::default() }
    }

    fn motion(rig: &mut Rig, now: Instant, id: u64) -> Option<DeviceMotion> {
        rig.tracking(now, Duration::ZERO).device_motions.into_iter().find(|(d, _)| *d == id).map(|(_, m)| m)
    }

    #[test]
    fn worn_by_default() {
        // the rig outlives headsets, so what set_proximity stores here survives a recreated one
        assert!(Rig::new().on_head);
    }

    #[test]
    fn yaw_90_turns_left() {
        let pose = PoseArgs { yaw: Some(90.0), ..pose_at([0.0; 3]) }.to_pose().unwrap();
        assert!(close(pose.orientation * Vec3::NEG_Z, Vec3::NEG_X));
    }

    #[test]
    fn pose_args_reject_bad_input() {
        let quat = |q| PoseArgs { quat: Some(q), ..pose_at([0.0; 3]) };
        assert!(quat([0.0; 4]).to_pose().is_err());
        assert!(quat([1e20, 0.0, 0.0, 1.0]).to_pose().is_err(), "length overflows to inf");
        assert!(quat([0.0, f32::NAN, 0.0, 1.0]).to_pose().is_err());
        let both = PoseArgs { yaw: Some(10.0), ..quat([0.0, 0.0, 0.0, 1.0]) };
        assert_eq!(both.to_pose().unwrap_err(), "give yaw/pitch/roll or quat, not both");
        assert!(pose_at([f32::NAN, 0.0, 0.0]).to_pose().is_err());
        assert!(PoseArgs { pitch: Some(f32::INFINITY), ..pose_at([0.0; 3]) }.to_pose().is_err());
    }

    #[test]
    fn quat_is_normalized() {
        let pose = PoseArgs { quat: Some([0.0, 0.0, 0.0, 2.0]), ..pose_at([0.0; 3]) }.to_pose().unwrap();
        assert!(pose.orientation.abs_diff_eq(Quat::IDENTITY, 1e-6));
    }

    #[test]
    fn move_local_follows_orientation() {
        let (mut rig, now) = (Rig::new(), Instant::now());
        rig.apply(RigAction::SetHeadPose(PoseArgs { yaw: Some(90.0), ..pose_at([0.0, 1.6, 0.0]) }), now).unwrap();
        let mv = MoveArgs { offset: [0.0, 0.0, -1.0], frame: Frame::Local, ..Default::default() };
        rig.apply(RigAction::MoveHead(mv), now).unwrap();
        assert!(close(motion(&mut rig, now, *HEAD_ID).unwrap().pose.position, Vec3::new(-1.0, 1.6, 0.0)));
        // playspace offsets ignore the orientation; rotations are about the device's own axes
        let mv = MoveArgs { offset: [0.0, 0.0, -1.0], yaw: 90.0, ..Default::default() };
        rig.apply(RigAction::MoveHead(mv), now).unwrap();
        let head = motion(&mut rig, now, *HEAD_ID).unwrap().pose;
        assert!(close(head.position, Vec3::new(-1.0, 1.6, -1.0)));
        assert!(close(head.orientation * Vec3::NEG_Z, Vec3::Z), "two 90° yaws face +Z");
    }

    #[test]
    fn animation_hits_waypoints_and_stops() {
        let (mut rig, t0) = (Rig::new(), Instant::now());
        let (p0, p1, p2) = (Vec3::new(0.0, 1.6, 0.0), Vec3::new(1.0, 1.6, 0.0), Vec3::new(1.0, 1.6, -1.0));
        let anim = AnimateArgs {
            waypoints: vec![PoseArgs { yaw: Some(90.0), ..pose_at(p1.into()) }, pose_at(p2.into())],
            duration_s: 2.0,
        };
        assert_eq!(rig.apply(RigAction::AnimateHead(anim), t0).unwrap(), Duration::from_secs(2));
        let at = |rig: &mut Rig, s: f32| motion(rig, t0 + Duration::from_secs_f32(s), *HEAD_ID).unwrap();

        let half = at(&mut rig, 0.5);
        assert!(half.linear_velocity.length() > 0.1 && half.linear_velocity.dot(p1 - p0) > 0.0, "{half:?}");
        // yaw 0 -> 90 over 1 s: 90°/s about +Y
        assert!(close(half.angular_velocity, Vec3::new(0.0, 90f32.to_radians(), 0.0)), "{half:?}");

        let mid = at(&mut rig, 1.0);
        assert!(close(mid.pose.position, p1), "{mid:?}");
        assert!(mid.linear_velocity.length() > 0.1, "Catmull-Rom keeps moving through a waypoint");

        for s in [2.0, 3.0] {
            let end = at(&mut rig, s);
            assert_eq!(end.pose.position, p2);
            assert_eq!((end.linear_velocity, end.angular_velocity), (Vec3::ZERO, Vec3::ZERO));
        }
    }

    #[test]
    fn animation_rejects_bad_duration() {
        let mut rig = Rig::new();
        let anim = |waypoints: usize, duration_s| {
            RigAction::AnimateHead(AnimateArgs { waypoints: vec![pose_at([0.0, 1.6, 0.0]); waypoints], duration_s })
        };
        for bad in [anim(1, 0.0), anim(1, 1e-6), anim(1, 0.009), anim(1, 600.5), anim(1, f32::NAN), anim(0, 1.0), anim(65, 1.0)] {
            assert!(rig.apply(bad, Instant::now()).is_err());
        }
        assert!(rig.apply(anim(64, 600.0), Instant::now()).is_ok());
        assert!(rig.apply(anim(1, 0.01), Instant::now()).is_ok());
    }

    #[test]
    fn controller_relative_to_head() {
        let (mut rig, now) = (Rig::new(), Instant::now());
        rig.apply(RigAction::SetHeadPose(PoseArgs { yaw: Some(90.0), ..pose_at([0.0, 1.6, 0.0]) }), now).unwrap();
        let args = ControllerPoseArgs { hand: Hand::Right, pose: pose_at([0.0, 0.0, -0.5]), relative_to: RelativeTo::Head };
        rig.apply(RigAction::SetControllerPose(args), now).unwrap();
        let right = motion(&mut rig, now, *HAND_RIGHT_ID).unwrap().pose;
        assert!(close(right.position, Vec3::new(-0.5, 1.6, 0.0)), "{right:?}");
        assert!(close(right.orientation * Vec3::NEG_Z, Vec3::NEG_X), "{right:?}");
        let left = motion(&mut rig, now, *HAND_LEFT_ID).unwrap().pose;
        assert!(close(left.position, Vec3::new(-0.2, 1.2, -0.3)), "default left pose: {left:?}");
    }

    #[test]
    fn inactive_controller_sends_no_motion() {
        let (mut rig, now) = (Rig::new(), Instant::now());
        let state = |connected, tracked| RigAction::SetControllerState(ControllerStateArgs { hand: Hand::Right, connected, tracked });
        assert!(motion(&mut rig, now, *HAND_RIGHT_ID).is_some(), "active by default");
        rig.apply(state(Some(false), None), now).unwrap();
        assert!(motion(&mut rig, now, *HAND_RIGHT_ID).is_none());
        assert!(motion(&mut rig, now, *HAND_LEFT_ID).is_some() && motion(&mut rig, now, *HEAD_ID).is_some());
        rig.apply(state(Some(true), None), now).unwrap();
        assert!(motion(&mut rig, now, *HAND_RIGHT_ID).is_some());
        rig.apply(state(None, Some(false)), now).unwrap();
        assert!(motion(&mut rig, now, *HAND_RIGHT_ID).is_none(), "tracked is the same flag");
        assert!(rig.apply(state(None, None), now).is_err());
        let data = rig.tracking(now, Duration::from_millis(7));
        assert_eq!(data.poll_timestamp, Duration::from_millis(7));
        assert!(matches!(data.hand_skeletons, [None, None]) && data.body.is_none());
    }

    fn finger_curl(hand: Hand) -> FingerCurlArgs {
        FingerCurlArgs { hand, thumb: None, index: None, middle: None, ring: None, pinky: None, splay: None }
    }

    #[test]
    fn curl_args_reject_out_of_range() {
        let (mut rig, now) = (Rig::new(), Instant::now());
        for bad in [1.2, -0.1, f32::NAN] {
            let args = FingerCurlArgs { index: Some(bad), ..finger_curl(Hand::Right) };
            assert!(rig.apply(RigAction::SetFingerCurl(args), now).is_err(), "{bad}");
        }
        assert!(rig.tracking(now, Duration::ZERO).hand_skeletons[1].is_none(), "a rejected call changes nothing");
        let args = FingerCurlArgs { index: Some(1.0), splay: Some(0.0), ..finger_curl(Hand::Right) };
        assert!(rig.apply(RigAction::SetFingerCurl(args), now).is_ok());
    }

    #[test]
    fn finger_curl_keeps_unset_fingers() {
        let (mut rig, now) = (Rig::new(), Instant::now());
        let palm = motion(&mut rig, now, *HAND_RIGHT_ID).unwrap().pose;
        rig.apply(RigAction::SetFingerCurl(FingerCurlArgs { index: Some(1.0), ..finger_curl(Hand::Right) }), now).unwrap();
        rig.apply(RigAction::SetFingerCurl(FingerCurlArgs { ring: Some(0.5), ..finger_curl(Hand::Right) }), now).unwrap();
        let want = hand::skeleton(Hand::Right, palm, &Curls { index: 1.0, ring: 0.5, ..Default::default() });
        let got = rig.tracking(now, Duration::ZERO).hand_skeletons[1].unwrap();
        assert!(got.iter().zip(&want).all(|(g, w)| g.position.abs_diff_eq(w.position, 1e-6)));
    }

    #[test]
    fn skeleton_rides_with_controller_pose() {
        let (mut rig, now) = (Rig::new(), Instant::now());
        assert!(rig.tracking(now, Duration::ZERO).hand_skeletons[1].is_none(), "no skeleton before curls are set");
        rig.apply(RigAction::SetFingerCurl(FingerCurlArgs { index: Some(1.0), ..finger_curl(Hand::Right) }), now).unwrap();
        let data = rig.tracking(now, Duration::from_millis(5));
        let pose = data.device_motions.iter().find(|(d, _)| *d == *HAND_RIGHT_ID).expect("controller motion").1.pose;
        let joints = data.hand_skeletons[1].expect("skeleton");
        assert!(joints[0].position.abs_diff_eq(pose.position, 1e-6), "palm at the controller pose");
        assert!(data.hand_skeletons[0].is_none(), "the left hand had no curls set");
    }

    #[test]
    fn bare_hand_sends_skeleton_only() {
        let (mut rig, now) = (Rig::new(), Instant::now());
        rig.apply(RigAction::SetHandMode(HandModeArgs { hand: Hand::Right, mode: HandMode::Hand }), now).unwrap();
        let data = rig.tracking(now, Duration::ZERO);
        assert!(!data.device_motions.iter().any(|(d, _)| *d == *HAND_RIGHT_ID), "{:?}", data.device_motions);
        assert!(data.device_motions.iter().any(|(d, _)| *d == *HAND_LEFT_ID));
        let open = hand::skeleton(Hand::Right, Pose { orientation: Quat::IDENTITY, position: Vec3::new(0.2, 1.2, -0.3) }, &Curls::default());
        let joints = data.hand_skeletons[1].expect("an open hand by default");
        assert!(joints[10].position.abs_diff_eq(open[10].position, 1e-6));
        rig.apply(RigAction::SetHandMode(HandModeArgs { hand: Hand::Right, mode: HandMode::Controller }), now).unwrap();
        let data = rig.tracking(now, Duration::ZERO);
        assert!(data.device_motions.iter().any(|(d, _)| *d == *HAND_RIGHT_ID) && data.hand_skeletons[1].is_none());
    }

    #[test]
    fn hand_skeleton_needs_26_joints() {
        let (mut rig, now) = (Rig::new(), Instant::now());
        let joint = |i: usize| [0.0, 1.0, -(i as f32) * 0.01, 0.0, 0.0, 0.0, 1.0];
        let args = |n: usize| RigAction::SetHandSkeleton(HandSkeletonArgs { hand: Hand::Left, joints: (0..n).map(joint).collect() });
        assert_eq!(rig.apply(args(25), now).unwrap_err(), "joints must have 26 entries");
        let mut bad = HandSkeletonArgs { hand: Hand::Left, joints: (0..26).map(joint).collect() };
        bad.joints[3][6] = f32::NAN;
        assert!(rig.apply(RigAction::SetHandSkeleton(bad), now).is_err());
        rig.apply(args(26), now).unwrap();
        let joints = rig.tracking(now, Duration::ZERO).hand_skeletons[0].expect("raw skeleton");
        assert_eq!(joints[25].position, Vec3::new(0.0, 1.0, -0.25));
        // the next curl call goes back to the hand model
        rig.apply(RigAction::SetFingerCurl(finger_curl(Hand::Left)), now).unwrap();
        let joints = rig.tracking(now, Duration::ZERO).hand_skeletons[0].unwrap();
        assert_ne!(joints[25].position, Vec3::new(0.0, 1.0, -0.25));
    }

    #[test]
    fn hand_gesture_requires_exactly_one() {
        let (mut rig, now) = (Rig::new(), Instant::now());
        let args = |gesture, curls| RigAction::SetHandGesture(HandGestureArgs { hand: Hand::Right, gesture, curls });
        assert!(rig.apply(args(None, None), now).is_err());
        assert!(rig.apply(args(Some(Gesture::Fist), Some(Curls::default())), now).is_err());
        assert!(rig.apply(args(None, Some(Curls { index: 2.0, ..Default::default() })), now).is_err(), "custom curls are checked");
        assert!(rig.tracking(now, Duration::ZERO).hand_skeletons[1].is_none(), "rejected calls change nothing");
        rig.apply(args(Some(Gesture::Fist), None), now).unwrap();
        assert!(rig.tracking(now, Duration::ZERO).hand_skeletons[1].is_some());
        rig.apply(args(None, Some(Curls { index: 0.5, ..Default::default() })), now).unwrap();
    }

    /// (path, value) pairs; ButtonValue has no PartialEq, so values compare by Debug text.
    fn entries(v: Vec<ButtonEntry>) -> Vec<(u64, String)> {
        v.into_iter().map(|e| (e.path_id, format!("{:?}", e.value))).collect()
    }

    fn want(hand: &str, e: &[(&str, ButtonValue)]) -> Vec<(u64, String)> {
        e.iter().map(|(p, v)| (alvr_common::hash_string(&format!("/user/hand/{hand}/input/{p}")), format!("{v:?}"))).collect()
    }

    fn press(hand: Hand, button: Button) -> RigAction {
        RigAction::PressButton(ButtonArgs { hand, button })
    }

    #[test]
    fn press_trigger_entries() {
        let (mut rig, now) = (Rig::new(), Instant::now());
        rig.apply(press(Hand::Right, Button::Trigger), now).unwrap();
        use ButtonValue::{Binary, Scalar};
        let trigger = [("trigger/value", Scalar(1.0)), ("trigger/click", Binary(true)), ("trigger/touch", Binary(true))];
        assert_eq!(entries(rig.take_buttons()), want("right", &trigger));
        assert!(rig.take_buttons().is_empty(), "taken once");
        rig.apply(RigAction::ReleaseButton(ButtonArgs { hand: Hand::Left, button: Button::A }), now).unwrap();
        assert_eq!(entries(rig.take_buttons()), want("left", &[("a/click", Binary(false)), ("a/touch", Binary(false))]));
        rig.apply(press(Hand::Left, Button::Trackpad), now).unwrap();
        rig.apply(press(Hand::Left, Button::Grip), now).unwrap();
        let pad_grip = [("trackpad/force", Scalar(1.0)), ("trackpad/touch", Binary(true)), ("squeeze/value", Scalar(1.0)), ("squeeze/force", Scalar(0.0))];
        assert_eq!(entries(rig.take_buttons()), want("left", &pad_grip));
        let stick = AxisArgs { hand: Hand::Right, x: 0.0, y: 0.5, click: None, touch: None };
        rig.apply(RigAction::SetThumbstick(stick), now).unwrap();
        let stick = [("thumbstick/x", Scalar(0.0)), ("thumbstick/y", Scalar(0.5)), ("thumbstick/click", Binary(false)), ("thumbstick/touch", Binary(true))];
        assert_eq!(entries(rig.take_buttons()), want("right", &stick));
    }

    #[test]
    fn button_args_reject_unknown_and_touchless() {
        assert!(serde_json::from_value::<ButtonArgs>(serde_json::json!({"hand": "right", "button": "x"})).is_err());
        let ok: ButtonArgs = serde_json::from_value(serde_json::json!({"hand": "right", "button": "thumbstick"})).unwrap();
        assert_eq!(ok.button, Button::Thumbstick);
        let (mut rig, now) = (Rig::new(), Instant::now());
        assert!(rig.apply(RigAction::TouchButton(TouchArgs { hand: Hand::Right, button: Button::Grip, touched: true }), now).is_err());
        for bad in [1.5, -0.1, f32::NAN] {
            assert!(rig.apply(RigAction::SetTrigger(TriggerArgs { hand: Hand::Right, value: bad }), now).is_err(), "{bad}");
            assert!(rig.apply(RigAction::SetGrip(GripArgs { hand: Hand::Right, value: 0.5, force: Some(bad) }), now).is_err());
        }
        let pad = |x| RigAction::SetTrackpad(TrackpadArgs { hand: Hand::Left, x, y: 0.0, force: None, touch: None });
        assert!(rig.apply(pad(1.1), now).is_err() && rig.apply(pad(f32::NAN), now).is_err());
        assert!(rig.take_buttons().is_empty(), "rejected calls send nothing");
        rig.apply(RigAction::TouchButton(TouchArgs { hand: Hand::Right, button: Button::B, touched: true }), now).unwrap();
        assert_eq!(entries(rig.take_buttons()), want("right", &[("b/touch", ButtonValue::Binary(true))]));
    }

    #[test]
    fn buttons_refused_when_inactive() {
        let (mut rig, now) = (Rig::new(), Instant::now());
        let state = ControllerStateArgs { hand: Hand::Right, connected: Some(false), tracked: None };
        rig.apply(RigAction::SetControllerState(state), now).unwrap();
        assert_eq!(rig.apply(press(Hand::Right, Button::A), now).unwrap_err(), "right controller is not tracked");
        assert!(rig.take_buttons().is_empty());
        assert!(rig.apply(press(Hand::Left, Button::A), now).is_ok(), "the left hand is still tracked");
    }

    #[test]
    fn inputs_refuse_for_bare_hand() {
        let (mut rig, now) = (Rig::new(), Instant::now());
        rig.apply(RigAction::SetHandMode(HandModeArgs { hand: Hand::Left, mode: HandMode::Hand }), now).unwrap();
        let err = rig.apply(RigAction::SetTrigger(TriggerArgs { hand: Hand::Left, value: 0.5 }), now).unwrap_err();
        assert!(err.contains("bare hand"), "{err}");
        assert!(rig.take_buttons().is_empty());
    }

    #[test]
    fn bare_hand_mode_releases_held_inputs() {
        let (mut rig, now) = (Rig::new(), Instant::now());
        rig.apply(press(Hand::Right, Button::Grip), now).unwrap();
        rig.apply(press(Hand::Left, Button::A), now).unwrap();
        rig.take_buttons();
        rig.apply(RigAction::SetHandMode(HandModeArgs { hand: Hand::Right, mode: HandMode::Hand }), now).unwrap();
        use ButtonValue::{Binary, Scalar};
        let sorted = |mut v: Vec<(u64, String)>| {
            v.sort();
            v
        };
        let released = want("right", &[("squeeze/value", Scalar(0.0)), ("squeeze/force", Scalar(0.0))]);
        assert_eq!(sorted(entries(rig.take_buttons())), sorted(released), "a held grab is let go now");
        let held = sorted(want("left", &[("a/click", Binary(true)), ("a/touch", Binary(true))]));
        let all = sorted(entries(rig.all_buttons()));
        assert_eq!(all, held, "and is not resent to a new stream; the left hand keeps its press");
    }

    #[test]
    fn all_buttons_resend_current_state() {
        let (mut rig, now) = (Rig::new(), Instant::now());
        rig.apply(RigAction::SetTrigger(TriggerArgs { hand: Hand::Right, value: 0.5 }), now).unwrap();
        rig.apply(press(Hand::Left, Button::B), now).unwrap();
        rig.take_buttons();
        let all = entries(rig.all_buttons());
        use ButtonValue::{Binary, Scalar};
        let expected = want("right", &[("trigger/value", Scalar(0.5)), ("trigger/click", Binary(false)), ("trigger/touch", Binary(true))])
            .into_iter()
            .chain(want("left", &[("b/click", Binary(true)), ("b/touch", Binary(true))]));
        for e in expected {
            assert!(all.contains(&e), "{e:?} missing from {all:?}");
        }
        assert_eq!(all.len(), 5);
    }

    #[test]
    fn rig_action_json_shape() {
        let a: RigAction = serde_json::from_str(
            r#"{"action":"set_controller_pose","args":{"hand":"left","position":[0,1,0],"yaw":10,"relative_to":"head"}}"#,
        )
        .unwrap();
        assert!(matches!(a, RigAction::SetControllerPose(ControllerPoseArgs { hand: Hand::Left, relative_to: RelativeTo::Head, .. })));
        let a: RigAction = serde_json::from_str(r#"{"action":"move_controller","args":{"hand":"right","yaw":5}}"#).unwrap();
        assert!(matches!(a, RigAction::MoveController(MoveControllerArgs { mv: MoveArgs { frame: Frame::Playspace, .. }, .. })));
        let a: RigAction =
            serde_json::from_str(r#"{"action":"set_hand_gesture","args":{"hand":"left","gesture":"thumbs_up"}}"#).unwrap();
        assert!(matches!(a, RigAction::SetHandGesture(HandGestureArgs { gesture: Some(Gesture::ThumbsUp), curls: None, .. })));
        let a: RigAction = serde_json::from_str(r#"{"action":"set_hand_gesture","args":{"hand":"left","curls":{"index":0.5}}}"#).unwrap();
        assert!(matches!(a, RigAction::SetHandGesture(HandGestureArgs { curls: Some(Curls { index: 0.5, thumb: 0.0, .. }), .. })));
        let a: RigAction = serde_json::from_str(r#"{"action":"set_grip","args":{"hand":"left","value":0.5}}"#).unwrap();
        assert!(matches!(a, RigAction::SetGrip(GripArgs { force: None, .. })));
    }
    #[test]
    fn reset_restores_defaults_and_keeps_the_send_order() {
        let (mut rig, now) = (Rig::new(), Instant::now());
        let send_order = Arc::clone(&rig.proximity_send);
        rig.apply(RigAction::SetHeadPose(pose_at([1.0, 1.0, 1.0])), now).unwrap();
        rig.apply(RigAction::AnimateHead(AnimateArgs { waypoints: vec![pose_at([0.0, 2.0, 0.0])], duration_s: 5.0 }), now).unwrap();
        let args = ControllerPoseArgs { hand: Hand::Right, pose: pose_at([0.5, 1.0, 0.0]), relative_to: RelativeTo::Playspace };
        rig.apply(RigAction::SetControllerPose(args), now).unwrap();
        rig.apply(press(Hand::Right, Button::A), now).unwrap();
        rig.apply(RigAction::SetFingerCurl(FingerCurlArgs { index: Some(1.0), ..finger_curl(Hand::Right) }), now).unwrap();
        let joints = (0..26).map(|i| [0.0, 1.0, -(i as f32) * 0.01, 0.0, 0.0, 0.0, 1.0]).collect();
        rig.apply(RigAction::SetHandSkeleton(HandSkeletonArgs { hand: Hand::Right, joints }), now).unwrap();
        rig.apply(RigAction::SetHandMode(HandModeArgs { hand: Hand::Left, mode: HandMode::Hand }), now).unwrap();
        rig.apply(RigAction::SetControllerState(ControllerStateArgs { hand: Hand::Left, connected: None, tracked: Some(false) }), now).unwrap();
        rig.set_playspace(4.0, 4.0).unwrap();
        rig.on_head = false;
        rig.reset();
        let json = |rig: &mut Rig| serde_json::to_value(rig.tracking(now + Duration::from_secs(1), Duration::ZERO)).unwrap();
        assert_eq!(json(&mut rig), json(&mut Rig::new()));
        assert!(rig.take_buttons().is_empty() && rig.all_buttons().is_empty());
        assert_eq!(rig.take_playspace(), None);
        rig.recenter();
        assert_eq!(rig.take_playspace(), Some(Vec2::ONE), "the default 2 x 2 area");
        assert!(rig.on_head);
        assert!(Arc::ptr_eq(&rig.proximity_send, &send_order), "a send racing the reset still orders against later ones");
    }

    #[test]
    fn playspace_sends_half_extents() {
        let mut rig = Rig::new();
        assert_eq!(rig.take_playspace(), None);
        rig.set_playspace(3.0, 2.0).unwrap();
        // ALVR draws the chaperone at ±area (ChaperoneUpdater.cpp:69-76)
        assert_eq!(rig.take_playspace(), Some(Vec2::new(1.5, 1.0)));
        assert_eq!(rig.take_playspace(), None);
        rig.recenter();
        assert_eq!(rig.take_playspace(), Some(Vec2::new(1.5, 1.0)), "recenter re-sends the current area");
        let mut fresh = Rig::new();
        fresh.recenter();
        assert_eq!(fresh.take_playspace(), Some(Vec2::new(1.0, 1.0)), "default 2 x 2");
    }

    #[test]
    fn playspace_rejects_bad_size() {
        let mut rig = Rig::new();
        for (w, d) in [(0.0, 2.0), (2.0, -1.0), (f32::NAN, 2.0), (2.0, f32::INFINITY), (1001.0, 2.0)] {
            assert!(rig.set_playspace(w, d).is_err(), "{w} x {d}");
        }
        assert_eq!(rig.take_playspace(), None, "a refused size changes nothing");
        rig.recenter();
        assert_eq!(rig.take_playspace(), Some(Vec2::new(1.0, 1.0)));
    }

    fn step(at_s: f32, action: &str, args: serde_json::Value) -> SequenceStep {
        SequenceStep { at_s, action: action.into(), args }
    }

    #[test]
    fn sequence_steps_parse_as_rig_actions() {
        let trigger = step(0.0, "set_trigger", serde_json::json!({ "hand": "right", "value": 1 }));
        assert!(matches!(trigger.parse(), Ok(RigAction::SetTrigger(TriggerArgs { hand: Hand::Right, .. }))));
        let swing = step(0.0, "animate_controller", serde_json::json!({ "hand": "right", "relative_to": "head", "duration_s": 1.5, "waypoints": [{ "position": [0, 0, -0.5] }] }));
        assert!(matches!(swing.parse(), Ok(RigAction::AnimateController(AnimateControllerArgs { relative_to: RelativeTo::Head, .. }))));
        let shot = step(0.0, "screenshot", serde_json::json!({}));
        assert!(shot.parse().unwrap_err().contains("not allowed in a sequence"));
        let bad = step(0.0, "set_trigger", serde_json::json!({ "hand": "right" }));
        assert!(bad.parse().unwrap_err().contains("value"), "a bad argument names the field");
    }

    #[test]
    fn sequence_orders_and_bounds() {
        let t = |at_s, hand| step(at_s, "set_trigger", serde_json::json!({ "hand": hand, "value": 0 }));
        let plan = plan_sequence(vec![t(1.0, "left"), t(0.0, "right"), t(1.0, "right")], &Rig::new()).unwrap();
        let order: Vec<_> = plan.iter().map(|(s, _)| (s.at_s, matches!(s.parse(), Ok(RigAction::SetTrigger(TriggerArgs { hand: Hand::Left, .. }))))).collect();
        assert_eq!(order, [(0.0, false), (1.0, true), (1.0, false)], "sorted by at_s, equal times keep their order");
        for bad in [-0.1, 600.1, f32::NAN] {
            assert!(plan_sequence(vec![t(bad, "left")], &Rig::new()).is_err(), "{bad}");
        }
        assert!(plan_sequence(vec![t(600.0, "left")], &Rig::new()).is_ok());
        assert!(plan_sequence((0..256).map(|_| t(0.0, "left")).collect(), &Rig::new()).is_ok());
        assert!(plan_sequence((0..257).map(|_| t(0.0, "left")).collect(), &Rig::new()).is_err());
        let err = plan_sequence(vec![t(0.0, "left"), step(1.0, "screenshot", serde_json::json!({}))], &Rig::new()).unwrap_err();
        assert!(err.starts_with("step 1"), "{err}");
    }

    #[test]
    fn sequence_dry_run_refuses_what_the_rig_would() {
        use serde_json::json as j;
        let bad = |steps: Vec<SequenceStep>| plan_sequence(steps, &Rig::new()).unwrap_err();
        let pose = step(0.0, "set_head_pose", j!({ "position": [0, 1, 0] }));
        let err = bad(vec![pose.clone(), step(0.5, "set_trigger", j!({ "hand": "right", "value": 7 }))]);
        assert!(err.starts_with("step 1:") && err.contains("value"), "{err}");
        let err = bad(vec![step(0.0, "set_head_pose", j!({ "position": [0, 1, 0], "quat": [0, 0, 0, 0] }))]);
        assert!(err.starts_with("step 0:"), "{err}");
        let err = bad(vec![step(0.0, "animate_head", j!({ "waypoints": [{ "position": [0, 1, 0] }], "duration_s": 0 }))]);
        assert!(err.contains("duration_s"), "{err}");
        // state carries through the dry run: the step order is by time, the name is the caller's index
        let off = step(0.0, "set_controller_state", j!({ "hand": "right", "connected": false }));
        let err = bad(vec![step(1.0, "set_trigger", j!({ "hand": "right", "value": 1 })), off]);
        assert!(err.starts_with("step 0:") && err.contains("not tracked"), "{err}");
        let bare = step(0.0, "set_hand_mode", j!({ "hand": "left", "mode": "hand" }));
        let err = bad(vec![bare, step(0.1, "press_button", j!({ "hand": "left", "button": "a" }))]);
        assert!(err.starts_with("step 1:") && err.contains("bare hand"), "{err}");
        // the live rig is untouched, and a sequence the rig accepts plans
        let mut rig = Rig::new();
        assert!(plan_sequence(vec![pose, step(0.0, "set_controller_state", j!({ "hand": "left", "connected": false }))], &rig).is_ok());
        assert!(rig.apply(RigAction::SetTrigger(TriggerArgs { hand: Hand::Left, value: 1.0 }), Instant::now()).is_ok());
    }
}
