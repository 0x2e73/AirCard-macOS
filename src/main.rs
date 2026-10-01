#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod afc;
mod airlift;
mod airtraffic;
mod app;
mod apple;
mod device;
mod flasher;
mod i18n;
mod image_skin;
mod passthm;
mod platform;
mod protected_files;
mod safety;
mod scanner;
mod wallet_backup;

fn main() -> eframe::Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|arg| {
        matches!(
            arg.as_str(),
            "--probe-device"
                | "--recover-device"
                | "--device-log"
                | "--wallet-log"
                | "--back-up-card"
        )
    }) {
        let result = (|| -> anyhow::Result<()> {
            let devices = device::list_connected_devices()?;
            anyhow::ensure!(
                devices.len() == 1,
                "Connect exactly one unlocked iPhone for this command"
            );
            let phone = &devices[0];
            println!(
                "Selected {} ({}, iOS {}) over USB",
                phone.name, phone.product_type, phone.ios_version
            );
            if args[0] == "--device-log" || args[0] == "--wallet-log" {
                scanner::diagnostic_log(&phone.udid, args[0] == "--wallet-log")
            } else if args[0] == "--back-up-card" {
                anyhow::ensure!(
                    matches!(args.len(), 2 | 3),
                    "Usage: --back-up-card <exact pass ID> [artwork filename]"
                );
                let _guard = safety::OperationGuard::acquire(&phone.udid)?;
                safety::PendingBooks::ensure_clear(&phone.udid)?;
                wallet_backup::capture_original_card_asset(
                    &phone.udid,
                    device::ConnectionMode::Usb,
                    &args[1],
                    args.get(2)
                        .map(String::as_str)
                        .unwrap_or(wallet_backup::CARD_ARTWORK_ASSETS[1]),
                    |message| println!("{message}"),
                )?;
                Ok(())
            } else if args[0] == "--recover-device" {
                flasher::recover_books(&phone.udid, device::ConnectionMode::Usb)
            } else {
                flasher::probe_device(&phone.udid, device::ConnectionMode::Usb, |s| {
                    println!("{s}")
                })
            }
        })();
        if let Err(error) = result {
            eprintln!("{error:#}");
            std::process::exit(1);
        }
        return Ok(());
    }
    if args.first().is_some_and(|arg| arg == "--airtraffic-worker") {
        match airtraffic::run_worker() {
            Ok(()) => std::process::exit(0),
            Err(error) => {
                eprintln!("{error:#}");
                std::process::exit(1);
            }
        }
    }
    if args.iter().any(|arg| arg == "--check-runtime") {
        match apple::verify_support() {
            Ok(message) => {
                println!("{message}");
                return Ok(());
            }
            Err(error) => {
                eprintln!("{error:#}");
                std::process::exit(1);
            }
        }
    }
    if args.iter().any(|arg| arg == "--help") {
        println!(
            "AirCard {}\n  --preview         Open without connecting to an iPhone\n  --check-runtime   Check Apple libraries without contacting devices\n  --smoke-test      Open a preview window and close automatically\n  --probe-device    Test a temporary file on exactly one connected USB iPhone\n  --recover-device  Resume pending recovery on exactly one connected USB iPhone\n  --back-up-card ID [FILE]  Back up one known artwork asset (default: 2x PNG)\n  --wallet-log      Read Wallet/transfer diagnostics for 90 seconds\n  --device-log      Read transfer diagnostics for 90 seconds",
            env!("CARGO_PKG_VERSION")
        );
        return Ok(());
    }
    let smoke = args.iter().any(|arg| arg == "--smoke-test");
    let preview = smoke || args.iter().any(|arg| arg == "--preview");

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1000.0, 640.0])
            .with_min_inner_size([880.0, 560.0])
            .with_title(concat!("AirCard v", env!("CARGO_PKG_VERSION"))),
        ..Default::default()
    };

    eframe::run_native(
        concat!("AirCard v", env!("CARGO_PKG_VERSION")),
        options,
        Box::new(move |cc| Ok(Box::new(app::AirCardApp::new(cc, preview, smoke)))),
    )
}
