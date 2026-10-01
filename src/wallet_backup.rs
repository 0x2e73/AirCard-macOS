use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use base64::Engine;
use serde::{Deserialize, Serialize};

use crate::afc::AfcClient;
use crate::airlift::{restore_books, snapshot_books};
use crate::device::{ActiveDeviceSession, ConnectionMode};
use crate::safety::PendingBooks;
use crate::safety::{atomic_create, file_key, read_bounded};

pub const CARD_ARTWORK_ASSETS: [&str; 3] = [
    "cardBackgroundCombined@3x.png",
    "cardBackgroundCombined@2x.png",
    "cardBackgroundCombined.pdf",
];

type ArtworkAssets = Vec<(String, Vec<u8>)>;

/// Accept an exact single path component, never a path or a trimmed approximation.
/// The decoded value is also the canonical identity for padded/URL-safe variants.
pub fn card_identity(value: &str) -> Result<String> {
    ensure!(
        !value.is_empty()
            && value.len() <= 44
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"+-_=".contains(&b)),
        "Invalid Wallet card path identifier (path separators are not allowed)"
    );
    let normalized = value
        .trim_end_matches('=')
        .replace('-', "+")
        .replace('_', "/");
    let bytes = base64::engine::general_purpose::STANDARD_NO_PAD
        .decode(normalized)
        .context("Invalid Wallet card identifier encoding")?;
    ensure!(
        matches!(bytes.len(), 20 | 32),
        "Invalid Wallet card identifier length"
    );
    ensure!(
        value.len() - value.trim_end_matches('=').len() <= 2,
        "Invalid identifier padding"
    );
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn backup_path(udid: &str, card_hash: &str) -> Result<PathBuf> {
    ensure!(
        !udid.is_empty() && udid.len() <= 128,
        "Invalid device identifier"
    );
    Ok(crate::platform::data_dir()
        .join("wallet-backups")
        .join("v2")
        .join(format!(
            "{}-{}.json",
            file_key(&udid.to_ascii_lowercase()),
            card_identity(card_hash)?
        )))
}

#[derive(Debug, Serialize, Deserialize)]
struct OriginalCard {
    schema: u8,
    udid: String,
    identity: String,
    resolved_hash: String,
    assets: ArtworkAssets,
}

impl OriginalCard {
    fn validate(&self, udid: &str, hash: &str) -> Result<()> {
        ensure!(
            matches!(self.schema, 2 | 3) && self.udid.eq_ignore_ascii_case(udid),
            "Original backup belongs to another device or format"
        );
        ensure!(
            self.identity == card_identity(hash)?
                && self.identity == card_identity(&self.resolved_hash)?,
            "Original backup belongs to another Wallet card"
        );
        ensure!(
            !self.assets.is_empty()
                && self.assets.len() <= CARD_ARTWORK_ASSETS.len()
                && (self.schema == 3 || self.assets.len() == CARD_ARTWORK_ASSETS.len()),
            "Original backup is empty or incomplete"
        );
        let mut seen = std::collections::HashSet::new();
        for (asset, bytes) in &self.assets {
            ensure!(
                CARD_ARTWORK_ASSETS.contains(&asset.as_str()) && seen.insert(asset),
                "Original backup has an unexpected or duplicated asset: {asset}"
            );
            validate_artwork(asset, bytes)?;
        }
        Ok(())
    }

    fn load(path: &Path, udid: &str, hash: &str) -> Result<Self> {
        let backup: Self = serde_json::from_slice(&read_bounded(path, 192 * 1024 * 1024)?)
            .context("Could not read original backup manifest")?;
        backup.validate(udid, hash)?;
        Ok(backup)
    }
}

fn validate_artwork(name: &str, data: &[u8]) -> Result<()> {
    ensure!(
        !data.is_empty() && data.len() <= 16 * 1024 * 1024,
        "Invalid size for original {name}"
    );
    if name.ends_with(".png") {
        let image = image::load_from_memory_with_format(data, image::ImageFormat::Png)
            .with_context(|| format!("Original {name} is not a readable PNG"))?;
        ensure!(image.width() > 0 && image.height() > 0, "Empty artwork");
    } else {
        ensure!(
            data.starts_with(b"%PDF-") && data.windows(5).any(|w| w == b"%%EOF"),
            "Original {name} is not a complete PDF"
        );
    }
    Ok(())
}

pub fn backup_exists(udid: &str, card_hash: &str) -> bool {
    // UI availability only. The operation reopens and fully validates the
    // manifest before writing; don't decode megabytes of images every frame.
    backup_path(udid, card_hash).is_ok_and(|path| path.is_file())
}

fn card_hash_candidates(card_hash: &str) -> Vec<String> {
    let trimmed = card_hash.trim_end_matches('=');
    let mut candidates = vec![
        card_hash.to_string(),
        trimmed.to_string(),
        format!("{trimmed}="),
        format!("{trimmed}=="),
    ];
    let mut seen = std::collections::HashSet::new();
    candidates.retain(|s| seen.insert(s.clone()));
    candidates
}

fn capture_at<R>(
    path: &Path,
    udid: &str,
    hash: &str,
    resolved_hash: &str,
    mut read: R,
) -> Result<OriginalCard>
where
    R: FnMut(&str) -> Result<Vec<u8>>,
{
    // A partial previous attempt never becomes a trusted backup. Publish a
    // single complete manifest only after all assets have been read and checked.
    let mut assets = Vec::new();
    for asset in CARD_ARTWORK_ASSETS {
        let data = read(asset)
            .with_context(|| format!("Cannot back up {asset}; no artwork will be changed"))?;
        validate_artwork(asset, &data)?;
        assets.push((asset.to_string(), data));
    }
    let backup = OriginalCard {
        schema: 2,
        udid: udid.into(),
        identity: card_identity(hash)?,
        resolved_hash: resolved_hash.into(),
        assets,
    };
    backup.validate(udid, hash)?;
    atomic_create(path, &serde_json::to_vec(&backup)?)?;
    OriginalCard::load(path, udid, hash)
}

pub fn capture_original_card<L>(
    udid: &str,
    connection_mode: ConnectionMode,
    card_hash: &str,
    mut log: L,
) -> Result<String>
where
    L: FnMut(&str),
{
    let path = backup_path(udid, card_hash)?;
    if path.try_exists()? {
        let backup = OriginalCard::load(&path, udid, card_hash)
            .context("The existing backup is incomplete or damaged; refusing to replace it")?;
        log("Verified the saved originals. Only backed-up artwork files may be replaced.");
        return Ok(backup.resolved_hash);
    }
    let session = ActiveDeviceSession::open(Some(udid), connection_mode)?;
    let afc = AfcClient::new(&session)?;
    let mut resolved = None;
    for candidate in card_hash_candidates(card_hash) {
        if afc.path_exists(&format!(
            "/var/mobile/Library/Passes/Cards/{candidate}.pkpass"
        ))? {
            resolved = Some(candidate);
            break;
        }
    }
    let Some(resolved) = resolved else {
        log("Direct AFC access is unavailable. Using journaled AirTraffic export and return.");
        let pending = PendingBooks::create(udid, snapshot_books(&afc)?)?;
        let exported = (|| -> Result<OriginalCard> {
            for candidate in card_hash_candidates(card_hash) {
                card_identity(&candidate)?;
                let dir = format!("/var/mobile/Library/Passes/Cards/{candidate}.pkpass");
                let assets = crate::protected_files::export_files(
                    &session,
                    &afc,
                    &dir,
                    // The 3x face is used by this iPhone class. Do not require
                    // a PDF or 2x file that may not exist, and never write one
                    // unless a previous verified manifest contains it.
                    &[CARD_ARTWORK_ASSETS[0]],
                    &mut log,
                )?;
                if assets.is_empty() {
                    continue;
                }
                // Schema 3 records only successfully exported files. The writer
                // must never touch an asset missing from this manifest.
                let backup = OriginalCard {
                    schema: 3,
                    udid: udid.into(),
                    identity: card_identity(card_hash)?,
                    resolved_hash: candidate,
                    assets,
                };
                backup.validate(udid, card_hash)?;
                atomic_create(&path, &serde_json::to_vec(&backup)?)?;
                return OriginalCard::load(&path, udid, card_hash);
            }
            bail!(
                "No original artwork was exported. Check the selected card; this AirTraffic operation did not establish compatibility."
            )
        })();
        let books = restore_books(&afc, &pending.snapshot);
        match (exported, books) {
            (Ok(backup), Ok(())) => {
                pending.complete()?;
                log(&format!(
                    "Saved {} original artwork file(s); each was returned to Wallet. Files without a backup will not be replaced.",
                    backup.assets.len()
                ));
                return Ok(backup.resolved_hash);
            }
            (exported, books) => bail!(
                "Original backup incomplete. Export: {}. Books restore: {}. Recovery records retained; use Recover Interrupted Operation.",
                exported
                    .err()
                    .map(|e| format!("{e:#}"))
                    .unwrap_or_else(|| "saved".into()),
                books
                    .err()
                    .map(|e| format!("{e:#}"))
                    .unwrap_or_else(|| "verified".into())
            ),
        }
    };
    let dir = format!("/var/mobile/Library/Passes/Cards/{resolved}.pkpass");
    let backup = capture_at(&path, udid, card_hash, &resolved, |asset| {
        afc.read_file(&format!("{dir}/{asset}"))
    })?;
    log(&format!(
        "Saved and verified complete original artwork backup: {}",
        path.display()
    ));
    Ok(backup.resolved_hash)
}

pub fn load_original_assets(udid: &str, card_hash: &str) -> Result<(String, ArtworkAssets)> {
    let backup = OriginalCard::load(&backup_path(udid, card_hash)?, udid, card_hash)?;
    Ok((backup.resolved_hash, backup.assets))
}

#[cfg(test)]
mod tests {
    use super::*;
    const HASH: &str = "d64fKk0kyHWP11IWV2GRLud4XQk=";

    fn artwork(name: &str) -> Vec<u8> {
        if name.ends_with(".pdf") {
            return b"%PDF-1.4\n%%EOF\n".to_vec();
        }
        let mut output = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(2, 2)
            .write_to(&mut output, image::ImageFormat::Png)
            .unwrap();
        output.into_inner()
    }

    #[test]
    fn inaccessible_or_partial_originals_never_publish_a_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("original.json");
        assert!(capture_at(&path, "phone", HASH, HASH, |_| bail!("access denied")).is_err());
        assert!(!path.exists());
        assert!(
            capture_at(&path, "phone", HASH, HASH, |name| {
                if name.ends_with(".pdf") {
                    bail!("missing PDF");
                }
                Ok(artwork(name))
            })
            .is_err()
        );
        assert!(!path.exists());
    }

    #[test]
    fn complete_backups_are_bound_to_device_and_normalized_card() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("original.json");
        capture_at(&path, "phone", HASH, HASH, |name| Ok(artwork(name))).unwrap();
        assert!(OriginalCard::load(&path, "phone", HASH.trim_end_matches('=')).is_ok());
        assert!(OriginalCard::load(&path, "another-phone", HASH).is_err());
        assert!(capture_at(&path, "phone", HASH, HASH, |name| Ok(artwork(name))).is_err());
        let mut backup = OriginalCard::load(&path, "phone", HASH).unwrap();
        backup.assets[0].1 = b"corrupted".to_vec();
        assert!(backup.validate("phone", HASH).is_err());
    }

    #[test]
    fn card_identifiers_cannot_escape_the_target_directory() {
        for hash in [
            "../test",
            "/var/mobile",
            "a\\b",
            "'d64fKk0kyHWP11IWV2GRLud4XQk='",
            "d64fKk0kyHWP11IWV2GRLud4XQk=.",
        ] {
            assert!(card_identity(hash).is_err(), "accepted {hash}");
        }
        assert_eq!(
            card_identity(HASH).unwrap(),
            card_identity(HASH.trim_end_matches('=')).unwrap()
        );
    }

    #[test]
    fn version_three_only_authorizes_the_assets_actually_saved() {
        let backup = OriginalCard {
            schema: 3,
            udid: "phone".into(),
            identity: card_identity(HASH).unwrap(),
            resolved_hash: HASH.into(),
            assets: vec![(
                CARD_ARTWORK_ASSETS[0].into(),
                artwork(CARD_ARTWORK_ASSETS[0]),
            )],
        };
        assert!(backup.validate("phone", HASH).is_ok());
        assert!(
            !backup
                .assets
                .iter()
                .any(|(name, _)| name == CARD_ARTWORK_ASSETS[1])
        );
        let mut legacy = backup;
        legacy.schema = 2;
        assert!(legacy.validate("phone", HASH).is_err());
    }
}
