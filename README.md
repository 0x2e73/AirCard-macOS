# AirCard macOS — experimental port

A native Rust client for previewing and changing Apple Wallet card artwork, derived from [Lumid-Off/AirCard-Windows](https://github.com/Lumid-Off/AirCard-Windows). The Apple Silicon macOS build uses Apple's system frameworks and the local `usbmuxd` socket. Windows support is retained; CI also defines an Intel Mac build.

**Experimental device operations.** Standard AFC cannot read Wallet artwork directly. This version uses AirTraffic to move selected artwork into Media, save immutable local copies, and return the files to Wallet. Backup therefore performs temporary device writes. A durable recovery record is saved before moving anything. Only artwork present in a validated original backup can be replaced; there is no skip-backup switch. A successful temporary-file probe does not guarantee that a particular Wallet card can be customized.

## Run on macOS

### Download the app

Download **AirCard-macOS-arm64.zip** from [Releases](https://github.com/0x2e73/AirCard-macOS/releases), extract it, and open **AirCard.app** on an Apple Silicon Mac. Rust and the Xcode Command Line Tools are only needed when building from source. The release is experimental, signed ad hoc, and not notarized by Apple. An Intel binary is not included in this initial release.

### Build from source

Requirements: macOS 13 or later, the Xcode Command Line Tools, and a current stable Rust toolchain. The local build produces the architecture of the Mac running it. Apple MobileDevice and AirTrafficHost frameworks must be available; no Windows drivers, iTunes installation, or 3uTools are needed on macOS.

```sh
git clone https://github.com/0x2e73/AirCard-macOS.git
cd AirCard-macOS
cargo test --locked
./scripts/package-macos.sh
open dist/AirCard.app
```

The resulting `dist/AirCard.app` is signed ad hoc for local use, not signed with an Apple Developer identity or notarized. macOS compatibility below the version used for local validation and Intel execution still need testing.

Useful checks that do not contact an iPhone:

```sh
cargo run --locked -- --check-runtime
cargo run --locked -- --preview
cargo run --locked -- --smoke-test
```

`--check-runtime` loads the system libraries and checks every required symbol. `--preview` opens the UI without device discovery or syslog access. `--smoke-test` opens a preview window and closes it automatically; set `AIRCARD_SMOKE_SCREENSHOT` to a PNG path to save a capture of that window using the app's renderer.

## Wallet workflow

There is no built-in artwork gallery. Use **Choose Image…** to import a PNG, JPG, or WebP, drag inside the preview to adjust its crop, and optionally use **Export PNG** to save the 1536×969 result without connecting an iPhone. Each card can use a different image.

1. Make a normal iPhone backup first. This application's artwork backups do not replace a device backup. [Apple Pay information and settings are excluded from normal iPhone backups](https://support.apple.com/en-us/108771).
2. Connect the unlocked iPhone by USB and establish trust in Finder and on the iPhone.
3. Open AirCard and refresh devices. Choose the correct iPhone and transport.
4. Click **Scan**, open Wallet on the iPhone, and select the intended card. Stop scanning when its identifier appears.
5. Click **Back Up Original**. Keep the iPhone connected and unlocked: if AFC cannot access the files directly, this operation temporarily exports `cardBackgroundCombined@2x.png` into Media, saves a local copy, and returns it. This asset was successfully backed up from the tested card; an iPhone's screen scale does not determine which artwork files its issuer supplies. Backup is not a read-only device operation. Only files saved in a validated original manifest are eligible for replacement.
6. If backup succeeds, choose an image, adjust the crop, and click **Apply Card Skin**. Each card has its own backup and can be styled separately.
7. After a successful verified write, close and reopen Wallet. **Restore Original** uses the saved original assets.

If an operation fails, use **Recover Interrupted Operation** before trying again. It returns outstanding exported artwork before restoring the saved Books state. Keep recovery records and original backups; do not substitute another card's backup.

A timeout does not establish that a file is absent: the move itself may have failed. Recovery reports when no exported files were found instead of claiming they were returned. The scanner preserves identifiers exactly as logged, accepts Wallet dashboard/pass events, and ignores generic base64 tokens and AirTraffic's echoes of requested paths. The Mac's Wallet cache may be stale and is not proof of the cards currently installed on an iPhone.

## Safety changes

- Original artwork is stored in a versioned manifest bound to the device and canonical card identifier. Schema 3 permits a subset of known artwork files, and the writer only replaces that subset; this does not claim that unexported files are absent. Existing backups are never overwritten. The original PNGs must decode and the PDF must have a header and end marker; this is not a full PDF parser or a guarantee that Apple will accept the restored file.
- Backups are written atomically, synchronized to disk, and read back before use. Incomplete legacy backups from the Windows version are not accepted.
- Books sync files are saved to a durable recovery journal **before** any staging or device write. The snapshot is limited to the six tracked Books files; it is not a complete backup of the Books library.
- A pending recovery journal blocks subsequent writes. **Recover Interrupted Operation** returns outstanding exports first, then restores the tracked Books files. It does not undo an already applied skin; use **Restore Original** separately afterwards. Exported originals are never deleted during recovery. Failed-operation staging files can remain on the phone.
- AirTraffic runs in a bounded child process, which is stopped and reaped on timeout before cleanup. A separate lock prevents recovery from racing a helper that outlived the UI. Already-dispatched iPhone operations cannot be cancelled with certainty.
- Cross-process locks serialize changes per iPhone. Write destinations are restricted to known Wallet artwork/cache names, and path components are validated.
- No automatic artwork-write retries after partial failure. Artwork is verified through export/read-back; direct reads of private paths are no longer assumed to work. Returning an export is checked by observing its removal from Media after the return operation; this cannot guarantee cancellation of already queued iPhone work. Books files are verified by AFC read-back. Derived Wallet caches are exported and removed, rather than overwritten with corrupt bytes.
- Passcode themes can be imported and previewed; **keypad writes are disabled** until there is a complete backup/restore implementation. Theme archive sizes are bounded.

These changes reduce avoidable failures; they do not make the underlying AirTraffic exploit safe or transactional. A disconnect, framework incompatibility, concurrent Finder sync, or device crash can still leave partial changes. Close other device-management/sync apps before a deliberate write and keep an alternative way to pay.

## Local files

On macOS: `~/Library/Application Support/AirCard/`.
On Windows: `%LOCALAPPDATA%\AirCard\`.

- `cards.json`: discovered card names and identifiers.
- `settings.json`: UI language.
- `wallet-backups/v2/`: immutable original artwork manifests.
- `recovery/`: unfinished Books and export recovery journals; `exported-files/` retains immutable local recovery copies even after a successful operation.
- `locks/`: operation lock files (the OS releases locks when the owning process exits).

Keep the backup directory. Diagnostic logs may contain device/card identifiers; review them before sharing.

## Development and validation

The temporary-file probe passed on an iPhone 14 Pro Max (`iPhone15,3`) running iOS 26.6: create, overwrite with different bytes, export/read-back, return, a second read after return, and removal. A subsequent Wallet backup successfully exported, validated, saved and returned a real card's `cardBackgroundCombined@2x.png`; exports of its 3x PNG and PDF timed out. The tracked Books state was restored. Applying a replacement design to that card is not yet validated. These checks do not guarantee compatibility with other cards or iOS versions.

```sh
cargo fmt --all -- --check
cargo test --locked
cargo check --locked
./scripts/package-macos.sh
./dist/AirCard.app/Contents/MacOS/aircard --check-runtime
AIRCARD_SMOKE_SCREENSHOT="$PWD/dist/smoke-test.png" ./dist/AirCard.app/Contents/MacOS/aircard --smoke-test
```

Explicit device checks (these perform temporary writes on the connected iPhone):

```sh
cargo run --locked -- --probe-device
cargo run --locked -- --recover-device
```

For explicit diagnostics, `--back-up-card <exact-pass-ID> [artwork-filename]` uses the same locks, recovery and immutable backup validation as the UI. Its default is the 2x PNG; only the three known artwork filenames are accepted. It never overwrites an existing backup or automatically retries another file after an error. `--wallet-log` and `--device-log` read a bounded 90-second device log stream without changing Wallet files; their output can include private identifiers.

The probe uses one randomly named file in `Library/Caches` to exercise writing, overwriting, export/read-back, return and removal. It also snapshots and restores the tracked Books sync files. It does not access Wallet passes. Exactly one unlocked USB iPhone must be connected.

Default tests use temporary files and do not contact an iPhone. Four inherited integration tests are explicitly ignored because they access real devices or the user's card database. Only run ignored tests deliberately on a test setup. CI builds artifacts for Apple Silicon, Intel Mac, and Windows, without publishing releases automatically.

## Credits and license

MIT; see [LICENSE](LICENSE). This fork retains the original project's license and history.

- [Lumid-Off/AirCard-Windows](https://github.com/Lumid-Off/AirCard-Windows): Rust/egui client and Windows port.
- [Mak5er/AirCard](https://github.com/Mak5er/AirCard): original macOS project and Wallet research.
- [0xjohnnydev/airlift](https://github.com/0xjohnnydev/airlift): AirTraffic/ATAirlock research and protocol reference.

This is an unofficial experimental fork, not an Apple or Revolut product.
