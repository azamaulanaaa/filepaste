pub mod config;
pub mod handlers;
pub mod lib;

use std::sync::Arc;

use actix_web::{App, HttpServer, web};
use totp_rs::TOTP;
use tracing_actix_web::TracingLogger;

use crate::storage::StorageProvider;

pub async fn serve<S>(
    config: config::EndpointConfig,
    storage: Arc<S>,
    totp: TOTP,
) -> Result<(), actix_web::Error>
where
    S: StorageProvider + 'static,
{
    let config_data = web::Data::new(config.clone());
    let storage_data = web::Data::new(storage);
    let totp_data = web::Data::new(totp);

    HttpServer::new(move || {
        App::new()
            .wrap(TracingLogger::default())
            .app_data(config_data.clone())
            .app_data(storage_data.clone())
            .app_data(totp_data.clone())
            .configure(handlers::configure::<S>)
    })
    .bind((config.host, config.port))?
    .run()
    .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use actix_web::{App, http::header, test, web};
    use base64ct::Encoding;

    use crate::{storage::in_memory::InMemoryStorage, totp::TotpExt};

    #[actix_web::test]
    async fn test_unified_router() {
        let storage = InMemoryStorage::new();
        let storage_arc = Arc::new(storage);
        let storage_data = web::Data::new(storage_arc);

        let totp = TOTP::from_password("password", "salt").expect("Failed to create totp");
        let totp_data = web::Data::new(totp.clone());

        let app = test::init_service(
            App::new()
                .app_data(storage_data)
                .app_data(totp_data)
                .configure(handlers::configure::<InMemoryStorage>),
        )
        .await;

        let test_path = "test_file.txt";
        let test_content = "hello world";
        let test_otp = totp.generate_current().expect("Failed to generate otp");

        let auth = format!("{}:{}", test_otp, "");
        let auth_base64 = base64ct::Base64::encode_string(auth.as_bytes());

        // --- TEST 1: PUT (Upload) ---
        let req = test::TestRequest::put()
            .uri(&format!("/{}", test_path))
            .insert_header((header::AUTHORIZATION, format!("Basic {}", auth_base64)))
            .set_payload(test_content)
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_success());

        let body_bytes = test::read_body(resp).await;
        let body_str = std::str::from_utf8(&body_bytes).unwrap().trim();

        // Extract path from "http://localhost:8080/ABC/123"
        let url = url::Url::parse(body_str).expect("Failed to parse URL");
        let generated_path = url.path(); // This returns "/ABC/123"

        // --- TEST 2: GET (Download) ---
        // 3. Use the extracted path for the GET request
        let req = test::TestRequest::get().uri(generated_path).to_request();

        let resp = test::call_service(&app, req).await;
        assert!(
            resp.status().is_success(),
            "Download failed for generated path: {}",
            generated_path
        );

        let body = test::read_body(resp).await;
        assert_eq!(body, test_content.as_bytes());
    }
}
