//! The packets: what the keyboard is asked, byte for byte, and what it says
//! back.
//!
//! Keychron's Launcher protocol is VIA's raw-HID framing — 32-byte reports on
//! usage page `0xFF60`, usage `0x61`, report id 0 — with Keychron's own
//! command groups on top. Everything in here is pure so it can be tested
//! without a keyboard plugged in; [`super::hid`] moves the bytes.
//!
//! **Nothing here can persist anything.** VIA's `custom_save` (`07 03 09`)
//! and Keychron's per-key save (`A8 02`) write the keyboard's settings to
//! flash, and this module has no way to spell either: every change the app
//! makes lives in the keyboard's RAM, and a power cycle is always a complete
//! undo. A test holds that line.

use crate::settings::Rgb;

pub const VENDOR_ID: u16 = 0x3434;
pub const USAGE_PAGE: u16 = 0xFF60;
pub const USAGE: u16 = 0x61;

pub const REPORT_LEN: usize = 32;
pub type Report = [u8; REPORT_LEN];

/// RGB effect numbers the app cares about; 1–22 are the keyboard's own
/// animations, which the app never runs itself.
pub const EFFECT_OFF: u8 = 0;
pub const EFFECT_PER_KEY: u8 = 23;
pub const EFFECT_MIXED: u8 = 24;

/// Per-key rendering types; only solid is ever asked for.
pub const PER_KEY_SOLID: u8 = 0;

/// The firmware's per-packet limits. Refused, not truncated, when exceeded.
pub const COLOURS_PER_PACKET: usize = 9;
pub const REGIONS_PER_SET: usize = 28;
pub const REGIONS_PER_GET: usize = 29;
pub const SLOTS_PER_PACKET: usize = 3;

/// The firmware's protocol version for the `A8` group, as `A8 01` reports it.
pub const PER_KEY_PROTOCOL: u16 = 1;

const VIA_SET_VALUE: u8 = 0x07;
const VIA_GET_VALUE: u8 = 0x08;
const VIA_CHANNEL_RGB_MATRIX: u8 = 0x03;
const VALUE_BRIGHTNESS: u8 = 0x01;
const VALUE_EFFECT: u8 = 0x02;
const VALUE_SPEED: u8 = 0x03;
const VALUE_COLOUR: u8 = 0x04;

const KC_FIRMWARE_VERSION: u8 = 0xA1;
const KC_RGB: u8 = 0xA8;
const RGB_PROTOCOL: u8 = 0x01;
const RGB_LED_COUNT: u8 = 0x05;
const RGB_ROW_MAP: u8 = 0x06;
const RGB_GET_TYPE: u8 = 0x07;
const RGB_SET_TYPE: u8 = 0x08;
const RGB_GET_COLOURS: u8 = 0x09;
const RGB_SET_COLOURS: u8 = 0x0A;
const RGB_GET_REGIONS: u8 = 0x0C;
const RGB_SET_REGIONS: u8 = 0x0D;
const RGB_GET_EFFECTS: u8 = 0x0E;
const RGB_SET_EFFECTS: u8 = 0x0F;

/// How many matrix columns a row map can carry: the packet has room for
/// 29 after the header, and no Keychron matrix is that wide.
const ROW_MAP_COLUMNS: usize = REPORT_LEN - 3;

/// A mixed-mode region. The firmware has exactly two, and its bounds check
/// lets a third through into memory it does not own — so this type cannot
/// name one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region(u8);

impl Region {
    /// Region 0: the rest of the keyboard, running whatever the user chose.
    pub const AMBIENT: Self = Self(0);
    /// Region 1: the F-row, rendered per key from what the app sends.
    pub const OURS: Self = Self(1);

    pub const fn id(self) -> u8 {
        self.0
    }

    pub fn from_id(id: u8) -> Option<Self> {
        match id {
            0 => Some(Self::AMBIENT),
            1 => Some(Self::OURS),
            _ => None,
        }
    }
}

/// A colour as the keyboard stores it: hue around a 0–255 wheel, then
/// saturation and value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Hsv {
    pub h: u8,
    pub s: u8,
    pub v: u8,
}

impl From<Rgb> for Hsv {
    /// The inverse of the firmware's `hsv_to_rgb`, whose sextants are
    /// `h * 6 / 255`: full red is hue 0, green 85, blue 170. Black is value 0,
    /// which the fixed firmware renders as black; on stock firmware, which
    /// ignores value, this is the difference between off and white.
    fn from(rgb: Rgb) -> Self {
        let (r, g, b) = (f32::from(rgb.r), f32::from(rgb.g), f32::from(rgb.b));
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        let delta = max - min;
        let v = max;
        let s = if max == 0.0 { 0.0 } else { delta / max * 255.0 };
        let h = if delta == 0.0 {
            0.0
        } else {
            let sextant = if max == r {
                ((g - b) / delta).rem_euclid(6.0)
            } else if max == g {
                (b - r) / delta + 2.0
            } else {
                (r - g) / delta + 4.0
            };
            sextant / 6.0 * 255.0
        };
        let byte = |value: f32| value.round().clamp(0.0, 255.0) as u8;
        Self {
            h: byte(h),
            s: byte(s),
            v: byte(v),
        }
    }
}

/// One entry of a region's effect list: the effect and how it is coloured,
/// paced and, when several rotate, how long it holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EffectSlot {
    pub effect: u8,
    pub hue: u8,
    pub sat: u8,
    pub speed: u8,
    pub time_ms: u32,
}

impl EffectSlot {
    /// An empty slot: the firmware skips effect 0.
    pub const NONE: Self = Self {
        effect: 0,
        hue: 0,
        sat: 0,
        speed: 0,
        time_ms: 0,
    };
}

/// Everything the app ever asks the keyboard. Note what is absent: any
/// command that writes flash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    GetBrightness,
    GetEffect,
    GetSpeed,
    GetColour,
    SetBrightness(u8),
    SetEffect(u8),
    SetSpeed(u8),
    SetColour {
        hue: u8,
        sat: u8,
    },
    FirmwareVersion,
    ProtocolVersion,
    LedCount,
    /// The LED index under each column of one matrix row.
    RowMap(u8),
    GetPerKeyType,
    SetPerKeyType(u8),
    GetColours {
        start: u8,
        count: u8,
    },
    SetColours {
        start: u8,
        colours: Vec<Hsv>,
    },
    GetRegions {
        start: u8,
        count: u8,
    },
    SetRegions {
        start: u8,
        regions: Vec<Region>,
    },
    GetEffectList(Region),
    SetEffectList {
        region: Region,
        slots: Vec<EffectSlot>,
    },
}

/// What came back, already checked against what was asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    Byte(u8),
    Colour {
        hue: u8,
        sat: u8,
    },
    Text(String),
    Version(u16),
    /// Per column; `None` where the matrix has no LED.
    RowMap(Vec<Option<u8>>),
    Colours(Vec<Hsv>),
    Regions(Vec<u8>),
    EffectList(Vec<EffectSlot>),
    Done,
}

impl Command {
    /// The 32 bytes to send. `Err` when the firmware would refuse it — over
    /// a per-packet limit — so a refusal is never discovered on the wire.
    pub fn encode(&self) -> Result<Report, String> {
        let mut report = [0u8; REPORT_LEN];
        let via = |report: &mut Report, cmd: u8, value: u8, data: &[u8]| {
            report[0] = cmd;
            report[1] = VIA_CHANNEL_RGB_MATRIX;
            report[2] = value;
            report[3..3 + data.len()].copy_from_slice(data);
        };
        let rgb = |report: &mut Report, sub: u8, data: &[u8]| {
            report[0] = KC_RGB;
            report[1] = sub;
            report[2..2 + data.len()].copy_from_slice(data);
        };
        match self {
            Self::GetBrightness => via(&mut report, VIA_GET_VALUE, VALUE_BRIGHTNESS, &[]),
            Self::GetEffect => via(&mut report, VIA_GET_VALUE, VALUE_EFFECT, &[]),
            Self::GetSpeed => via(&mut report, VIA_GET_VALUE, VALUE_SPEED, &[]),
            Self::GetColour => via(&mut report, VIA_GET_VALUE, VALUE_COLOUR, &[]),
            Self::SetBrightness(v) => via(&mut report, VIA_SET_VALUE, VALUE_BRIGHTNESS, &[*v]),
            Self::SetEffect(v) => via(&mut report, VIA_SET_VALUE, VALUE_EFFECT, &[*v]),
            Self::SetSpeed(v) => via(&mut report, VIA_SET_VALUE, VALUE_SPEED, &[*v]),
            Self::SetColour { hue, sat } => {
                via(&mut report, VIA_SET_VALUE, VALUE_COLOUR, &[*hue, *sat]);
            }
            Self::FirmwareVersion => report[0] = KC_FIRMWARE_VERSION,
            Self::ProtocolVersion => rgb(&mut report, RGB_PROTOCOL, &[]),
            Self::LedCount => rgb(&mut report, RGB_LED_COUNT, &[]),
            // Every column: a 24-bit mask, little-endian, all set.
            Self::RowMap(row) => rgb(&mut report, RGB_ROW_MAP, &[*row, 0xFF, 0xFF, 0xFF]),
            Self::GetPerKeyType => rgb(&mut report, RGB_GET_TYPE, &[]),
            Self::SetPerKeyType(kind) => rgb(&mut report, RGB_SET_TYPE, &[*kind]),
            Self::GetColours { start, count } => {
                limit(usize::from(*count), COLOURS_PER_PACKET, "colours")?;
                rgb(&mut report, RGB_GET_COLOURS, &[*start, *count]);
            }
            Self::SetColours { start, colours } => {
                limit(colours.len(), COLOURS_PER_PACKET, "colours")?;
                let mut data = vec![*start, colours.len() as u8];
                for colour in colours {
                    data.extend_from_slice(&[colour.h, colour.s, colour.v]);
                }
                rgb(&mut report, RGB_SET_COLOURS, &data);
            }
            Self::GetRegions { start, count } => {
                limit(usize::from(*count), REGIONS_PER_GET, "regions")?;
                rgb(&mut report, RGB_GET_REGIONS, &[*start, *count]);
            }
            Self::SetRegions { start, regions } => {
                limit(regions.len(), REGIONS_PER_SET, "regions")?;
                let mut data = vec![*start, regions.len() as u8];
                data.extend(regions.iter().map(|region| region.id()));
                rgb(&mut report, RGB_SET_REGIONS, &data);
            }
            Self::GetEffectList(region) => {
                rgb(
                    &mut report,
                    RGB_GET_EFFECTS,
                    &[region.id(), 0, SLOTS_PER_PACKET as u8],
                );
            }
            Self::SetEffectList { region, slots } => {
                limit(slots.len(), SLOTS_PER_PACKET, "effect slots")?;
                let mut data = vec![region.id(), 0, slots.len() as u8];
                for slot in slots {
                    data.extend_from_slice(&[slot.effect, slot.hue, slot.sat, slot.speed]);
                    data.extend_from_slice(&slot.time_ms.to_le_bytes());
                }
                rgb(&mut report, RGB_SET_EFFECTS, &data);
            }
        }
        Ok(report)
    }

    /// Reads the keyboard's answer to this command. Every command is echoed
    /// back with its data filled in; the `A8` group also carries a status
    /// byte, and a refusal there is an error here.
    pub fn decode(&self, reply: &Report) -> Result<Reply, String> {
        let sent = self.encode()?;
        if reply[0] != sent[0] {
            return Err(format!(
                "the keyboard answered {:02X} to {:02X}",
                reply[0], sent[0]
            ));
        }
        if sent[0] == KC_RGB {
            if reply[1] != sent[1] {
                return Err(format!(
                    "the keyboard answered A8 {:02X} to A8 {:02X}",
                    reply[1], sent[1]
                ));
            }
            if reply[2] != 0 {
                return Err(format!("the keyboard refused A8 {:02X}", sent[1]));
            }
        }
        let byte = |at: usize| reply[at];
        Ok(match self {
            Self::GetBrightness | Self::GetEffect | Self::GetSpeed => Reply::Byte(byte(3)),
            Self::GetColour => Reply::Colour {
                hue: byte(3),
                sat: byte(4),
            },
            Self::SetBrightness(_)
            | Self::SetEffect(_)
            | Self::SetSpeed(_)
            | Self::SetColour { .. }
            | Self::SetPerKeyType(_)
            | Self::SetColours { .. }
            | Self::SetRegions { .. }
            | Self::SetEffectList { .. } => Reply::Done,
            Self::FirmwareVersion => {
                let text = reply[1..].split(|byte| *byte == 0).next().unwrap_or(&[]);
                Reply::Text(String::from_utf8_lossy(text).into_owned())
            }
            Self::ProtocolVersion => Reply::Version(u16::from_le_bytes([byte(3), byte(4)])),
            Self::LedCount | Self::GetPerKeyType => Reply::Byte(byte(3)),
            Self::RowMap(_) => Reply::RowMap(
                reply[3..3 + ROW_MAP_COLUMNS]
                    .iter()
                    .map(|led| (*led != 0xFF).then_some(*led))
                    .collect(),
            ),
            Self::GetColours { count, .. } => Reply::Colours(
                (0..usize::from(*count))
                    .map(|i| Hsv {
                        h: byte(3 + 3 * i),
                        s: byte(4 + 3 * i),
                        v: byte(5 + 3 * i),
                    })
                    .collect(),
            ),
            Self::GetRegions { count, .. } => {
                Reply::Regions(reply[3..3 + usize::from(*count)].to_vec())
            }
            Self::GetEffectList(_) => Reply::EffectList(
                (0..SLOTS_PER_PACKET)
                    .map(|i| {
                        let at = 3 + 8 * i;
                        EffectSlot {
                            effect: byte(at),
                            hue: byte(at + 1),
                            sat: byte(at + 2),
                            speed: byte(at + 3),
                            time_ms: u32::from_le_bytes([
                                byte(at + 4),
                                byte(at + 5),
                                byte(at + 6),
                                byte(at + 7),
                            ]),
                        }
                    })
                    .collect(),
            ),
        })
    }
}

fn limit(count: usize, max: usize, what: &str) -> Result<(), String> {
    if count > max {
        return Err(format!(
            "{count} {what} in one packet; the keyboard takes {max}"
        ));
    }
    Ok(())
}

/// The F-row from a row map: every LED after the first (Escape) until twelve
/// are found. Columns with no LED are skipped, so a matrix with a gap in the
/// top row — the V3 Ultra has one before Print Screen — still reads straight.
pub fn f_row(map: &[Option<u8>]) -> Vec<u8> {
    map.iter()
        .flatten()
        .skip(1)
        .take(crate::settings::KEYS)
        .copied()
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn bytes(command: &Command) -> Vec<u8> {
        let report = command.encode().unwrap();
        // Trailing zeros are padding; compare what the command wrote.
        let end = report.iter().rposition(|b| *b != 0).map_or(0, |i| i + 1);
        report[..end].to_vec()
    }

    #[test]
    fn packets_match_the_firmware_byte_for_byte() {
        assert_eq!(bytes(&Command::GetEffect), [0x08, 0x03, 0x02]);
        assert_eq!(bytes(&Command::SetEffect(24)), [0x07, 0x03, 0x02, 24]);
        assert_eq!(
            bytes(&Command::SetColour { hue: 5, sat: 9 }),
            [0x07, 0x03, 0x04, 5, 9]
        );
        assert_eq!(bytes(&Command::FirmwareVersion), [0xA1]);
        assert_eq!(bytes(&Command::LedCount), [0xA8, 0x05]);
        assert_eq!(
            bytes(&Command::RowMap(0)),
            [0xA8, 0x06, 0, 0xFF, 0xFF, 0xFF]
        );
        assert_eq!(bytes(&Command::SetPerKeyType(0)), [0xA8, 0x08]);
        assert_eq!(
            bytes(&Command::SetColours {
                start: 1,
                colours: vec![Hsv { h: 1, s: 2, v: 3 }, Hsv { h: 4, s: 5, v: 6 }],
            }),
            [0xA8, 0x0A, 1, 2, 1, 2, 3, 4, 5, 6]
        );
        assert_eq!(
            bytes(&Command::SetRegions {
                start: 1,
                regions: vec![Region::OURS, Region::AMBIENT, Region::OURS],
            }),
            [0xA8, 0x0D, 1, 3, 1, 0, 1]
        );
        assert_eq!(
            bytes(&Command::SetEffectList {
                region: Region::OURS,
                slots: vec![EffectSlot {
                    effect: 23,
                    hue: 0,
                    sat: 255,
                    speed: 128,
                    time_ms: 5000,
                }],
            }),
            [0xA8, 0x0F, 1, 0, 1, 23, 0, 255, 128, 0x88, 0x13]
        );
        assert_eq!(
            bytes(&Command::GetEffectList(Region::AMBIENT)),
            [0xA8, 0x0E, 0, 0, 3]
        );
    }

    #[test]
    fn the_firmware_limits_are_refused_before_the_wire() {
        assert!(
            Command::SetColours {
                start: 0,
                colours: vec![Hsv::default(); 10]
            }
            .encode()
            .is_err()
        );
        assert!(
            Command::SetRegions {
                start: 0,
                regions: vec![Region::AMBIENT; 29]
            }
            .encode()
            .is_err()
        );
        assert!(
            Command::GetRegions {
                start: 0,
                count: 30
            }
            .encode()
            .is_err()
        );
        assert!(
            Command::SetEffectList {
                region: Region::OURS,
                slots: vec![EffectSlot::NONE; 4]
            }
            .encode()
            .is_err()
        );
    }

    #[test]
    fn nothing_here_can_write_flash() {
        // VIA's custom_save and Keychron's per-key save. There is no variant
        // for either, and this pins it: every set command is RAM only.
        let every = [
            Command::GetBrightness,
            Command::GetEffect,
            Command::GetSpeed,
            Command::GetColour,
            Command::SetBrightness(1),
            Command::SetEffect(1),
            Command::SetSpeed(1),
            Command::SetColour { hue: 1, sat: 1 },
            Command::FirmwareVersion,
            Command::ProtocolVersion,
            Command::LedCount,
            Command::RowMap(0),
            Command::GetPerKeyType,
            Command::SetPerKeyType(0),
            Command::GetColours { start: 0, count: 1 },
            Command::SetColours {
                start: 0,
                colours: vec![Hsv::default()],
            },
            Command::GetRegions { start: 0, count: 1 },
            Command::SetRegions {
                start: 0,
                regions: vec![Region::OURS],
            },
            Command::GetEffectList(Region::OURS),
            Command::SetEffectList {
                region: Region::OURS,
                slots: vec![],
            },
        ];
        for command in every {
            let report = command.encode().unwrap();
            assert_ne!(
                &report[..3],
                &[0x07, 0x03, 0x09],
                "{command:?} is custom_save"
            );
            assert_ne!(&report[..2], &[0xA8, 0x02], "{command:?} is per-key save");
        }
    }

    #[test]
    fn a_region_can_only_be_one_of_the_two() {
        assert_eq!(Region::from_id(0), Some(Region::AMBIENT));
        assert_eq!(Region::from_id(1), Some(Region::OURS));
        assert_eq!(Region::from_id(2), None, "the firmware's out-of-bounds bug");
    }

    #[test]
    fn replies_are_checked_against_the_question() {
        let mut reply = [0u8; REPORT_LEN];
        reply[..4].copy_from_slice(&[0xA8, 0x05, 0x00, 87]);
        assert_eq!(Command::LedCount.decode(&reply).unwrap(), Reply::Byte(87));
        reply[2] = 1;
        assert!(Command::LedCount.decode(&reply).is_err(), "refused");
        reply[2] = 0;
        assert!(
            Command::ProtocolVersion.decode(&reply).is_err(),
            "wrong sub"
        );
        reply[0] = 0x08;
        assert!(Command::LedCount.decode(&reply).is_err(), "wrong command");

        // The firmware string overwrites byte 1, so only byte 0 must echo.
        let mut reply = [0u8; REPORT_LEN];
        reply[0] = 0xA1;
        reply[1..7].copy_from_slice(b"v1.0.3");
        assert_eq!(
            Command::FirmwareVersion.decode(&reply).unwrap(),
            Reply::Text("v1.0.3".to_owned())
        );

        let mut reply = [0u8; REPORT_LEN];
        reply[..5].copy_from_slice(&[0x08, 0x03, 0x04, 170, 200]);
        assert_eq!(
            Command::GetColour.decode(&reply).unwrap(),
            Reply::Colour { hue: 170, sat: 200 }
        );
    }

    #[test]
    fn the_row_map_reads_as_the_v3_ultra_reports_it() {
        let mut reply = [0u8; REPORT_LEN];
        reply[..3].copy_from_slice(&[0xA8, 0x06, 0]);
        let row0 = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 0xFF, 13, 14, 15];
        reply[3..3 + row0.len()].copy_from_slice(&row0);
        let Reply::RowMap(map) = Command::RowMap(0).decode(&reply).unwrap() else {
            panic!()
        };
        assert_eq!(map[13], None, "the gap before Print Screen");
        assert_eq!(f_row(&map), (1..=12).collect::<Vec<u8>>());
    }

    #[test]
    fn effect_lists_decode_in_eight_byte_records() {
        let mut reply = [0u8; REPORT_LEN];
        reply[..3].copy_from_slice(&[0xA8, 0x0E, 0]);
        reply[3..11].copy_from_slice(&[5, 0, 255, 127, 0x88, 0x13, 0, 0]);
        let Reply::EffectList(slots) = Command::GetEffectList(Region::AMBIENT)
            .decode(&reply)
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(slots.len(), 3);
        assert_eq!(
            slots[0],
            EffectSlot {
                effect: 5,
                hue: 0,
                sat: 255,
                speed: 127,
                time_ms: 5000
            }
        );
        assert_eq!(slots[1], EffectSlot::NONE);
    }

    #[test]
    fn hsv_matches_the_firmware_wheel() {
        assert_eq!(
            Hsv::from(Rgb::new(255, 0, 0)),
            Hsv {
                h: 0,
                s: 255,
                v: 255
            }
        );
        assert_eq!(
            Hsv::from(Rgb::new(0, 255, 0)),
            Hsv {
                h: 85,
                s: 255,
                v: 255
            }
        );
        assert_eq!(
            Hsv::from(Rgb::new(0, 0, 255)),
            Hsv {
                h: 170,
                s: 255,
                v: 255
            }
        );
        assert_eq!(
            Hsv::from(Rgb::new(0, 0, 0)),
            Hsv { h: 0, s: 0, v: 0 },
            "black is value 0"
        );
        assert_eq!(
            Hsv::from(Rgb::new(255, 255, 255)).s,
            0,
            "white has no saturation"
        );
        // Dimming keeps the hue and drops only the value: what the palette's
        // brightness scaling relies on.
        let full = Hsv::from(Rgb::new(80, 170, 255));
        let dim = Hsv::from(Rgb::new(16, 34, 51));
        assert_eq!(full.h, dim.h);
        assert!(full.v > dim.v);
    }
}
