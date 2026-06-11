//! Leap Motion (Ultraleap) hand-tracking gesture recognition.
//!
//! The tracking service (`leapd`, Ultraleap Gemini) does perception: it
//! turns stereo IR images into per-hand skeletal frames carrying palm
//! position/velocity and pre-computed `pinch_strength` / `grab_strength`
//! scalars. This module does intent: a hysteresis state machine over
//! those frames that recognizes the `Leap*` bind families:
//!
//!   - `LeapPinch` — pinch strength crosses the trigger threshold (discrete, re-arms below the
//!     release threshold)
//!   - `LeapSwipe` — palm speed crosses the trigger speed along a screen axis (discrete, re-arms
//!     when the palm slows down)
//!   - `LeapGrabDrag` — fist closes, palm displacement drives a continuous action; axis latches on
//!     first dominant movement
//!   - `LeapPresence` — debounced hand appear/vanish
//!   - `LeapPose` — N extended fingers held steady on a still palm (fires once per stable count,
//!     re-arms when the count changes)
//!
//! The recognizer is pure (frames in, events out) and testable without
//! hardware; the LeapC connection thread lives in the [`source`] module
//! behind the `leap` cargo feature.
//!
//! Coordinate system (device flat on the desk, cable to the user's
//! right): x grows to the user's right, y grows straight up away from
//! the device, z grows toward the user. Units are millimeters.

use std::time::Duration;

use niri_config::binds::{LeapAxis, LeapHand, LeapPresenceEvent, SwipeDirection, Trigger};
use niri_config::input::Leap as LeapInputConfig;
use niri_config::touch_binds::{continuous_gesture_kind, ContinuousGestureKind};

use super::find_configured_bind;
use crate::niri::{ActiveSwipeBind, State};

/// How long an active grab survives the hand dropping out of tracking.
/// A fully closed fist is the hardest pose for the sensor and routinely
/// flickers out for a frame or two; without this grace the drag anchor
/// resets (or the drag dies) on every dropout. If the hand stays gone
/// longer than this, the grab ends.
const GRAB_LOSS_GRACE_US: i64 = 200_000;

/// Which physical hand a frame's hand entry belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeapChirality {
    Left,
    Right,
}

impl LeapChirality {
    pub fn name(self) -> &'static str {
        match self {
            LeapChirality::Left => "left",
            LeapChirality::Right => "right",
        }
    }
}

fn opposite_direction(direction: SwipeDirection) -> SwipeDirection {
    match direction {
        SwipeDirection::Left => SwipeDirection::Right,
        SwipeDirection::Right => SwipeDirection::Left,
        SwipeDirection::Up => SwipeDirection::Down,
        SwipeDirection::Down => SwipeDirection::Up,
    }
}

/// One tracked hand within a [`LeapFrame`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LeapHandData {
    pub chirality: LeapChirality,
    /// Palm position, mm, device coordinates.
    pub palm_position: [f64; 3],
    /// Palm velocity, mm/s.
    pub palm_velocity: [f64; 3],
    /// 0..1 thumb-to-finger pinch estimate from the tracking service.
    pub pinch_strength: f64,
    /// 0..1 fist estimate from the tracking service.
    pub grab_strength: f64,
    /// Per-digit "more or less straight" flags from the tracking
    /// service, in anatomical order: thumb, index, middle, ring, pinky.
    pub fingers_extended: [bool; 5],
}

impl LeapHandData {
    /// Number of extended fingers (0 = fist, 5 = open palm).
    pub fn extended_count(&self) -> u8 {
        self.fingers_extended.iter().filter(|e| **e).count() as u8
    }
}

/// One tracking frame from the service (~100/s while a client is
/// subscribed).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LeapFrame {
    /// Service clock, microseconds.
    pub timestamp_us: i64,
    pub hands: Vec<LeapHandData>,
}

/// Recognized gesture events, consumed by the dispatch layer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LeapGestureEvent {
    /// An air pinch fired (already hysteresis-filtered).
    Pinch { hand: LeapChirality },
    /// A palm swipe fired.
    Swipe {
        hand: LeapChirality,
        direction: SwipeDirection,
    },
    /// A grab-drag latched onto an axis. Followed by `GrabUpdate`s and
    /// eventually `GrabEnd`.
    GrabBegin { hand: LeapChirality, axis: LeapAxis },
    /// Palm moved during a latched grab-drag. `delta_mm` is the change
    /// since the previous update along the latched axis; `total_mm` the
    /// signed displacement from the latch point. Positive = right, up,
    /// or forward (pushing away from you) depending on the axis.
    GrabUpdate {
        hand: LeapChirality,
        axis: LeapAxis,
        delta_mm: f64,
        total_mm: f64,
    },
    /// The fist opened (or the hand left the view) ending a latched
    /// grab-drag.
    GrabEnd { hand: LeapChirality, axis: LeapAxis },
    /// A hand appeared over / vanished from the sensor (debounced).
    Presence {
        hand: LeapChirality,
        event: LeapPresenceEvent,
    },
    /// A finger-count pose settled: `fingers` (1..=5) held extended on a
    /// near-still palm for the settle interval. Fires once per stable
    /// count; the next event needs the count to change first.
    Pose { hand: LeapChirality, fingers: u8 },
}

/// Recognition thresholds, snapshotted from config each frame so config
/// hot-reload takes effect immediately.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LeapTuning {
    pub pinch_trigger: f64,
    pub pinch_release: f64,
    pub swipe_trigger_speed: f64,
    pub swipe_rearm_speed: f64,
    pub swipe_vertical_scale: f64,
    pub swipe_return_window_us: i64,
    pub grab_trigger: f64,
    pub grab_release: f64,
    pub grab_axis_latch_mm: f64,
    pub grab_settle_us: i64,
    pub grab_vertical_scale: f64,
    pub grab_depth_scale: f64,
    pub presence_debounce_us: i64,
    pub pose_settle_us: i64,
    pub pose_max_speed: f64,
}

impl LeapTuning {
    pub fn from_config(config: &LeapInputConfig) -> Self {
        Self {
            pinch_trigger: config.pinch_trigger_strength(),
            pinch_release: config.pinch_release_strength(),
            swipe_trigger_speed: config.swipe_trigger_speed(),
            swipe_rearm_speed: config.swipe_rearm_speed(),
            swipe_vertical_scale: config.swipe_vertical_scale(),
            swipe_return_window_us: (config.swipe_return_window_ms() * 1000.0) as i64,
            grab_trigger: config.grab_trigger_strength(),
            grab_release: config.grab_release_strength(),
            grab_axis_latch_mm: config.grab_axis_latch_distance(),
            grab_settle_us: (config.grab_settle_ms() * 1000.0) as i64,
            grab_vertical_scale: config.grab_vertical_scale(),
            grab_depth_scale: config.grab_depth_scale(),
            presence_debounce_us: (config.presence_debounce_ms() * 1000.0) as i64,
            pose_settle_us: (config.pose_settle_ms() * 1000.0) as i64,
            pose_max_speed: config.pose_max_speed(),
        }
    }
}

/// A grab-drag in progress.
#[derive(Debug, Clone, Copy, PartialEq)]
struct GrabState {
    /// When the fist closed (grab strength crossed the trigger).
    triggered_us: i64,
    /// Drag anchor, captured `grab-settle-ms` after the trigger so the
    /// palm-position shift from the fingers curling doesn't count as
    /// displacement. `None` while settling.
    anchor: Option<[f64; 3]>,
    /// Set once displacement exceeds the axis-latch distance: the locked
    /// axis and the displacement reported in the previous `GrabUpdate`.
    latched: Option<(LeapAxis, f64)>,
}

/// Per-hand recognition state.
#[derive(Debug, Default)]
struct HandTracker {
    /// Debounced presence (what `Presence` events report).
    visible: bool,
    /// Raw presence as of the last frame.
    raw_visible: bool,
    /// Timestamp of the last raw presence flip.
    raw_since_us: i64,
    /// Timestamp of the last frame that actually contained this hand.
    last_seen_us: i64,
    /// Pinch can fire. Arms when strength drops below the release
    /// threshold, so a hand entering the view mid-pinch doesn't fire.
    pinch_armed: bool,
    /// Swipe can fire. Arms when the palm slows below the re-arm speed,
    /// so a hand entering the view at speed doesn't fire.
    swipe_armed: bool,
    /// Direction and timestamp of the last fired swipe, for
    /// return-stroke suppression.
    last_swipe: Option<(SwipeDirection, i64)>,
    grab: Option<GrabState>,
    /// Pose candidate: an extended-finger count and when it became the
    /// candidate. Resets whenever the count changes, the palm moves
    /// faster than `pose-max-speed`, or a grab is in progress — the
    /// settle clock only runs on a still, ungrabbed hand.
    pose_candidate: Option<(u8, i64)>,
    /// The last count that completed a settle (0..=5; 0 settles silently
    /// — it's a fist, grab territory). A `Pose` event fires only when a
    /// settling count differs from this, so flicker or movement
    /// mid-hold can't repeat-fire, while relaxing the hand (which
    /// settles a different count) re-arms the same pose for another
    /// deliberate hold. Cleared on debounced hand loss.
    pose_settled: Option<u8>,
    /// Extended-finger count as of the last frame, only for logging
    /// count transitions at debug level.
    last_count: Option<u8>,
}

/// Hysteresis state machine over leap frames. One instance lives on
/// `Niri`; `advance` is called per tracking frame.
#[derive(Debug, Default)]
pub struct LeapGestureState {
    left: HandTracker,
    right: HandTracker,
}

impl LeapGestureState {
    /// Feed one frame, appending recognized events to `out` in firing
    /// order.
    pub fn advance(
        &mut self,
        frame: &LeapFrame,
        tuning: &LeapTuning,
        out: &mut Vec<LeapGestureEvent>,
    ) {
        for chirality in [LeapChirality::Left, LeapChirality::Right] {
            let hand = frame
                .hands
                .iter()
                .find(|h| h.chirality == chirality)
                .copied();
            let tracker = match chirality {
                LeapChirality::Left => &mut self.left,
                LeapChirality::Right => &mut self.right,
            };
            Self::advance_hand(tracker, chirality, hand, frame.timestamp_us, tuning, out);
        }
    }

    fn advance_hand(
        tracker: &mut HandTracker,
        chirality: LeapChirality,
        hand: Option<LeapHandData>,
        now_us: i64,
        tuning: &LeapTuning,
        out: &mut Vec<LeapGestureEvent>,
    ) {
        // Presence debounce. Raw flips immediately; the debounced state
        // (and the Presence event) follows only after the raw state has
        // held for the debounce interval, filtering tracking flicker at
        // the view edge.
        let raw = hand.is_some();
        if raw != tracker.raw_visible {
            tracker.raw_visible = raw;
            tracker.raw_since_us = now_us;
        }
        if tracker.raw_visible != tracker.visible
            && now_us - tracker.raw_since_us >= tuning.presence_debounce_us
        {
            tracker.visible = tracker.raw_visible;
            if !tracker.visible {
                // Sustained absence re-arms poses entirely; a hand
                // re-entering the view holding up N fingers should fire
                // even if N was the last pose before it left.
                tracker.pose_settled = None;
            }
            let event = if tracker.visible {
                LeapPresenceEvent::Appear
            } else {
                LeapPresenceEvent::Vanish
            };
            tracing::debug!(
                "LEAP-DBG PRESENCE: hand={} event={event:?}",
                chirality.name()
            );
            out.push(LeapGestureEvent::Presence {
                hand: chirality,
                event,
            });
        }

        let Some(hand) = hand else {
            // Hand left the view. Pinch and swipe disarm immediately, but
            // an active grab survives a short grace period: a closed fist
            // is the hardest pose for the sensor and routinely flickers
            // out of tracking for a frame or two. Ending (or re-anchoring)
            // the drag on every flicker makes grab-drag unusable.
            if tracker.grab.is_some() && now_us - tracker.last_seen_us > GRAB_LOSS_GRACE_US {
                if let Some(GrabState {
                    latched: Some((axis, _)),
                    ..
                }) = tracker.grab
                {
                    tracing::debug!("LEAP-DBG GRAB-END: hand={} (lost)", chirality.name());
                    out.push(LeapGestureEvent::GrabEnd {
                        hand: chirality,
                        axis,
                    });
                }
                tracker.grab = None;
            }
            tracker.pinch_armed = false;
            tracker.swipe_armed = false;
            // The settle clock stops on dropout; `pose_settled` survives
            // until the debounced Vanish so tracking flicker mid-hold
            // can't re-fire the same pose.
            tracker.pose_candidate = None;
            tracker.last_count = None;
            return;
        };
        tracker.last_seen_us = now_us;

        let grabbing = tracker.grab.is_some() || hand.grab_strength >= tuning.grab_release;

        // Pinch (suppressed while the hand is in or near a fist — a
        // closing fist drives pinch_strength up too).
        if tracker.pinch_armed && !grabbing && hand.pinch_strength >= tuning.pinch_trigger {
            tracker.pinch_armed = false;
            tracing::debug!(
                "LEAP-DBG PINCH: hand={} strength={:.2}",
                chirality.name(),
                hand.pinch_strength
            );
            out.push(LeapGestureEvent::Pinch { hand: chirality });
        } else if !tracker.pinch_armed && hand.pinch_strength <= tuning.pinch_release && !grabbing {
            tracker.pinch_armed = true;
        }

        // Swipe (per-axis palm velocity; suppressed during grabs). The
        // dominant axis must individually exceed its trigger speed —
        // vertical gets a raised threshold (`swipe_vertical_scale`)
        // because hands enter and leave the view cone vertically, which
        // would otherwise fire Up on every hand withdrawal.
        let vx = hand.palm_velocity[0];
        let vy = hand.palm_velocity[1];
        let planar_speed = vx.hypot(vy);
        let horizontal_fire = vx.abs() > vy.abs() && vx.abs() >= tuning.swipe_trigger_speed;
        let vertical_fire = vy.abs() > vx.abs()
            && vy.abs() >= tuning.swipe_trigger_speed * tuning.swipe_vertical_scale;
        if tracker.swipe_armed && !grabbing && (horizontal_fire || vertical_fire) {
            tracker.swipe_armed = false;
            let direction = if horizontal_fire {
                if vx > 0.0 {
                    SwipeDirection::Right
                } else {
                    SwipeDirection::Left
                }
            } else if vy > 0.0 {
                SwipeDirection::Up
            } else {
                SwipeDirection::Down
            };

            // Return-stroke suppression: chaining swipes in one
            // direction means winding the hand back between strokes,
            // and the wind-back is itself a fast opposite-direction
            // movement. Swallow it so swipe-swipe-swipe works.
            let is_return_stroke = tracker.last_swipe.is_some_and(|(last_dir, last_us)| {
                last_dir == opposite_direction(direction)
                    && now_us - last_us <= tuning.swipe_return_window_us
            });
            if is_return_stroke {
                tracing::debug!(
                    "LEAP-DBG SWIPE-RETURN: hand={} direction={direction:?} swallowed",
                    chirality.name()
                );
            } else {
                tracker.last_swipe = Some((direction, now_us));
                tracing::debug!(
                    "LEAP-DBG SWIPE: hand={} direction={direction:?} vx={vx:.0} vy={vy:.0}mm/s",
                    chirality.name()
                );
                out.push(LeapGestureEvent::Swipe {
                    hand: chirality,
                    direction,
                });
            }
        } else if !tracker.swipe_armed && planar_speed <= tuning.swipe_rearm_speed && !grabbing {
            tracker.swipe_armed = true;
        }

        // Pose (N extended fingers held on a still hand). A candidate
        // count settles after `pose-settle-ms` with the palm under
        // `pose-max-speed`; movement, a grab, a pinch, or a count change
        // resets the clock. This filters the intermediate counts a hand
        // passes through while unfolding (fist → 3 crosses 1 and 2 on
        // the way). A settling count fires only if it differs from the
        // previous settled count, and 0 (fist — grab territory) settles
        // silently: together these re-arm a pose via any hand relaxation
        // without ever repeat-firing on hold.
        //
        // The pinch gate matters because a held thumb-to-index pinch
        // curls only those two digits — middle/ring/pinky stay extended,
        // so without it every pinch settles a phantom 3-finger pose.
        // Same hysteresis floor as pinch re-arm: the hand counts as
        // pinching until strength drops below the release threshold.
        let count = hand.extended_count();
        if tracker.last_count != Some(count) {
            tracing::debug!(
                "LEAP-DBG POSE-COUNT: hand={} fingers={count} extended={:?}",
                chirality.name(),
                hand.fingers_extended
            );
            tracker.last_count = Some(count);
        }
        let pinching = hand.pinch_strength >= tuning.pinch_release;
        if grabbing || pinching || planar_speed > tuning.pose_max_speed {
            tracker.pose_candidate = None;
        } else {
            match tracker.pose_candidate {
                Some((c, since)) if c == count => {
                    if tracker.pose_settled != Some(count)
                        && now_us - since >= tuning.pose_settle_us
                    {
                        tracker.pose_settled = Some(count);
                        if count >= 1 {
                            tracing::debug!(
                                "LEAP-DBG POSE: hand={} fingers={count}",
                                chirality.name()
                            );
                            out.push(LeapGestureEvent::Pose {
                                hand: chirality,
                                fingers: count,
                            });
                        }
                    }
                }
                _ => tracker.pose_candidate = Some((count, now_us)),
            }
        }

        // Grab-drag.
        match &mut tracker.grab {
            None => {
                if hand.grab_strength >= tuning.grab_trigger {
                    tracing::debug!(
                        "LEAP-DBG GRAB: hand={} strength={:.2}",
                        chirality.name(),
                        hand.grab_strength
                    );
                    tracker.grab = Some(GrabState {
                        triggered_us: now_us,
                        anchor: None,
                        latched: None,
                    });
                    // A closed fist can't pinch or swipe; disarm both so
                    // they re-arm cleanly after release.
                    tracker.pinch_armed = false;
                    tracker.swipe_armed = false;
                }
            }
            Some(grab) => {
                if hand.grab_strength <= tuning.grab_release {
                    if let Some((axis, _)) = grab.latched {
                        tracing::debug!("LEAP-DBG GRAB-END: hand={}", chirality.name());
                        out.push(LeapGestureEvent::GrabEnd {
                            hand: chirality,
                            axis,
                        });
                    }
                    tracker.grab = None;
                } else {
                    // Anchor only after the settle delay: the fingers
                    // curling into the fist shifts the tracked palm
                    // position (mostly vertically), and counting that as
                    // drag displacement made the axis race latch vertical
                    // nearly every time.
                    let Some(anchor) = grab.anchor else {
                        if now_us - grab.triggered_us >= tuning.grab_settle_us {
                            grab.anchor = Some(hand.palm_position);
                        }
                        return;
                    };
                    let dx = hand.palm_position[0] - anchor[0];
                    let dy = hand.palm_position[1] - anchor[1];
                    let dz = hand.palm_position[2] - anchor[2];
                    match &mut grab.latched {
                        None => {
                            if (dx * dx + dy * dy + dz * dz).sqrt() >= tuning.grab_axis_latch_mm {
                                // Three-way race on weighted magnitudes:
                                // vertical and depth are divided by their
                                // scale factors first, so values above 1.0
                                // make that axis harder to win. Vertical's
                                // default bias compensates for the residual
                                // vertical drift inherent in fist motion;
                                // ties go to horizontal.
                                let sx = dx.abs();
                                let sy = dy.abs() / tuning.grab_vertical_scale;
                                let sz = dz.abs() / tuning.grab_depth_scale;
                                let axis = if sy > sx && sy > sz {
                                    LeapAxis::Vertical
                                } else if sz > sx {
                                    LeapAxis::Depth
                                } else {
                                    LeapAxis::Horizontal
                                };
                                tracing::debug!(
                                    "LEAP-DBG GRAB-LATCH: hand={} axis={axis:?} dx={dx:.0} dy={dy:.0} dz={dz:.0}",
                                    chirality.name()
                                );
                                grab.latched = Some((axis, 0.0));
                                // Re-anchor at the latch point so progress
                                // starts at zero rather than jumping by the
                                // latch radius.
                                grab.anchor = Some(hand.palm_position);
                                out.push(LeapGestureEvent::GrabBegin {
                                    hand: chirality,
                                    axis,
                                });
                            }
                        }
                        Some((axis, last_total)) => {
                            let total = match axis {
                                LeapAxis::Horizontal => dx,
                                LeapAxis::Vertical => dy,
                                // The device's z axis grows *toward* the
                                // user; flip so positive = pushing the
                                // fist away, matching the right/up
                                // convention of the other axes.
                                LeapAxis::Depth => -dz,
                            };
                            let delta = total - *last_total;
                            if delta != 0.0 {
                                *last_total = total;
                                let (axis, _) = grab.latched.unwrap();
                                out.push(LeapGestureEvent::GrabUpdate {
                                    hand: chirality,
                                    axis,
                                    delta_mm: delta,
                                    total_mm: total,
                                });
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Default per-bind sensitivity for leap gesture binds.
const LEAP_DEFAULT_SENSITIVITY: f64 = 1.0;

/// Touchpad gesture units corresponding to one full workspace switch /
/// overview toggle (`WORKSPACE_GESTURE_MOVEMENT` / `OVERVIEW_GESTURE_MOVEMENT`
/// in the layout code, both 300). Grab-drag mm deltas are scaled so
/// `grab-drag-distance` millimeters of palm travel produce this many units.
const FULL_GESTURE_UNITS: f64 = 300.0;

fn specific_hand(hand: LeapChirality) -> LeapHand {
    match hand {
        LeapChirality::Left => LeapHand::Left,
        LeapChirality::Right => LeapHand::Right,
    }
}

impl State {
    /// Entry point for tracking frames arriving from the LeapC thread.
    pub fn on_leap_frame(&mut self, frame: LeapFrame) {
        let tuning = {
            let config = self.niri.config.borrow();
            if config.input.leap.off {
                return;
            }
            LeapTuning::from_config(&config.input.leap)
        };

        let mut events = Vec::new();
        self.niri
            .leap_gesture_state
            .advance(&frame, &tuning, &mut events);

        let timestamp = Duration::from_micros(frame.timestamp_us.max(0) as u64);
        for event in events {
            self.on_leap_gesture_event(event, timestamp);
        }
    }

    fn on_leap_gesture_event(&mut self, event: LeapGestureEvent, timestamp: Duration) {
        // Debug aid (`input.leap.notify-gestures`): announce every
        // recognized gesture, independent of what it's bound to.
        // Per-frame grab updates are excluded — they'd flood at ~100/s.
        if self.niri.config.borrow().input.leap.notify_gestures
            && !matches!(event, LeapGestureEvent::GrabUpdate { .. })
        {
            let body = match event {
                LeapGestureEvent::Pinch { hand } => format!("Pinch ({})", hand.name()),
                LeapGestureEvent::Swipe { hand, direction } => {
                    format!("Swipe {direction:?} ({})", hand.name())
                }
                LeapGestureEvent::GrabBegin { hand, axis } => {
                    format!("Grab-Drag {axis:?} begin ({})", hand.name())
                }
                LeapGestureEvent::GrabEnd { hand, axis } => {
                    format!("Grab-Drag {axis:?} end ({})", hand.name())
                }
                LeapGestureEvent::Presence { hand, event } => {
                    format!("Hand {event:?} ({})", hand.name())
                }
                LeapGestureEvent::Pose { hand, fingers } => {
                    let plural = if fingers == 1 { "" } else { "s" };
                    format!("Pose {fingers} finger{plural} ({})", hand.name())
                }
                LeapGestureEvent::GrabUpdate { .. } => unreachable!(),
            };
            crate::utils::spawning::spawn(
                vec!["notify-send".to_owned(), "Leap".to_owned(), body],
                None,
            );
        }

        match event {
            LeapGestureEvent::Pinch { hand } => {
                self.leap_fire_discrete(&[
                    Trigger::LeapPinch {
                        hand: specific_hand(hand),
                    },
                    Trigger::LeapPinch {
                        hand: LeapHand::Any,
                    },
                ]);
            }
            LeapGestureEvent::Swipe { hand, direction } => {
                self.leap_fire_discrete(&[
                    Trigger::LeapSwipe {
                        hand: specific_hand(hand),
                        direction,
                    },
                    Trigger::LeapSwipe {
                        hand: LeapHand::Any,
                        direction,
                    },
                ]);
            }
            LeapGestureEvent::Presence { hand, event } => {
                self.leap_fire_discrete(&[
                    Trigger::LeapPresence {
                        hand: specific_hand(hand),
                        event,
                    },
                    Trigger::LeapPresence {
                        hand: LeapHand::Any,
                        event,
                    },
                ]);
            }
            LeapGestureEvent::Pose { hand, fingers } => {
                self.leap_fire_discrete(&[
                    Trigger::LeapPose {
                        hand: specific_hand(hand),
                        fingers,
                    },
                    Trigger::LeapPose {
                        hand: LeapHand::Any,
                        fingers,
                    },
                ]);
            }
            LeapGestureEvent::GrabBegin { hand, axis } => self.leap_grab_begin(hand, axis),
            LeapGestureEvent::GrabUpdate { delta_mm, .. } => {
                self.leap_grab_update(delta_mm, timestamp)
            }
            LeapGestureEvent::GrabEnd { .. } => self.leap_grab_end(),
        }
    }

    /// Find the first configured bind among `triggers` (callers list the
    /// hand-specific trigger before the `hand="any"` fallback).
    fn leap_find_bind(&mut self, triggers: &[Trigger]) -> Option<niri_config::Bind> {
        let mods = self.niri.seat.get_keyboard().unwrap().modifier_state();
        let mod_key = self.backend.mod_key(&self.niri.config.borrow());
        let config = self.niri.config.borrow();
        triggers.iter().find_map(|&trigger| {
            find_configured_bind(config.binds.0.iter(), mod_key, trigger, mods)
        })
    }

    fn leap_fire_discrete(&mut self, triggers: &[Trigger]) {
        let Some(bind) = self.leap_find_bind(triggers) else {
            return;
        };
        self.do_action(bind.action, bind.allow_when_locked);
    }

    fn leap_grab_begin(&mut self, hand: LeapChirality, axis: LeapAxis) {
        let Some(bind) = self.leap_find_bind(&[
            Trigger::LeapGrabDrag {
                hand: specific_hand(hand),
                axis,
            },
            Trigger::LeapGrabDrag {
                hand: LeapHand::Any,
                axis,
            },
        ]) else {
            return;
        };

        let mut sensitivity = bind.sensitivity.unwrap_or(LEAP_DEFAULT_SENSITIVITY);
        // `natural-scroll=true` on the bind inverts the drag direction,
        // same as on the touch gesture binds. A negative sensitivity
        // flips every update's sign, which is exactly that.
        if bind.natural_scroll {
            sensitivity = -sensitivity;
        }
        let Some(kind) = continuous_gesture_kind(&bind.action) else {
            // Discrete action on a grab-drag bind: fire once at latch.
            self.do_action(bind.action, bind.allow_when_locked);
            return;
        };

        let is_overview_open = self.niri.layout.is_overview_open();
        match kind {
            ContinuousGestureKind::OverviewToggle => {
                self.niri.layout.overview_gesture_begin();
                self.niri.queue_redraw_all();
            }
            ContinuousGestureKind::WorkspaceSwitch => {
                if let Some(output) = self.niri.output_under_cursor() {
                    self.niri
                        .layout
                        .workspace_switch_gesture_begin(&output, true);
                }
            }
            ContinuousGestureKind::ViewScroll => {
                let output_ws = if is_overview_open {
                    self.niri.workspace_under_cursor(true)
                } else {
                    self.niri.output_under_cursor().and_then(|output| {
                        let mon = self.niri.layout.monitor_for_output(&output)?;
                        Some((output, mon.active_workspace_ref()))
                    })
                };
                if let Some((output, ws)) = output_ws {
                    let ws_idx = self.niri.layout.find_workspace_by_id(ws.id()).unwrap().0;
                    self.niri
                        .layout
                        .view_offset_gesture_begin(&output, Some(ws_idx), true);
                }
            }
            ContinuousGestureKind::Noop => {
                // No compositor animation.
            }
        }
        self.niri.leap_grab_bind = Some(ActiveSwipeBind { kind, sensitivity });
    }

    fn leap_grab_update(&mut self, delta_mm: f64, timestamp: Duration) {
        let Some(ActiveSwipeBind { kind, sensitivity }) = self.niri.leap_grab_bind else {
            return;
        };

        // Scale palm millimeters so that `grab-drag-distance` mm of travel
        // equals one full workspace switch.
        let grab_drag_distance = {
            let config = self.niri.config.borrow();
            config.input.leap.grab_drag_distance()
        };
        let scaled = delta_mm * sensitivity * (FULL_GESTURE_UNITS / grab_drag_distance.max(1.0));

        match kind {
            ContinuousGestureKind::WorkspaceSwitch => {
                // Hand up = fingers up on a touchpad (negative delta).
                let res = self
                    .niri
                    .layout
                    .workspace_switch_gesture_update(-scaled, timestamp, true);
                if let Some(Some(output)) = res {
                    self.niri.queue_redraw(&output);
                }
            }
            ContinuousGestureKind::ViewScroll => {
                let res = self
                    .niri
                    .layout
                    .view_offset_gesture_update(scaled, timestamp, true);
                if let Some(Some(output)) = res {
                    self.niri.queue_redraw(&output);
                }
            }
            ContinuousGestureKind::OverviewToggle => {
                // Hand up opens the overview.
                let res = self.niri.layout.overview_gesture_update(scaled, timestamp);
                if let Some(true) = res {
                    self.niri.queue_redraw_all();
                }
            }
            ContinuousGestureKind::Noop => {}
        }
    }

    fn leap_grab_end(&mut self) {
        let Some(ActiveSwipeBind { kind, .. }) = self.niri.leap_grab_bind.take() else {
            return;
        };

        match kind {
            ContinuousGestureKind::WorkspaceSwitch => {
                if let Some(output) = self.niri.layout.workspace_switch_gesture_end(Some(true)) {
                    self.niri.queue_redraw(&output);
                }
            }
            ContinuousGestureKind::ViewScroll => {
                if let Some(output) = self.niri.layout.view_offset_gesture_end(Some(true)) {
                    self.niri.queue_redraw(&output);
                }
            }
            ContinuousGestureKind::OverviewToggle => {
                if self.niri.layout.overview_gesture_end() {
                    self.niri.queue_redraw_all();
                }
            }
            ContinuousGestureKind::Noop => {}
        }
    }
}

/// LeapC connection thread: polls the Ultraleap tracking service and
/// forwards tracking frames into the compositor's event loop through a
/// calloop channel. Reconnects with backoff if the service is absent or
/// goes away.
#[cfg(feature = "leap")]
pub mod source {
    use std::time::Duration;

    use super::{LeapChirality, LeapFrame, LeapHandData};

    /// Spawn the polling thread; the returned channel yields tracking
    /// frames. The thread exits when the receiving end is dropped.
    pub fn start() -> calloop::channel::Channel<LeapFrame> {
        let (tx, rx) = calloop::channel::channel();
        std::thread::Builder::new()
            .name("leap-input".to_owned())
            .spawn(move || run(tx))
            .unwrap();
        rx
    }

    /// TCP port of the Ultraleap tracking service on localhost. Fixed in
    /// libLeapC; used to find the library's socket fd (see below).
    const LEAP_SERVICE_PORT: u16 = 12345;

    /// libLeapC opens its service socket without `O_CLOEXEC`, so every
    /// process the compositor spawns would inherit it — keeping a zombie
    /// connection to the tracking service alive for as long as that app
    /// runs (and the daemon's client slot occupied). Find fds whose
    /// socket is connected to the LeapC port and mark them `FD_CLOEXEC`.
    ///
    /// Best-effort: a spawn racing the brief window between connect and
    /// this call can still inherit the fd; that costs one leaked socket,
    /// not correctness.
    fn mark_leap_sockets_cloexec() {
        // Collect socket inodes connected to the leap port from
        // /proc/self/net/tcp (fields: sl, local_address, rem_address, st,
        // ..., inode at index 9; addresses are hex ip:port).
        let Ok(tcp) = std::fs::read_to_string("/proc/self/net/tcp") else {
            return;
        };
        let port_hex = format!(":{LEAP_SERVICE_PORT:04X}");
        let inodes: Vec<&str> = tcp
            .lines()
            .skip(1)
            .filter_map(|line| {
                let fields: Vec<&str> = line.split_whitespace().collect();
                (fields.len() > 9 && fields[2].ends_with(&port_hex)).then(|| fields[9])
            })
            .collect();
        if inodes.is_empty() {
            return;
        }

        let Ok(fds) = std::fs::read_dir("/proc/self/fd") else {
            return;
        };
        for entry in fds.flatten() {
            let Ok(target) = std::fs::read_link(entry.path()) else {
                continue;
            };
            let target = target.to_string_lossy();
            let is_leap_socket = inodes
                .iter()
                .any(|inode| target == format!("socket:[{inode}]"));
            if !is_leap_socket {
                continue;
            }
            let Some(fd) = entry
                .file_name()
                .to_string_lossy()
                .parse::<std::os::raw::c_int>()
                .ok()
            else {
                continue;
            };
            unsafe {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags != -1 {
                    libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
                }
            }
        }
    }

    fn run(tx: calloop::channel::Sender<LeapFrame>) {
        use leaprs::{Connection, ConnectionConfig, Error, EventRef, HandType};

        const RETRY: Duration = Duration::from_secs(5);
        loop {
            let mut connection = match Connection::create(ConnectionConfig::default()) {
                Ok(c) => c,
                Err(err) => {
                    tracing::warn!("leap: failed to create LeapC connection: {err}; retrying");
                    std::thread::sleep(RETRY);
                    continue;
                }
            };
            if let Err(err) = connection.open() {
                tracing::warn!("leap: failed to open LeapC connection: {err}; retrying");
                std::thread::sleep(RETRY);
                continue;
            }
            mark_leap_sockets_cloexec();
            tracing::info!("leap: connected to Ultraleap tracking service");

            loop {
                match connection.poll(1000) {
                    Ok(message) => {
                        let EventRef::Tracking(tracking) = message.event() else {
                            continue;
                        };
                        let frame = LeapFrame {
                            timestamp_us: tracking.info().timestamp,
                            hands: tracking
                                .hands()
                                .iter()
                                .map(|hand| LeapHandData {
                                    chirality: match hand.hand_type() {
                                        HandType::Left => LeapChirality::Left,
                                        HandType::Right => LeapChirality::Right,
                                    },
                                    palm_position: hand.palm().position().array().map(f64::from),
                                    palm_velocity: hand.palm().velocity().array().map(f64::from),
                                    pinch_strength: f64::from(hand.pinch_strength),
                                    grab_strength: f64::from(hand.grab_strength),
                                    // digits() is anatomical order:
                                    // thumb, index, middle, ring, pinky.
                                    fingers_extended: hand
                                        .digits()
                                        .map(|digit| digit.is_extended()),
                                })
                                .collect(),
                        };
                        if tx.send(frame).is_err() {
                            // Compositor side is gone; we're done.
                            return;
                        }
                    }
                    // Quiet periods are normal: the service stops
                    // streaming when no hands are in view.
                    // Quiet periods are normal (no hands in view), and
                    // HandshakeIncomplete is the expected state for the
                    // first polls right after open() — keep polling, the
                    // handshake completes inside LeapPollConnection.
                    Err(Error::Timeout | Error::HandshakeIncomplete) => continue,
                    // These mean the service really is gone.
                    Err(err @ (Error::NotConnected | Error::UnexpectedClosed)) => {
                        tracing::warn!("leap: connection lost: {err}; reconnecting");
                        break;
                    }
                    // Anything else: log and keep the connection; back off
                    // briefly so a persistent error can't busy-spin.
                    Err(err) => {
                        tracing::warn!("leap: poll error: {err}");
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
            }
            std::thread::sleep(RETRY);
        }
    }
}

#[cfg(test)]
mod tests;
