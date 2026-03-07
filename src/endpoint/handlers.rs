use std::io;
use std::path::PathBuf;

use actix_web::{HttpResponse, Responder, get, put, web};
use futures_util::StreamExt;
use tokio_util::bytes::Bytes;
use tokio_util::io::StreamReader;

use crate::storage::{AsyncFileReader, FileStorage};

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

#[put("/{filename}")]
async fn upload(
    path: web::Path<String>,
    payload: web::Payload,
    storage: web::Data<dyn FileStorage>,
) -> impl Responder {
    let filename = path.into_inner();
    let file_path = PathBuf::from(&filename);

    let reader = payload_to_reader(payload);

    match storage.put(&file_path, reader).await {
        Ok(size) => HttpResponse::Ok().body(format!("Upload successful. Size: {} bytes\n", size)),
        Err(e) => HttpResponse::InternalServerError().body(e.to_string()),
    }
}

#[get("/{filename}")]
async fn download(path: web::Path<String>, storage: web::Data<dyn FileStorage>) -> impl Responder {
    let filename = path.into_inner();
    let file_path = PathBuf::from(&filename);

    match storage.get(&file_path).await {
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
