//! Export known Wallet files through AirTraffic, then return the same files.
//! Every export has a durable plan and immutable local copies. Recovery only
//! returns outstanding exports; it never deletes them or repeats an export.
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::afc::AfcClient;
use crate::airlift::{build_books_plist, build_streaming_zip_archive_multi, stage_streaming_zip};
use crate::device::ActiveDeviceSession;
use crate::safety::{PendingBooks, atomic_create, file_key, read_bounded, remove_durable};
use crate::wallet_backup::{CARD_ARTWORK_ASSETS, card_identity};

pub const CACHE_FILES: [&str; 3] = ["FrontFace", "PlaceHolder", "Preview"];
pub const PROBE_DIR: &str = "/var/mobile/Library/Caches";
pub type FileBytes = Vec<(String, Vec<u8>)>;

fn valid_token(token: &str) -> bool {
    token.len() == 32 && token.bytes().all(|c| c.is_ascii_hexdigit())
}

pub fn validate_target(dir: &str, leaves: &[String]) -> Result<()> {
    ensure!(
        !leaves.is_empty() && leaves.len() <= 3,
        "Invalid export batch size"
    );
    let mut seen = std::collections::HashSet::new();
    let allowed: &[&str] = if dir == PROBE_DIR {
        &[]
    } else {
        let component = dir
            .strip_prefix("/var/mobile/Library/Passes/Cards/")
            .context("Only known Wallet artwork, caches or the temporary probe may be accessed")?;
        if let Some(hash) = component.strip_suffix(".pkpass") {
            card_identity(hash)?;
            &CARD_ARTWORK_ASSETS
        } else {
            let hash = component
                .strip_suffix(".pkcache")
                .or_else(|| component.strip_suffix(".cache"))
                .context("Unsupported Wallet directory")?;
            card_identity(hash)?;
            &CACHE_FILES
        }
    };
    for leaf in leaves {
        let permitted = if dir == PROBE_DIR {
            leaves.len() == 1
                && leaf
                    .strip_prefix("aircard-probe-")
                    .and_then(|s| s.strip_suffix(".bin"))
                    .is_some_and(valid_token)
        } else {
            allowed.contains(&leaf.as_str())
        };
        ensure!(
            permitted && seen.insert(leaf),
            "Unexpected or duplicate file: {leaf}"
        );
    }
    Ok(())
}

pub fn recovery_path(udid: &str) -> PathBuf {
    recovery_path_at(&crate::platform::data_dir(), udid)
}

fn recovery_path_at(root: &Path, udid: &str) -> PathBuf {
    root.join("recovery").join(format!(
        "{}-export.json",
        file_key(&udid.to_ascii_lowercase())
    ))
}

#[derive(Debug, Serialize, Deserialize)]
struct ExportPlan {
    schema: u8,
    udid: String,
    dir: String,
    leaves: Vec<String>,
    token: String,
    return_token: String,
    #[serde(default)]
    discard: bool,
}

impl ExportPlan {
    fn validate(&self, udid: &str) -> Result<()> {
        ensure!(
            self.schema == 1 && self.udid.eq_ignore_ascii_case(udid),
            "Export recovery belongs to another device or format"
        );
        ensure!(
            !udid.is_empty() && udid.len() <= 128,
            "Invalid device identifier"
        );
        ensure!(
            valid_token(&self.token)
                && valid_token(&self.return_token)
                && self.token != self.return_token,
            "Invalid export recovery token"
        );
        validate_target(&self.dir, &self.leaves).and_then(|_| {
            ensure!(
                !self.discard
                    || self.dir == PROBE_DIR
                    || self.dir.ends_with(".cache")
                    || self.dir.ends_with(".pkcache"),
                "Original Wallet artwork may never be discarded during export"
            );
            Ok(())
        })
    }

    fn source(token: &str) -> String {
        format!("airlift-src-{token}")
    }
    fn link(token: &str) -> String {
        format!("airlift-link-{token}")
    }
    fn recovered(&self, index: usize) -> String {
        format!("airlift-recovered-{}-{index}", self.token)
    }
    fn local_copy(&self, root: &Path, index: usize) -> PathBuf {
        root.join("recovery")
            .join("exported-files")
            .join(file_key(&self.udid.to_ascii_lowercase()))
            .join(&self.token)
            .join(&self.leaves[index])
    }
    fn checkpoint(&self, root: &Path, name: &str) -> PathBuf {
        self.local_copy(root, 0).parent().unwrap().join(name)
    }

    fn load(root: &Path, udid: &str) -> Result<Self> {
        let plan: Self =
            serde_json::from_slice(&read_bounded(&recovery_path_at(root, udid), 16 * 1024)?)?;
        plan.validate(udid)?;
        Ok(plan)
    }
}

trait ExportIo {
    fn exists(&mut self, path: &str) -> Result<bool>;
    fn read(&mut self, path: &str) -> Result<Vec<u8>>;
    fn stage(&mut self, dir: &str, token: &str) -> Result<()>;
    fn sync(&mut self, assets: &[(String, String)]) -> Result<()>;
    fn cleanup(&mut self, token: &str) -> Result<()>;
    fn discard(&mut self, path: &str) -> Result<()>;
}

struct DeviceIo<'a, L> {
    session: &'a ActiveDeviceSession,
    afc: &'a AfcClient,
    log: &'a mut L,
}

impl<L: FnMut(&str)> ExportIo for DeviceIo<'_, L> {
    fn exists(&mut self, path: &str) -> Result<bool> {
        self.afc.path_exists(path)
    }
    fn read(&mut self, path: &str) -> Result<Vec<u8>> {
        self.afc.read_file(path)
    }
    fn stage(&mut self, dir: &str, token: &str) -> Result<()> {
        let source = ExportPlan::source(token);
        let archive = build_streaming_zip_archive_multi(dir, &[])?;
        let stage = stage_streaming_zip(self.session, &source, &archive);
        // StreamingZip may return an error after extracting a complete archive.
        // The known link is still inside its staging tree at this point.
        if self.afc.path_exists(&format!("{source}/p0/p1/p2/link"))? {
            if let Err(error) = stage {
                (self.log)(&format!("StreamingZip: {error:#}; staging link verified."));
            }
            Ok(())
        } else {
            stage?;
            bail!("Export staging link was not created")
        }
    }
    fn sync(&mut self, assets: &[(String, String)]) -> Result<()> {
        let pending = PendingBooks::load(&self.session.udid)?;
        crate::airlift::restore_books(self.afc, &pending.snapshot)?;
        std::thread::sleep(Duration::from_millis(800));
        let ids: Vec<_> = assets.iter().map(|(a, _)| a.clone()).collect();
        self.afc.make_directory_recursive("Books/Sync")?;
        self.afc
            .write_file("Books/Sync/Books.plist", &build_books_plist(&ids)?)?;
        let refs: Vec<_> = assets
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        crate::airtraffic::sync_assets_via_airtraffic(
            &self.session.udid,
            self.session.transport,
            &refs,
            &mut *self.log,
        )
    }
    fn cleanup(&mut self, token: &str) -> Result<()> {
        self.afc.remove_path(&ExportPlan::link(token))?;
        self.afc.remove_tree(&ExportPlan::source(token))
    }
    fn discard(&mut self, path: &str) -> Result<()> {
        self.afc.remove_path(path)
    }
}

fn link_move(token: &str) -> (String, String) {
    (
        format!("../../{}/p0/p1/p2/link", ExportPlan::source(token)),
        ExportPlan::link(token),
    )
}

fn protected_identifier(dir: &str, leaf: &str) -> Result<String> {
    Ok(format!(
        "../../../{}/{}",
        dir.strip_prefix("/var/mobile/")
            .context("Unexpected file root")?,
        leaf
    ))
}

fn save_copy(root: &Path, plan: &ExportPlan, index: usize, data: &[u8]) -> Result<()> {
    let path = plan.local_copy(root, index);
    if path.try_exists()? {
        ensure!(
            read_bounded(&path, 64 * 1024 * 1024)? == data,
            "Export changed since the saved recovery copy; keeping both copies"
        );
    } else {
        atomic_create(&path, data)?;
    }
    Ok(())
}

/// Return the very same exported files using move semantics, preserving their
/// contents and file attributes. Never remove or rewrite an outstanding export.
fn return_exports(io: &mut impl ExportIo, root: &Path, plan: &ExportPlan) -> Result<usize> {
    let mut moves = Vec::new();
    let mut handled = 0;
    for (i, leaf) in plan.leaves.iter().enumerate() {
        let exported = plan.recovered(i);
        if io.exists(&exported)? {
            if plan.discard {
                io.discard(&exported)?;
                ensure!(
                    !io.exists(&exported)?,
                    "Temporary file or cache was not removed"
                );
                handled += 1;
                continue;
            }
            // Best-effort salvage must not prevent moving the original home if
            // the local disk is full or AFC cannot read it.
            if let Ok(data) = io.read(&exported) {
                let _ = save_copy(root, plan, i, &data);
            }
            moves.push((
                format!("../../{exported}"),
                format!("{}/{leaf}", ExportPlan::link(&plan.return_token)),
            ));
        }
    }
    if moves.is_empty() {
        return Ok(handled);
    }
    handled += moves.len();
    io.cleanup(&plan.return_token)?;
    io.stage(&plan.dir, &plan.return_token)?;
    moves.insert(0, link_move(&plan.return_token));
    io.sync(&moves)?;
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let mut remains = false;
        for i in 0..plan.leaves.len() {
            remains |= io.exists(&plan.recovered(i))?;
        }
        if !remains {
            return Ok(handled);
        }
        ensure!(
            Instant::now() < deadline,
            "An original file is still exported; recovery data retained. Reconnect and use Recover Interrupted Operation."
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn cleanup_plan(io: &mut impl ExportIo, root: &Path, plan: &ExportPlan) -> Result<()> {
    for i in 0..plan.leaves.len() {
        ensure!(
            !io.exists(&plan.recovered(i))?,
            "Refusing to clean up an outstanding original file"
        );
    }
    io.cleanup(&plan.token)?;
    io.cleanup(&plan.return_token)?;
    remove_durable(&recovery_path_at(root, &plan.udid))
}

fn execute_export(io: &mut impl ExportIo, root: &Path, plan: &ExportPlan) -> Result<FileBytes> {
    plan.validate(&plan.udid)?;
    atomic_create(
        &recovery_path_at(root, &plan.udid),
        &serde_json::to_vec(plan)?,
    )?;
    let read_result = (|| -> Result<FileBytes> {
        io.stage(&plan.dir, &plan.token)?;
        let mut moves = vec![link_move(&plan.token)];
        for (i, leaf) in plan.leaves.iter().enumerate() {
            moves.push((protected_identifier(&plan.dir, leaf)?, plan.recovered(i)));
        }
        atomic_create(&plan.checkpoint(root, "export-started"), b"1")?;
        io.sync(&moves)?;
        // FileComplete only queues a move. The upstream canary waits up to
        // 15 seconds for read-back; an absent path immediately after sync is
        // not proof of a missing original. Never clear recovery on that basis.
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let mut all_present = true;
            for i in 0..plan.leaves.len() {
                all_present &= io.exists(&plan.recovered(i))?;
            }
            if all_present {
                break;
            }
            if Instant::now() >= deadline {
                if plan.discard {
                    break;
                }
                bail!(
                    "No exported artwork appeared within 15 seconds. Check the exact card ID and artwork filename. A missing file and an unsuccessful move cannot be distinguished here; recovery data retained."
                );
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        let mut data = Vec::new();
        for (i, leaf) in plan.leaves.iter().enumerate() {
            if io.exists(&plan.recovered(i))? {
                let bytes = io.read(&plan.recovered(i))?;
                save_copy(root, plan, i, &bytes)?;
                data.push((leaf.clone(), bytes));
            }
        }
        Ok(data)
    })();
    // Attempt to return exported originals even if sync or saving a local copy
    // failed. The helper has exited; no detached host thread keeps writing.
    let returned = return_exports(io, root, plan);
    match (read_result, returned) {
        (Ok(data), Ok(_)) => {
            cleanup_plan(io, root, plan)?;
            Ok(data)
        }
        (read, returned) => bail!(
            "Export incomplete. Read: {}. Recovery: {}. Recovery records retained; use Recover Interrupted Operation.",
            read.err()
                .map(|e| format!("{e:#}"))
                .unwrap_or_else(|| "saved".into()),
            match returned {
                Ok(0) => "no exported files were found to return".into(),
                Ok(count) if plan.discard => format!("{count} temporary/cache file(s) removed"),
                Ok(count) => format!("{count} exported file(s) returned"),
                Err(error) => format!("{error:#}"),
            }
        ),
    }
}

/// Caller holds the device operation lock and has already saved PendingBooks.
pub fn export_files<L: FnMut(&str)>(
    session: &ActiveDeviceSession,
    afc: &AfcClient,
    dir: &str,
    leaves: &[&str],
    log: &mut L,
) -> Result<FileBytes> {
    transfer_files(session, afc, dir, leaves, false, log)
}

pub fn discard_files<L: FnMut(&str)>(
    session: &ActiveDeviceSession,
    afc: &AfcClient,
    dir: &str,
    leaves: &[&str],
    log: &mut L,
) -> Result<()> {
    transfer_files(session, afc, dir, leaves, true, log).map(|_| ())
}

fn transfer_files<L: FnMut(&str)>(
    session: &ActiveDeviceSession,
    afc: &AfcClient,
    dir: &str,
    leaves: &[&str],
    discard: bool,
    log: &mut L,
) -> Result<FileBytes> {
    PendingBooks::load(&session.udid).context("Books snapshot must exist before export")?;
    let plan = ExportPlan {
        schema: 1,
        udid: session.udid.clone(),
        dir: dir.into(),
        leaves: leaves.iter().map(|s| s.to_string()).collect(),
        token: crate::platform::random_hex()?,
        return_token: crate::platform::random_hex()?,
        discard,
    };
    log(if discard {
        "Moving derived caches / temporary probe into Media for removal..."
    } else {
        "Exporting known files to Media, saving local copies, then returning the originals..."
    });
    let result = execute_export(
        &mut DeviceIo { session, afc, log },
        &crate::platform::data_dir(),
        &plan,
    );
    if result.is_ok() {
        log(if discard {
            "Derived caches / temporary probe removed."
        } else {
            "Exported files read and returned. Local recovery copies retained."
        });
    }
    result
}

pub fn recover_export<L: FnMut(&str)>(
    session: &ActiveDeviceSession,
    afc: &AfcClient,
    log: &mut L,
) -> Result<()> {
    let root = crate::platform::data_dir();
    if !recovery_path(&session.udid).try_exists()? {
        return Ok(());
    }
    let plan = ExportPlan::load(&root, &session.udid)?;
    let mut io = DeviceIo { session, afc, log };
    let count = return_exports(&mut io, &root, &plan)?;
    (io.log)(&format!(
        "Recovery handled {count} outstanding exported file(s)."
    ));
    cleanup_plan(&mut io, &root, &plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    #[derive(Default)]
    struct FakeIo {
        files: HashMap<String, Vec<u8>>,
        fail_export: bool,
        fail_return: bool,
        fail_read: bool,
        moves: usize,
    }
    impl ExportIo for FakeIo {
        fn exists(&mut self, path: &str) -> Result<bool> {
            Ok(self.files.contains_key(path))
        }
        fn read(&mut self, path: &str) -> Result<Vec<u8>> {
            ensure!(!self.fail_read, "read disconnected");
            self.files.get(path).cloned().context("missing")
        }
        fn stage(&mut self, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        fn sync(&mut self, moves: &[(String, String)]) -> Result<()> {
            for (from, to) in moves.iter().skip(1) {
                let returning = from.starts_with("../../airlift-recovered-");
                if returning && self.fail_return {
                    bail!("return disconnected");
                }
                let from = if returning {
                    from.strip_prefix("../../").unwrap().to_string()
                } else {
                    from.rsplit('/').next().unwrap().to_string()
                };
                let to = if returning {
                    to.rsplit('/').next().unwrap().to_string()
                } else {
                    to.clone()
                };
                if let Some(bytes) = self.files.remove(&from) {
                    self.files.insert(to, bytes);
                    self.moves += 1;
                }
                if !returning && self.fail_export {
                    bail!("export disconnected after moving one file");
                }
            }
            Ok(())
        }
        fn cleanup(&mut self, _: &str) -> Result<()> {
            Ok(())
        }
        fn discard(&mut self, path: &str) -> Result<()> {
            self.files.remove(path);
            Ok(())
        }
    }
    fn plan() -> ExportPlan {
        ExportPlan {
            schema: 1,
            udid: "phone".into(),
            dir: "/var/mobile/Library/Passes/Cards/d64fKk0kyHWP11IWV2GRLud4XQk=.pkpass".into(),
            leaves: CARD_ARTWORK_ASSETS.iter().map(|s| s.to_string()).collect(),
            token: "a".repeat(32),
            return_token: "b".repeat(32),
            discard: false,
        }
    }
    fn phone(p: &ExportPlan) -> FakeIo {
        FakeIo {
            files: p
                .leaves
                .iter()
                .map(|s| (s.clone(), s.as_bytes().to_vec()))
                .collect(),
            ..Default::default()
        }
    }
    #[test]
    fn successful_export_returns_originals_and_keeps_local_copies() {
        let root = tempfile::tempdir().unwrap();
        let p = plan();
        let mut io = phone(&p);
        let before = io.files.clone();
        let data = execute_export(&mut io, root.path(), &p).unwrap();
        assert_eq!(data.len(), 3);
        assert_eq!(io.files, before);
        assert_eq!(io.moves, 6);
        assert!(!recovery_path_at(root.path(), "phone").exists());
        for i in 0..3 {
            assert_eq!(
                std::fs::read(p.local_copy(root.path(), i)).unwrap(),
                p.leaves[i].as_bytes()
            );
        }
    }
    #[test]
    fn partial_export_failure_returns_the_file_but_retains_journal() {
        let root = tempfile::tempdir().unwrap();
        let p = plan();
        let mut io = phone(&p);
        let before = io.files.clone();
        io.fail_export = true;
        assert!(execute_export(&mut io, root.path(), &p).is_err());
        assert_eq!(io.files, before);
        assert!(ExportPlan::load(root.path(), "phone").is_ok());
    }
    #[test]
    fn interrupted_return_can_resume_without_reexporting_or_deleting_originals() {
        let root = tempfile::tempdir().unwrap();
        let p = plan();
        let mut io = phone(&p);
        let before = io.files.clone();
        io.fail_return = true;
        assert!(execute_export(&mut io, root.path(), &p).is_err());
        for i in 0..3 {
            assert!(io.files.contains_key(&p.recovered(i)));
            assert!(p.local_copy(root.path(), i).exists());
        }
        let loaded = ExportPlan::load(root.path(), "phone").unwrap();
        io.fail_return = false;
        return_exports(&mut io, root.path(), &loaded).unwrap();
        cleanup_plan(&mut io, root.path(), &loaded).unwrap();
        assert_eq!(io.files, before);
        assert_eq!(io.moves, 6);
    }
    #[test]
    fn read_failure_still_returns_original_files() {
        let root = tempfile::tempdir().unwrap();
        let p = plan();
        let mut io = phone(&p);
        let before = io.files.clone();
        io.fail_read = true;
        assert!(execute_export(&mut io, root.path(), &p).is_err());
        assert_eq!(io.files, before);
    }
    #[test]
    fn recovery_does_not_claim_a_return_when_nothing_was_exported() {
        let root = tempfile::tempdir().unwrap();
        let p = plan();
        let mut io = phone(&p);
        let before = io.files.clone();
        assert_eq!(return_exports(&mut io, root.path(), &p).unwrap(), 0);
        assert_eq!(io.files, before);
        assert_eq!(io.moves, 0);
    }
    #[test]
    fn recovery_plan_cannot_target_other_files_or_devices() {
        let mut p = plan();
        assert!(p.validate("other").is_err());
        p.leaves[0] = "../../escape".into();
        assert!(p.validate("phone").is_err());
        let mut p = plan();
        p.dir = "/var/mobile/Library/Safari".into();
        assert!(p.validate("phone").is_err());
        let mut p = plan();
        p.token = "../escape".into();
        assert!(p.validate("phone").is_err());
        let mut p = plan();
        p.discard = true;
        assert!(p.validate("phone").is_err());
    }

    #[test]
    fn no_device_move_if_the_recovery_record_cannot_be_saved() {
        let root = tempfile::tempdir().unwrap();
        let p = plan();
        let mut io = phone(&p);
        std::fs::write(root.path().join("recovery"), b"not a directory").unwrap();
        let before = io.files.clone();
        assert!(execute_export(&mut io, root.path(), &p).is_err());
        assert_eq!(io.moves, 0);
        assert_eq!(io.files, before);
    }
}
