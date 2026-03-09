use pbkdf2::pbkdf2_hmac;
use sha2::Sha256;
use totp_rs::{Algorithm, TOTP};

pub trait TotpExt {
    fn from_password(password: &str, salt: &str) -> Result<Self, totp_rs::TotpUrlError>
    where
        Self: Sized;
    fn gen_url(&self, account_name: &str, issuer: &str) -> String;
    fn print_qr(&self, account_name: &str, issuer: &str) -> Result<(), qr2term::QrError>;
}

impl TotpExt for TOTP {
    fn from_password(password: &str, salt: &str) -> Result<Self, totp_rs::TotpUrlError> {
        let mut seed = [0u8; 20];
        pbkdf2_hmac::<Sha256>(password.as_bytes(), salt.as_bytes(), 600_000, &mut seed);

        let totp = TOTP::new(Algorithm::SHA1, 6, 1, 30, seed.to_vec())?;

        Ok(totp)
    }

    fn gen_url(&self, account_name: &str, issuer: &str) -> String {
        let secret_base32 = self.get_secret_base32();

        format!(
            "otpauth://totp/{}:{}?secret={}&issuer={}&digits={}&period={}",
            issuer, account_name, secret_base32, issuer, self.digits, self.step
        )
    }

    fn print_qr(&self, account_name: &str, issuer: &str) -> Result<(), qr2term::QrError> {
        let url = self.gen_url(account_name, issuer);
        println!("Generated URL: {}", url);
        qr2term::print_qr(&url)?;
        Ok(())
    }
}
