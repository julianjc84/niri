use super::*;

const TUNING: LeapTuning = LeapTuning {
    pinch_trigger: 0.8,
    pinch_release: 0.4,
    swipe_trigger_speed: 400.0,
    swipe_rearm_speed: 100.0,
    swipe_vertical_scale: 1.5,
    swipe_return_window_us: 400_000,
    grab_trigger: 0.8,
    grab_release: 0.4,
    grab_axis_latch_mm: 15.0,
    grab_settle_us: 50_000,
    grab_vertical_scale: 1.5,
    grab_depth_scale: 1.0,
    presence_debounce_us: 250_000,
    pose_settle_us: 150_000,
    pose_max_speed: 120.0,
};

/// Extended-flag array with the first `n` digits extended. Which
/// digits are extended doesn't matter to the recognizer — only the
/// count does.
fn extended(n: usize) -> [bool; 5] {
    let mut flags = [false; 5];
    flags[..n].fill(true);
    flags
}

fn hand(chirality: LeapChirality) -> LeapHandData {
    LeapHandData {
        chirality,
        palm_position: [0.0, 200.0, 0.0],
        palm_velocity: [0.0, 0.0, 0.0],
        pinch_strength: 0.0,
        grab_strength: 0.0,
        fingers_extended: [false; 5],
    }
}

fn frame(timestamp_us: i64, hands: Vec<LeapHandData>) -> LeapFrame {
    LeapFrame {
        timestamp_us,
        hands,
    }
}

fn advance(state: &mut LeapGestureState, frame_: &LeapFrame) -> Vec<LeapGestureEvent> {
    let mut out = Vec::new();
    state.advance(frame_, &TUNING, &mut out);
    out
}

#[test]
fn pinch_fires_once_with_hysteresis() {
    let mut state = LeapGestureState::default();
    let mut h = hand(LeapChirality::Right);

    // Hand appears unpinched: arms, no event yet (presence debounce).
    assert_eq!(advance(&mut state, &frame(0, vec![h])), vec![]);

    // Strength rises past the trigger: fires exactly once.
    h.pinch_strength = 0.85;
    let events = advance(&mut state, &frame(10_000, vec![h]));
    assert_eq!(
        events,
        vec![LeapGestureEvent::Pinch {
            hand: LeapChirality::Right
        }]
    );

    // Hovering above the release threshold must not re-fire.
    h.pinch_strength = 0.75;
    assert_eq!(advance(&mut state, &frame(20_000, vec![h])), vec![]);
    h.pinch_strength = 0.9;
    assert_eq!(advance(&mut state, &frame(30_000, vec![h])), vec![]);

    // Dropping below release re-arms; crossing again re-fires.
    h.pinch_strength = 0.3;
    assert_eq!(advance(&mut state, &frame(40_000, vec![h])), vec![]);
    h.pinch_strength = 0.85;
    assert_eq!(
        advance(&mut state, &frame(50_000, vec![h])),
        vec![LeapGestureEvent::Pinch {
            hand: LeapChirality::Right
        }]
    );
}

#[test]
fn pinch_does_not_fire_if_hand_enters_pinched() {
    let mut state = LeapGestureState::default();
    let mut h = hand(LeapChirality::Right);
    h.pinch_strength = 0.9;

    // Enters the view already pinched: no fire until released once.
    assert_eq!(advance(&mut state, &frame(0, vec![h])), vec![]);
    assert_eq!(advance(&mut state, &frame(10_000, vec![h])), vec![]);

    h.pinch_strength = 0.2;
    assert_eq!(advance(&mut state, &frame(20_000, vec![h])), vec![]);
    h.pinch_strength = 0.9;
    assert_eq!(
        advance(&mut state, &frame(30_000, vec![h])),
        vec![LeapGestureEvent::Pinch {
            hand: LeapChirality::Right
        }]
    );
}

#[test]
fn swipe_picks_dominant_axis_and_rearms() {
    let mut state = LeapGestureState::default();
    let mut h = hand(LeapChirality::Left);

    // Slow first frame arms swipe.
    assert_eq!(advance(&mut state, &frame(0, vec![h])), vec![]);

    // Fast leftward palm: fires Left (|vx| > |vy|).
    h.palm_velocity = [-500.0, 120.0, 0.0];
    assert_eq!(
        advance(&mut state, &frame(10_000, vec![h])),
        vec![LeapGestureEvent::Swipe {
            hand: LeapChirality::Left,
            direction: SwipeDirection::Left,
        }]
    );

    // Still fast: no repeat fire.
    assert_eq!(advance(&mut state, &frame(20_000, vec![h])), vec![]);

    // Slow down below re-arm, then move up at a speed that would fire
    // horizontally but is below the *scaled* vertical threshold
    // (400 × 1.5 = 600): must NOT fire — this is the hand-withdrawal
    // false-positive guard.
    h.palm_velocity = [10.0, 20.0, 0.0];
    assert_eq!(advance(&mut state, &frame(30_000, vec![h])), vec![]);
    h.palm_velocity = [50.0, 450.0, 0.0];
    assert_eq!(advance(&mut state, &frame(40_000, vec![h])), vec![]);

    // A genuinely fast upward stroke fires.
    h.palm_velocity = [50.0, 650.0, 0.0];
    assert_eq!(
        advance(&mut state, &frame(50_000, vec![h])),
        vec![LeapGestureEvent::Swipe {
            hand: LeapChirality::Left,
            direction: SwipeDirection::Up,
        }]
    );
}

#[test]
fn swipe_chain_swallows_return_strokes() {
    let mut state = LeapGestureState::default();
    let mut h = hand(LeapChirality::Right);

    // Arm.
    assert_eq!(advance(&mut state, &frame(0, vec![h])), vec![]);

    // Stroke right: fires.
    h.palm_velocity = [500.0, 0.0, 0.0];
    assert_eq!(
        advance(&mut state, &frame(10_000, vec![h])),
        vec![LeapGestureEvent::Swipe {
            hand: LeapChirality::Right,
            direction: SwipeDirection::Right,
        }]
    );

    // Reversal apex re-arms, then the fast wind-back to the left
    // (within the return window) is swallowed.
    h.palm_velocity = [-20.0, 0.0, 0.0];
    assert_eq!(advance(&mut state, &frame(100_000, vec![h])), vec![]);
    h.palm_velocity = [-500.0, 0.0, 0.0];
    assert_eq!(advance(&mut state, &frame(150_000, vec![h])), vec![]);

    // Second stroke right (same direction): fires. (The slow frame
    // at 250 ms also crosses the presence debounce — the hand has
    // now been visible long enough for its Appear event.)
    h.palm_velocity = [30.0, 0.0, 0.0];
    assert_eq!(
        advance(&mut state, &frame(250_000, vec![h])),
        vec![LeapGestureEvent::Presence {
            hand: LeapChirality::Right,
            event: LeapPresenceEvent::Appear,
        }]
    );
    h.palm_velocity = [500.0, 0.0, 0.0];
    assert_eq!(
        advance(&mut state, &frame(300_000, vec![h])),
        vec![LeapGestureEvent::Swipe {
            hand: LeapChirality::Right,
            direction: SwipeDirection::Right,
        }]
    );

    // A genuine leftward swipe after the window expires fires.
    h.palm_velocity = [-20.0, 0.0, 0.0];
    assert_eq!(advance(&mut state, &frame(750_000, vec![h])), vec![]);
    h.palm_velocity = [-500.0, 0.0, 0.0];
    assert_eq!(
        advance(&mut state, &frame(800_000, vec![h])),
        vec![LeapGestureEvent::Swipe {
            hand: LeapChirality::Right,
            direction: SwipeDirection::Left,
        }]
    );
}

#[test]
fn grab_drag_latches_axis_and_streams_updates() {
    let mut state = LeapGestureState::default();
    let mut h = hand(LeapChirality::Right);

    assert_eq!(advance(&mut state, &frame(0, vec![h])), vec![]);

    // Fist closes: grab held, nothing emitted until the axis latches.
    h.grab_strength = 0.9;
    assert_eq!(advance(&mut state, &frame(10_000, vec![h])), vec![]);

    // During the settle delay, palm drift (the finger-curl artifact)
    // is ignored entirely.
    h.palm_position = [3.0, 212.0, 0.0];
    assert_eq!(advance(&mut state, &frame(30_000, vec![h])), vec![]);

    // Settle elapses: this frame captures the anchor.
    h.palm_position = [4.0, 210.0, 0.0];
    assert_eq!(advance(&mut state, &frame(60_000, vec![h])), vec![]);

    // Clear vertical movement from the anchor: latches Vertical,
    // re-anchored at the latch point.
    h.palm_position = [4.0, 235.0, 0.0];
    assert_eq!(
        advance(&mut state, &frame(70_000, vec![h])),
        vec![LeapGestureEvent::GrabBegin {
            hand: LeapChirality::Right,
            axis: LeapAxis::Vertical,
        }]
    );

    // Further movement streams updates measured from the latch point.
    h.palm_position = [4.0, 255.0, 0.0];
    assert_eq!(
        advance(&mut state, &frame(80_000, vec![h])),
        vec![LeapGestureEvent::GrabUpdate {
            hand: LeapChirality::Right,
            axis: LeapAxis::Vertical,
            delta_mm: 20.0,
            total_mm: 20.0,
        }]
    );
    h.palm_position = [4.0, 245.0, 0.0];
    assert_eq!(
        advance(&mut state, &frame(90_000, vec![h])),
        vec![LeapGestureEvent::GrabUpdate {
            hand: LeapChirality::Right,
            axis: LeapAxis::Vertical,
            delta_mm: -10.0,
            total_mm: 10.0,
        }]
    );

    // Fist opens: drag ends.
    h.grab_strength = 0.2;
    assert_eq!(
        advance(&mut state, &frame(100_000, vec![h])),
        vec![LeapGestureEvent::GrabEnd {
            hand: LeapChirality::Right,
            axis: LeapAxis::Vertical,
        }]
    );
}

#[test]
fn grab_axis_race_favors_horizontal() {
    let mut state = LeapGestureState::default();
    let mut h = hand(LeapChirality::Right);

    assert_eq!(advance(&mut state, &frame(0, vec![h])), vec![]);
    h.grab_strength = 0.9;
    assert_eq!(advance(&mut state, &frame(10_000, vec![h])), vec![]);
    // Anchor capture after settle.
    assert_eq!(advance(&mut state, &frame(60_000, vec![h])), vec![]);

    // Diagonal movement with MORE vertical than horizontal (dy=25 vs
    // dx=20) still latches Horizontal: vertical only wins past
    // dx × 1.5 (= 30). Compensates the vertical drift of fist motion.
    h.palm_position = [20.0, 225.0, 0.0];
    assert_eq!(
        advance(&mut state, &frame(70_000, vec![h])),
        vec![LeapGestureEvent::GrabBegin {
            hand: LeapChirality::Right,
            axis: LeapAxis::Horizontal,
        }]
    );
}

#[test]
fn grab_drag_depth_axis_latches_and_streams_updates() {
    let mut state = LeapGestureState::default();
    let mut h = hand(LeapChirality::Right);

    assert_eq!(advance(&mut state, &frame(0, vec![h])), vec![]);
    h.grab_strength = 0.9;
    assert_eq!(advance(&mut state, &frame(10_000, vec![h])), vec![]);
    // Anchor capture after settle.
    assert_eq!(advance(&mut state, &frame(60_000, vec![h])), vec![]);

    // Pushing the fist away from the user (z grows toward the user, so
    // away = negative z) with a little vertical drift: depth wins the
    // race (dz=20 vs dy/1.5≈10.7 vs dx=5).
    h.palm_position = [5.0, 216.0, -20.0];
    assert_eq!(
        advance(&mut state, &frame(70_000, vec![h])),
        vec![LeapGestureEvent::GrabBegin {
            hand: LeapChirality::Right,
            axis: LeapAxis::Depth,
        }]
    );

    // Updates report positive = pushing away, measured from the latch
    // point.
    h.palm_position = [5.0, 216.0, -50.0];
    assert_eq!(
        advance(&mut state, &frame(80_000, vec![h])),
        vec![LeapGestureEvent::GrabUpdate {
            hand: LeapChirality::Right,
            axis: LeapAxis::Depth,
            delta_mm: 30.0,
            total_mm: 30.0,
        }]
    );
    // Pulling back toward the user reverses sign.
    h.palm_position = [5.0, 216.0, -40.0];
    assert_eq!(
        advance(&mut state, &frame(90_000, vec![h])),
        vec![LeapGestureEvent::GrabUpdate {
            hand: LeapChirality::Right,
            axis: LeapAxis::Depth,
            delta_mm: -10.0,
            total_mm: 20.0,
        }]
    );

    h.grab_strength = 0.2;
    assert_eq!(
        advance(&mut state, &frame(100_000, vec![h])),
        vec![LeapGestureEvent::GrabEnd {
            hand: LeapChirality::Right,
            axis: LeapAxis::Depth,
        }]
    );
}

#[test]
fn grab_suppresses_pinch_and_swipe() {
    let mut state = LeapGestureState::default();
    let mut h = hand(LeapChirality::Right);

    assert_eq!(advance(&mut state, &frame(0, vec![h])), vec![]);

    // A closing fist drives pinch strength and palm speed up too;
    // neither may fire while grabbing.
    h.grab_strength = 0.9;
    h.pinch_strength = 0.95;
    h.palm_velocity = [600.0, 0.0, 0.0];
    assert_eq!(advance(&mut state, &frame(10_000, vec![h])), vec![]);
    assert_eq!(advance(&mut state, &frame(20_000, vec![h])), vec![]);

    // After release (still fast, still pinched) nothing fires until
    // both re-arm conditions are met.
    h.grab_strength = 0.1;
    assert_eq!(advance(&mut state, &frame(30_000, vec![h])), vec![]);
    assert_eq!(advance(&mut state, &frame(40_000, vec![h])), vec![]);
}

#[test]
fn grab_end_when_hand_lost() {
    let mut state = LeapGestureState::default();
    let mut h = hand(LeapChirality::Right);

    assert_eq!(advance(&mut state, &frame(0, vec![h])), vec![]);
    h.grab_strength = 0.9;
    advance(&mut state, &frame(10_000, vec![h]));
    // Anchor capture after settle.
    assert_eq!(advance(&mut state, &frame(60_000, vec![h])), vec![]);
    h.palm_position = [40.0, 200.0, 0.0];
    let events = advance(&mut state, &frame(70_000, vec![h]));
    assert_eq!(
        events,
        vec![LeapGestureEvent::GrabBegin {
            hand: LeapChirality::Right,
            axis: LeapAxis::Horizontal,
        }]
    );

    // Hand flickers out mid-drag: the grab survives the grace period
    // (closed fists routinely drop out of tracking for a frame).
    assert_eq!(advance(&mut state, &frame(80_000, vec![])), vec![]);
    assert_eq!(advance(&mut state, &frame(150_000, vec![])), vec![]);

    // Reappears still fisted and farther along: the anchor is
    // preserved, so the update reports total displacement including
    // the movement during the dropout.
    h.palm_position = [60.0, 200.0, 0.0];
    assert_eq!(
        advance(&mut state, &frame(200_000, vec![h])),
        vec![LeapGestureEvent::GrabUpdate {
            hand: LeapChirality::Right,
            axis: LeapAxis::Horizontal,
            delta_mm: 20.0,
            total_mm: 20.0,
        }]
    );

    // Gone for longer than the grace period: drag ends.
    assert_eq!(advance(&mut state, &frame(250_000, vec![])), vec![]);
    assert_eq!(
        advance(&mut state, &frame(450_000, vec![])),
        vec![LeapGestureEvent::GrabEnd {
            hand: LeapChirality::Right,
            axis: LeapAxis::Horizontal,
        }]
    );
}

#[test]
fn presence_is_debounced() {
    let mut state = LeapGestureState::default();
    let h = hand(LeapChirality::Left);

    // Appears at t=0: no event until it has stayed for the debounce
    // interval.
    assert_eq!(advance(&mut state, &frame(0, vec![h])), vec![]);
    assert_eq!(advance(&mut state, &frame(100_000, vec![h])), vec![]);
    assert_eq!(
        advance(&mut state, &frame(260_000, vec![h])),
        vec![LeapGestureEvent::Presence {
            hand: LeapChirality::Left,
            event: LeapPresenceEvent::Appear,
        }]
    );

    // Brief flicker out and back: no events.
    assert_eq!(advance(&mut state, &frame(300_000, vec![])), vec![]);
    assert_eq!(advance(&mut state, &frame(350_000, vec![h])), vec![]);
    assert_eq!(advance(&mut state, &frame(700_000, vec![h])), vec![]);

    // Sustained absence: Vanish after the debounce interval.
    assert_eq!(advance(&mut state, &frame(800_000, vec![])), vec![]);
    assert_eq!(
        advance(&mut state, &frame(1_100_000, vec![])),
        vec![LeapGestureEvent::Presence {
            hand: LeapChirality::Left,
            event: LeapPresenceEvent::Vanish,
        }]
    );
}

#[test]
fn pose_fires_after_settle_on_still_hand() {
    let mut state = LeapGestureState::default();
    let mut h = hand(LeapChirality::Right);
    h.fingers_extended = extended(3);

    // The count must hold for the settle interval before firing.
    assert_eq!(advance(&mut state, &frame(0, vec![h])), vec![]);
    assert_eq!(advance(&mut state, &frame(100_000, vec![h])), vec![]);
    assert_eq!(
        advance(&mut state, &frame(160_000, vec![h])),
        vec![LeapGestureEvent::Pose {
            hand: LeapChirality::Right,
            fingers: 3,
        }]
    );

    // Continuing to hold must not repeat-fire.
    assert_eq!(advance(&mut state, &frame(200_000, vec![h])), vec![]);
}

#[test]
fn pose_rearms_via_count_change() {
    let mut state = LeapGestureState::default();
    let mut h = hand(LeapChirality::Right);
    h.fingers_extended = extended(3);

    assert_eq!(advance(&mut state, &frame(0, vec![h])), vec![]);
    assert_eq!(
        advance(&mut state, &frame(160_000, vec![h])),
        vec![LeapGestureEvent::Pose {
            hand: LeapChirality::Right,
            fingers: 3,
        }]
    );

    // Extending to 5 restarts the settle clock; the new count fires
    // after its own settle. (The 300 ms frame also crosses the
    // presence debounce.)
    h.fingers_extended = extended(5);
    assert_eq!(advance(&mut state, &frame(200_000, vec![h])), vec![]);
    assert_eq!(
        advance(&mut state, &frame(300_000, vec![h])),
        vec![LeapGestureEvent::Presence {
            hand: LeapChirality::Right,
            event: LeapPresenceEvent::Appear,
        }]
    );
    assert_eq!(
        advance(&mut state, &frame(360_000, vec![h])),
        vec![LeapGestureEvent::Pose {
            hand: LeapChirality::Right,
            fingers: 5,
        }]
    );

    // Back to 3: a different settled count re-arms it, so it fires
    // again.
    h.fingers_extended = extended(3);
    assert_eq!(advance(&mut state, &frame(400_000, vec![h])), vec![]);
    assert_eq!(
        advance(&mut state, &frame(560_000, vec![h])),
        vec![LeapGestureEvent::Pose {
            hand: LeapChirality::Right,
            fingers: 3,
        }]
    );
}

#[test]
fn pose_requires_still_palm() {
    let mut state = LeapGestureState::default();
    let mut h = hand(LeapChirality::Left);
    h.fingers_extended = extended(2);
    // Above pose-max-speed (120) but below the swipe trigger (400):
    // neither a pose nor a swipe.
    h.palm_velocity = [200.0, 0.0, 0.0];

    assert_eq!(advance(&mut state, &frame(0, vec![h])), vec![]);
    assert_eq!(advance(&mut state, &frame(100_000, vec![h])), vec![]);
    assert_eq!(advance(&mut state, &frame(200_000, vec![h])), vec![]);

    // The hand stops: the settle clock starts only now. (The 300 ms
    // frame also crosses the presence debounce.)
    h.palm_velocity = [0.0, 0.0, 0.0];
    assert_eq!(
        advance(&mut state, &frame(300_000, vec![h])),
        vec![LeapGestureEvent::Presence {
            hand: LeapChirality::Left,
            event: LeapPresenceEvent::Appear,
        }]
    );
    assert_eq!(advance(&mut state, &frame(400_000, vec![h])), vec![]);
    assert_eq!(
        advance(&mut state, &frame(460_000, vec![h])),
        vec![LeapGestureEvent::Pose {
            hand: LeapChirality::Left,
            fingers: 2,
        }]
    );
}

#[test]
fn pose_does_not_refire_on_tracking_flicker() {
    let mut state = LeapGestureState::default();
    let mut h = hand(LeapChirality::Right);
    h.fingers_extended = extended(3);

    assert_eq!(advance(&mut state, &frame(0, vec![h])), vec![]);
    assert_eq!(
        advance(&mut state, &frame(160_000, vec![h])),
        vec![LeapGestureEvent::Pose {
            hand: LeapChirality::Right,
            fingers: 3,
        }]
    );

    // Brief dropout and reacquire while the user keeps holding: the
    // settled count survives (it only clears on debounced Vanish),
    // so re-settling the same count must not fire again.
    assert_eq!(advance(&mut state, &frame(200_000, vec![])), vec![]);
    assert_eq!(advance(&mut state, &frame(230_000, vec![h])), vec![]);
    assert_eq!(advance(&mut state, &frame(400_000, vec![h])), vec![]);
}

#[test]
fn pose_refires_after_debounced_vanish() {
    let mut state = LeapGestureState::default();
    let mut h = hand(LeapChirality::Right);
    h.fingers_extended = extended(3);

    assert_eq!(advance(&mut state, &frame(0, vec![h])), vec![]);
    assert_eq!(
        advance(&mut state, &frame(160_000, vec![h])),
        vec![LeapGestureEvent::Pose {
            hand: LeapChirality::Right,
            fingers: 3,
        }]
    );
    assert_eq!(
        advance(&mut state, &frame(300_000, vec![h])),
        vec![LeapGestureEvent::Presence {
            hand: LeapChirality::Right,
            event: LeapPresenceEvent::Appear,
        }]
    );

    // The hand leaves for real (debounced Vanish)...
    assert_eq!(advance(&mut state, &frame(350_000, vec![])), vec![]);
    assert_eq!(
        advance(&mut state, &frame(650_000, vec![])),
        vec![LeapGestureEvent::Presence {
            hand: LeapChirality::Right,
            event: LeapPresenceEvent::Vanish,
        }]
    );

    // ...and re-enters holding the same 3 fingers: fires again.
    assert_eq!(advance(&mut state, &frame(700_000, vec![h])), vec![]);
    assert_eq!(
        advance(&mut state, &frame(900_000, vec![h])),
        vec![LeapGestureEvent::Pose {
            hand: LeapChirality::Right,
            fingers: 3,
        }]
    );
}

#[test]
fn pose_suppressed_while_grabbing() {
    let mut state = LeapGestureState::default();
    let mut h = hand(LeapChirality::Right);
    // Contrived: grab strength high while digits read extended, to
    // isolate the grab gate from the count.
    h.fingers_extended = extended(2);
    h.grab_strength = 0.9;

    assert_eq!(advance(&mut state, &frame(0, vec![h])), vec![]);
    assert_eq!(advance(&mut state, &frame(160_000, vec![h])), vec![]);

    // Fist opens to five extended fingers. The release frame itself
    // still counts as grabbing (the gate clears one frame later), so
    // the settle clock starts at the following frame (300 ms, which
    // also crosses the presence debounce).
    h.fingers_extended = extended(5);
    h.grab_strength = 0.1;
    assert_eq!(advance(&mut state, &frame(200_000, vec![h])), vec![]);
    assert_eq!(
        advance(&mut state, &frame(300_000, vec![h])),
        vec![LeapGestureEvent::Presence {
            hand: LeapChirality::Right,
            event: LeapPresenceEvent::Appear,
        }]
    );
    assert_eq!(advance(&mut state, &frame(400_000, vec![h])), vec![]);
    assert_eq!(
        advance(&mut state, &frame(460_000, vec![h])),
        vec![LeapGestureEvent::Pose {
            hand: LeapChirality::Right,
            fingers: 5,
        }]
    );
}

#[test]
fn pose_suppressed_while_pinching() {
    let mut state = LeapGestureState::default();
    let mut h = hand(LeapChirality::Right);
    // Open palm enters and settles its (harmless) 5-count.
    h.fingers_extended = extended(5);

    assert_eq!(advance(&mut state, &frame(0, vec![h])), vec![]);
    assert_eq!(
        advance(&mut state, &frame(160_000, vec![h])),
        vec![LeapGestureEvent::Pose {
            hand: LeapChirality::Right,
            fingers: 5,
        }]
    );

    // Thumb-to-index pinch: the index curls onto the thumb, so the
    // tracker reads middle/ring/pinky extended — count 3. The pinch
    // fires, and the held pinch must NOT settle a phantom 3-finger
    // pose, no matter how long it's held.
    h.pinch_strength = 0.9;
    h.fingers_extended = extended(3);
    assert_eq!(
        advance(&mut state, &frame(200_000, vec![h])),
        vec![LeapGestureEvent::Pinch {
            hand: LeapChirality::Right
        }]
    );
    assert_eq!(
        advance(&mut state, &frame(300_000, vec![h])),
        vec![LeapGestureEvent::Presence {
            hand: LeapChirality::Right,
            event: LeapPresenceEvent::Appear,
        }]
    );
    assert_eq!(advance(&mut state, &frame(460_000, vec![h])), vec![]);

    // Releasing the pinch opens the hand back to 5, which already
    // settled before the pinch: still nothing fires.
    h.pinch_strength = 0.2;
    h.fingers_extended = extended(5);
    assert_eq!(advance(&mut state, &frame(500_000, vec![h])), vec![]);
    assert_eq!(advance(&mut state, &frame(700_000, vec![h])), vec![]);
}

#[test]
fn hands_tracked_independently() {
    let mut state = LeapGestureState::default();
    let mut left = hand(LeapChirality::Left);
    let mut right = hand(LeapChirality::Right);

    assert_eq!(advance(&mut state, &frame(0, vec![left, right])), vec![]);

    // Left pinches while right grabs; both fire for their own hand.
    left.pinch_strength = 0.9;
    right.grab_strength = 0.9;
    let events = advance(&mut state, &frame(10_000, vec![left, right]));
    assert_eq!(
        events,
        vec![LeapGestureEvent::Pinch {
            hand: LeapChirality::Left
        }]
    );

    // Right hand's grab anchors after the settle delay, then drags.
    assert_eq!(
        advance(&mut state, &frame(70_000, vec![left, right])),
        vec![]
    );
    right.palm_position = [-30.0, 200.0, 0.0];
    let events = advance(&mut state, &frame(80_000, vec![left, right]));
    assert_eq!(
        events,
        vec![LeapGestureEvent::GrabBegin {
            hand: LeapChirality::Right,
            axis: LeapAxis::Horizontal,
        }]
    );
}
