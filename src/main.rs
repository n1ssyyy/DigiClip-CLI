use anyhow::Result;
use clap::Parser;
use digiclip_rs::cli::Args;
use digiclip_rs::pipeline;

fn filter() -> tracing_subscriber::EnvFilter {
    tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("digiclip_rs=info"))
}

/// `<data>/logs/serve.log` (the previous one kept as `serve.log.1` once it
/// passes 5 MB), for the diagnostics export.
fn serve_log(data_dir: &std::path::Path) -> Option<std::fs::File> {
    let dir = data_dir.join("logs");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("serve.log");
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 5 << 20) {
        let _ = std::fs::rename(&path, dir.join("serve.log.1"));
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()
}

fn main() -> Result<()> {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let args = Args::parse();
    let console = tracing_subscriber::fmt::layer()
        .with_target(false)
        .without_time();
    let file = args
        .serve
        .then(|| {
            serve_log(
                &args
                    .data_dir
                    .clone()
                    .unwrap_or_else(digiclip_rs::provision::root),
            )
        })
        .flatten()
        .map(|f| {
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_target(false)
                .with_writer(std::sync::Mutex::new(f))
        });
    tracing_subscriber::registry()
        .with(filter())
        .with(console)
        .with(file)
        .init();

    digiclip_rs::process::gentle_self();
    if args.serve {
        // Never outlive our spawner: an orphaned engine keeps
        // `resources\digiclip.exe` locked and the next install/update
        // fails with "Error opening file for writing".
        digiclip_rs::watchdog::watch_parent();
        // Own runtime (fixed workers): the default runtime sizes itself
        // to the machine and a 1-CPU sandbox would starve the socket pump.
        return digiclip_rs::serve::run_serve_blocking(
            args.port,
            args.token.clone(),
            args.data_dir.clone(),
        );
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(pipeline::run(args))
}
