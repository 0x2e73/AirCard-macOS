use std::collections::HashMap;
use std::thread::sleep;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::apple::{ATHostConnectionRef, get_apple_libraries};
use crate::device::{DeviceTransport, ensure_transport_available};

fn generate_uuid_v4() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("System random source failed: {e}"))?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40; // version 4
    bytes[8] = (bytes[8] & 0x3f) | 0x80; // variant
    Ok(format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15]
    ))
}

#[derive(serde::Serialize, serde::Deserialize)]
struct SyncRequest {
    udid: String,
    transport: DeviceTransport,
    assets: Vec<(String, String)>,
}

/// Run the blocking private framework in a child process. A detached Rust
/// thread cannot be cancelled and could otherwise write after Books cleanup.
pub fn sync_assets_via_airtraffic<L>(
    udid: &str,
    transport: DeviceTransport,
    assets: &[(&str, &str)],
    mut log: L,
) -> Result<()>
where
    L: FnMut(&str),
{
    use std::io::{Read, Seek, Write};
    use std::process::{Command, Stdio};
    let request = SyncRequest {
        udid: udid.into(),
        transport,
        assets: assets
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect(),
    };
    let mut output = tempfile::tempfile()?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("--airtraffic-worker")
        .stdin(Stdio::piped())
        .stdout(Stdio::from(output.try_clone()?))
        .stderr(Stdio::from(output.try_clone()?));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    let mut child = command
        .spawn()
        .context("Could not start AirTraffic helper")?;
    let send = (|| -> Result<()> {
        let mut input = child.stdin.take().context("Helper input missing")?;
        input.write_all(&serde_json::to_vec(&request)?)?;
        Ok(()) // Close stdin so the worker can read to EOF.
    })();
    if let Err(error) = send {
        let _ = child.kill();
        child.wait()?;
        return Err(error);
    }
    log("Synchronizing Wallet artwork with Apple's service...");
    let timeout = Duration::from_secs(if transport == DeviceTransport::Wifi {
        120
    } else {
        60
    });
    let wait = wait_for_child(&mut child, timeout);
    output.rewind()?;
    let mut text = String::new();
    output.take(64 * 1024).read_to_string(&mut text)?;
    for line in text.lines() {
        log(line);
    }
    let status = wait?;
    anyhow::ensure!(
        status.success(),
        "AirTraffic helper failed; consult the operation log"
    );
    Ok(())
}

fn wait_for_child(
    child: &mut std::process::Child,
    timeout: Duration,
) -> Result<std::process::ExitStatus> {
    let started = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if started.elapsed() < timeout => sleep(Duration::from_millis(50)),
            result => {
                // Reap the helper before returning, including polling errors.
                let kill_result = child.kill();
                child.wait().context("Could not reap AirTraffic helper")?;
                kill_result.context("Could not stop AirTraffic helper")?;
                if let Err(error) = result {
                    return Err(error.into());
                }
                bail!(
                    "AirTraffic timed out; its helper was stopped. Device writes may be partial. Recovery data has been retained."
                );
            }
        }
    }
}

pub fn run_worker() -> Result<()> {
    use std::io::Read;
    // Also bound the helper lifetime if the GUI crashes while a framework call
    // is blocked. The OS releases the helper's operation lock on exit.
    std::thread::spawn(|| {
        sleep(Duration::from_secs(150));
        eprintln!("AirTraffic helper watchdog expired");
        std::process::exit(2);
    });
    let mut input = Vec::new();
    std::io::stdin()
        .take(1024 * 1024 + 1)
        .read_to_end(&mut input)?;
    anyhow::ensure!(input.len() <= 1024 * 1024, "Helper request too large");
    let request: SyncRequest = serde_json::from_slice(&input)?;
    let _guard = crate::safety::OperationGuard::acquire(&format!("{}.airtraffic", request.udid))?;
    anyhow::ensure!(
        !request.assets.is_empty() && request.assets.len() <= 4,
        "Invalid helper batch"
    );
    let refs: Vec<_> = request
        .assets
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    sync_assets_via_airtraffic_internal(&request.udid, request.transport, &refs, |message| {
        println!("{message}")
    })
}

#[cfg(all(test, unix))]
mod process_tests {
    use super::*;
    #[test]
    fn timed_out_helper_is_reaped_before_returning() {
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("10")
            .spawn()
            .unwrap();
        assert!(wait_for_child(&mut child, Duration::from_millis(20)).is_err());
        assert!(child.try_wait().unwrap().is_some());
    }
}

fn sync_assets_via_airtraffic_internal<L>(
    udid: &str,
    transport: DeviceTransport,
    assets: &[(&str, &str)],
    mut log: L,
) -> Result<()>
where
    L: FnMut(&str),
{
    ensure_transport_available(udid, transport)
        .context("Selected device transport disappeared before AirTraffic sync")?;
    log(&format!(
        "Connecting to iOS AirTraffic service (com.apple.atc) over {}...",
        transport.label()
    ));
    let libs = get_apple_libraries()?;
    let cf_udid = libs.create_cf_string(udid)?;

    let conn: ATHostConnectionRef = unsafe { (libs.at_host_connection_create)(cf_udid.raw) };
    if conn.is_null() {
        bail!("ATHostConnectionCreate failed for UDID: {}", udid);
    }

    let retry_scale = if transport == DeviceTransport::Wifi {
        2
    } else {
        1
    };
    let mut run_sync = || -> Result<()> {
        log("Waiting for SyncAllowed from iPhone (keep screen unlocked)...");
        // 1. Wait for SyncAllowed message
        let mut sync_allowed = false;
        for _ in 0..(15 * retry_scale) {
            let msg = unsafe { (libs.at_host_connection_read_message)(conn) };
            if msg.is_null() {
                sleep(Duration::from_millis(150));
                continue;
            }
            let name_ref = unsafe { (libs.at_cf_message_get_name)(msg) };
            let name = libs.to_rust_string(name_ref);
            unsafe { (libs.cf_release)(msg) };
            if name == "SyncAllowed" {
                sync_allowed = true;
                break;
            } else {
                log(&format!("AirTraffic message: {}", name));
            }
        }
        if !sync_allowed {
            bail!(
                "AirTraffic: SyncAllowed message not received. Ensure iPhone screen is unlocked and Books app is opened."
            );
        }

        log("SyncAllowed received! Handshaking Books sync request...");
        // 2. Send HostInfo
        let mut host_info_dict = HashMap::new();
        host_info_dict.insert(
            "Type".to_string(),
            plist::Value::String("iTunes".to_string()),
        );
        host_info_dict.insert(
            "Version".to_string(),
            plist::Value::String("13.7.0.161".to_string()),
        );
        host_info_dict.insert(
            "MacOSVersion".to_string(),
            plist::Value::String(std::env::consts::OS.to_string()),
        );
        host_info_dict.insert(
            "SyncHostName".to_string(),
            plist::Value::String("airlift".to_string()),
        );
        host_info_dict.insert(
            "LibraryID".to_string(),
            plist::Value::String(generate_uuid_v4()?),
        );
        host_info_dict.insert(
            "SyncedDataclasses".to_string(),
            plist::Value::Array(vec![plist::Value::String("Book".to_string())]),
        );
        host_info_dict.insert(
            "SyncedAssetTypes".to_string(),
            plist::Value::Array(vec![plist::Value::String("Book".to_string())]),
        );
        host_info_dict.insert("Wakeable".to_string(), plist::Value::Boolean(false));

        let mut host_info_bytes = Vec::new();
        plist::to_writer_binary(
            &mut host_info_bytes,
            &plist::Value::Dictionary(host_info_dict.into_iter().collect()),
        )?;
        let cf_host_info = libs.create_cf_plist_from_bytes(&host_info_bytes)?;

        unsafe {
            (libs.at_host_connection_send_host_info)(conn, cf_host_info.raw);
        }
        sleep(Duration::from_millis(200));

        // 3. Send SyncRequest
        let mut dataclasses_bytes = Vec::new();
        plist::to_writer_binary(
            &mut dataclasses_bytes,
            &plist::Value::Array(vec![plist::Value::String("Book".to_string())]),
        )?;
        let cf_dataclasses = libs.create_cf_plist_from_bytes(&dataclasses_bytes)?;

        let mut anchors_bytes = Vec::new();
        plist::to_writer_binary(
            &mut anchors_bytes,
            &plist::Value::Dictionary(HashMap::<String, plist::Value>::new().into_iter().collect()),
        )?;
        let cf_anchors = libs.create_cf_plist_from_bytes(&anchors_bytes)?;

        unsafe {
            (libs.at_host_connection_send_sync_request)(
                conn,
                cf_dataclasses.raw,
                cf_anchors.raw,
                cf_host_info.raw,
            );
        }

        log("Waiting for ReadyForSync from iPhone...");
        // 4. Wait for ReadyForSync
        let mut ready_for_sync = false;
        for _ in 0..(20 * retry_scale) {
            let msg = unsafe { (libs.at_host_connection_read_message)(conn) };
            if msg.is_null() {
                sleep(Duration::from_millis(150));
                continue;
            }
            let name_ref = unsafe { (libs.at_cf_message_get_name)(msg) };
            let name = libs.to_rust_string(name_ref);
            unsafe { (libs.cf_release)(msg) };
            if name == "ReadyForSync" {
                ready_for_sync = true;
                break;
            }
        }
        if !ready_for_sync {
            bail!("AirTraffic: ReadyForSync message not received from device");
        }

        // 5. Send MetadataSyncFinished
        let mut sync_types_dict = HashMap::new();
        sync_types_dict.insert("Book".to_string(), plist::Value::Integer(1.into()));
        let mut sync_types_bytes = Vec::new();
        plist::to_writer_binary(
            &mut sync_types_bytes,
            &plist::Value::Dictionary(sync_types_dict.into_iter().collect()),
        )?;
        let cf_sync_types = libs.create_cf_plist_from_bytes(&sync_types_bytes)?;

        unsafe {
            (libs.at_host_connection_send_metadata_sync_finished)(
                conn,
                cf_sync_types.raw,
                cf_anchors.raw,
            );
        }

        // 6. Read AssetManifest
        let cf_key_manifest = libs.create_cf_string("AssetManifest")?;
        let mut manifest_val: Option<plist::Value> = None;

        for _ in 0..(30 * retry_scale) {
            let msg = unsafe { (libs.at_host_connection_read_message)(conn) };
            if msg.is_null() {
                sleep(Duration::from_millis(150));
                continue;
            }
            let name_ref = unsafe { (libs.at_cf_message_get_name)(msg) };
            let name = libs.to_rust_string(name_ref);
            if name == "AssetManifest" {
                let param = unsafe { (libs.at_cf_message_get_param)(msg, cf_key_manifest.raw) };
                if !param.is_null()
                    && let Ok(bytes) = libs.cf_plist_to_bytes(param)
                {
                    manifest_val = plist::Value::from_reader(std::io::Cursor::new(bytes)).ok();
                }
                unsafe { (libs.cf_release)(msg) };
                break;
            } else if name == "SyncFailed" || name == "SyncFinished" {
                unsafe { (libs.cf_release)(msg) };
                bail!(
                    "AirTraffic returned unexpected terminating message: {}",
                    name
                );
            }
            unsafe { (libs.cf_release)(msg) };
        }

        let Some(manifest) = manifest_val else {
            bail!("AirTraffic: AssetManifest was not received or failed to parse");
        };

        // Validate Book manifest contains downloads
        let book_entries = manifest
            .as_dictionary()
            .and_then(|d| d.get("Book"))
            .and_then(|v| v.as_array())
            .context("AssetManifest does not contain Book list")?;

        let mut available_downloads = Vec::new();
        for entry in book_entries {
            if let Some(dict) = entry.as_dictionary() {
                let is_dl = dict
                    .get("IsDownload")
                    .and_then(|b| b.as_boolean())
                    .unwrap_or(false);
                if is_dl && let Some(asset_id) = dict.get("AssetID").and_then(|s| s.as_string()) {
                    available_downloads.push(asset_id.to_string());
                }
            }
        }

        for (ident, _) in assets {
            if !available_downloads.iter().any(|d| d == ident) {
                bail!(
                    "Asset '{}' missing from device download manifest (available: {:?})",
                    ident,
                    available_downloads
                );
            }
        }

        // 7. Dispatch AssetCompleted for each asset
        let cf_dataclass = libs.create_cf_string("Book")?;
        for (idx, (ident, dest)) in assets.iter().enumerate() {
            let cf_ident = libs.create_cf_string(ident)?;
            let cf_dest = libs.create_cf_string(dest)?;

            unsafe {
                (libs.at_host_connection_send_asset_completed)(
                    conn,
                    cf_ident.raw,
                    cf_dataclass.raw,
                    cf_dest.raw,
                );
            }

            if idx + 1 < assets.len() {
                if idx == 0 {
                    sleep(Duration::from_millis(400));
                } else {
                    sleep(Duration::from_millis(60));
                }
            }
        }

        sleep(Duration::from_millis(2000));
        Ok(())
    };

    let result = run_sync();
    unsafe {
        (libs.at_host_connection_release)(conn);
    }
    result
}
