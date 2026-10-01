//! Host-specific paths and socket options. No device operations happen here.
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, ensure};

pub fn data_dir() -> PathBuf {
    #[cfg(target_os = "macos")]
    let base = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join("Library/Application Support"));
    #[cfg(windows)]
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    #[cfg(not(any(target_os = "macos", windows)))]
    let base = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from);
    // Do not silently put backups in a temporary or shared user directory.
    base.expect("The operating system must provide a user data directory")
        .join("AirCard")
}

pub fn random_hex() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("System random source failed: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

pub fn set_receive_timeout(socket: i32, timeout: Duration) -> Result<()> {
    ensure!(socket >= 0, "Invalid service socket");
    #[cfg(unix)]
    let status = unsafe {
        let value = libc::timeval {
            tv_sec: timeout.as_secs().try_into()?,
            tv_usec: timeout.subsec_micros().try_into()?,
        };
        libc::setsockopt(
            socket,
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            &value as *const _ as *const libc::c_void,
            std::mem::size_of_val(&value) as libc::socklen_t,
        )
    };
    #[cfg(windows)]
    let status = unsafe {
        #[link(name = "ws2_32")]
        unsafe extern "system" {
            fn setsockopt(s: usize, level: i32, name: i32, value: *const u8, len: i32) -> i32;
        }
        let value: u32 = timeout.as_millis().try_into()?;
        setsockopt(
            socket as usize,
            0xffff,
            0x1006,
            &value as *const _ as *const u8,
            std::mem::size_of_val(&value) as i32,
        )
    };
    if status != 0 {
        return Err(std::io::Error::last_os_error())
            .context("Could not set service receive timeout");
    }
    Ok(())
}
