//! The seam between the surface and a Stream Deck.
//!
//! [`Deck`] is a trait so the surface's logic — which keys to write, when to
//! flush, what a press is — runs against a recording deck in tests. The one
//! real implementation is the `elgato-streamdeck` crate over hidapi on
//! Windows; elsewhere there is nothing to open, said in a sentence.
//!
//! The deck is taken whole. Unlike a keyboard there is no "our keys and the
//! user's": the Elgato app repaints every key from its own profile and reads
//! every press, so the two cannot share, and the surface only opens the deck
//! while that app is not running.

use std::time::Duration;

use super::canvas::Canvas;

/// Elgato's USB vendor id, the one thing every model has in common.
pub const VENDOR_ID: u16 = 0x0FD9;

/// The Elgato app's process name, as the process list spells it.
pub const ELGATO_APP: &str = "StreamDeck.exe";

/// A deck on the bus, as much as can be known without opening it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub product_id: u16,
    pub serial: String,
    /// The model, as the driver names it.
    pub model: String,
    pub keys: usize,
    pub rows: usize,
    pub cols: usize,
    /// One key's image, in pixels.
    pub size: (usize, usize),
}

impl Found {
    /// One line for the window and `doctor`.
    pub fn describe(&self) -> String {
        format!(
            "{} (serial {}): {} keys, {}×{}, {}×{} px each",
            self.model, self.serial, self.keys, self.rows, self.cols, self.size.0, self.size.1
        )
    }
}

/// A Stream Deck, from the surface's side.
pub trait Deck {
    fn keys(&self) -> usize;
    fn key_size(&self) -> (usize, usize);
    /// The deck's own backlight, 0–100.
    fn set_brightness(&mut self, percent: u8) -> Result<(), String>;
    /// Queues one key's picture; nothing reaches the deck until [`flush`].
    ///
    /// [`flush`]: Self::flush
    fn paint(&mut self, key: u8, canvas: &Canvas) -> Result<(), String>;
    fn flush(&mut self) -> Result<(), String>;
    /// Back to the logo — what a deck shows when nothing is driving it.
    fn reset(&mut self) -> Result<(), String>;
    /// Waits up to `timeout` for the buttons to change. `Some` is every
    /// button's state, pressed or not; `None` is the timeout; `Err` is a
    /// deck that is no longer there.
    fn poll(&mut self, timeout: Duration) -> Result<Option<Vec<bool>>, String>;
}

/// Every deck with a screen currently on the bus, in a stable order.
pub fn find() -> Result<Vec<Found>, String> {
    platform::find()
}

pub fn open(found: &Found) -> Result<Box<dyn Deck>, String> {
    platform::open(found)
}

/// Whether the Elgato app is running — in which case the deck is its.
pub fn elgato_app_running() -> bool {
    platform::elgato_app_running()
}

#[cfg(windows)]
mod platform {
    use std::time::Duration;

    use elgato_streamdeck::info::Kind;
    use elgato_streamdeck::{StreamDeck, StreamDeckInput, list_devices, new_hidapi};
    use image::{DynamicImage, RgbImage};
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };

    use super::super::canvas::Canvas;
    use super::{Deck, ELGATO_APP, Found, VENDOR_ID};

    struct Device {
        deck: StreamDeck,
        keys: usize,
        size: (usize, usize),
    }

    impl Deck for Device {
        fn keys(&self) -> usize {
            self.keys
        }

        fn key_size(&self) -> (usize, usize) {
            self.size
        }

        fn set_brightness(&mut self, percent: u8) -> Result<(), String> {
            self.deck
                .set_brightness(percent)
                .map_err(|error| format!("brightness: {error}"))
        }

        fn paint(&mut self, key: u8, canvas: &Canvas) -> Result<(), String> {
            let image = RgbImage::from_raw(
                canvas.width as u32,
                canvas.height as u32,
                canvas.rgb.clone(),
            )
            .ok_or_else(|| "a canvas that is not its own size".to_owned())?;
            self.deck
                .set_button_image(key, DynamicImage::ImageRgb8(image))
                .map_err(|error| format!("key {key}: {error}"))
        }

        fn flush(&mut self) -> Result<(), String> {
            self.deck.flush().map_err(|error| format!("write: {error}"))
        }

        fn reset(&mut self) -> Result<(), String> {
            self.deck.reset().map_err(|error| format!("reset: {error}"))
        }

        fn poll(&mut self, timeout: Duration) -> Result<Option<Vec<bool>>, String> {
            match self.deck.read_input(Some(timeout)) {
                Ok(StreamDeckInput::ButtonStateChange(buttons)) => Ok(Some(buttons)),
                // Dials, touch strips, or nothing: not a key.
                Ok(_) => Ok(None),
                Err(error) => Err(format!("read: {error}")),
            }
        }
    }

    fn model(kind: Kind) -> String {
        format!("Stream Deck {kind:?}")
    }

    pub fn find() -> Result<Vec<Found>, String> {
        let api = new_hidapi().map_err(|error| format!("HID: {error}"))?;
        let mut found: Vec<Found> = list_devices(&api)
            .into_iter()
            .filter(|(kind, _)| kind.is_visual())
            .map(|(kind, serial)| {
                let (rows, cols) = kind.key_layout();
                Found {
                    product_id: kind.product_id(),
                    serial,
                    model: model(kind),
                    keys: usize::from(kind.key_count()),
                    rows: usize::from(rows),
                    cols: usize::from(cols),
                    size: kind.key_image_format().size,
                }
            })
            .collect();
        // The driver hands them over from a set; the same bus should read
        // the same way twice.
        found.sort_by(|a, b| a.serial.cmp(&b.serial));
        Ok(found)
    }

    pub fn open(found: &Found) -> Result<Box<dyn Deck>, String> {
        let api = new_hidapi().map_err(|error| format!("HID: {error}"))?;
        let kind = Kind::from_vid_pid(VENDOR_ID, found.product_id)
            .ok_or_else(|| format!("{}: not a model the driver knows", found.model))?;
        let deck = StreamDeck::connect(&api, kind, &found.serial)
            .map_err(|error| format!("{}: {error}", found.model))?;
        Ok(Box::new(Device {
            deck,
            keys: found.keys,
            size: found.size,
        }))
    }

    pub fn elgato_app_running() -> bool {
        // SAFETY: FFI; the handle is closed below on every path.
        let Ok(snapshot) = (unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }) else {
            return false;
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut running = false;
        // SAFETY: `entry` is a live, correctly sized structure.
        let mut ok = unsafe { Process32FirstW(snapshot, &mut entry) }.is_ok();
        while ok && !running {
            let len = entry
                .szExeFile
                .iter()
                .position(|&unit| unit == 0)
                .unwrap_or(entry.szExeFile.len());
            let name = String::from_utf16_lossy(&entry.szExeFile[..len]);
            running = name.eq_ignore_ascii_case(ELGATO_APP);
            // SAFETY: as above.
            ok = unsafe { Process32NextW(snapshot, &mut entry) }.is_ok();
        }
        // SAFETY: the handle came from CreateToolhelp32Snapshot and is done with.
        let _ = unsafe { CloseHandle(snapshot) };
        running
    }
}

#[cfg(not(windows))]
mod platform {
    use super::{Deck, Found};

    pub fn find() -> Result<Vec<Found>, String> {
        Err("the deck is only driven on Windows".to_owned())
    }

    pub fn open(_found: &Found) -> Result<Box<dyn Deck>, String> {
        Err("the deck is only driven on Windows".to_owned())
    }

    pub fn elgato_app_running() -> bool {
        false
    }
}

/// A deck that remembers what it was told, for the surface's tests.
#[cfg(test)]
pub mod fake {
    use std::collections::VecDeque;
    use std::time::Duration;

    use super::super::canvas::Canvas;
    use super::Deck;

    pub struct Recorder {
        pub keys: usize,
        pub size: (usize, usize),
        /// Every `paint`, in order.
        pub writes: Vec<(u8, Canvas)>,
        pub flushes: usize,
        pub brightness: Option<u8>,
        pub resets: usize,
        /// What each `poll` answers, front first; empty means the timeout.
        pub presses: VecDeque<Option<Vec<bool>>>,
    }

    impl Recorder {
        pub fn new(keys: usize) -> Self {
            Self {
                keys,
                size: (72, 72),
                writes: Vec::new(),
                flushes: 0,
                brightness: None,
                resets: 0,
                presses: VecDeque::new(),
            }
        }
    }

    impl Deck for Recorder {
        fn keys(&self) -> usize {
            self.keys
        }

        fn key_size(&self) -> (usize, usize) {
            self.size
        }

        fn set_brightness(&mut self, percent: u8) -> Result<(), String> {
            self.brightness = Some(percent);
            Ok(())
        }

        fn paint(&mut self, key: u8, canvas: &Canvas) -> Result<(), String> {
            self.writes.push((key, canvas.clone()));
            Ok(())
        }

        fn flush(&mut self) -> Result<(), String> {
            self.flushes += 1;
            Ok(())
        }

        fn reset(&mut self) -> Result<(), String> {
            self.resets += 1;
            Ok(())
        }

        fn poll(&mut self, _timeout: Duration) -> Result<Option<Vec<bool>>, String> {
            Ok(self.presses.pop_front().flatten())
        }
    }

    #[cfg(test)]
    #[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    mod tests {
        use super::super::super::canvas;
        use super::*;

        #[test]
        fn the_fake_records_every_write_and_flush() {
            let mut deck = Recorder::new(15);
            deck.paint(3, &canvas::blank((72, 72))).unwrap();
            deck.paint(4, &canvas::blank((72, 72))).unwrap();
            deck.flush().unwrap();
            deck.set_brightness(80).unwrap();
            deck.reset().unwrap();
            assert_eq!(deck.writes.len(), 2);
            assert_eq!(deck.writes[1].0, 4);
            assert_eq!(deck.flushes, 1);
            assert_eq!(deck.brightness, Some(80));
            assert_eq!(deck.resets, 1);
            deck.presses.push_back(Some(vec![true; 15]));
            assert_eq!(
                deck.poll(Duration::from_millis(1)).unwrap(),
                Some(vec![true; 15])
            );
            assert_eq!(deck.poll(Duration::from_millis(1)).unwrap(), None);
        }
    }
}
