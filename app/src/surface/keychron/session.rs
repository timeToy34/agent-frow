//! One keyboard, from handshake to handing it back.
//!
//! The app never owns more than the twelve F-row LEDs. It puts the keyboard
//! in its mixed mode — two regions, each running its own effect — gives the
//! F-row to region 1 rendered per key from what the app sends, and leaves
//! region 0, everything else, running whatever the user had. Everything it
//! changes is read first into a [`Snapshot`] and written back on the way out,
//! so the board the user gets back is the board they had.
//!
//! All of it is written against [`Transport`], so the whole sequence — what
//! is asked, in which order, what is restored — is tested against a scripted
//! keyboard rather than discovered on a real one.

use serde_json::{Map, Value};

use super::hid::Transport;
use super::protocol::{
    COLOURS_PER_PACKET, Command, EFFECT_MIXED, EFFECT_OFF, EFFECT_PER_KEY, EffectSlot, Hsv,
    PER_KEY_PROTOCOL, PER_KEY_SOLID, REGIONS_PER_GET, REGIONS_PER_SET, Region, Reply,
    SLOTS_PER_PACKET, f_row,
};
use crate::settings::{KEYS, Rgb};

/// The matrix row the F-row is on, on every Keychron.
const F_ROW: u8 = 0;

/// How long a slot holds when a region rotates effects. Irrelevant with one
/// effect per region, which is all the app ever sets; the firmware's own
/// default, so a restored board reads naturally.
const SLOT_HOLD_MS: u32 = 5000;

/// Where the F-row's LEDs are when the keyboard will not say: the V3 Ultra's
/// layout, and every Ultra seen so far.
const DEFAULT_F_ROW: [u8; KEYS] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];

/// What the keyboard was showing before the app touched it — everything the
/// app changes, so everything it can put back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub effect: u8,
    pub brightness: u8,
    pub speed: u8,
    pub hue: u8,
    pub sat: u8,
    pub per_key_type: u8,
    /// The stored per-key colour of each F-row LED.
    pub colours: Vec<(u8, Hsv)>,
    /// The region of every LED on the board.
    pub regions: Vec<u8>,
    pub ambient: Vec<EffectSlot>,
    pub ours: Vec<EffectSlot>,
}

impl Snapshot {
    pub fn to_json(&self) -> String {
        let slots = |slots: &[EffectSlot]| {
            Value::Array(
                slots
                    .iter()
                    .map(|slot| {
                        Value::Array(vec![
                            Value::from(slot.effect),
                            Value::from(slot.hue),
                            Value::from(slot.sat),
                            Value::from(slot.speed),
                            Value::from(slot.time_ms),
                        ])
                    })
                    .collect(),
            )
        };
        let mut root = Map::new();
        for (key, value) in [
            ("effect", self.effect),
            ("brightness", self.brightness),
            ("speed", self.speed),
            ("hue", self.hue),
            ("sat", self.sat),
            ("per_key_type", self.per_key_type),
        ] {
            root.insert(key.to_owned(), Value::from(value));
        }
        root.insert(
            "colours".to_owned(),
            Value::Array(
                self.colours
                    .iter()
                    .map(|(led, hsv)| {
                        Value::Array(vec![
                            Value::from(*led),
                            Value::from(hsv.h),
                            Value::from(hsv.s),
                            Value::from(hsv.v),
                        ])
                    })
                    .collect(),
            ),
        );
        root.insert(
            "regions".to_owned(),
            Value::Array(self.regions.iter().map(|r| Value::from(*r)).collect()),
        );
        root.insert("ambient".to_owned(), slots(&self.ambient));
        root.insert("ours".to_owned(), slots(&self.ours));
        serde_json::to_string_pretty(&Value::Object(root)).unwrap_or_else(|_| "{}".to_owned())
    }

    /// Strict, unlike the settings file: a snapshot that does not parse is
    /// not something to restore from.
    pub fn parse(text: &str) -> Result<Self, String> {
        let value: Value = serde_json::from_str(text).map_err(|error| format!("{error}"))?;
        let root = value.as_object().ok_or("not an object")?;
        let byte = |key: &str| -> Result<u8, String> {
            root.get(key)
                .and_then(Value::as_u64)
                .and_then(|v| u8::try_from(v).ok())
                .ok_or_else(|| format!("missing {key}"))
        };
        let bytes = |value: &Value| -> Option<Vec<u64>> {
            value.as_array()?.iter().map(Value::as_u64).collect()
        };
        let slots = |key: &str| -> Result<Vec<EffectSlot>, String> {
            root.get(key)
                .and_then(Value::as_array)
                .ok_or_else(|| format!("missing {key}"))?
                .iter()
                .map(|slot| {
                    let fields = bytes(slot).filter(|f| f.len() == 5).ok_or("bad slot")?;
                    Ok(EffectSlot {
                        effect: fields[0] as u8,
                        hue: fields[1] as u8,
                        sat: fields[2] as u8,
                        speed: fields[3] as u8,
                        time_ms: fields[4] as u32,
                    })
                })
                .collect()
        };
        let colours = root
            .get("colours")
            .and_then(Value::as_array)
            .ok_or("missing colours")?
            .iter()
            .map(|entry| {
                let fields = bytes(entry).filter(|f| f.len() == 4).ok_or("bad colour")?;
                Ok((
                    fields[0] as u8,
                    Hsv {
                        h: fields[1] as u8,
                        s: fields[2] as u8,
                        v: fields[3] as u8,
                    },
                ))
            })
            .collect::<Result<Vec<_>, String>>()?;
        let regions = root
            .get("regions")
            .and_then(bytes)
            .ok_or("missing regions")?
            .into_iter()
            .map(|r| r as u8)
            .collect();
        Ok(Self {
            effect: byte("effect")?,
            brightness: byte("brightness")?,
            speed: byte("speed")?,
            hue: byte("hue")?,
            sat: byte("sat")?,
            per_key_type: byte("per_key_type")?,
            colours,
            regions,
            ambient: slots("ambient")?,
            ours: slots("ours")?,
        })
    }
}

/// A keyboard the app is talking to.
pub struct Board<T: Transport> {
    transport: T,
    pub firmware: String,
    pub led_count: u8,
    /// The LED index of each F-row key, by key position.
    pub leds: Vec<u8>,
    /// What each key was last sent, so a frame writes only what changed.
    painted: Vec<Option<Hsv>>,
}

impl<T: Transport> Board<T> {
    /// The handshake: the protocol the app speaks, how many LEDs there are,
    /// and where the F-row's are — asked, not assumed, so another Ultra with
    /// a different matrix reads its own row.
    pub fn connect(transport: T) -> Result<Self, String> {
        let mut board = Self {
            transport,
            firmware: String::new(),
            led_count: 0,
            leds: Vec::new(),
            painted: Vec::new(),
        };
        match board.ask(Command::ProtocolVersion)? {
            Reply::Version(PER_KEY_PROTOCOL) => {}
            Reply::Version(other) => {
                return Err(format!("per-key protocol {other}, and this app speaks 1"));
            }
            _ => return Err("no protocol version".to_owned()),
        }
        board.led_count = board.byte(Command::LedCount)?;
        board.firmware = match board.ask(Command::FirmwareVersion)? {
            Reply::Text(text) => text,
            _ => String::new(),
        };
        board.leds = match board.ask(Command::RowMap(F_ROW)) {
            Ok(Reply::RowMap(map)) if f_row(&map).len() == KEYS => f_row(&map),
            _ => DEFAULT_F_ROW.to_vec(),
        };
        board.painted = vec![None; board.leds.len()];
        Ok(board)
    }

    fn ask(&mut self, command: Command) -> Result<Reply, String> {
        let report = command.encode()?;
        let reply = self.transport.exchange(&report)?;
        command.decode(&reply)
    }

    fn byte(&mut self, command: Command) -> Result<u8, String> {
        match self.ask(command)? {
            Reply::Byte(value) => Ok(value),
            other => Err(format!("unexpected answer {other:?}")),
        }
    }

    fn tell(&mut self, command: Command) -> Result<(), String> {
        self.ask(command).map(|_| ())
    }

    /// Which of the twelve keys this keyboard has, by position — all of them,
    /// on every Ultra, but the palette asks.
    pub fn available(&self) -> Vec<usize> {
        (0..self.leds.len()).collect()
    }

    /// Reads everything the app is about to change.
    pub fn snapshot(&mut self) -> Result<Snapshot, String> {
        let effect = self.byte(Command::GetEffect)?;
        let brightness = self.byte(Command::GetBrightness)?;
        let speed = self.byte(Command::GetSpeed)?;
        let (hue, sat) = match self.ask(Command::GetColour)? {
            Reply::Colour { hue, sat } => (hue, sat),
            other => return Err(format!("unexpected answer {other:?}")),
        };
        let per_key_type = self.byte(Command::GetPerKeyType)?;
        let mut colours = Vec::with_capacity(self.leds.len());
        for run in runs(&self.leds, COLOURS_PER_PACKET) {
            let (start, count) = (run[0], run.len() as u8);
            match self.ask(Command::GetColours { start, count })? {
                Reply::Colours(read) => colours.extend(run.iter().copied().zip(read)),
                other => return Err(format!("unexpected answer {other:?}")),
            }
        }
        let mut regions = Vec::with_capacity(usize::from(self.led_count));
        let mut start = 0u8;
        while usize::from(start) < usize::from(self.led_count) {
            let count =
                (usize::from(self.led_count) - usize::from(start)).min(REGIONS_PER_GET) as u8;
            match self.ask(Command::GetRegions { start, count })? {
                Reply::Regions(read) => regions.extend(read),
                other => return Err(format!("unexpected answer {other:?}")),
            }
            start += count;
        }
        let ambient = self.effect_list(Region::AMBIENT)?;
        let ours = self.effect_list(Region::OURS)?;
        Ok(Snapshot {
            effect,
            brightness,
            speed,
            hue,
            sat,
            per_key_type,
            colours,
            regions,
            ambient,
            ours,
        })
    }

    fn effect_list(&mut self, region: Region) -> Result<Vec<EffectSlot>, String> {
        match self.ask(Command::GetEffectList(region))? {
            Reply::EffectList(slots) => Ok(slots),
            other => Err(format!("unexpected answer {other:?}")),
        }
    }

    /// Whether the keyboard is already set up the way [`Self::take_over`]
    /// leaves it — the case after the keyboard re-enumerated, when reading a
    /// "snapshot" would capture the app's own work and later restore that.
    pub fn is_ours(&mut self) -> Result<bool, String> {
        if self.byte(Command::GetEffect)? != EFFECT_MIXED {
            return Ok(false);
        }
        let ours = self.effect_list(Region::OURS)?;
        if ours.first().map(|slot| slot.effect) != Some(EFFECT_PER_KEY) {
            return Ok(false);
        }
        for run in runs(&self.leds, REGIONS_PER_GET) {
            let (start, count) = (run[0], run.len() as u8);
            match self.ask(Command::GetRegions { start, count })? {
                Reply::Regions(read) => {
                    if read.iter().any(|region| *region != Region::OURS.id()) {
                        return Ok(false);
                    }
                }
                other => return Err(format!("unexpected answer {other:?}")),
            }
        }
        Ok(true)
    }

    /// Puts the keyboard in mixed mode with the F-row as the app's region and
    /// the rest of the board running what the user had — or nothing, if they
    /// had the lighting off. The effect is switched last: that is what makes
    /// the rest take.
    pub fn take_over(&mut self, remembered: &Snapshot) -> Result<(), String> {
        let ambient = match remembered.effect {
            // Their own mixed setup: region 0 keeps its list.
            EFFECT_MIXED => remembered.ambient.clone(),
            EFFECT_OFF => vec![EffectSlot::NONE],
            effect => vec![EffectSlot {
                effect,
                hue: remembered.hue,
                sat: remembered.sat,
                speed: remembered.speed,
                time_ms: SLOT_HOLD_MS,
            }],
        };
        let ours = vec![EffectSlot {
            effect: EFFECT_PER_KEY,
            hue: 0,
            sat: 255,
            speed: 128,
            time_ms: SLOT_HOLD_MS,
        }];
        let regions: Vec<Region> = (0..self.led_count)
            .map(|led| {
                if self.leds.contains(&led) {
                    Region::OURS
                } else {
                    Region::AMBIENT
                }
            })
            .collect();
        self.set_regions(&regions)?;
        self.set_effect_list(Region::AMBIENT, &ambient)?;
        self.set_effect_list(Region::OURS, &ours)?;
        self.tell(Command::SetPerKeyType(PER_KEY_SOLID))?;
        self.tell(Command::SetEffect(EFFECT_MIXED))?;
        self.painted.iter_mut().for_each(|key| *key = None);
        Ok(())
    }

    fn set_regions(&mut self, regions: &[Region]) -> Result<(), String> {
        for (chunk, start) in regions
            .chunks(REGIONS_PER_SET)
            .zip((0..).step_by(REGIONS_PER_SET))
        {
            self.tell(Command::SetRegions {
                start: start as u8,
                regions: chunk.to_vec(),
            })?;
        }
        Ok(())
    }

    fn set_effect_list(&mut self, region: Region, slots: &[EffectSlot]) -> Result<(), String> {
        // Always the full three the packet carries, so a shorter list clears
        // the slots after it rather than leaving an old effect to rotate in.
        let mut padded = slots.to_vec();
        padded.truncate(SLOTS_PER_PACKET);
        padded.resize(SLOTS_PER_PACKET, EffectSlot::NONE);
        self.tell(Command::SetEffectList {
            region,
            slots: padded,
        })
    }

    /// One frame, by key position. Sends only the keys whose colour changed,
    /// in as few packets as their LEDs allow, so a still board costs nothing
    /// and a moving light is usually one packet.
    pub fn paint(&mut self, colours: &[(usize, Rgb)]) -> Result<(), String> {
        let mut changed: Vec<(u8, Hsv)> = Vec::new();
        for (key, rgb) in colours {
            let Some(led) = self.leds.get(*key).copied() else {
                continue;
            };
            let hsv = Hsv::from(*rgb);
            if self.painted[*key] != Some(hsv) {
                changed.push((led, hsv));
            }
        }
        changed.sort_by_key(|(led, _)| *led);
        let leds: Vec<u8> = changed.iter().map(|(led, _)| *led).collect();
        for run in runs(&leds, COLOURS_PER_PACKET) {
            let start = run[0];
            let batch: Vec<Hsv> = changed
                .iter()
                .filter(|(led, _)| run.contains(led))
                .map(|(_, hsv)| *hsv)
                .collect();
            self.tell(Command::SetColours {
                start,
                colours: batch,
            })?;
        }
        for (led, hsv) in changed {
            if let Some(key) = self.leds.iter().position(|known| *known == led) {
                self.painted[key] = Some(hsv);
            }
        }
        Ok(())
    }

    /// Puts back everything [`Self::snapshot`] read, the effect last so the
    /// board re-renders once, from its restored state.
    pub fn restore(&mut self, snapshot: &Snapshot) -> Result<(), String> {
        let leds: Vec<u8> = snapshot.colours.iter().map(|(led, _)| *led).collect();
        for run in runs(&leds, COLOURS_PER_PACKET) {
            let batch: Vec<Hsv> = snapshot
                .colours
                .iter()
                .filter(|(led, _)| run.contains(led))
                .map(|(_, hsv)| *hsv)
                .collect();
            self.tell(Command::SetColours {
                start: run[0],
                colours: batch,
            })?;
        }
        self.tell(Command::SetPerKeyType(snapshot.per_key_type))?;
        let regions: Vec<Region> = snapshot
            .regions
            .iter()
            .map(|id| Region::from_id(*id).unwrap_or(Region::AMBIENT))
            .collect();
        self.set_regions(&regions)?;
        self.set_effect_list(Region::AMBIENT, &snapshot.ambient)?;
        self.set_effect_list(Region::OURS, &snapshot.ours)?;
        self.tell(Command::SetSpeed(snapshot.speed))?;
        self.tell(Command::SetColour {
            hue: snapshot.hue,
            sat: snapshot.sat,
        })?;
        self.tell(Command::SetBrightness(snapshot.brightness))?;
        self.tell(Command::SetEffect(snapshot.effect))?;
        self.painted.iter_mut().for_each(|key| *key = None);
        Ok(())
    }
}

/// Splits sorted LED indices into runs of consecutive ones, each at most
/// `max` long — the shape one packet can carry.
fn runs(leds: &[u8], max: usize) -> Vec<Vec<u8>> {
    let mut runs: Vec<Vec<u8>> = Vec::new();
    for led in leds {
        match runs.last_mut() {
            Some(run) if run.len() < max && run.last().is_some_and(|last| *last + 1 == *led) => {
                run.push(*led);
            }
            _ => runs.push(vec![*led]),
        }
    }
    runs
}

/// A keyboard in software: the firmware's state machine for every command the
/// app sends, faithful to the byte layouts in [`super::protocol`]. What the
/// session logic is tested against.
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
pub mod fake {
    use super::super::protocol::{REPORT_LEN, Report};
    use super::*;

    pub const LEDS: usize = 87;

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct State {
        pub effect: u8,
        pub brightness: u8,
        pub speed: u8,
        pub hue: u8,
        pub sat: u8,
        pub per_key_type: u8,
        pub per_key: Vec<Hsv>,
        pub regions: Vec<u8>,
        pub lists: [[EffectSlot; 5]; 2],
    }

    pub struct ScriptedKeyboard {
        pub state: State,
        /// Every report received, for asserting how many a frame cost.
        pub packets: usize,
        pub sets: usize,
    }

    impl Default for ScriptedKeyboard {
        fn default() -> Self {
            Self::new()
        }
    }

    impl ScriptedKeyboard {
        /// Fresh from the factory, lighting off — as the owner's arrived.
        pub fn new() -> Self {
            let mut lists = [[EffectSlot::NONE; 5]; 2];
            lists[0][0] = EffectSlot {
                effect: 5,
                hue: 0,
                sat: 255,
                speed: 127,
                time_ms: 5000,
            };
            lists[1][0] = EffectSlot {
                effect: 2,
                hue: 0,
                sat: 255,
                speed: 127,
                time_ms: 5000,
            };
            Self {
                state: State {
                    effect: 0,
                    brightness: 255,
                    speed: 127,
                    hue: 0,
                    sat: 255,
                    per_key_type: 0,
                    per_key: vec![
                        Hsv {
                            h: 43,
                            s: 255,
                            v: 255
                        };
                        LEDS
                    ],
                    regions: vec![0; LEDS],
                    lists,
                },
                packets: 0,
                sets: 0,
            }
        }

        fn row_map(row: u8) -> Vec<u8> {
            if row == 0 {
                vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 0xFF, 13, 14, 15]
            } else {
                vec![0xFF; 17]
            }
        }
    }

    impl Transport for ScriptedKeyboard {
        fn exchange(&mut self, report: &Report) -> Result<Report, String> {
            self.packets += 1;
            let mut reply = *report;
            let s = &mut self.state;
            match report[0] {
                0x07 => {
                    self.sets += 1;
                    match report[2] {
                        0x01 => s.brightness = report[3],
                        0x02 => s.effect = report[3],
                        0x03 => s.speed = report[3],
                        0x04 => {
                            s.hue = report[3];
                            s.sat = report[4];
                        }
                        _ => {}
                    }
                }
                0x08 => match report[2] {
                    0x01 => reply[3] = s.brightness,
                    0x02 => reply[3] = s.effect,
                    0x03 => reply[3] = s.speed,
                    0x04 => {
                        reply[3] = s.hue;
                        reply[4] = s.sat;
                    }
                    _ => {}
                },
                0xA1 => {
                    let text = b"v1.0.3-test";
                    reply[1..1 + text.len()].copy_from_slice(text);
                }
                0xA8 => {
                    let mut ok = true;
                    match report[1] {
                        0x01 => reply[3..5].copy_from_slice(&1u16.to_le_bytes()),
                        0x05 => reply[3] = LEDS as u8,
                        0x06 => {
                            let map = Self::row_map(report[2]);
                            reply[3..3 + map.len()].copy_from_slice(&map);
                        }
                        0x07 => reply[3] = s.per_key_type,
                        0x08 => {
                            self.sets += 1;
                            s.per_key_type = report[2];
                        }
                        0x09 => {
                            let (start, count) = (report[2] as usize, report[3] as usize);
                            ok = count <= 9 && start + count <= LEDS;
                            if ok {
                                for i in 0..count {
                                    let hsv = s.per_key[start + i];
                                    reply[3 + 3 * i..6 + 3 * i]
                                        .copy_from_slice(&[hsv.h, hsv.s, hsv.v]);
                                }
                            }
                        }
                        0x0A => {
                            self.sets += 1;
                            let (start, count) = (report[2] as usize, report[3] as usize);
                            ok = count <= 9 && start + count <= LEDS;
                            if ok {
                                for i in 0..count {
                                    s.per_key[start + i] = Hsv {
                                        h: report[4 + 3 * i],
                                        s: report[5 + 3 * i],
                                        v: report[6 + 3 * i],
                                    };
                                }
                            }
                        }
                        0x0C => {
                            let (start, count) = (report[2] as usize, report[3] as usize);
                            ok = count <= 29 && start + count <= LEDS;
                            if ok {
                                reply[3..3 + count]
                                    .copy_from_slice(&s.regions[start..start + count]);
                            }
                        }
                        0x0D => {
                            self.sets += 1;
                            let (start, count) = (report[2] as usize, report[3] as usize);
                            ok = count <= 28
                                && start + count <= LEDS
                                && report[4..4 + count].iter().all(|r| *r < 2);
                            if ok {
                                s.regions[start..start + count]
                                    .copy_from_slice(&report[4..4 + count]);
                            }
                        }
                        0x0E => {
                            let (region, start, count) =
                                (report[2] as usize, report[3] as usize, report[4] as usize);
                            ok = count <= 3 && region < 2 && start + count <= 5;
                            if ok {
                                for i in 0..count {
                                    let slot = s.lists[region][start + i];
                                    let at = 3 + 8 * i;
                                    reply[at..at + 4].copy_from_slice(&[
                                        slot.effect,
                                        slot.hue,
                                        slot.sat,
                                        slot.speed,
                                    ]);
                                    reply[at + 4..at + 8]
                                        .copy_from_slice(&slot.time_ms.to_le_bytes());
                                }
                            }
                        }
                        0x0F => {
                            self.sets += 1;
                            let (region, start, count) =
                                (report[2] as usize, report[3] as usize, report[4] as usize);
                            ok = count <= 3 && region < 2 && start + count <= 5;
                            if ok {
                                for i in 0..count {
                                    let at = 5 + 8 * i;
                                    s.lists[region][start + i] = EffectSlot {
                                        effect: report[at],
                                        hue: report[at + 1],
                                        sat: report[at + 2],
                                        speed: report[at + 3],
                                        time_ms: u32::from_le_bytes([
                                            report[at + 4],
                                            report[at + 5],
                                            report[at + 6],
                                            report[at + 7],
                                        ]),
                                    };
                                }
                            }
                        }
                        _ => {
                            reply[0] = 0xFF;
                            ok = false;
                        }
                    }
                    reply[2] = u8::from(!ok);
                }
                _ => {}
            }
            let _ = REPORT_LEN;
            Ok(reply)
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::fake::ScriptedKeyboard;
    use super::*;

    fn board() -> Board<ScriptedKeyboard> {
        Board::connect(ScriptedKeyboard::new()).unwrap()
    }

    #[test]
    fn the_handshake_learns_the_board_rather_than_assuming_it() {
        let board = board();
        assert_eq!(board.led_count, 87);
        assert_eq!(board.leds, (1..=12).collect::<Vec<u8>>());
        assert_eq!(board.firmware, "v1.0.3-test");
        assert_eq!(board.available(), (0..12).collect::<Vec<usize>>());
    }

    #[test]
    fn the_board_comes_back_exactly_as_it_was_found() {
        let mut board = board();
        let before = board.transport.state.clone();
        let snapshot = board.snapshot().unwrap();
        board.take_over(&snapshot).unwrap();
        assert_ne!(board.transport.state, before, "something was changed");
        assert_eq!(board.transport.state.effect, EFFECT_MIXED);
        board
            .paint(&[(0, Rgb::new(255, 0, 0)), (11, Rgb::new(0, 0, 255))])
            .unwrap();
        board.restore(&snapshot).unwrap();
        assert_eq!(board.transport.state, before);
    }

    #[test]
    fn taking_over_owns_the_f_row_and_leaves_the_rest_to_the_user() {
        let mut board = board();
        board.transport.state.effect = 7;
        board.transport.state.hue = 40;
        let snapshot = board.snapshot().unwrap();
        board.take_over(&snapshot).unwrap();
        let state = &board.transport.state;
        for led in 0..fake::LEDS as u8 {
            let expected = u8::from((1..=12).contains(&led));
            assert_eq!(state.regions[led as usize], expected, "led {led}");
        }
        assert_eq!(state.lists[1][0].effect, EFFECT_PER_KEY);
        assert_eq!(
            state.lists[0][0].effect, 7,
            "the user's effect keeps running"
        );
        assert_eq!(state.lists[0][0].hue, 40);
        assert_eq!(
            state.lists[0][1],
            EffectSlot::NONE,
            "and rotates with nothing"
        );
        assert_eq!(state.per_key_type, PER_KEY_SOLID);
    }

    #[test]
    fn a_board_with_the_lighting_off_gets_an_empty_ambient_region() {
        let mut board = board();
        let snapshot = board.snapshot().unwrap();
        assert_eq!(snapshot.effect, EFFECT_OFF);
        board.take_over(&snapshot).unwrap();
        assert_eq!(board.transport.state.lists[0][0], EffectSlot::NONE);
        board.restore(&snapshot).unwrap();
        assert_eq!(
            board.transport.state.effect, EFFECT_OFF,
            "and goes back off"
        );
    }

    #[test]
    fn a_reenumerated_board_is_recognised_as_already_ours() {
        let mut board = board();
        assert!(!board.is_ours().unwrap());
        let snapshot = board.snapshot().unwrap();
        board.take_over(&snapshot).unwrap();
        assert!(board.is_ours().unwrap());
        // The same keyboard on a fresh handshake — after a cable wiggle.
        let state = board.transport.state.clone();
        let mut again = ScriptedKeyboard::new();
        again.state = state;
        let mut board = Board::connect(again).unwrap();
        assert!(board.is_ours().unwrap());
    }

    #[test]
    fn a_frame_costs_only_what_changed() {
        let mut board = board();
        let snapshot = board.snapshot().unwrap();
        board.take_over(&snapshot).unwrap();
        let full: Vec<(usize, Rgb)> = (0..12).map(|key| (key, Rgb::new(10, 20, 30))).collect();
        board.transport.packets = 0;
        board.paint(&full).unwrap();
        assert_eq!(board.transport.packets, 2, "twelve LEDs, nine per packet");
        board.transport.packets = 0;
        board.paint(&full).unwrap();
        assert_eq!(board.transport.packets, 0, "nothing changed");
        let mut one = full.clone();
        one[5].1 = Rgb::new(200, 0, 0);
        board.transport.packets = 0;
        board.paint(&one).unwrap();
        assert_eq!(board.transport.packets, 1, "one key, one packet");
        assert_eq!(
            board.transport.state.per_key[6],
            Hsv::from(Rgb::new(200, 0, 0))
        );
    }

    #[test]
    fn a_snapshot_survives_the_file() {
        let mut board = board();
        board.transport.state.regions[3] = 1;
        let snapshot = board.snapshot().unwrap();
        assert_eq!(Snapshot::parse(&snapshot.to_json()).unwrap(), snapshot);
        assert!(Snapshot::parse("{}").is_err());
        assert!(Snapshot::parse("nope").is_err());
    }

    #[test]
    fn runs_follow_the_packet_shape() {
        assert_eq!(runs(&[1, 2, 3, 5, 6], 9), vec![vec![1, 2, 3], vec![5, 6]]);
        assert_eq!(
            runs(&(1..=12).collect::<Vec<u8>>(), 9),
            vec![(1..=9).collect::<Vec<u8>>(), vec![10, 11, 12]]
        );
        assert!(runs(&[], 9).is_empty());
    }
}
