use thiserror::Error;

#[derive(Error, Debug)]
pub enum AppError {
    #[error("Failed to load configuration: {0}")]
    ConfigError(#[from] confy::ConfyError),

    #[error("IO Error: {0}")]
    IoError(#[from] std::io::Error),

    #[error("An unexpected error occurred: {0}")]
    RuntimeError(String),
}
