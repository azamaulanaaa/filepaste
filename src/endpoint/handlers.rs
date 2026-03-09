use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use actix_files::NamedFile;
use actix_web::HttpRequest;
use actix_web::http::header;
use actix_web::{
    FromRequest, HttpResponse, Responder, get,
    web::{self},
};
use actix_web_httpauth::extractors::basic::BasicAuth;
use futures_util::StreamExt;
use rand::distr::Alphanumeric;
use rand::distr::Distribution;
use rust_embed::Embed;
use tokio_util::bytes::Bytes;
use tokio_util::io::StreamReader;
use totp_rs::TOTP;

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

fn generate_random_path(filename: &str) -> PathBuf {
    // 1. Generate 8 random alphanumeric characters
    let random_dir: String = Alphanumeric
        .sample_iter(rand::rng())
        .take(8)
        .map(char::from)
        .collect();

    // 2. Encode filename to base16 (hex)
    let encoded_filename = base16ct::lower::encode_string(filename.as_bytes());

    PathBuf::new().join(random_dir).join(encoded_filename)
}

async fn upload<S: StorageProvider>(
    req: HttpRequest,
    path: web::Path<String>,
    payload: web::Payload,
    auth: BasicAuth,
    storage: web::Data<Arc<S>>,
    totp: web::Data<TOTP>,
    ctx: S::Context,
) -> impl Responder {
    match totp.into_inner().check_current(auth.user_id()) {
        Ok(true) => (),
        Ok(false) => return HttpResponse::Unauthorized().body("Invalid TOTP"),
        Err(e) => return HttpResponse::InternalServerError().body(e.to_string()),
    }

    let raw_path = path.into_inner();

    let safe_path = match super::lib::sanitize_relative_path(&raw_path) {
        Ok(p) => p,
        Err(e) => return HttpResponse::BadRequest().body(e),
    };

    let path_obj = Path::new(&safe_path);

    let components: Vec<_> = path_obj.components().collect();

    let is_single_file = components.len() == 2
        && matches!(components[0], Component::CurDir)
        && matches!(components[1], Component::Normal(_));

    if !is_single_file {
        return HttpResponse::BadRequest().body("Sub-paths are not allowed in upload\n");
    }

    let filename = match path_obj.file_name().and_then(|s| s.to_str()) {
        Some(name) => name,
        None => return HttpResponse::BadRequest().body("Invalid filename\n"),
    };

    let randomized_path = generate_random_path(filename);

    let reader = payload_to_reader(payload);

    match storage
        .put(&PathBuf::from(".").join(&randomized_path), reader, &ctx)
        .await
    {
        Ok(_) => {
            // Get the connection info (e.g., "localhost:8080" or "example.com")
            let conn = req.connection_info();
            let scheme = conn.scheme();
            let host = conn.host();

            // Construct the full URL
            // Note: safe_path is a PathBuf, so we convert it to a string for the URL
            let file_url = PathBuf::from(format!("{}://{}", scheme, host)).join(randomized_path);

            HttpResponse::Ok().body(file_url.to_string_lossy().to_string())
        }
        Err(e) => HttpResponse::InternalServerError().body(e.to_string()),
    }
}

async fn download<S: StorageProvider>(
    path: web::Path<String>,
    storage: web::Data<Arc<S>>,
    ctx: S::Context,
) -> impl Responder {
    let raw_path = path.into_inner();

    let safe_path = match super::lib::sanitize_relative_path(&raw_path) {
        Ok(p) => p,
        Err(e) => return HttpResponse::BadRequest().body(e),
    };

    let hex_filename = match safe_path.file_name().and_then(|s| s.to_str()) {
        Some(name) => name,
        None => return HttpResponse::BadRequest().body("Invalid path structure\n"),
    };

    let original_filename = match base16ct::lower::decode_vec(hex_filename.as_bytes()) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(_) => "file.bin".to_string(), // Fallback if decoding fails
    };

    let metadata = match storage.metadata(&safe_path, &ctx).await {
        Ok(Some(metadata)) => metadata,
        Ok(None) => return HttpResponse::NotFound().body("File not found\n"),
        Err(e) => return HttpResponse::InternalServerError().body(e.to_string()),
    };

    match storage.get(&safe_path, &ctx).await {
        Ok(Some(reader)) => {
            // We turn the AsyncRead back into a stream for Actix
            let stream = tokio_util::io::ReaderStream::new(reader);

            let cd = header::ContentDisposition {
                disposition: header::DispositionType::Attachment,
                parameters: vec![header::DispositionParam::Filename(original_filename)],
            };

            HttpResponse::Ok()
                .content_type("application/octet-stream")
                .insert_header(header::ContentLength(metadata.size as usize))
                .insert_header(cd)
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
