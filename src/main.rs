use clap::Parser;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{Level, error, info};
use tracing_subscriber::FmtSubscriber;

/// Custom Error type for the application
#[derive(Error, Debug)]
pub enum AppError {
    #[error("Failed to load configuration: {0}")]
    ConfigError(#[from] confy::ConfyError),

    #[error("IO Error: {0}")]
    IoError(#[from] std::io::Error),

    #[error("An unexpected error occurred: {0}")]
    RuntimeError(String),
}

/// Configuration structure (saved to disk)
#[derive(Serialize, Deserialize, Debug)]
struct AppConfig {
    api_key: String,
    timeout: u64,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            api_key: "default_key".into(),
            timeout: 30,
        }
    }
}

/// Command Line Arguments
#[derive(Parser, Debug)]
#[command(author, version, about = "A clean Rust starter template")]
struct Args {
    /// Path to a custom config file
    #[arg(short, long)]
    config: Option<String>,

    /// Increase verbosity
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
}

fn main() -> Result<(), AppError> {
    // 1. Initialize Tracing (Logging)
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("setting default subscriber failed");

    // 2. Parse CLI Args
    let args = Args::parse();
    info!("Starting application with verbosity: {}", args.verbose);

    // 3. Load Config
    // This loads from the OS-default config folder or creates a default one
    let cfg: AppConfig = confy::load("my-rust-app", None)?;
    info!("Configuration loaded: {:?}", cfg);

    // 4. Run Logic
    if let Err(e) = run_app(cfg) {
        error!("Application crashed: {}", e);
        return Err(e);
    }

    info!("Application finished successfully.");
    Ok(())
}

fn run_app(config: AppConfig) -> Result<(), AppError> {
    // Your actual logic goes here
    if config.api_key == "default_key" {
        return Err(AppError::RuntimeError(
            "Please update your API key in the config file".into(),
        ));
    }
    Ok(())
}
