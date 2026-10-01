# AirCard macOS — experimental port

A native Rust client for previewing and changing Apple Wallet card artwork, derived from [Lumid-Off/AirCard-Windows](https://github.com/Lumid-Off/AirCard-Windows). The Apple Silicon macOS build uses Apple's system frameworks and the local `usbmuxd` socket. Windows support is retained; CI also defines an Intel Mac build.

**A running Mac app does not establish iPhone compatibility.** This port has not been validated by writing to an iPhone 14 Pro Max / iOS 26.6. Apple's standard AFC file service normally exposes the Media directory, not Wallet's private artwork directory. If the original artwork cannot be read, this version deliberately refuses to apply a skin. There is no “continue without backup” option and no additional exploit to move private files out for backup.

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
5. Click **Back Up Original**. This only reads device files and saves them on the computer. It must obtain all three original artwork files (`@3x.png`, `@2x.png`, and `.pdf`) and verify the local backup. A card that lacks any of these files is also blocked; supporting absent original assets safely requires a different restore mechanism.
6. If backup succeeds, choose an image, adjust the crop, and click **Apply Card Skin**. Each card has its own backup and can be styled separately.
7. After a successful verified write, close and reopen Wallet. **Restore Original** uses the saved original assets.

If backup is refused, stop: this device/card cannot currently be modified with this port's backup requirement. Do not delete recovery files or substitute an unrelated card's backup to enable writes.

## Safety changes

- Original artwork is stored in a complete versioned manifest bound to the device and canonical card identifier. Existing backups are never overwritten. The original PNGs must decode and the PDF must have a header and end marker; this is not a full PDF parser or a guarantee that Apple will accept the restored file.
- Backups are written atomically, synchronized to disk, and read back before use. Incomplete legacy backups from the Windows version are not accepted.
- Books sync files are saved to a durable recovery journal **before** any staging or device write. The snapshot is limited to the six tracked Books files; it is not a complete backup of the Books library.
- A pending recovery journal blocks subsequent writes. **Restore Books** restores and verifies those tracked files, then clears the journal. It does not undo a partially changed card face; use **Restore Original** separately afterwards. Failed-operation staging files may remain on the phone.
- AirTraffic runs in a bounded child process, which is stopped and reaped on timeout before cleanup. A separate lock prevents recovery from racing a helper that outlived the UI. Already-dispatched iPhone operations cannot be cancelled with certainty.
- Cross-process locks serialize changes per iPhone. Write destinations are restricted to known Wallet artwork/cache names, and path components are validated.
- No automatic write retries after partial failure. Artwork and Books restoration are checked by reading bytes back. Cache failures are reported instead of being silently ignored.
- Passcode themes can be imported and previewed; **keypad writes are disabled** until there is a complete backup/restore implementation. Theme archive sizes are bounded.

These changes reduce avoidable failures; they do not make the underlying AirTraffic exploit safe or transactional. A disconnect, framework incompatibility, concurrent Finder sync, or device crash can still leave partial changes. Close other device-management/sync apps before a deliberate write and keep an alternative way to pay.

## Local files

On macOS: `~/Library/Application Support/AirCard/`.
On Windows: `%LOCALAPPDATA%\AirCard\`.

- `cards.json`: discovered card names and identifiers.
- `settings.json`: UI language.
- `wallet-backups/v2/`: immutable original artwork manifests.
- `recovery/`: unfinished Books recovery journals.
- `locks/`: operation lock files (the OS releases locks when the owning process exits).

Keep the backup directory. Diagnostic logs may contain device/card identifiers; review them before sharing.

## Development and validation

```sh
cargo fmt --all -- --check
cargo test --locked
cargo check --locked
./scripts/package-macos.sh
./dist/AirCard.app/Contents/MacOS/aircard --check-runtime
AIRCARD_SMOKE_SCREENSHOT="$PWD/dist/smoke-test.png" ./dist/AirCard.app/Contents/MacOS/aircard --smoke-test
```

Default tests use temporary files and do not contact an iPhone. Four inherited integration tests are explicitly ignored because they access real devices or the user's card database. Only run ignored tests deliberately on a test setup. CI builds artifacts for Apple Silicon, Intel Mac, and Windows, without publishing releases automatically.

## Credits and license

MIT; see [LICENSE](LICENSE). This fork retains the original project's license and history.

- [Lumid-Off/AirCard-Windows](https://github.com/Lumid-Off/AirCard-Windows): Rust/egui client and Windows port.
- [Mak5er/AirCard](https://github.com/Mak5er/AirCard): original macOS project and Wallet research.
- [0xjohnnydev/airlift](https://github.com/0xjohnnydev/airlift): AirTraffic/ATAirlock research and protocol reference.

This is an unofficial experimental fork, not an Apple or Revolut product.
