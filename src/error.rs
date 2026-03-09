use thiserror::Error;

#[derive(Error, Debug)]
pub enum AppError {
    #[error("Failed to load configuration: {0}")]
    ConfigError(#[from] confy::ConfyError),

    #[error("IO Error: {0}")]
    IoError(#[from] std::io::Error),

    #[error("Http Error: {0}")]
    HttpError(#[from] actix_web::Error),

    #[error("TOTP Error: {0}")]
    TotpError(#[from] totp_rs::TotpUrlError),

    #[error("QR Error: {0}")]
    QRError(#[from] qr2term::QrError),
}
