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

use crate::settings::{KEYS, Rgb, Settings};
use crate::state::State;

/// A lane with no session. The one colour that is not a preference: darkness is
/// how the row says nothing is there.
const OFF: Rgb = Rgb::new(0, 0, 0);

/// Error. Fixed, not a setting — red only means anything if nothing else may
/// use it, which is also why lane colours are asked to stay away from it. The
/// deep shade, by the owner's eye on the real keys.
const DARK_RED: Rgb = Rgb::new(110, 0, 0);

/// The resting glow: how bright "present, nothing to report" sits.
const BASE: f32 = 0.20;

/// One full circuit of Running's travelling light.
const SCANNER_PERIOD_MS: u64 = 1400;

/// One double-pulse cycle of Waiting: two beats, then quiet. Short enough that
/// the beats read as urgent, which nothing else on the board is allowed to.
const PULSE_PERIOD_MS: u64 = 1200;

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

/// Whether any of these states needs a fresh frame every tick. Everything
/// else is motionless, and a motionless board is repainted only when it
/// changes — the difference between a resting app and one that allocates and
/// writes the SDK thirty times a second to say nothing.
pub fn animated(states: &[Option<State>]) -> bool {
    states
        .iter()
        .flatten()
        .any(|state| matches!(state, State::Running | State::Waiting))
}

/// One frame: which key gets which colour, by F-row position.
///
/// **Only ever the twelve.** An earlier version of this returned an entry for
/// every LED the device reported and then overwrote twelve of them, which meant
/// it explicitly blacked out the whole keyboard and took the rest of it away
/// from whoever owns it. Naming twelve keys is all it takes to leave every
/// other key to the user's own lighting, whichever keyboard renders it.
///
/// `available` is which of the twelve the device actually has, so a keyboard
/// without a full F-row is never handed keys it does not have.
pub fn frame(
    states: &[Option<State>],
    settings: &Settings,
    elapsed_ms: u64,
    available: &[usize],
) -> Vec<(usize, Rgb)> {
    let keys = settings.keys_per_lane();
    let brightness = settings.brightness.clamp(0.0, 1.0);
    let mut frame = Vec::with_capacity(KEYS);
    for lane in 0..settings.lane_count {
        let lane_color = settings
            .lanes
            .get(lane)
            .map(|configured| configured.color)
            .unwrap_or(OFF);
        let state = states.get(lane).copied().flatten();
        for (offset, color) in lane_colors(state, lane_color, keys, elapsed_ms)
            .into_iter()
            .enumerate()
        {
            let key = lane * keys + offset;
            if available.contains(&key) {
                frame.push((
                    key,
                    corrected(scale(color, brightness), settings.color_gain),
                ));
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
        settings.brightness = 1.0;
        settings
    }

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
        for lanes in [3, 4, 6] {
            let states = vec![Some(State::Running); lanes];
            let frame = frame(&states, &settings(lanes), 0, &available);
            assert_eq!(frame.len(), KEYS, "{lanes} lanes");
            for (key, _) in frame {
                assert!(key < KEYS, "key {key} is not one of ours");
            }
        }
    }

    #[test]
    fn a_keyboard_missing_those_keys_is_never_handed_them() {
        let available = [0, 1, 2];
        let frame = frame(&[Some(State::Running); 4], &settings(4), 0, &available);
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
        let mut bright = settings(4);
        bright.brightness = 1.0;
        let mut dim = bright.clone();
        dim.brightness = 0.25;
        let available: Vec<usize> = (0..KEYS).collect();
        let states = vec![Some(State::Done); 4];
        for ((_, full), (_, low)) in frame(&states, &bright, 0, &available)
            .into_iter()
            .zip(frame(&states, &dim, 0, &available))
        {
            assert!(low.r <= full.r && low.g <= full.g && low.b <= full.b);
        }
    }
}
