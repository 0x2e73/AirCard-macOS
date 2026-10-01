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
mod safety;
mod scanner;
mod wallet_backup;

fn main() -> eframe::Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
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
            "AirCard {}\n  --preview         Open without connecting to an iPhone\n  --check-runtime   Check Apple libraries without contacting devices\n  --smoke-test      Open a preview window and close automatically",
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
