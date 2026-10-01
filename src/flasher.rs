use std::thread::sleep;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};

use crate::afc::AfcClient;
use crate::airlift::{
    LINK_PREFIX, SOURCE_PREFIX, build_books_plist, build_streaming_zip_archive_multi,
    restore_books, snapshot_books, stage_streaming_zip,
};
use crate::airtraffic::sync_assets_via_airtraffic;
use crate::device::{ActiveDeviceSession, ConnectionMode};
use crate::safety::{OperationGuard, PendingBooks};
use crate::wallet_backup::{
    CARD_ARTWORK_ASSETS, capture_original_card, card_identity, load_original_assets,
};

fn validate_wallet_targets(target_dir: &str, items: &[(&str, &[u8])]) -> Result<()> {
    let component = target_dir
        .strip_prefix("/var/mobile/Library/Passes/Cards/")
        .context("Only Wallet artwork and its caches may be written")?;
    let (hash, cache) = if let Some(hash) = component.strip_suffix(".pkpass") {
        (hash, false)
    } else if let Some(hash) = component.strip_suffix(".pkcache") {
        (hash, true)
    } else if let Some(hash) = component.strip_suffix(".cache") {
        (hash, true)
    } else {
        bail!("Unsupported Wallet target");
    };
    card_identity(hash)?;
    ensure!(
        !items.is_empty() && items.len() <= 3,
        "Invalid artwork batch size"
    );
    let mut seen = std::collections::HashSet::new();
    for (leaf, data) in items {
        let allowed = if cache {
            ["FrontFace", "PlaceHolder", "Preview"].contains(leaf)
        } else {
            CARD_ARTWORK_ASSETS.contains(leaf)
        };
        ensure!(
            allowed && seen.insert(*leaf),
            "Unexpected or duplicate Wallet asset: {leaf}"
        );
        ensure!(
            !data.is_empty() && data.len() <= 16 * 1024 * 1024,
            "Invalid payload size"
        );
    }
    Ok(())
}

/// Caller holds OperationGuard for the whole multi-step operation. No retries:
/// a failed sync may already have written some files and needs explicit recovery.
fn write_wallet_files<L>(
    udid: &str,
    mode: ConnectionMode,
    target_dir: &str,
    items: &[(&str, &[u8])],
    mut log: L,
) -> Result<()>
where
    L: FnMut(&str),
{
    validate_wallet_targets(target_dir, items)?;
    PendingBooks::ensure_clear(udid)?;
    let token = crate::platform::random_hex()?;
    let source = format!("{SOURCE_PREFIX}{token}");
    let link = format!("{LINK_PREFIX}{token}");
    let mut assets = vec![(format!("../../{source}/p0/p1/p2/link"), link.clone())];
    for (index, (leaf, _)) in items.iter().enumerate() {
        assets.push((
            format!("../../{source}/payload_{index}"),
            format!("{link}/{leaf}"),
        ));
    }
    let ids: Vec<_> = assets.iter().map(|(id, _)| id.clone()).collect();
    let books = build_books_plist(&ids)?;
    let archive = build_streaming_zip_archive_multi(target_dir, items)?;
    let session = ActiveDeviceSession::open(Some(udid), mode)?;
    let afc = AfcClient::new(&session)?;
    let pending = PendingBooks::create(udid, snapshot_books(&afc)?)
        .context("Could not durably save Books recovery data; no device write attempted")?;
    log("Books recovery backup saved to disk before writing.");

    let write_result = (|| -> Result<()> {
        stage_streaming_zip(&session, &source, &archive)?;
        ensure!(
            afc.path_exists(&format!("{source}/p0/p1/p2/link"))?,
            "Staging link missing"
        );
        for index in 0..items.len() {
            ensure!(
                afc.path_exists(&format!("{source}/payload_{index}"))?,
                "Staging artwork missing"
            );
        }
        afc.make_directory_recursive("Books/Sync")?;
        afc.write_file("Books/Sync/Books.plist", &books)?;
        let refs: Vec<_> = assets
            .iter()
            .map(|(id, dest)| (id.as_str(), dest.as_str()))
            .collect();
        sync_assets_via_airtraffic(udid, session.transport, &refs, &mut log)?;
        for (leaf, expected) in items {
            let actual = afc
                .read_file(&format!("{target_dir}/{leaf}"))
                .with_context(|| {
                    format!("Cannot verify {leaf} after writing; the outcome is uncertain")
                })?;
            ensure!(
                actual == *expected,
                "Read-back verification failed for {leaf}"
            );
        }
        Ok(())
    })();

    // The worker process has exited before sync returns. Restore errors are
    // reported alongside write errors instead of being hidden by early return.
    sleep(Duration::from_millis(800));
    let restore_result = restore_books(&afc, &pending.snapshot);
    if let Err(error) = &write_result {
        log(&format!(
            "Write failed: {error:#}. Recovery data kept at {}",
            PendingBooks::path(udid).display()
        ));
    }
    if let Err(error) = &restore_result {
        log(&format!("Books restore failed: {error:#}"));
    }
    match (write_result, restore_result) {
        (Ok(()), Ok(())) => {
            // Cleanup is confined to the unique staging area; never recurse
            // through the relocated link to the live Wallet directory.
            afc.remove_path(&link)
                .context("Could not remove staging link; recovery backup retained")?;
            afc.remove_tree(&source)
                .context("Could not remove staging directory; recovery backup retained")?;
            pending.complete()?;
            Ok(())
        }
        (write, restore) => bail!(
            "Operation incomplete. Write: {}. Books restore: {}. Recovery backup retained; use Restore Books before another attempt.",
            write
                .err()
                .map(|e| format!("{e:#}"))
                .unwrap_or_else(|| "verified".into()),
            restore
                .err()
                .map(|e| format!("{e:#}"))
                .unwrap_or_else(|| "verified".into())
        ),
    }
}

pub fn recover_books(udid: &str, mode: ConnectionMode) -> Result<()> {
    let _guard = OperationGuard::acquire(udid)?;
    // A helper can outlive a crashed GUI. Do not race it during recovery.
    let _worker_guard = OperationGuard::acquire(&format!("{udid}.airtraffic"))?;
    let pending = PendingBooks::load(udid)?;
    let session = ActiveDeviceSession::open(Some(udid), mode)?;
    let afc = AfcClient::new(&session)?;
    restore_books(&afc, &pending.snapshot)?;
    pending.complete()
}

pub fn flash_wallet_skin<F, L>(
    udid: &str,
    mode: ConnectionMode,
    card_hash: &str,
    skin_png: &[u8],
    skin_pdf: &[u8],
    mut progress: F,
    mut log: L,
) -> Result<()>
where
    F: FnMut(usize, usize, &str),
    L: FnMut(&str),
{
    card_identity(card_hash)?;
    let _guard = OperationGuard::acquire(udid)?;
    PendingBooks::ensure_clear(udid)?;
    progress(0, 3, "Backing up original artwork...");
    let hash = capture_original_card(udid, mode, card_hash, &mut log)?;
    let dir = format!("/var/mobile/Library/Passes/Cards/{hash}.pkpass");
    let items = [
        (CARD_ARTWORK_ASSETS[0], skin_png),
        (CARD_ARTWORK_ASSETS[1], skin_png),
        (CARD_ARTWORK_ASSETS[2], skin_pdf),
    ];
    progress(1, 3, "Writing card artwork...");
    write_wallet_files(udid, mode, &dir, &items, &mut log)?;
    invalidate_wallet_caches(udid, mode, &hash, &mut progress, &mut log)?;
    progress(3, 3, "Card skin updated successfully!");
    Ok(())
}

pub fn restore_wallet_original<F, L>(
    udid: &str,
    mode: ConnectionMode,
    card_hash: &str,
    mut progress: F,
    mut log: L,
) -> Result<()>
where
    F: FnMut(usize, usize, &str),
    L: FnMut(&str),
{
    card_identity(card_hash)?;
    let _guard = OperationGuard::acquire(udid)?;
    PendingBooks::ensure_clear(udid)?;
    let (hash, assets) = load_original_assets(udid, card_hash)?;
    let items: Vec<_> = assets
        .iter()
        .map(|(name, bytes)| (name.as_str(), bytes.as_slice()))
        .collect();
    progress(1, 3, "Restoring original card artwork...");
    write_wallet_files(
        udid,
        mode,
        &format!("/var/mobile/Library/Passes/Cards/{hash}.pkpass"),
        &items,
        &mut log,
    )?;
    invalidate_wallet_caches(udid, mode, &hash, &mut progress, &mut log)?;
    progress(3, 3, "Original card face restored successfully!");
    Ok(())
}

fn invalidate_wallet_caches<F, L>(
    udid: &str,
    mode: ConnectionMode,
    hash: &str,
    progress: &mut F,
    log: &mut L,
) -> Result<()>
where
    F: FnMut(usize, usize, &str),
    L: FnMut(&str),
{
    let session = ActiveDeviceSession::open(Some(udid), mode)?;
    let afc = AfcClient::new(&session)?;
    for (index, suffix) in [".cache", ".pkcache"].iter().enumerate() {
        let dir = format!("/var/mobile/Library/Passes/Cards/{hash}{suffix}");
        if !afc.path_exists(&dir)? {
            continue;
        }
        let mut items = Vec::new();
        for leaf in ["FrontFace", "PlaceHolder", "Preview"] {
            if afc.path_exists(&format!("{dir}/{leaf}"))? {
                items.push((leaf, &b"corrupted"[..]));
            }
        }
        if !items.is_empty() {
            progress(index + 2, 3, "Refreshing Wallet artwork cache...");
            write_wallet_files(udid, mode, &dir, &items, &mut *log)
                .context("Artwork was written, but refreshing the Wallet cache failed")?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn writer_only_accepts_known_wallet_assets() {
        let dir = "/var/mobile/Library/Passes/Cards/d64fKk0kyHWP11IWV2GRLud4XQk=.pkpass";
        assert!(validate_wallet_targets(dir, &[(CARD_ARTWORK_ASSETS[0], b"png")]).is_ok());
        assert!(validate_wallet_targets(dir, &[("../../escape", b"x")]).is_err());
        assert!(
            validate_wallet_targets(
                "/var/mobile/Library/Safari",
                &[(CARD_ARTWORK_ASSETS[0], b"x")]
            )
            .is_err()
        );
        assert!(
            validate_wallet_targets(
                dir,
                &[
                    (CARD_ARTWORK_ASSETS[0], b"x"),
                    (CARD_ARTWORK_ASSETS[0], b"y")
                ]
            )
            .is_err()
        );
    }
}
