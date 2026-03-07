mod args;
mod config;
mod endpoint;
mod error;
mod storage;

use argon2::password_hash::SaltString;
use clap::Parser;
use tracing::{Level, error, info};
use tracing_subscriber::FmtSubscriber;

use crate::args::Args;
use crate::config::AppConfig;
use crate::endpoint::serve;
use crate::error::AppError;
use crate::storage::Storage;
use crate::storage::encryption::EncryptedStorage;

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
    let storage = Storage::init(config.storage).await?;
    let password_salt = SaltString::encode_b64(config.password_salt.as_bytes())?;
    let encrypted_storage = EncryptedStorage::new(storage, password_salt);

    serve(config.endpoint, encrypted_storage).await?;

    Ok(())
}
