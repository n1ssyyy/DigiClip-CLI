use anyhow::Result;
use clap::Parser;
use digiclip_rs::cli::Args;
use digiclip_rs::pipeline;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("digiclip_rs=info")),
        )
        .with_target(false)
        .without_time()
        .init();

    let args = Args::parse();
    if args.serve {
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
