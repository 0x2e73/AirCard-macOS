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
    if target_dir == crate::protected_files::PROBE_DIR {
        crate::protected_files::validate_target(
            target_dir,
            &items.iter().map(|(s, _)| s.to_string()).collect::<Vec<_>>(),
        )?;
        ensure!(
            items.iter().all(|(_, b)| !b.is_empty() && b.len() <= 4096),
            "Invalid probe payload"
        );
        return Ok(());
    }
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
        let staged = stage_streaming_zip(&session, &source, &archive);
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
        if let Err(error) = staged {
            log(&format!(
                "StreamingZip: {error:#}; staging objects verified."
            ));
        }
        afc.make_directory_recursive("Books/Sync")?;
        afc.write_file("Books/Sync/Books.plist", &books)?;
        let refs: Vec<_> = assets
            .iter()
            .map(|(id, dest)| (id.as_str(), dest.as_str()))
            .collect();
        sync_assets_via_airtraffic(udid, session.transport, &refs, &mut log)?;
        let leaves: Vec<_> = items.iter().map(|(leaf, _)| *leaf).collect();
        let exported =
            crate::protected_files::export_files(&session, &afc, target_dir, &leaves, &mut log)?;
        for (leaf, expected) in items {
            let actual = &exported
                .iter()
                .find(|(name, _)| name == leaf)
                .with_context(|| {
                    format!("Written file {leaf} could not be exported for verification")
                })?
                .1;
            ensure!(
                actual.as_slice() == *expected,
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
            "Operation incomplete. Write: {}. Books restore: {}. Recovery backup retained; use Recover Interrupted Operation before another attempt.",
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
    drop(OperationGuard::acquire(&format!("{udid}.airtraffic"))?);
    let pending = PendingBooks::load(udid)?;
    let session = ActiveDeviceSession::open(Some(udid), mode)?;
    let afc = AfcClient::new(&session)?;
    crate::protected_files::recover_export(&session, &afc, &mut |message| println!("{message}"))?;
    restore_books(&afc, &pending.snapshot)?;
    pending.complete()
}

fn replacement_assets(
    skin_png: &[u8],
    skin_pdf: &[u8],
    originals: &[(String, Vec<u8>)],
) -> Result<crate::protected_files::FileBytes> {
    let skin = image::load_from_memory_with_format(skin_png, image::ImageFormat::Png)?;
    originals
        .iter()
        .map(|(name, original)| {
            ensure!(
                CARD_ARTWORK_ASSETS.contains(&name.as_str()),
                "Unexpected artwork asset"
            );
            let bytes = if name.ends_with(".pdf") {
                skin_pdf.to_vec()
            } else {
                let original =
                    image::load_from_memory_with_format(original, image::ImageFormat::Png)?;
                // Issuers can store a 1536x969 image under an @2x filename.
                // Match the actual saved dimensions, not an inferred scale.
                if (skin.width(), skin.height()) == (original.width(), original.height()) {
                    skin_png.to_vec()
                } else {
                    let mut output = std::io::Cursor::new(Vec::new());
                    skin.resize_exact(
                        original.width(),
                        original.height(),
                        image::imageops::FilterType::Lanczos3,
                    )
                    .write_to(&mut output, image::ImageFormat::Png)?;
                    output.into_inner()
                }
            };
            Ok((name.clone(), bytes))
        })
        .collect()
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
    let (_, originals) = load_original_assets(udid, &hash)?;
    let replacements = replacement_assets(skin_png, skin_pdf, &originals)?;
    let items: Vec<_> = replacements
        .iter()
        .map(|(name, bytes)| (name.as_str(), bytes.as_slice()))
        .collect();
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
        progress(index + 2, 3, "Refreshing Wallet artwork cache...");
        let pending = PendingBooks::create(udid, snapshot_books(&afc)?)?;
        let removed = crate::protected_files::discard_files(
            &session,
            &afc,
            &dir,
            &crate::protected_files::CACHE_FILES,
            log,
        );
        let restored = restore_books(&afc, &pending.snapshot);
        removed.context("Artwork was written; cache cleanup needs recovery")?;
        restored?;
        pending.complete()?;
    }
    Ok(())
}

/// Exercise write, overwrite, export, return, byte comparison and removal using
/// only a randomly named file in Library/Caches; never touches a Wallet pass.
pub fn probe_device<L: FnMut(&str)>(udid: &str, mode: ConnectionMode, mut log: L) -> Result<()> {
    let _guard = OperationGuard::acquire(udid)?;
    PendingBooks::ensure_clear(udid)?;
    let leaf = format!("aircard-probe-{}.bin", crate::platform::random_hex()?);
    let dir = crate::protected_files::PROBE_DIR;
    log(&format!("Testing a disposable file only: {dir}/{leaf}"));
    for payload in [
        b"AirCard temporary round-trip probe A".as_slice(),
        b"AirCard temporary round-trip probe B",
    ] {
        write_wallet_files(udid, mode, dir, &[(leaf.as_str(), payload)], &mut log)?;
        let session = ActiveDeviceSession::open(Some(udid), mode)?;
        let afc = AfcClient::new(&session)?;
        let pending = PendingBooks::create(udid, snapshot_books(&afc)?)?;
        log("Checking that the returned probe is still at its original path...");
        let checked = crate::protected_files::export_files(&session, &afc, dir, &[&leaf], &mut log);
        let restored = restore_books(&afc, &pending.snapshot);
        let data = checked?;
        restored?;
        ensure!(
            data.len() == 1 && data[0].1 == payload,
            "The returned probe was not found at its original path; refusing Wallet operations"
        );
        pending.complete()?;
    }
    let session = ActiveDeviceSession::open(Some(udid), mode)?;
    let afc = AfcClient::new(&session)?;
    let pending = PendingBooks::create(udid, snapshot_books(&afc)?)?;
    let removed = crate::protected_files::discard_files(&session, &afc, dir, &[&leaf], &mut log);
    let restored = restore_books(&afc, &pending.snapshot);
    removed?;
    restored?;
    pending.complete()?;
    log(
        "Probe passed: create, overwrite, export/read, return and removal verified; Books restored. Wallet files were not accessed.",
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn replacement_matches_saved_dimensions_and_only_saved_assets() {
        fn png(width: u32, height: u32) -> Vec<u8> {
            let mut output = std::io::Cursor::new(Vec::new());
            image::DynamicImage::new_rgb8(width, height)
                .write_to(&mut output, image::ImageFormat::Png)
                .unwrap();
            output.into_inner()
        }
        let skin = png(1536, 969);
        for (width, height) in [(1536, 969), (1024, 646)] {
            let originals = vec![(CARD_ARTWORK_ASSETS[1].into(), png(width, height))];
            let replacements = replacement_assets(&skin, b"unused PDF", &originals).unwrap();
            assert_eq!(replacements.len(), 1);
            assert_eq!(replacements[0].0, CARD_ARTWORK_ASSETS[1]);
            let decoded = image::load_from_memory(&replacements[0].1).unwrap();
            assert_eq!((decoded.width(), decoded.height()), (width, height));
        }
    }

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
