mod args;
mod config;
mod endpoint;
mod error;
mod storage;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use argon2::password_hash::SaltString;
use clap::Parser;
use tracing::{Level, error, info};
use tracing_subscriber::FmtSubscriber;

use crate::args::Args;
use crate::config::AppConfig;
use crate::endpoint::serve;
use crate::error::AppError;
use crate::storage::{
    Storage, StoragePrune, encryption::EncryptedStorage, retention::RetentionStorage,
};

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

    let retention_duration = Duration::from_hours(config.default_retention_hours);
    let retention_storage = RetentionStorage::new(storage, retention_duration);

    let password_salt = SaltString::encode_b64(config.password_salt.as_bytes())?;
    let encrypted_storage = EncryptedStorage::new(retention_storage, password_salt);

    let storage_arc = Arc::new(encrypted_storage);

    let gc_handle = Arc::clone(&storage_arc);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_hours(1));

        loop {
            interval.tick().await;
            info!("GC: Starting hourly prune...");
            // .prune() is available here because of the blanket implementation
            let _ = gc_handle.prune(Path::new(""), &Default::default()).await;
        }
    });

    serve(config.endpoint, storage_arc).await?;

    Ok(())
}
