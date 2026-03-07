pub mod config;
pub mod handlers;

use std::sync::Arc;

use actix_web::{App, HttpServer, web};
use tracing_actix_web::TracingLogger;

use crate::storage::{FileStorage, in_memory::InMemoryStorage};

pub async fn serve(config: config::EndpointConfig) -> Result<(), actix_web::Error> {
    let storage: Arc<dyn FileStorage> = Arc::new(InMemoryStorage::new());
    let storage_data: web::Data<dyn FileStorage> = web::Data::from(storage);

    HttpServer::new(move || {
        App::new()
            .app_data(storage_data.clone())
            .wrap(TracingLogger::default())
            .service(handlers::upload)
            .service(handlers::download)
    })
    .bind(("0.0.0.0", config.port))?
    .run()
    .await?;

    Ok(())
}
