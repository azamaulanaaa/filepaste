use thiserror::Error;

#[derive(Error, Debug)]
pub enum AppError {
    #[error("Failed to load configuration: {0}")]
    ConfigError(#[from] confy::ConfyError),

    #[error("IO Error: {0}")]
    IoError(#[from] std::io::Error),

    #[error("Http Error: {0}")]
    HttpError(#[from] actix_web::Error),

    #[error("Password Hash Error: {0}")]
    PasswordHashError(#[from] argon2::password_hash::Error),
}
