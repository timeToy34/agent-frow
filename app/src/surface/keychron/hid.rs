//! Moving reports to and from the keyboard's Launcher interface.
//!
//! The keyboard shows up as several HID interfaces; the one that speaks the
//! protocol is the vendor-defined page `0xFF60`, usage `0x61`, and that pair
//! is how it is found — never by product id, because the 2.4 GHz receiver is
//! a different USB device with its own id and the same interface.
//!
//! [`Transport`] is a trait so everything above it can be exercised against a
//! scripted keyboard in tests. The only real implementation is `hidapi` on
//! Windows; elsewhere there is nothing to open, said in a sentence.

use super::protocol::{REPORT_LEN, Report, USAGE, USAGE_PAGE, VENDOR_ID};

/// How long the keyboard gets to echo a report. Measured at 0.4 ms on the
/// cable and 1.3 ms through the receiver; anything near this is a keyboard
/// that has gone away.
pub const ECHO_TIMEOUT_MS: i32 = 250;

/// One report out, its echo back. Every command the keyboard accepts is
/// answered, so an exchange that returns nothing is a broken link, not a
/// quiet success.
pub trait Transport {
    fn exchange(&mut self, report: &Report) -> Result<Report, String>;
}

impl<T: Transport + ?Sized> Transport for Box<T> {
    fn exchange(&mut self, report: &Report) -> Result<Report, String> {
        (**self).exchange(report)
    }
}

/// A Launcher interface on the bus, before anything has been said to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub product_id: u16,
    pub product: String,
    /// The OS path to open it by.
    pub path: String,
}

impl Found {
    /// The cable, or the receiver — Keychron names the receiver "Ultra-Link",
    /// and that is the only way to tell the two paths apart from here.
    pub fn link(&self) -> &'static str {
        if self.product.contains("Link") {
            "2.4 GHz"
        } else {
            "USB"
        }
    }
}

/// Paths a surface is currently holding. Two surfaces share this bus — the
/// Ultra F-row and the V0 numpad are the same usage pair to the filter below
/// — and a probe from one on an interface the other is driving would eat its
/// echoes. So: claim before opening, keep the claim as long as the
/// connection, and skip what another surface holds.
static CLAIMED: std::sync::Mutex<std::collections::BTreeSet<String>> =
    std::sync::Mutex::new(std::collections::BTreeSet::new());

/// A held interface, freed when dropped. Keep it exactly as long as the
/// transport opened for it.
#[derive(Debug)]
pub struct Claim {
    path: String,
}

impl Claim {
    /// Whether `change` is about the interface this claim holds.
    pub fn is_about(&self, change: &Change) -> bool {
        change.path().eq_ignore_ascii_case(&self.path)
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        if let Ok(mut claimed) = CLAIMED.lock() {
            claimed.remove(&self.path);
        }
    }
}

/// Claims `found` for the calling surface — `None` while another holds it.
pub fn claim(found: &Found) -> Option<Claim> {
    let mut claimed = CLAIMED.lock().ok()?;
    claimed.insert(found.path.clone()).then(|| Claim {
        path: found.path.clone(),
    })
}

/// Whether `reply` answers `sent`: the command byte echoes, and for the
/// per-key group the sub-command too. Anything else is the keyboard talking
/// on its own — a layer change, say — and not what was asked for.
pub fn is_echo(sent: &Report, reply: &Report) -> bool {
    reply[0] == sent[0] && (sent[0] != 0xA8 || reply[1] == sent[1])
}

/// Every Launcher interface currently on the bus.
pub fn find() -> Result<Vec<Found>, String> {
    platform::find()
}

/// A Keychron HID interface arriving or leaving, as Windows reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Arrived(String),
    Removed(String),
}

impl Change {
    /// The interface's OS path — the form [`Found::path`] holds.
    pub fn path(&self) -> &str {
        match self {
            Self::Arrived(path) | Self::Removed(path) => path,
        }
    }
}

/// What a surface does about a [`Change`], given the claim it holds, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Response {
    /// The interface its board was on has gone: drop the board.
    Drop,
    /// Something arrived while it holds a board — perhaps the keyboard on its
    /// other link: make sure the board still answers.
    Check,
    /// It holds nothing and something arrived: look now, not at the retry.
    Look,
    Ignore,
}

pub fn respond(change: &Change, held: Option<&Claim>) -> Response {
    match (change, held) {
        (Change::Removed(_), Some(claim)) if claim.is_about(change) => Response::Drop,
        (Change::Arrived(_), Some(_)) => Response::Check,
        (Change::Arrived(_), None) => Response::Look,
        (Change::Removed(_), _) => Response::Ignore,
    }
}

/// Keychron interfaces arriving and leaving, pushed by Windows the moment it
/// knows — a receiver unplugged, the keyboard switched to its cable, a device
/// re-enumerated on wake — so a surface neither holds a board that has gone
/// nor waits for its next retry to find one that came. Each caller gets its
/// own queue; nothing is ever sent to the keyboard to learn this.
pub fn watch() -> std::sync::mpsc::Receiver<Change> {
    platform::watch()
}

pub fn open(found: &Found) -> Result<Box<dyn Transport>, String> {
    platform::open(found)
}

#[cfg(windows)]
mod platform {
    use std::ffi::CString;
    use std::sync::mpsc::{Receiver, Sender, channel};
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};

    use hidapi::{HidApi, HidDevice};
    use windows::Win32::Devices::DeviceAndDriverInstallation::{
        CM_NOTIFY_ACTION, CM_NOTIFY_ACTION_DEVICEINTERFACEARRIVAL,
        CM_NOTIFY_ACTION_DEVICEINTERFACEREMOVAL, CM_NOTIFY_EVENT_DATA, CM_NOTIFY_FILTER,
        CM_NOTIFY_FILTER_TYPE_DEVICEINTERFACE, CM_Register_Notification, CR_SUCCESS,
        HCMNOTIFICATION,
    };
    use windows::core::GUID;

    use super::{
        Change, ECHO_TIMEOUT_MS, Found, REPORT_LEN, Report, Transport, USAGE, USAGE_PAGE,
        VENDOR_ID, is_echo,
    };

    /// The HID interface class — the GUID every HID path ends in.
    const HID_INTERFACES: GUID = GUID::from_u128(0x4d1e55b2_f16f_11cf_88cb_001111000030);

    /// Every queue handed out by [`watch`]; a queue whose receiver is gone is
    /// dropped on the next change.
    static WATCHERS: Mutex<Vec<Sender<Change>>> = Mutex::new(Vec::new());

    pub fn watch() -> Receiver<Change> {
        static REGISTERED: OnceLock<bool> = OnceLock::new();
        let (send, receive) = channel();
        if let Ok(mut watchers) = WATCHERS.lock() {
            watchers.push(send);
        }
        REGISTERED.get_or_init(register);
        receive
    }

    /// Asks Windows for every HID interface arrival and removal, for the life
    /// of the process. `false` leaves the surfaces on their retry, as before.
    fn register() -> bool {
        let mut filter = CM_NOTIFY_FILTER {
            cbSize: std::mem::size_of::<CM_NOTIFY_FILTER>() as u32,
            FilterType: CM_NOTIFY_FILTER_TYPE_DEVICEINTERFACE,
            ..Default::default()
        };
        filter.u.DeviceInterface.ClassGuid = HID_INTERFACES;
        let mut handle = HCMNOTIFICATION::default();
        // SAFETY: FFI; the filter is fully initialised and outlives the call,
        // and the callback is a plain function valid for the whole process.
        // The handle is never unregistered: the watch lasts as long as we do.
        let result =
            unsafe { CM_Register_Notification(&filter, None, Some(notified), &mut handle) };
        result == CR_SUCCESS
    }

    /// SAFETY: called by Windows on a thread-pool thread with `data` pointing
    /// at `size` bytes of `CM_NOTIFY_EVENT_DATA`, whose symbolic link is a
    /// null-terminated UTF-16 string running to the end of that block.
    unsafe extern "system" fn notified(
        _handle: HCMNOTIFICATION,
        _context: *const core::ffi::c_void,
        action: CM_NOTIFY_ACTION,
        data: *const CM_NOTIFY_EVENT_DATA,
        size: u32,
    ) -> u32 {
        const HANDLED: u32 = 0;
        if data.is_null() {
            return HANDLED;
        }
        let start =
            unsafe { std::ptr::addr_of!((*data).u.DeviceInterface.SymbolicLink) } as *const u16;
        let offset = start as usize - data as usize;
        let room = (size as usize).saturating_sub(offset) / 2;
        let link = unsafe { std::slice::from_raw_parts(start, room) };
        let length = link.iter().position(|&unit| unit == 0).unwrap_or(room);
        let path = String::from_utf16_lossy(&link[..length]);
        if !path
            .to_ascii_uppercase()
            .contains(&format!("VID_{VENDOR_ID:04X}"))
        {
            return HANDLED;
        }
        let change = match action {
            CM_NOTIFY_ACTION_DEVICEINTERFACEARRIVAL => Change::Arrived(path),
            CM_NOTIFY_ACTION_DEVICEINTERFACEREMOVAL => Change::Removed(path),
            _ => return HANDLED,
        };
        if let Ok(mut watchers) = WATCHERS.lock() {
            watchers.retain(|watcher| watcher.send(change.clone()).is_ok());
        }
        HANDLED
    }

    struct Device {
        device: HidDevice,
    }

    impl Transport for Device {
        fn exchange(&mut self, report: &Report) -> Result<Report, String> {
            // The keyboard also speaks unasked — a layer change is pushed as
            // an `A3` report — so the queue can hold things that are not the
            // echo. Anything already waiting is stale; anything that arrives
            // after the write and is not the echo is a push, and skipped.
            let mut stale = [0u8; REPORT_LEN];
            for _ in 0..32 {
                match self.device.read_timeout(&mut stale, 0) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
            // Report id 0 goes first on the wire; the keyboard's replies carry
            // none, so they come back as the bare 32 bytes.
            let mut out = [0u8; REPORT_LEN + 1];
            out[1..].copy_from_slice(report);
            self.device
                .write(&out)
                .map_err(|error| format!("write: {error}"))?;
            let deadline = Instant::now() + Duration::from_millis(ECHO_TIMEOUT_MS as u64);
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err("no answer from the keyboard".to_owned());
                }
                let mut reply = [0u8; REPORT_LEN];
                let read = self
                    .device
                    .read_timeout(&mut reply, left.as_millis().max(1) as i32)
                    .map_err(|error| format!("read: {error}"))?;
                if read == 0 {
                    return Err("no answer from the keyboard".to_owned());
                }
                if is_echo(report, &reply) {
                    return Ok(reply);
                }
            }
        }
    }

    pub fn find() -> Result<Vec<Found>, String> {
        let api = HidApi::new().map_err(|error| format!("HID: {error}"))?;
        Ok(api
            .device_list()
            .filter(|info| {
                info.vendor_id() == VENDOR_ID
                    && info.usage_page() == USAGE_PAGE
                    && info.usage() == USAGE
            })
            .map(|info| Found {
                product_id: info.product_id(),
                product: info.product_string().unwrap_or_default().to_owned(),
                path: info.path().to_string_lossy().into_owned(),
            })
            .collect())
    }

    pub fn open(found: &Found) -> Result<Box<dyn Transport>, String> {
        let api = HidApi::new().map_err(|error| format!("HID: {error}"))?;
        let path = CString::new(found.path.clone()).map_err(|error| format!("{error}"))?;
        let device = api
            .open_path(&path)
            .map_err(|error| format!("{}: {error}", found.product))?;
        Ok(Box::new(Device { device }))
    }
}

#[cfg(not(windows))]
mod platform {
    use super::{Change, Found, Transport};

    /// Nothing arrives where nothing is driven.
    pub fn watch() -> std::sync::mpsc::Receiver<Change> {
        std::sync::mpsc::channel().1
    }

    pub fn find() -> Result<Vec<Found>, String> {
        Err("the keyboard is only driven on Windows".to_owned())
    }

    pub fn open(_found: &Found) -> Result<Box<dyn Transport>, String> {
        Err("the keyboard is only driven on Windows".to_owned())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn a_claimed_path_is_held_until_the_claim_drops() {
        let found = Found {
            product_id: 0x0800,
            product: "V0 Ultra".to_owned(),
            path: "test-claim-path".to_owned(),
        };
        let first = claim(&found).expect("free to claim");
        assert!(claim(&found).is_none(), "held by the first surface");
        drop(first);
        let again = claim(&found).expect("freed by the drop");
        drop(again);
    }

    #[test]
    fn a_change_is_about_its_interface_whatever_the_case() {
        let found = Found {
            product_id: 0xD028,
            product: "Keychron Ultra-Link 8K".to_owned(),
            path: "\\\\?\\HID#VID_3434&PID_D028&MI_02#7&1de32dba&0&0000#{4d1e55b2}".to_owned(),
        };
        let held = claim(&found).expect("free to claim");
        let lower = found.path.to_ascii_lowercase();
        assert!(held.is_about(&Change::Removed(lower.clone())));
        assert!(held.is_about(&Change::Arrived(lower)));
        let cable = found.path.replace("PID_D028&MI_02", "PID_0C30&MI_01");
        assert!(!held.is_about(&Change::Removed(cable)));
    }

    #[test]
    fn a_surface_drops_only_its_own_interface_and_looks_when_empty() {
        let receiver = Found {
            product_id: 0xD028,
            product: "Keychron Ultra-Link 8K".to_owned(),
            path: "respond-receiver-path".to_owned(),
        };
        let held = claim(&receiver).expect("free to claim");
        let own = Change::Removed(receiver.path.clone());
        let other = Change::Removed("respond-keyboard-mi-00".to_owned());
        let cable = Change::Arrived("respond-cable-path".to_owned());
        assert_eq!(respond(&own, Some(&held)), Response::Drop);
        assert_eq!(respond(&other, Some(&held)), Response::Ignore);
        assert_eq!(respond(&cable, Some(&held)), Response::Check);
        assert_eq!(respond(&cable, None), Response::Look);
        assert_eq!(respond(&own, None), Response::Ignore);
    }

    #[test]
    fn a_push_is_not_an_echo() {
        let mut sent = [0u8; REPORT_LEN];
        sent[..2].copy_from_slice(&[0xA8, 0x0A]);
        let mut layer_change = [0u8; REPORT_LEN];
        layer_change[0] = 0xA3;
        assert!(!is_echo(&sent, &layer_change));
        let mut other_sub = sent;
        other_sub[1] = 0x09;
        assert!(!is_echo(&sent, &other_sub));
        assert!(is_echo(&sent, &sent));
        // Outside the A8 group only the command byte is stable: the firmware
        // string overwrites byte 1 of an A1 reply.
        let mut version = [0u8; REPORT_LEN];
        version[0] = 0xA1;
        let mut reply = version;
        reply[1] = b'v';
        assert!(is_echo(&version, &reply));
    }
}
