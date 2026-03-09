use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use filepaste::gc::spawn_gc;
use filepaste::totp::TotpExt;
use totp_rs::TOTP;
use tracing::{Level, error, info};
use tracing_subscriber::FmtSubscriber;

use filepaste::args::Args;
use filepaste::config::AppConfig;
use filepaste::endpoint::serve;
use filepaste::error::AppError;
use filepaste::storage::{Storage, encryption::EncryptedStorage, retention::RetentionStorage};

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
    let totp = TOTP::from_password(&config.totp_secret, &config.password_salt)?;
    totp.print_qr("default", "filepaste")?;

    let storage = {
        let storage = Storage::init(config.storage).await?;

        let retention_duration = Duration::from_hours(config.default_retention_hours);
        let retention_storage = RetentionStorage::new(storage, retention_duration);

        let encrypted_storage = EncryptedStorage::new(retention_storage, config.password_salt);

        Arc::new(encrypted_storage)
    };

    spawn_gc(storage.clone(), Duration::from_hours(1));
    serve(config.endpoint, storage, totp).await?;

    Ok(())
}
