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

pub fn open(found: &Found) -> Result<Box<dyn Transport>, String> {
    platform::open(found)
}

#[cfg(windows)]
mod platform {
    use std::ffi::CString;
    use std::time::{Duration, Instant};

    use hidapi::{HidApi, HidDevice};

    use super::{
        ECHO_TIMEOUT_MS, Found, REPORT_LEN, Report, Transport, USAGE, USAGE_PAGE, VENDOR_ID,
        is_echo,
    };

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
    use super::{Found, Transport};

    pub fn find() -> Result<Vec<Found>, String> {
        Err("the keyboard is only driven on Windows".to_owned())
    }

    pub fn open(_found: &Found) -> Result<Box<dyn Transport>, String> {
        Err("the keyboard is only driven on Windows".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
