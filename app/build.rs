//! Two build-time jobs, both Windows-only and both optional in the sense that
//! failing softly beats failing the build:
//!
//! - Puts the iCUE SDK's redistributable DLL next to the built binary when a
//!   copy of the SDK sits in the repository. Nothing is linked; the app loads
//!   it by name at runtime and runs without it.
//! - Renders the application icon from `src/icon.rs` — the same code the tray
//!   and the window draw with at runtime, so there is exactly one definition —
//!   into an `.ico`, and embeds it with the version and authorship strings as
//!   a Windows resource.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::{Path, PathBuf};

// The icon drawing, compiled into the build script itself. Std-only by design.
#[path = "src/icon.rs"]
mod icon;

const DLL: &str = "iCUESDK.x64_2019.dll";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/icon.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    embed_icon_and_authorship();
    copy_sdk();
}

fn copy_sdk() {
    let Ok(manifest) = std::env::var("CARGO_MANIFEST_DIR") else {
        return;
    };
    // app -> the repository root, where an iCUESDK folder is kept per machine
    // and never committed.
    let Some(root) = Path::new(&manifest).parent() else {
        return;
    };
    let source = root.join("iCUESDK").join("redist").join("x64").join(DLL);
    if !source.exists() {
        return;
    }
    println!("cargo:rerun-if-changed={}", source.display());

    // OUT_DIR is <target>/<profile>/build/<pkg>/out, so the binary's own
    // directory is four levels up.
    let Ok(out_dir) = std::env::var("OUT_DIR") else {
        return;
    };
    let Some(profile_dir) = Path::new(&out_dir).ancestors().nth(3) else {
        return;
    };
    let target: PathBuf = profile_dir.join(DLL);
    if let Err(error) = std::fs::copy(&source, &target) {
        println!(
            "cargo:warning={} -> {}: {error}",
            source.display(),
            target.display()
        );
    }
}

fn embed_icon_and_authorship() {
    let Ok(out_dir) = std::env::var("OUT_DIR") else {
        return;
    };
    let ico = Path::new(&out_dir).join("agent-frow.ico");
    if let Err(error) = write_ico(&ico, &[16, 24, 32, 48, 64]) {
        println!("cargo:warning=could not render the icon: {error}");
        return;
    }
    let mut resource = winresource::WindowsResource::new();
    resource.set_icon(&ico.display().to_string());
    resource.set("ProductName", "Agent F-Row");
    resource.set("FileDescription", "Agent F-Row");
    resource.set("CompanyName", "timeToy");
    resource.set(
        "LegalCopyright",
        "Copyright (c) 2026 Jerome Rota \"timeToy\"",
    );
    if let Err(error) = resource.compile() {
        // A missing rc.exe should cost the Explorer icon, not the build.
        println!("cargo:warning=could not embed the icon resource: {error}");
    }
}

/// Writes a multi-size `.ico`: 32-bit BGRA DIB entries, alpha carried in the
/// pixels, AND mask present-but-empty as modern Windows expects.
fn write_ico(path: &Path, sizes: &[u32]) -> Result<(), std::io::Error> {
    let mut file: Vec<u8> = Vec::new();
    // ICONDIR
    file.extend_from_slice(&0u16.to_le_bytes());
    file.extend_from_slice(&1u16.to_le_bytes());
    file.extend_from_slice(&(sizes.len() as u16).to_le_bytes());

    let mut images: Vec<Vec<u8>> = Vec::new();
    for &size in sizes {
        let rgba = icon::rgba(size);
        let mut image: Vec<u8> = Vec::new();
        // BITMAPINFOHEADER, with the doubled height the format demands: the
        // XOR pixels and the AND mask are one bitmap as far as it is concerned.
        image.extend_from_slice(&40u32.to_le_bytes());
        image.extend_from_slice(&(size as i32).to_le_bytes());
        image.extend_from_slice(&(size as i32 * 2).to_le_bytes());
        image.extend_from_slice(&1u16.to_le_bytes());
        image.extend_from_slice(&32u16.to_le_bytes());
        image.extend_from_slice(&[0u8; 24]);
        // Pixel rows, bottom-up, BGRA.
        for y in (0..size).rev() {
            for x in 0..size {
                let at = ((y * size + x) * 4) as usize;
                image.extend_from_slice(&[rgba[at + 2], rgba[at + 1], rgba[at], rgba[at + 3]]);
            }
        }
        // AND mask: one bit per pixel, rows padded to 32 bits, all zero —
        // the alpha channel is the real mask.
        let mask_row = size.div_ceil(32) * 4;
        image.extend(std::iter::repeat_n(0u8, (mask_row * size) as usize));
        images.push(image);
    }

    let mut offset = 6 + 16 * sizes.len() as u32;
    for (&size, image) in sizes.iter().zip(&images) {
        let dimension = if size >= 256 { 0 } else { size as u8 };
        file.push(dimension);
        file.push(dimension);
        file.push(0);
        file.push(0);
        file.extend_from_slice(&1u16.to_le_bytes());
        file.extend_from_slice(&32u16.to_le_bytes());
        file.extend_from_slice(&(image.len() as u32).to_le_bytes());
        file.extend_from_slice(&offset.to_le_bytes());
        offset += image.len() as u32;
    }
    for image in &images {
        file.extend_from_slice(image);
    }
    std::fs::write(path, file)
}
