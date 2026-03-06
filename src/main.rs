mod args;
mod config;
mod endpoint;
mod error;
mod storage;

use clap::Parser;
use tracing::{Level, error, info};
use tracing_subscriber::FmtSubscriber;

use crate::args::Args;
use crate::config::AppConfig;
use crate::endpoint::serve;
use crate::error::AppError;

#[tokio::main]
async fn main() -> Result<(), AppError> {
    let app_name = env!("CARGO_PKG_NAME");

    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("setting default subscriber failed");

    let args = Args::parse();
    info!("Starting application with verbosity: {}", args.verbose);

    let cfg: AppConfig = confy::load(app_name, None)?;
    info!("Configuration loaded: {:?}", cfg);

    if let Err(e) = app(cfg).await {
        error!("Application crashed: {}", e);
        return Err(e);
    }

    Ok(())
}

async fn app(config: AppConfig) -> Result<(), AppError> {
    serve(config.endpoint).await?;

    Ok(())
}
