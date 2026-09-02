//! What the twelve keys look like, for one frame.
//!
//! Pure arithmetic and no device: it names keys by their position on the
//! F-row, 0 through 11, and never an LED id. Each surface maps a key index to
//! whatever its keyboard calls that LED and hands the result across its own
//! boundary — so a second keyboard is a second mapping, not a second palette.
//!
//! **The lane's colour is the base for everything**, and a change of colour
//! only ever means trouble: red is Error, and nothing else on the row is
//! allowed to be red. Everything ordinary — idle,
//! working, finished, being asked a question — is the lane's own colour at some
//! brightness and in some motion, which is what lets a glance answer "which
//! lane" and "how bad" as two separate questions.

use crate::settings::{KEYBOARD_LANES, KEYS, KEYS_PER_LANE, Rgb, Settings, Tuning};
use crate::state::State;

/// A lane with no session. The one colour that is not a preference: darkness is
/// how the row says nothing is there.
pub const OFF: Rgb = Rgb::new(0, 0, 0);

/// Error. Fixed, not a setting — red only means anything if nothing else may
/// use it, which is also why lane colours are asked to stay away from it. The
/// deep shade, by the owner's eye on the real keys.
pub const DARK_RED: Rgb = Rgb::new(110, 0, 0);

/// The resting glow: how bright "present, nothing to report" sits.
const BASE: f32 = 0.20;

/// The lane's colour at the resting glow — what "present, nothing to report"
/// looks like, for a surface that needs the shade by itself.
pub fn base(lane_color: Rgb) -> Rgb {
    scale(lane_color, BASE)
}

/// One full circuit of Running's travelling light.
const SCANNER_PERIOD_MS: u64 = 1400;

/// One double-pulse cycle of Waiting: two beats, then quiet. Short enough that
/// the beats read as urgent, which nothing else on the board is allowed to.
const PULSE_PERIOD_MS: u64 = 1200;

/// One low-to-full breathe of a Running agent on a single key — the numpad's
/// M column, where a lane is one key and the scanner has nowhere to travel.
const BREATHE_PERIOD_MS: u64 = 2000;

/// One single-pulse cycle of Done on a single key: a beat, then a long quiet.
/// Slower than Waiting's urgent pair — finished is news, not a question.
const DONE_PERIOD_MS: u64 = 2400;

/// One colour-to-white-and-back fade of the locked agent's key.
const LOCK_PERIOD_MS: u64 = 2400;

/// Where the selected agent's key rests: its envelope is lifted from
/// `BASE..1.0` to this floor, so the selected key reads brighter than its
/// neighbours at a glance while its motion survives. Owner-eye constant.
const SELECTED_FLOOR: f32 = 0.55;

/// Per-channel calibration, last thing before the wire: some keyboards render
/// a colour completely differently from the screen (blue too strong, red too
/// weak), and a gain is the honest model for that — black stays black, dim
/// colours correct in proportion, and it composes with brightness. The window
/// is never corrected; this exists to make the keys match it.
fn corrected(color: Rgb, gain: [f32; 3]) -> Rgb {
    let channel = |value: u8, gain: f32| (f32::from(value) * gain).round().clamp(0.0, 255.0) as u8;
    Rgb::new(
        channel(color.r, gain[0]),
        channel(color.g, gain[1]),
        channel(color.b, gain[2]),
    )
}

fn scale(color: Rgb, factor: f32) -> Rgb {
    let f = factor.clamp(0.0, 1.0);
    let channel = |value: u8| (f32::from(value) * f).round().clamp(0.0, 255.0) as u8;
    Rgb::new(channel(color.r), channel(color.g), channel(color.b))
}

/// Triangle wave in `0.0..=1.0` with the given period.
fn triangle(elapsed_ms: u64, period_ms: u64) -> f32 {
    if period_ms == 0 {
        return 1.0;
    }
    let phase = (elapsed_ms % period_ms) as f32 / period_ms as f32;
    if phase < 0.5 {
        phase * 2.0
    } else {
        (1.0 - phase) * 2.0
    }
}

/// Two beats and a rest, `0.0..=1.0`. Reads as deliberate rather than as a
/// pulse that lost its rhythm.
fn double_pulse(elapsed_ms: u64, period_ms: u64) -> f32 {
    let period = period_ms.max(1);
    let phase = (elapsed_ms % period) as f32 / period as f32;
    if phase < 0.5 {
        triangle((elapsed_ms % period) * 4, period)
    } else {
        0.0
    }
}

/// How lit key `index` of `keys` is with the running light where it is now:
/// 1.0 under it, 0.0 a whole slot away, eased in between.
///
/// The light travels `keys + 1` slots — one per key, plus one more. That extra
/// slot is the whole trick: with one slot per key the light has to jump from
/// the last key straight back to the first, and however smoothly it crossfades,
/// the wrap is always a pop. The extra slot is where it sits while it is off
/// the lane — it leaves past the right-hand key, is nowhere for a moment, and
/// arrives at the left-hand one.
///
/// Eased rather than linear because the smallest steps land at the ends, where
/// something appearing or vanishing is most conspicuous.
fn scanner_weight(elapsed_ms: u64, period_ms: u64, index: usize, keys: usize) -> f32 {
    let slots = keys as f32 + 1.0;
    let period = period_ms.max(1);
    let position = (elapsed_ms % period) as f32 / period as f32 * slots;
    let raw = (position - index as f32).abs();
    let distance = raw.min(slots - raw);
    let t = (1.0 - distance.min(1.0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// The colours of one lane's keys — the owner's table, transcribed.
///
/// For every state that has something to report, the leftmost key holds the
/// lane's colour at full brightness: that is which lane is talking. Connected
/// and Running have nothing to report, so they get no marker — the whole lane
/// is simply its colour, resting or with the runner crossing it.
///
/// In Waiting the keys that beat after the marker are, on the F-row, exactly
/// the three answer keys: the pattern is the affordance, and needs no second
/// one.
pub fn lane_colors(
    state: Option<State>,
    lane_color: Rgb,
    keys: usize,
    elapsed_ms: u64,
) -> Vec<Rgb> {
    let Some(state) = state else {
        return vec![OFF; keys];
    };
    let base = scale(lane_color, BASE);
    let full = lane_color;
    match state {
        State::Connected => vec![base; keys],
        State::Running => (0..keys)
            .map(|index| {
                // The runner rides over the resting glow: never below BASE, and
                // 100% under the light itself.
                let lit = scanner_weight(elapsed_ms, SCANNER_PERIOD_MS, index, keys);
                scale(full, BASE.max(lit))
            })
            .collect(),
        State::Waiting => (0..keys)
            .map(|index| {
                if index == 0 {
                    full
                } else {
                    let beat = double_pulse(elapsed_ms, PULSE_PERIOD_MS);
                    scale(full, BASE + (1.0 - BASE) * beat)
                }
            })
            .collect(),
        State::Done => (0..keys)
            .map(|index| if index == 0 { full } else { base })
            .collect(),
        State::Error => (0..keys)
            .map(|index| if index == 0 { full } else { DARK_RED })
            .collect(),
        // Nothing heard for a while: the lane keeps its seat but stops taking
        // the eye — one dim key says "still here", the rest go dark.
        State::Idle => (0..keys)
            .map(|index| if index == 0 { base } else { OFF })
            .collect(),
    }
}

/// One beat and a rest, `0.0..=1.0` — [`double_pulse`]'s calm sibling.
fn single_pulse(elapsed_ms: u64, period_ms: u64) -> f32 {
    let period = period_ms.max(1);
    let phase = (elapsed_ms % period) as f32 / period as f32;
    if phase < 0.25 {
        triangle((elapsed_ms % period) * 4, period)
    } else {
        0.0
    }
}

/// One agent as one key — the numpad's M column. The owner's table:
/// Connected and Idle rest at the glow; Running breathes low to full; Waiting
/// double-pulses, unless the agent's terminal is the foreground window, in
/// which case the key holds full and the top line does the pulsing; Done
/// beats once a cycle; Error is the fixed red, steady. The selected agent's
/// key carries the same motion lifted onto a brighter floor, which is how the
/// column says which agent the top line is showing.
pub fn m_key(
    state: Option<State>,
    lane_color: Rgb,
    elapsed_ms: u64,
    selected: bool,
    terminal_foreground: bool,
) -> Rgb {
    let Some(state) = state else {
        return OFF;
    };
    let level = |raw: f32| {
        let lifted = if selected {
            SELECTED_FLOOR + (1.0 - SELECTED_FLOOR) * ((raw - BASE) / (1.0 - BASE)).clamp(0.0, 1.0)
        } else {
            raw
        };
        scale(lane_color, lifted)
    };
    let over_glow = |beat: f32| BASE + (1.0 - BASE) * beat;
    match state {
        State::Error => DARK_RED,
        State::Connected | State::Idle => level(BASE),
        State::Running => level(over_glow(triangle(elapsed_ms, BREATHE_PERIOD_MS))),
        State::Waiting if terminal_foreground => level(1.0),
        State::Waiting => level(over_glow(double_pulse(elapsed_ms, PULSE_PERIOD_MS))),
        State::Done => level(over_glow(single_pulse(elapsed_ms, DONE_PERIOD_MS))),
    }
}

/// The locked agent's key: the lane's colour at full, fading to white and
/// back. White is the one shade no lane may own and no state pattern uses, so
/// it can mean exactly one thing — pinned.
pub fn lock_blend(lane_color: Rgb, elapsed_ms: u64) -> Rgb {
    let toward = triangle(elapsed_ms, LOCK_PERIOD_MS);
    let channel = |value: u8| {
        (f32::from(value) + (255.0 - f32::from(value)) * toward)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    Rgb::new(
        channel(lane_color.r),
        channel(lane_color.g),
        channel(lane_color.b),
    )
}

/// A device's own brightness and colour balance, last thing before its wire —
/// what [`frame`] does for the F-row, for a surface that composes its keys
/// itself.
pub fn tune(color: Rgb, tuning: Tuning) -> Rgb {
    corrected(
        scale(color, tuning.brightness.clamp(0.0, 1.0)),
        tuning.color_gain,
    )
}

/// Whether any of these states needs a fresh frame every tick. Everything
/// else is motionless, and a motionless board is repainted only when it
/// changes — the difference between a resting app and one that allocates and
/// writes the SDK thirty times a second to say nothing.
pub fn animated(states: &[Option<State>]) -> bool {
    states.iter().flatten().any(|state| moves(*state))
}

/// Whether this one state moves: Running's light travels and Waiting beats;
/// everything else holds still.
pub fn moves(state: State) -> bool {
    matches!(state, State::Running | State::Waiting)
}

/// One frame: which key gets which colour, by F-row position.
///
/// **Only ever the twelve.** An earlier version of this returned an entry for
/// every LED the device reported and then overwrote twelve of them, which meant
/// it explicitly blacked out the whole keyboard and took the rest of it away
/// from whoever owns it. Naming twelve keys is all it takes to leave every
/// other key to the user's own lighting, whichever keyboard renders it.
///
/// The keyboard carries lanes 1–3 whatever the lane count, four keys each; a
/// lane past them is another surface's to show, so it never names a key here.
///
/// `available` is which of the twelve the device actually has, so a keyboard
/// without a full F-row is never handed keys it does not have; `tuning` is
/// that device's own brightness and colour balance.
pub fn frame(
    states: &[Option<State>],
    settings: &Settings,
    tuning: Tuning,
    elapsed_ms: u64,
    available: &[usize],
) -> Vec<(usize, Rgb)> {
    let brightness = tuning.brightness.clamp(0.0, 1.0);
    let mut frame = Vec::with_capacity(KEYS);
    for lane in 0..settings.lane_count.min(KEYBOARD_LANES) {
        let lane_color = settings
            .lanes
            .get(lane)
            .map(|configured| configured.color)
            .unwrap_or(OFF);
        let state = states.get(lane).copied().flatten();
        for (offset, color) in lane_colors(state, lane_color, KEYS_PER_LANE, elapsed_ms)
            .into_iter()
            .enumerate()
        {
            let key = lane * KEYS_PER_LANE + offset;
            if available.contains(&key) {
                frame.push((key, corrected(scale(color, brightness), tuning.color_gain)));
            }
        }
    }
    frame
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn settings(lane_count: usize) -> Settings {
        let mut settings = Settings::default();
        settings.set_lane_count(lane_count);
        settings
    }

    const FULL: Tuning = Tuning {
        brightness: 1.0,
        color_gain: [1.0, 1.0, 1.0],
    };

    const LANE: Rgb = Rgb::new(80, 170, 255);

    #[test]
    fn only_running_and_waiting_animate() {
        use State::*;
        assert!(animated(&[None, Some(Idle), Some(Running)]));
        assert!(animated(&[Some(Waiting)]));
        for still in [Connected, Done, Error, Idle] {
            assert!(!animated(&[Some(still), None]), "{still:?}");
        }
        assert!(!animated(&[]));
    }

    #[test]
    fn colour_gain_corrects_each_channel_and_clamps() {
        let gain = [0.5, 1.0, 2.0];
        assert_eq!(
            corrected(Rgb::new(100, 100, 100), gain),
            Rgb::new(50, 100, 200)
        );
        assert_eq!(
            corrected(Rgb::new(200, 0, 200), gain),
            Rgb::new(100, 0, 255),
            "clamps at full rather than wrapping"
        );
        assert_eq!(
            corrected(Rgb::new(80, 170, 255), [1.0, 1.0, 1.0]),
            Rgb::new(80, 170, 255),
            "unity gain is identity"
        );
        assert_eq!(
            corrected(OFF, [2.0, 2.0, 2.0]),
            OFF,
            "black stays black under any gain"
        );
    }

    #[test]
    fn idle_is_one_dim_key_and_darkness() {
        let colors = lane_colors(Some(State::Idle), LANE, 4, 0);
        assert_eq!(colors[0], scale(LANE, BASE), "leftmost holds the seat");
        for color in &colors[1..] {
            assert_eq!(*color, OFF, "the rest go dark");
        }
    }

    #[test]
    fn a_frame_never_names_a_key_that_is_not_ours() {
        // The constraint the whole surface exists under: write the twelve and
        // leave the rest of the keyboard to whoever set it up.
        let available: Vec<usize> = (0..200).collect();
        for lanes in crate::settings::LANE_COUNTS {
            let states = vec![Some(State::Running); lanes];
            let frame = frame(&states, &settings(lanes), FULL, 0, &available);
            assert_eq!(frame.len(), KEYS, "{lanes} lanes");
            for (key, _) in frame {
                assert!(key < KEYS, "key {key} is not one of ours");
            }
        }
    }

    #[test]
    fn lanes_past_the_keyboard_have_no_keys() {
        // Six lanes, and only the three past the keyboard have anything on
        // them: the twelve keys stay dark, and nothing is named past them.
        let available: Vec<usize> = (0..200).collect();
        let states = [
            None,
            None,
            None,
            Some(State::Waiting),
            Some(State::Error),
            Some(State::Running),
        ];
        let frame = frame(&states, &settings(6), FULL, 0, &available);
        assert_eq!(frame.len(), KEYS);
        for (key, color) in frame {
            assert!(key < KEYS, "key {key} is not one of ours");
            assert_eq!(color, OFF, "key {key} lit by a lane that has no keys");
        }
    }

    #[test]
    fn a_keyboard_missing_those_keys_is_never_handed_them() {
        let available = [0, 1, 2];
        let frame = frame(
            &[Some(State::Running); 4],
            &settings(4),
            FULL,
            0,
            &available,
        );
        assert_eq!(frame.len(), 3);
    }

    #[test]
    fn an_empty_lane_is_dark() {
        assert!(lane_colors(None, LANE, 4, 500).iter().all(|c| *c == OFF));
    }

    #[test]
    fn connected_is_the_whole_lane_resting_in_its_own_colour() {
        let colors = lane_colors(Some(State::Connected), LANE, 4, 987);
        assert!(colors.iter().all(|c| *c == scale(LANE, BASE)), "{colors:?}");
    }

    #[test]
    fn reporting_states_mark_the_leftmost_key_at_full_and_quiet_states_do_not() {
        for state in [State::Waiting, State::Done, State::Error] {
            let colors = lane_colors(Some(state), LANE, 4, 0);
            assert_eq!(colors[0], LANE, "{state:?} leftmost");
        }
        // Connected and Running have nothing to report, so no marker: at t=0
        // Running's light is at slot 0 itself, so sample it with the light in
        // the off-lane slot — exactly four-fifths through, position 4.0 of 5,
        // a whole slot from key 3 ahead and from key 0 behind.
        let connected = lane_colors(Some(State::Connected), LANE, 4, 0);
        assert_ne!(connected[0], LANE);
        let gap = SCANNER_PERIOD_MS * 4 / 5;
        let running = lane_colors(Some(State::Running), LANE, 4, gap);
        assert_eq!(running[0], scale(LANE, BASE));
    }

    #[test]
    fn error_is_dark_red_whatever_the_lane_colour() {
        for lane_color in [LANE, Rgb::new(250, 190, 60), Rgb::new(90, 210, 130)] {
            let error = lane_colors(Some(State::Error), lane_color, 4, 333);
            assert!(error[1..].iter().all(|c| *c == DARK_RED), "{error:?}");
            // And steady: trouble does not need to move to be seen.
            assert_eq!(error, lane_colors(Some(State::Error), lane_color, 4, 999));
        }
    }

    #[test]
    fn running_keeps_the_whole_lane_glowing_under_one_travelling_light() {
        let floor = scale(LANE, BASE);
        let mut bright_counts = Vec::new();
        for elapsed in (0..SCANNER_PERIOD_MS).step_by(50) {
            let colors = lane_colors(Some(State::Running), LANE, 4, elapsed);
            // Never below the resting glow anywhere.
            for c in &colors {
                assert!(
                    c.r >= floor.r && c.g >= floor.g && c.b >= floor.b,
                    "{colors:?}"
                );
            }
            // The light is a narrow window, not a wash: at most two adjacent
            // keys sit meaningfully above the floor at any instant.
            let bright = colors
                .iter()
                .filter(|c| c.r > floor.r + 20 || c.g > floor.g + 20 || c.b > floor.b + 20)
                .count();
            assert!(bright <= 2, "{bright} keys lit at {elapsed}ms: {colors:?}");
            bright_counts.push(bright);
        }
        // And it does actually travel: sometimes present, sometimes off-lane.
        assert!(bright_counts.iter().any(|n| *n > 0));
        assert!(bright_counts.contains(&0));
    }

    #[test]
    fn waiting_beats_between_the_glow_and_full_and_its_marker_never_moves() {
        let mut seen_high = false;
        let mut seen_low = false;
        for elapsed in (0..PULSE_PERIOD_MS).step_by(25) {
            let colors = lane_colors(Some(State::Waiting), LANE, 4, elapsed);
            assert_eq!(colors[0], LANE, "the marker moved at {elapsed}ms");
            // The beating keys all beat together.
            assert!(colors[1..].windows(2).all(|pair| pair[0] == pair[1]));
            if colors[1] == LANE {
                seen_high = true;
            }
            if colors[1] == scale(LANE, BASE) {
                seen_low = true;
            }
        }
        assert!(seen_high, "the pulse never reached 100%");
        assert!(seen_low, "the pulse never rested at the glow");
    }

    #[test]
    fn brightness_scales_everything_and_never_inverts_it() {
        let settings = settings(4);
        let dim = Tuning {
            brightness: 0.25,
            ..FULL
        };
        let available: Vec<usize> = (0..KEYS).collect();
        let states = vec![Some(State::Done); 4];
        for ((_, full), (_, low)) in frame(&states, &settings, FULL, 0, &available)
            .into_iter()
            .zip(frame(&states, &settings, dim, 0, &available))
        {
            assert!(low.r <= full.r && low.g <= full.g && low.b <= full.b);
        }
    }

    #[test]
    fn a_single_pulse_is_one_beat_and_a_long_rest() {
        let period = DONE_PERIOD_MS;
        assert_eq!(single_pulse(0, period), 0.0);
        assert_eq!(single_pulse(period / 8, period), 1.0, "the beat's peak");
        for elapsed in (period / 4..period).step_by(50) {
            assert_eq!(single_pulse(elapsed, period), 0.0, "resting at {elapsed}ms");
        }
        for elapsed in (0..period).step_by(25) {
            let level = single_pulse(elapsed, period);
            assert!((0.0..=1.0).contains(&level));
        }
    }

    #[test]
    fn an_m_key_follows_the_owners_table() {
        // Empty is dark; Connected and Idle rest at the glow.
        assert_eq!(m_key(None, LANE, 123, false, false), OFF);
        for quiet in [State::Connected, State::Idle] {
            assert_eq!(m_key(Some(quiet), LANE, 123, false, false), base(LANE));
        }
        // Error is the fixed red whatever the lane colour or selection.
        for selected in [false, true] {
            assert_eq!(
                m_key(
                    Some(State::Error),
                    Rgb::new(90, 210, 130),
                    55,
                    selected,
                    false
                ),
                DARK_RED
            );
        }
        // Waiting with the terminal focused holds full — the top line beats.
        assert_eq!(m_key(Some(State::Waiting), LANE, 999, false, true), LANE);
        // Waiting unfocused beats between the glow and full.
        let mut seen_high = false;
        let mut seen_low = false;
        for elapsed in (0..PULSE_PERIOD_MS).step_by(25) {
            let color = m_key(Some(State::Waiting), LANE, elapsed, false, false);
            seen_high |= color == LANE;
            seen_low |= color == base(LANE);
        }
        assert!(seen_high && seen_low, "the pulse spans glow to full");
        // Running breathes: it moves, and never sinks below the glow.
        let floor = base(LANE);
        let samples: Vec<Rgb> = (0..BREATHE_PERIOD_MS)
            .step_by(100)
            .map(|elapsed| m_key(Some(State::Running), LANE, elapsed, false, false))
            .collect();
        assert!(samples.windows(2).any(|pair| pair[0] != pair[1]));
        for color in &samples {
            assert!(color.r >= floor.r && color.g >= floor.g && color.b >= floor.b);
        }
    }

    #[test]
    fn the_selected_key_rests_brighter_and_keeps_its_motion() {
        let resting = m_key(Some(State::Connected), LANE, 0, false, false);
        let selected = m_key(Some(State::Connected), LANE, 0, true, false);
        assert!(
            selected.r > resting.r || selected.g > resting.g || selected.b > resting.b,
            "{selected:?} should out-glow {resting:?}"
        );
        // Full stays full: a selected Waiting-focused key is exactly the colour.
        assert_eq!(m_key(Some(State::Waiting), LANE, 7, true, true), LANE);
        // And a selected Running still visibly breathes.
        let samples: Vec<Rgb> = (0..BREATHE_PERIOD_MS)
            .step_by(100)
            .map(|elapsed| m_key(Some(State::Running), LANE, elapsed, true, false))
            .collect();
        assert!(samples.windows(2).any(|pair| pair[0] != pair[1]));
    }

    #[test]
    fn the_lock_fade_runs_the_lane_colour_to_pure_white_and_back() {
        // A lane with headroom on every channel, so "never darker" is a real
        // check on all three (LANE's blue is already full).
        let lane = Rgb::new(80, 170, 200);
        assert_eq!(lock_blend(lane, 0), lane, "starts as the lane");
        let peak = lock_blend(lane, LOCK_PERIOD_MS / 2);
        assert_eq!(peak, Rgb::new(255, 255, 255), "full white at the peak");
        for elapsed in (0..LOCK_PERIOD_MS).step_by(50) {
            let color = lock_blend(lane, elapsed);
            assert!(
                color.r >= lane.r && color.g >= lane.g && color.b >= lane.b,
                "never darker than the lane: {color:?}"
            );
        }
    }

    #[test]
    fn tune_composes_brightness_and_gain_like_the_f_row_does() {
        let tuning = Tuning {
            brightness: 0.5,
            color_gain: [1.0, 0.5, 2.0],
        };
        assert_eq!(
            tune(Rgb::new(200, 200, 100), tuning),
            Rgb::new(100, 50, 100)
        );
        assert_eq!(tune(OFF, tuning), OFF, "black stays black");
    }
}
