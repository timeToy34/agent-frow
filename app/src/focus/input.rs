//! One answer on key press, independent of the V0's Ctrl+Shift hotkey chord.
//!
//! SendInput preserves the current keyboard state. Release held Ctrl/Shift,
//! send the answer, and restore those same keys in a single ordered batch.
//! Physical input cannot interleave within that batch. The caller must first
//! verify the foreground and terminal focus; this module never acquires focus.

use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, MAPVK_VK_TO_VSC, MapVirtualKeyW, SendInput,
    VIRTUAL_KEY, VK_DOWN, VK_LCONTROL, VK_LSHIFT, VK_RCONTROL, VK_RETURN, VK_RSHIFT, VK_UP,
};

use super::Key;

const MODIFIERS: [VIRTUAL_KEY; 4] = [VK_LCONTROL, VK_RCONTROL, VK_LSHIFT, VK_RSHIFT];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stroke {
    vk: VIRTUAL_KEY,
    up: bool,
}

impl Stroke {
    fn input(self) -> INPUT {
        let mut flags = KEYBD_EVENT_FLAGS(0);
        if matches!(self.vk, VK_UP | VK_DOWN | VK_RCONTROL) {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        if self.up {
            flags |= KEYEVENTF_KEYUP;
        }
        // SAFETY: FFI, pure lookup of a known virtual key's scan code.
        let scan = unsafe { MapVirtualKeyW(u32::from(self.vk.0), MAPVK_VK_TO_VSC) } as u16;
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: self.vk,
                    wScan: scan,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }
}

struct Answer {
    key: VIRTUAL_KEY,
    held: Vec<VIRTUAL_KEY>,
    strokes: Vec<Stroke>,
}

impl Answer {
    fn new(key: Key, down: [bool; 4]) -> Self {
        let key = match key {
            Key::Up => VK_UP,
            Key::Down => VK_DOWN,
            Key::Enter => VK_RETURN,
        };
        let held: Vec<_> = MODIFIERS
            .into_iter()
            .zip(down)
            .filter_map(|(vk, down)| down.then_some(vk))
            .collect();
        let mut strokes = Vec::with_capacity(2 + held.len() * 2);
        strokes.extend(held.iter().rev().map(|&vk| Stroke { vk, up: true }));
        strokes.extend([Stroke { vk: key, up: false }, Stroke { vk: key, up: true }]);
        strokes.extend(held.iter().map(|&vk| Stroke { vk, up: false }));
        Self { key, held, strokes }
    }

    /// Undo only changes in the accepted prefix. In particular, recovery
    /// never presses the answer: Enter might already have confirmed a choice.
    fn cleanup(&self, accepted: usize) -> Vec<Stroke> {
        let prefix = &self.strokes[..accepted.min(self.strokes.len())];
        let last = |vk| prefix.iter().rev().find(|stroke| stroke.vk == vk);
        let mut cleanup = Vec::new();
        if last(self.key).is_some_and(|stroke| !stroke.up) {
            cleanup.push(Stroke {
                vk: self.key,
                up: true,
            });
        }
        for &vk in &self.held {
            if last(vk).is_some_and(|stroke| stroke.up) {
                cleanup.push(Stroke { vk, up: false });
            }
        }
        cleanup
    }

    fn send(&self, mut inject: impl FnMut(&[Stroke]) -> usize) -> Result<(), &'static str> {
        let accepted = inject(&self.strokes);
        if accepted == self.strokes.len() {
            return Ok(());
        }
        if accepted == 0 {
            return Err(
                "Windows refused the keystroke — an elevated terminal cannot be typed into from here",
            );
        }
        let cleanup = self.cleanup(accepted);
        if !cleanup.is_empty() && inject(&cleanup) != cleanup.len() {
            return Err(
                "Windows interrupted the answer and key recovery failed — release Ctrl, Shift and the answer key, then check the terminal",
            );
        }
        Err("Windows sent only part of the answer — check the terminal before pressing again")
    }
}

pub(super) fn send_key(key: Key) -> Result<(), String> {
    // Only the high (currently down) bit is reliable. Query each side so a
    // right modifier is not restored as its left counterpart. Do not touch
    // Alt, Win, lock keys, or modifiers that were already up.
    // SAFETY: FFI reads the state of four known virtual keys; no input is sent.
    let down = MODIFIERS.map(|vk| unsafe { GetAsyncKeyState(i32::from(vk.0)) } < 0);
    Answer::new(key, down)
        .send(|strokes| {
            let inputs: Vec<_> = strokes.iter().map(|stroke| stroke.input()).collect();
            // SAFETY: fully initialized INPUT_KEYBOARD records, correctly
            // sized. One call prevents other input interleaving the answer.
            unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) as usize }
        })
        .map_err(str::to_owned)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// A keyboard state model: assert what modifiers an application sees at
    /// the answer's keydown, and compare the final state with the physical
    /// keys originally held. No test injects input into the actual desktop.
    fn apply(down: &mut BTreeSet<u16>, strokes: &[Stroke], answers: &mut usize) {
        for stroke in strokes {
            if stroke.up {
                down.remove(&stroke.vk.0);
            } else {
                if matches!(stroke.vk, VK_UP | VK_DOWN | VK_RETURN) {
                    assert!(MODIFIERS.iter().all(|vk| !down.contains(&vk.0)));
                    *answers += 1;
                }
                down.insert(stroke.vk.0);
            }
        }
    }

    fn physical(mask: u8) -> ([bool; 4], BTreeSet<u16>) {
        let held = std::array::from_fn(|i| mask & (1 << i) != 0);
        let down = MODIFIERS
            .into_iter()
            .zip(held)
            .filter_map(|(vk, held)| held.then_some(vk.0))
            .collect();
        (held, down)
    }

    #[test]
    fn every_answer_is_plain_and_restores_each_modifier_combination() {
        for key in [Key::Up, Key::Down, Key::Enter] {
            for mask in 0..16 {
                let (held, mut down) = physical(mask);
                // The V0's F22 can remain held throughout the answer.
                down.insert(0x85);
                let original = down.clone();
                let answer = Answer::new(key, held);
                let (mut answers, mut batches) = (0, 0);
                assert!(
                    answer
                        .send(|strokes| {
                            batches += 1;
                            apply(&mut down, strokes, &mut answers);
                            strokes.len()
                        })
                        .is_ok()
                );
                assert_eq!((answers, batches), (1, 1));
                assert_eq!(down, original, "{key:?}, modifiers={mask}");
                if mask == 0 {
                    assert_eq!(answer.strokes.len(), 2);
                }
            }
        }
    }

    #[test]
    fn every_partial_batch_recovers_without_repeating_the_answer() {
        for key in [Key::Up, Key::Down, Key::Enter] {
            for mask in 0..16 {
                let (held, original) = physical(mask);
                let answer = Answer::new(key, held);
                for accepted in 0..answer.strokes.len() {
                    let mut down = original.clone();
                    let (mut answers, mut batches) = (0, 0);
                    assert!(
                        answer
                            .send(|strokes| {
                                batches += 1;
                                let count = if batches == 1 {
                                    accepted
                                } else {
                                    strokes.len()
                                };
                                if batches > 1 {
                                    assert!(strokes.iter().all(|s| s.vk != answer.key || s.up));
                                }
                                apply(&mut down, &strokes[..count], &mut answers);
                                count
                            })
                            .is_err()
                    );
                    assert_eq!(
                        down, original,
                        "{key:?}, modifiers={mask}, accepted={accepted}"
                    );
                    assert!(answers <= 1);
                    assert_eq!(batches, if accepted == 0 { 1 } else { 2 });
                }
            }
        }
    }

    #[test]
    fn failed_recovery_is_reported_without_retrying_the_answer() {
        let answer = Answer::new(Key::Enter, [true; 4]);
        for accepted in 1..answer.strokes.len() {
            let cleanup = answer.cleanup(accepted);
            for recovered in 0..cleanup.len() {
                let mut calls = 0;
                let error = answer
                    .send(|strokes| {
                        calls += 1;
                        if calls == 1 {
                            accepted
                        } else {
                            assert_eq!(strokes, cleanup);
                            recovered
                        }
                    })
                    .unwrap_err();
                assert_eq!(calls, 2);
                assert!(error.contains("key recovery failed"));
            }
        }
    }

    #[test]
    fn native_records_keep_scan_codes_and_extended_key_flags() {
        for (vk, scan, extended) in [
            (VK_UP, 0x48, true),
            (VK_DOWN, 0x50, true),
            (VK_RETURN, 0x1c, false),
            (VK_LCONTROL, 0x1d, false),
            (VK_RCONTROL, 0x1d, true),
            (VK_LSHIFT, 0x2a, false),
            (VK_RSHIFT, 0x36, false),
        ] {
            for up in [false, true] {
                let input = Stroke { vk, up }.input();
                assert_eq!(input.r#type, INPUT_KEYBOARD);
                // SAFETY: Stroke::input initializes exactly the ki member.
                let keyboard = unsafe { input.Anonymous.ki };
                assert_eq!(keyboard.wVk, vk);
                assert_eq!(keyboard.wScan, scan);
                assert_eq!(keyboard.dwFlags.contains(KEYEVENTF_EXTENDEDKEY), extended);
                assert_eq!(keyboard.dwFlags.contains(KEYEVENTF_KEYUP), up);
            }
        }
    }
}
