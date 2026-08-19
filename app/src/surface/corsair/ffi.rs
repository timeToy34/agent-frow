//! The iCUE SDK, loaded at runtime.
//!
//! Deliberately not linked at build time. The SDK is a redistributable DLL that
//! ships with the application, not something a machine has, and linking it
//! would mean this project could not be built at all without a copy of the SDK
//! sitting in the right place — for an application whose keyboard support is
//! optional at runtime anyway. Loading it by name means one binary works on a
//! machine with iCUE and on a machine without, and the difference is a sentence
//! in the window rather than a build failure.
#![allow(non_snake_case, non_camel_case_types)]

use std::ffi::c_void;
use std::os::raw::c_char;
use std::path::{Path, PathBuf};

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CorsairVersion {
    pub major: i32,
    pub minor: i32,
    pub patch: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CorsairSessionDetails {
    pub client_version: CorsairVersion,
    pub server_version: CorsairVersion,
    pub server_host_version: CorsairVersion,
}

#[repr(C)]
pub struct CorsairSessionStateChanged {
    pub state: i32,
    pub details: CorsairSessionDetails,
}

#[repr(C)]
pub struct CorsairDeviceInfo {
    pub device_type: i32,
    pub id: [c_char; 128],
    pub serial: [c_char; 128],
    pub model: [c_char; 128],
    pub led_count: i32,
    pub channel_count: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CorsairLedPosition {
    pub id: u32,
    pub cx: f64,
    pub cy: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CorsairLedColor {
    pub id: u32,
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

#[repr(C)]
pub struct CorsairDeviceFilter {
    pub device_type_mask: i32,
}

pub const CE_SUCCESS: i32 = 0;
pub const CSS_CONNECTED: i32 = 6;
pub const CDT_KEYBOARD: i32 = 0x0001;

/// Where our lighting sits relative to the user's own.
///
/// From `iCUESDK.h`: "By default iCUE has priority of 127 and all shared
/// clients have priority of 128 if they don't call this function. Layers with
/// higher priority value are shown on top of layers with lower priority."
///
/// So 128 is what we would get anyway. Saying it is what stops a future iCUE
/// default quietly sliding the F-row underneath whatever profile the user has
/// running. **Never ask for exclusive control**: it is per device, not per LED,
/// so it stops iCUE rendering the whole keyboard and every key outside the
/// F-row goes dark whether we paint it or not.
pub const SDK_LAYER_PRIORITY: u32 = 128;

/// The file every iCUE-aware application ships beside itself.
pub const DLL_NAME: &str = "iCUESDK.x64_2019.dll";

pub type StateHandler = extern "C" fn(*mut c_void, *const CorsairSessionStateChanged);

type FnConnect = unsafe extern "C" fn(StateHandler, *mut c_void) -> i32;
type FnGetDevices =
    unsafe extern "C" fn(*const CorsairDeviceFilter, i32, *mut CorsairDeviceInfo, *mut i32) -> i32;
type FnGetLedPositions =
    unsafe extern "C" fn(*const c_char, i32, *mut CorsairLedPosition, *mut i32) -> i32;
type FnSetLedColors = unsafe extern "C" fn(*const c_char, i32, *const CorsairLedColor) -> i32;
type FnSetLayerPriority = unsafe extern "C" fn(u32) -> i32;
type FnDisconnect = unsafe extern "C" fn() -> i32;

/// The handful of entry points the lighting needs.
pub struct Sdk {
    /// Where it was found, so the window can say which copy is being used.
    pub path: PathBuf,
    connect: FnConnect,
    get_devices: FnGetDevices,
    get_led_positions: FnGetLedPositions,
    set_led_colors: FnSetLedColors,
    set_layer_priority: FnSetLayerPriority,
    disconnect: FnDisconnect,
}

impl Sdk {
    /// SAFETY: all of these are FFI into the loaded SDK, with the argument
    /// shapes its header declares. The wrappers exist so the call sites read as
    /// ordinary code and every `unsafe` is one line from its justification.
    pub fn connect(&self, handler: StateHandler) -> i32 {
        unsafe { (self.connect)(handler, std::ptr::null_mut()) }
    }

    pub fn devices(
        &self,
        filter: &CorsairDeviceFilter,
        out: &mut [CorsairDeviceInfo],
    ) -> (i32, i32) {
        let mut count = 0i32;
        let code =
            unsafe { (self.get_devices)(filter, out.len() as i32, out.as_mut_ptr(), &mut count) };
        (code, count)
    }

    /// The device id is taken as the SDK's own fixed buffer rather than as a
    /// pointer, so nothing outside this module ever holds a raw one.
    pub fn led_positions(
        &self,
        device_id: &[c_char; 128],
        out: &mut [CorsairLedPosition],
    ) -> (i32, i32) {
        let mut count = 0i32;
        let code = unsafe {
            (self.get_led_positions)(
                device_id.as_ptr(),
                out.len() as i32,
                out.as_mut_ptr(),
                &mut count,
            )
        };
        (code, count)
    }

    pub fn set_led_colors(&self, device_id: &[c_char; 128], colors: &[CorsairLedColor]) -> i32 {
        unsafe { (self.set_led_colors)(device_id.as_ptr(), colors.len() as i32, colors.as_ptr()) }
    }

    pub fn set_layer_priority(&self, priority: u32) -> i32 {
        unsafe { (self.set_layer_priority)(priority) }
    }

    pub fn disconnect(&self) -> i32 {
        unsafe { (self.disconnect)() }
    }
}

/// Where to look for the SDK, in order.
///
/// Beside the running executable first, because that is where the installer
/// puts it and it is the copy we were built against. The bare name last, which
/// asks Windows to search its usual places for anyone who put it on `PATH`.
pub fn candidates() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        paths.push(dir.join(DLL_NAME));
    }
    if let Some(install) = crate::paths::install_dir() {
        paths.push(install.join(DLL_NAME));
    }
    paths.push(PathBuf::from(DLL_NAME));
    paths
}

#[cfg(windows)]
pub fn load() -> Result<Sdk, String> {
    let mut tried = Vec::new();
    for path in candidates() {
        match load_from(&path) {
            Ok(sdk) => return Ok(sdk),
            Err(reason) => tried.push(format!("{} ({reason})", path.display())),
        }
    }
    Err(format!(
        "could not load {DLL_NAME}: tried {}",
        tried.join(", ")
    ))
}

#[cfg(not(windows))]
pub fn load() -> Result<Sdk, String> {
    Err("the keyboard is only driven on Windows".to_owned())
}

#[cfg(windows)]
fn load_from(path: &Path) -> Result<Sdk, String> {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LoadLibraryW(name: *const u16) -> *mut c_void;
        fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
    }

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: a null-terminated wide string, which is what the call wants.
    let module = unsafe { LoadLibraryW(wide.as_ptr()) };
    if module.is_null() {
        return Err("not found".to_owned());
    }

    // The module is never freed. Unloading it after `CorsairDisconnect` would
    // be tidier and is not worth the risk: the SDK runs threads of its own, and
    // pulling the code out from under one that has not finished is a crash on
    // the way out of an application that had already done its job.
    let symbol = |name: &str| -> Result<*mut c_void, String> {
        let c_name = format!("{name}\0");
        // SAFETY: `module` is a live handle and `c_name` is null-terminated.
        let address = unsafe { GetProcAddress(module, c_name.as_ptr()) };
        if address.is_null() {
            return Err(format!("{name} is missing"));
        }
        Ok(address)
    };

    // SAFETY: each symbol is transmuted to the signature `iCUESDK.h` declares
    // for it. Getting one wrong is undefined behaviour, so the list is exactly
    // the header's and nothing is inferred.
    unsafe {
        Ok(Sdk {
            path: path.to_path_buf(),
            connect: std::mem::transmute::<*mut c_void, FnConnect>(symbol("CorsairConnect")?),
            get_devices: std::mem::transmute::<*mut c_void, FnGetDevices>(symbol(
                "CorsairGetDevices",
            )?),
            get_led_positions: std::mem::transmute::<*mut c_void, FnGetLedPositions>(symbol(
                "CorsairGetLedPositions",
            )?),
            set_led_colors: std::mem::transmute::<*mut c_void, FnSetLedColors>(symbol(
                "CorsairSetLedColors",
            )?),
            set_layer_priority: std::mem::transmute::<*mut c_void, FnSetLayerPriority>(symbol(
                "CorsairSetLayerPriority",
            )?),
            disconnect: std::mem::transmute::<*mut c_void, FnDisconnect>(symbol(
                "CorsairDisconnect",
            )?),
        })
    }
}

#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;

#[cfg(not(windows))]
fn load_from(_path: &Path) -> Result<Sdk, String> {
    Err("not Windows".to_owned())
}
