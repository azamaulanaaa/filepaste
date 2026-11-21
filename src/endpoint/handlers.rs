use std::io;
use std::net::{IpAddr, SocketAddr};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use actix_files::NamedFile;
use actix_web::HttpRequest;
use actix_web::http::header;
use actix_web::{
    FromRequest, HttpResponse, Responder, get,
    web::{self},
};
use actix_web_httpauth::extractors::basic::BasicAuth;
use base64ct::{Base64UrlUnpadded, Encoding};
use futures_util::StreamExt;
use rand::distr::Alphanumeric;
use rand::distr::Distribution;
use reqwest::redirect::Policy;
use rust_embed::Embed;
use serde::Deserialize;
use tokio_util::bytes::Bytes;
use tokio_util::io::StreamReader;
use totp_rs::TOTP;
use url::Host;

use super::lib::curl_command::parse_curl_command;
use super::lib::ssrf::{
    CONNECT_TIMEOUT_SECS, DEFAULT_TOTAL_TIMEOUT_SECS, MAX_RESPONSE_BYTES,
    MAX_RESPONSE_HEADER_BYTES, MAX_RESPONSE_HEADERS, MAX_TOTAL_TIMEOUT_SECS, is_forbidden_ip,
};
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
            let result = item.map_err(|e| io::Error::other(e));
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

    // 2. Encode filename
    let encoded_filename = Base64UrlUnpadded::encode_string(filename.as_bytes());

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

    let original_filename = match Base64UrlUnpadded::decode_vec(hex_filename) {
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
            let Ok(content_length) = usize::try_from(metadata.size) else {
                return HttpResponse::InternalServerError().body("File too large\n");
            };

            // We turn the AsyncRead back into a stream for Actix
            let stream = tokio_util::io::ReaderStream::new(reader);

            let cd = header::ContentDisposition {
                disposition: header::DispositionType::Attachment,
                parameters: vec![header::DispositionParam::Filename(original_filename)],
            };

            HttpResponse::Ok()
                .content_type("application/octet-stream")
                .insert_header(header::ContentLength(content_length))
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
#[derive(Deserialize)]
struct RemoteUploadPayload {
    command: String,
}

fn decode_command(req: &HttpRequest, body: &[u8]) -> Result<String, HttpResponse> {
    let content_type = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim)
        .unwrap_or("text/plain");

    if content_type.eq_ignore_ascii_case("application/json") {
        let payload: RemoteUploadPayload = serde_json::from_slice(body).map_err(|_| {
            HttpResponse::BadRequest().body("Request needs a JSON object with a command field")
        })?;
        Ok(payload.command)
    } else {
        std::str::from_utf8(body)
            .map(str::to_owned)
            .map_err(|_| HttpResponse::BadRequest().body("Request body must be UTF-8 text"))
    }
}

fn derive_filename(url: &url::Url) -> String {
    let segment = url
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .unwrap_or("");
    let cleaned: String = segment
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
        .take(120)
        .collect();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        "file.bin".to_owned()
    } else {
        cleaned
    }
}

/// Accept a pasted curl command in plain text or in a JSON object with a `command` field.
/// Set `Content-Type: text/plain` for plain text.
/// Set `Content-Type: application/json` for the JSON form.
/// The endpoint uses the same TOTP BasicAuth check as upload.
async fn remote_upload<S: StorageProvider>(
    req: HttpRequest,
    auth: BasicAuth,
    storage: web::Data<Arc<S>>,
    totp: web::Data<TOTP>,
    ctx: S::Context,
    body: web::Bytes,
) -> impl Responder {
    match totp.into_inner().check_current(auth.user_id()) {
        Ok(true) => (),
        Ok(false) => return HttpResponse::Unauthorized().body("Invalid TOTP"),
        Err(e) => return HttpResponse::InternalServerError().body(e.to_string()),
    }

    let command = match decode_command(&req, &body) {
        Ok(command) => command,
        Err(response) => return response,
    };

    let parsed = match parse_curl_command(&command) {
        Ok(parsed) => parsed,
        Err(e) => return HttpResponse::BadRequest().body(e.to_string()),
    };

    let host = match parsed.url.host() {
        Some(host) => host,
        None => return HttpResponse::BadRequest().body("URL has no host"),
    };
    let port = match parsed.url.port_or_known_default() {
        Some(port) => port,
        None => return HttpResponse::BadRequest().body("URL has no port"),
    };

    // Check every resolved address. A public host must not resolve to a private address.
    let addresses: Vec<SocketAddr> = match &host {
        Host::Ipv4(address) => vec![SocketAddr::new(IpAddr::V4(*address), port)],
        Host::Ipv6(address) => vec![SocketAddr::new(IpAddr::V6(*address), port)],
        Host::Domain(domain) => match tokio::net::lookup_host((*domain, port)).await {
            Ok(addresses) => addresses.collect(),
            Err(_) => return HttpResponse::BadGateway().body("Target host did not resolve"),
        },
    };
    if addresses.is_empty() {
        return HttpResponse::BadGateway().body("Target host has no addresses");
    }
    if addresses
        .iter()
        .any(|address| is_forbidden_ip(address.ip()))
    {
        return HttpResponse::Forbidden().body("Target address is blocked");
    }

    let total_timeout = match parsed.max_time {
        Some(seconds) if seconds > 0.0 => {
            let capped = seconds.min(MAX_TOTAL_TIMEOUT_SECS as f64);
            match Duration::try_from_secs_f64(capped) {
                Ok(duration) => duration,
                Err(_) => return HttpResponse::BadRequest().body("max-time value is not valid"),
            }
        }
        _ => Duration::from_secs(DEFAULT_TOTAL_TIMEOUT_SECS),
    };

    let mut client_builder = reqwest::Client::builder()
        .redirect(Policy::none())
        .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
        .timeout(total_timeout)
        .no_proxy();
    // Pin the validated addresses so the client cannot resolve a different address later.
    if let Host::Domain(domain) = &host {
        client_builder = client_builder.resolve_to_addrs(domain, &addresses);
    }
    let client = match client_builder.build() {
        Ok(client) => client,
        Err(_) => return HttpResponse::BadGateway().body("Fetch client setup failed"),
    };

    let method = match reqwest::Method::from_bytes(parsed.request_method().as_bytes()) {
        Ok(method) => method,
        Err(_) => return HttpResponse::BadRequest().body("Request method is not valid"),
    };
    let mut request = client.request(method, parsed.url.clone());

    for (name, value) in &parsed.headers {
        let header_name = match reqwest::header::HeaderName::from_bytes(name.as_bytes()) {
            Ok(header_name) => header_name,
            Err(_) => return HttpResponse::BadRequest().body("Curl header name is not valid"),
        };
        let header_value = match reqwest::header::HeaderValue::from_str(value) {
            Ok(header_value) => header_value,
            Err(_) => return HttpResponse::BadRequest().body("Curl header value is not valid"),
        };
        request = request.header(header_name, header_value);
    }

    if let Some(credentials) = &parsed.user {
        let (user, password) = match credentials.split_once(':') {
            Some((user, password)) => (user, Some(password)),
            None => (credentials.as_str(), None),
        };
        request = request.basic_auth(user, password);
    }

    if let Some(body) = parsed.body {
        request = request.body(body);
    }

    // The endpoint never follows redirects, even when the command sets -L.
    let response = match request.send().await {
        Ok(response) => response,
        Err(_) => return HttpResponse::BadGateway().body("Fetch failed"),
    };
    if response.status().is_redirection() {
        return HttpResponse::BadGateway().body("Target redirect is not allowed");
    }
    if !response.status().is_success() {
        return HttpResponse::BadGateway().body("Target returned an error status");
    }

    let response_headers = response.headers();
    if response_headers.len() > MAX_RESPONSE_HEADERS {
        return HttpResponse::BadGateway().body("Target returned too many headers");
    }
    let header_bytes: usize = response_headers
        .iter()
        .map(|(name, value)| name.as_str().len() + value.len())
        .sum();
    if header_bytes > MAX_RESPONSE_HEADER_BYTES {
        return HttpResponse::BadGateway().body("Target headers are too large");
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES)
    {
        return HttpResponse::BadGateway().body("Target body is too large");
    }

    let body_too_large = Arc::new(AtomicBool::new(false));
    let limit_flag = Arc::clone(&body_too_large);
    let received_bytes = Arc::new(AtomicU64::new(0));
    let stream = response.bytes_stream().map(move |chunk| match chunk {
        Ok(bytes) => {
            let length = bytes.len() as u64;
            let next = received_bytes.fetch_add(length, Ordering::SeqCst) + length;
            if next > MAX_RESPONSE_BYTES {
                limit_flag.store(true, Ordering::SeqCst);
                Err(io::Error::other("target body is too large"))
            } else {
                Ok(bytes)
            }
        }
        Err(error) => Err(io::Error::other(error)),
    });
    let reader: AsyncFileReader = Box::pin(StreamReader::new(Box::pin(stream)));

    let randomized_path = generate_random_path(&derive_filename(&parsed.url));
    match storage
        .put(&PathBuf::from(".").join(&randomized_path), reader, &ctx)
        .await
    {
        Ok(_) => {
            let conn = req.connection_info();
            let file_url = PathBuf::from(format!("{}://{}", conn.scheme(), conn.host()))
                .join(&randomized_path);
            HttpResponse::Ok().body(file_url.to_string_lossy().to_string())
        }
        Err(e) => {
            if body_too_large.load(Ordering::SeqCst) {
                HttpResponse::BadGateway().body("Target body is too large")
            } else {
                HttpResponse::InternalServerError().body(e.to_string())
            }
        }
    }
}

pub fn configure<S>(cfg: &mut web::ServiceConfig)
where
    S: StorageProvider + 'static,
    S::Context: Send + Sync + Default + FromRequest,
{
    cfg.service(web::resource("/api/remote-upload").route(web::post().to(remote_upload::<S>)))
        .service(index)
        .service(
            web::resource("/{path:.*}")
                .route(web::put().to(upload::<S>))
                .route(web::get().to(download::<S>)),
        );
}

#[cfg(test)]
mod tests {
    use super::*;

    use actix_web::{App, http::StatusCode, http::header, test};
    use base64ct::Encoding;
    use totp_rs::TOTP;

    use crate::storage::in_memory::InMemoryStorage;
    use crate::totp::TotpExt;

    fn auth_header() -> (TOTP, String) {
        let totp = TOTP::from_password("password", "salt").expect("create totp");
        let otp = totp.generate_current().expect("generate otp");
        let credentials = format!("{otp}:");
        let encoded = base64ct::Base64::encode_string(credentials.as_bytes());
        (totp, format!("Basic {encoded}"))
    }

    #[actix_web::test]
    async fn blocks_a_private_target_without_a_network_request() {
        let (totp, auth) = auth_header();
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(Arc::new(InMemoryStorage::new())))
                .app_data(web::Data::new(totp))
                .configure(configure::<InMemoryStorage>),
        )
        .await;

        let request = test::TestRequest::post()
            .uri("/api/remote-upload")
            .insert_header((header::AUTHORIZATION, auth))
            .insert_header((header::CONTENT_TYPE, "text/plain"))
            .set_payload("curl http://127.0.0.1/secret")
            .to_request();
        let response = test::call_service(&app, request).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[actix_web::test]
    async fn rejects_an_unknown_curl_option() {
        let (totp, auth) = auth_header();
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(Arc::new(InMemoryStorage::new())))
                .app_data(web::Data::new(totp))
                .configure(configure::<InMemoryStorage>),
        )
        .await;

        let request = test::TestRequest::post()
            .uri("/api/remote-upload")
            .insert_header((header::AUTHORIZATION, auth))
            .insert_header((header::CONTENT_TYPE, "text/plain"))
            .set_payload("curl --upload-file x https://example.com")
            .to_request();
        let response = test::call_service(&app, request).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[actix_web::test]
    async fn accepts_a_command_in_a_json_object() {
        let (totp, auth) = auth_header();
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(Arc::new(InMemoryStorage::new())))
                .app_data(web::Data::new(totp))
                .configure(configure::<InMemoryStorage>),
        )
        .await;

        let request = test::TestRequest::post()
            .uri("/api/remote-upload")
            .insert_header((header::AUTHORIZATION, auth))
            .insert_header((header::CONTENT_TYPE, "application/json"))
            .set_payload(r#"{"command":"curl http://127.0.0.1/secret"}"#)
            .to_request();
        let response = test::call_service(&app, request).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[actix_web::test]
    #[ignore = "requires network and FILEPASTE_LIVE_FETCH_TEST=1"]
    async fn fetches_a_public_url_into_storage() {
        if std::env::var_os("FILEPASTE_LIVE_FETCH_TEST").is_none() {
            return;
        }

        let (totp, auth) = auth_header();
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(Arc::new(InMemoryStorage::new())))
                .app_data(web::Data::new(totp))
                .configure(configure::<InMemoryStorage>),
        )
        .await;

        let request = test::TestRequest::post()
            .uri("/api/remote-upload")
            .insert_header((header::AUTHORIZATION, auth))
            .insert_header((header::CONTENT_TYPE, "text/plain"))
            .set_payload("curl https://example.com/")
            .to_request();
        let response = test::call_service(&app, request).await;
        assert!(response.status().is_success(), "remote upload failed");
    }
}
