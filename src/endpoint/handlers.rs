use std::io;
use std::path::{Component, Path, PathBuf};

use actix_files::NamedFile;
use actix_web::HttpRequest;
use actix_web::{
    FromRequest, HttpResponse, Responder, get,
    web::{self},
};
use futures_util::StreamExt;
use rust_embed::Embed;
use tokio_util::bytes::Bytes;
use tokio_util::io::StreamReader;

use crate::storage::{AsyncFileReader, StorageProvider};

// Helper to convert Actix Payload into your AsyncFileReader (Pin<Box<dyn AsyncRead>>)
fn payload_to_reader(mut payload: web::Payload) -> AsyncFileReader {
    // Create a channel to bridge the non-Send payload to a Send stream
    // Capacity 16 is usually plenty for streaming
    let (tx, rx) = tokio::sync::mpsc::channel::<io::Result<Bytes>>(16);

    // Spawn a task to "pump" the data.
    // This works because Actix runs the handler on a thread where the payload is valid.
    actix_web::rt::spawn(async move {
        while let Some(item) = payload.next().await {
            let result = item.map_err(|e| io::Error::new(io::ErrorKind::Other, e));
            if tx.send(result).await.is_err() {
                break; // Receiver dropped, stop pumping
            }
        }
    });

    // Convert the mpsc Receiver into a Stream
    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);

    // Now this is Send because ReceiverStream is Send!
    Box::pin(StreamReader::new(stream))
}

fn sanitize_relative_path(user_path: &str) -> Result<PathBuf, &'static str> {
    let mut resolved_path = PathBuf::from(".");
    let mut depth = 0;

    for component in Path::new(user_path).components() {
        match component {
            Component::Normal(name) => {
                resolved_path.push(name);
                depth += 1;
            }
            Component::ParentDir => {
                // If depth is 0, the user is trying to use `..` to escape `./`
                if depth == 0 {
                    return Err("Path traversal attempt detected");
                }
                resolved_path.pop();
                depth -= 1;
            }
            Component::CurDir => {} // Ignore `.` (current directory)
            Component::RootDir | Component::Prefix(_) => {
                return Err("Absolute paths are not allowed");
            }
        }
    }

    // Ensure they didn't just request the root directory itself
    if depth == 0 {
        return Err("File path cannot be empty");
    }

    Ok(resolved_path)
}

async fn upload<S: StorageProvider>(
    req: HttpRequest,
    path: web::Path<String>,
    payload: web::Payload,
    storage: web::Data<S>,
    ctx: S::Context,
) -> impl Responder {
    let raw_path = path.into_inner();

    let safe_path = match sanitize_relative_path(&raw_path) {
        Ok(p) => p,
        Err(e) => return HttpResponse::BadRequest().body(e),
    };

    let reader = payload_to_reader(payload);

    match storage.put(&safe_path, reader, &ctx).await {
        Ok(_) => {
            // Get the connection info (e.g., "localhost:8080" or "example.com")
            let conn = req.connection_info();
            let scheme = conn.scheme();
            let host = conn.host();

            // Construct the full URL
            // Note: safe_path is a PathBuf, so we convert it to a string for the URL
            let file_url = format!("{}://{}/{}\n", scheme, host, raw_path);

            HttpResponse::Ok().body(file_url)
        }
        Err(e) => HttpResponse::InternalServerError().body(e.to_string()),
    }
}

async fn download<S: StorageProvider>(
    path: web::Path<String>,
    storage: web::Data<S>,
    ctx: S::Context,
) -> impl Responder {
    let raw_path = path.into_inner();

    let safe_path = match sanitize_relative_path(&raw_path) {
        Ok(p) => p,
        Err(e) => return HttpResponse::BadRequest().body(e),
    };

    match storage.get(&safe_path, &ctx).await {
        Ok(Some(reader)) => {
            // We turn the AsyncRead back into a stream for Actix
            let stream = tokio_util::io::ReaderStream::new(reader);
            HttpResponse::Ok()
                .content_type("application/octet-stream")
                .streaming(stream)
        }
        Ok(None) => HttpResponse::NotFound().body("File not found\n"),
        Err(e) => HttpResponse::InternalServerError().body(e.to_string()),
    }
}

#[derive(Embed)]
#[folder = "static/"]
struct Assets;

#[get("/")]
async fn index(
    req: HttpRequest,
    config: web::Data<super::config::EndpointConfig>,
) -> impl Responder {
    if let Some(path) = &config.index_path {
        if let Ok(file) = NamedFile::open_async(path).await {
            return file.into_response(&req);
        }
    }

    if let Some(content) = Assets::get("index.html") {
        return HttpResponse::Ok()
            .content_type("text/html; charset=utf-8")
            // Best practice: Add a "Bundled" header to help debugging
            .insert_header(("X-Asset-Source", "Embedded"))
            .body(content.data.into_owned());
    }

    HttpResponse::NotFound().body("No index.html found in config or bundle.")
}
pub fn configure<S>(cfg: &mut web::ServiceConfig)
where
    S: StorageProvider + 'static,
    S::Context: Send + Sync + Default + FromRequest,
{
    cfg.service(index).service(
        web::resource("/{path:.*}")
            .route(web::put().to(upload::<S>))
            .route(web::get().to(download::<S>)),
    );
}
