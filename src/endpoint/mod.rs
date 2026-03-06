pub mod config;
pub mod handlers;

use actix_web::{App, HttpServer, web};
use tracing_actix_web::TracingLogger;

pub async fn serve(config: config::EndpointConfig) -> Result<(), actix_web::Error> {
    HttpServer::new(|| {
        App::new()
            .wrap(TracingLogger::default())
            .service(handlers::hello)
            .service(handlers::echo)
            .route("/hey", web::get().to(handlers::manual_hello))
    })
    .bind(("0.0.0.0", config.port))?
    .run()
    .await?;

    Ok(())
}
