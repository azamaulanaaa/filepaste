pub mod config;
pub mod handlers;

use std::sync::Arc;

use actix_web::{App, HttpServer, web};
use tracing_actix_web::TracingLogger;

use crate::storage::StorageProvider;

pub async fn serve<S>(
    config: config::EndpointConfig,
    storage: Arc<S>,
) -> Result<(), actix_web::Error>
where
    S: StorageProvider + 'static,
{
    let config_data = web::Data::new(config.clone());
    let storage_data = web::Data::new(storage);

    HttpServer::new(move || {
        App::new()
            .wrap(TracingLogger::default())
            .app_data(config_data.clone())
            .app_data(storage_data.clone())
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

    use actix_web::{App, test, web};

    use crate::storage::in_memory::InMemoryStorage;

    #[actix_web::test]
    async fn test_unified_router() {
        let storage = InMemoryStorage::new();
        let storage_data = web::Data::new(storage);

        let app = test::init_service(
            App::new()
                .app_data(storage_data)
                .configure(handlers::configure::<InMemoryStorage>),
        )
        .await;

        let test_path = "test_file.txt";
        let test_content = "hello world";

        // --- TEST 1: PUT (Upload) ---
        let req = test::TestRequest::put()
            .uri(&format!("/{}", test_path))
            .set_payload(test_content)
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert!(
            resp.status().is_success(),
            "Upload failed with status: {}",
            resp.status()
        );

        // --- TEST 2: GET (Download) ---
        let req = test::TestRequest::get()
            .uri(&format!("/{}", test_path))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert!(
            resp.status().is_success(),
            "Download failed with status: {}",
            resp.status()
        );

        let body = test::read_body(resp).await;
        assert_eq!(body, test_content.as_bytes());
    }
}
