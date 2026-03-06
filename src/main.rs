mod args;
mod config;
mod error;

use clap::Parser;
use tracing::{Level, error, info};
use tracing_subscriber::FmtSubscriber;

use crate::args::Args;
use crate::config::AppConfig;
use crate::error::AppError;

fn main() -> Result<(), AppError> {
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("setting default subscriber failed");

    let args = Args::parse();
    info!("Starting application with verbosity: {}", args.verbose);

    let cfg: AppConfig = confy::load("my-rust-app", None)?;
    info!("Configuration loaded: {:?}", cfg);

    if let Err(e) = app(cfg) {
        error!("Application crashed: {}", e);
        return Err(e);
    }

    Ok(())
}

fn app(config: AppConfig) -> Result<(), AppError> {
    if config.api_key == "default_key" {
        return Err(AppError::RuntimeError(
            "Please update your API key in the config file".into(),
        ));
    }
    Ok(())
}
