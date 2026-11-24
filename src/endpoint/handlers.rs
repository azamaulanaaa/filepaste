use std::io;
use std::net::{IpAddr, SocketAddr};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use actix_files::NamedFile;
use actix_web::HttpRequest;
use actix_web::http::StatusCode;
use actix_web::http::header;
use actix_web::{
    FromRequest, HttpResponse, Responder, get,
    web::{self},
};
use actix_web_httpauth::extractors::basic::BasicAuth;
use base64ct::{Base64, Base64UrlUnpadded, Encoding};
use futures_util::StreamExt;
use rand::distr::Alphanumeric;
use rand::distr::Distribution;
use reqwest::redirect::Policy;
use rust_embed::Embed;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tokio_util::bytes::Bytes;
use tokio_util::io::StreamReader;
use totp_rs::TOTP;
use url::Host;

use super::lib::curl_command::parse_curl_command;
use super::lib::ssrf::{
    CONNECT_TIMEOUT_SECS, DEFAULT_TOTAL_TIMEOUT_SECS, MAX_RESPONSE_BYTES,
    MAX_RESPONSE_HEADER_BYTES, MAX_RESPONSE_HEADERS, MAX_TOTAL_TIMEOUT_SECS, is_forbidden_ip,
};
use super::remote_jobs::{Job, JobState, JobStore};
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

/// A reason why a remote upload request cannot start.
enum FetchError {
    BadRequest(String),
    Unauthorized(&'static str),
    Forbidden(&'static str),
    BadGateway(&'static str),
    Internal(String),
}

impl FetchError {
    fn status(&self) -> StatusCode {
        match self {
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            Self::Forbidden(_) => StatusCode::FORBIDDEN,
            Self::BadGateway(_) => StatusCode::BAD_GATEWAY,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn message(&self) -> &str {
        match self {
            Self::BadRequest(message) | Self::Internal(message) => message,
            Self::Unauthorized(message) | Self::Forbidden(message) | Self::BadGateway(message) => {
                message
            }
        }
    }

    /// Return a plain-text response for the old endpoint.
    fn into_text(self) -> HttpResponse {
        HttpResponse::build(self.status()).body(self.message().to_owned())
    }

    /// Return a JSON response for the job endpoints.
    fn into_json(self) -> HttpResponse {
        json_error(self.status(), self.message())
    }
}

fn json_error(status: StatusCode, message: &str) -> HttpResponse {
    HttpResponse::build(status)
        .insert_header((header::CACHE_CONTROL, "no-store"))
        .json(serde_json::json!({ "error": message }))
}

fn check_totp(totp: &TOTP, user_id: &str) -> Result<(), FetchError> {
    match totp.check_current(user_id) {
        Ok(true) => Ok(()),
        Ok(false) => Err(FetchError::Unauthorized("Invalid TOTP")),
        Err(error) => Err(FetchError::Internal(error.to_string())),
    }
}

fn basic_user_id(req: &HttpRequest) -> Option<String> {
    let value = req.headers().get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, encoded) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("Basic") {
        return None;
    }
    let decoded = Base64::decode_vec(encoded.trim()).ok()?;
    let credentials = String::from_utf8(decoded).ok()?;
    let (user_id, _) = credentials.split_once(':')?;
    Some(user_id.to_owned())
}

fn decode_command(req: &HttpRequest, body: &[u8]) -> Result<String, FetchError> {
    let content_type = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim)
        .unwrap_or("text/plain");

    if content_type.eq_ignore_ascii_case("application/json") {
        let payload: RemoteUploadPayload = serde_json::from_slice(body).map_err(|_| {
            FetchError::BadRequest("Request needs a JSON object with a command field".to_owned())
        })?;
        Ok(payload.command)
    } else {
        std::str::from_utf8(body)
            .map(str::to_owned)
            .map_err(|_| FetchError::BadRequest("Request body must be UTF-8 text".to_owned()))
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

struct PreparedFetch {
    client: reqwest::Client,
    request: reqwest::Request,
    url: url::Url,
}

struct RemoteFetch {
    response: reqwest::Response,
    url: url::Url,
    total_bytes: Option<u64>,
}

/// Validate a pasted curl command and prepare the target request.
///
/// This function resolves the target host and blocks a private address.
async fn prepare_remote_fetch(req: &HttpRequest, body: &[u8]) -> Result<PreparedFetch, FetchError> {
    let command = decode_command(req, body)?;

    let parsed =
        parse_curl_command(&command).map_err(|error| FetchError::BadRequest(error.to_string()))?;

    let host = parsed
        .url
        .host()
        .ok_or_else(|| FetchError::BadRequest("URL has no host".to_owned()))?;
    let port = parsed
        .url
        .port_or_known_default()
        .ok_or_else(|| FetchError::BadRequest("URL has no port".to_owned()))?;

    // Check every resolved address. A public host must not resolve to a private address.
    let addresses: Vec<SocketAddr> = match &host {
        Host::Ipv4(address) => vec![SocketAddr::new(IpAddr::V4(*address), port)],
        Host::Ipv6(address) => vec![SocketAddr::new(IpAddr::V6(*address), port)],
        Host::Domain(domain) => match tokio::net::lookup_host((*domain, port)).await {
            Ok(addresses) => addresses.collect(),
            Err(_) => return Err(FetchError::BadGateway("Target host did not resolve")),
        },
    };
    if addresses.is_empty() {
        return Err(FetchError::BadGateway("Target host has no addresses"));
    }
    if addresses
        .iter()
        .any(|address| is_forbidden_ip(address.ip()))
    {
        return Err(FetchError::Forbidden("Target address is blocked"));
    }

    let total_timeout = match parsed.max_time {
        Some(seconds) if seconds > 0.0 => {
            let capped = seconds.min(MAX_TOTAL_TIMEOUT_SECS as f64);
            Duration::try_from_secs_f64(capped)
                .map_err(|_| FetchError::BadRequest("max-time value is not valid".to_owned()))?
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
    let client = client_builder
        .build()
        .map_err(|_| FetchError::BadGateway("Fetch client setup failed"))?;

    let method = reqwest::Method::from_bytes(parsed.request_method().as_bytes())
        .map_err(|_| FetchError::BadRequest("Request method is not valid".to_owned()))?;
    let mut request = client.request(method, parsed.url.clone());

    for (name, value) in &parsed.headers {
        let header_name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| FetchError::BadRequest("Curl header name is not valid".to_owned()))?;
        let header_value = reqwest::header::HeaderValue::from_str(value)
            .map_err(|_| FetchError::BadRequest("Curl header value is not valid".to_owned()))?;
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

    let request = request
        .build()
        .map_err(|_| FetchError::BadGateway("Fetch request setup failed"))?;

    Ok(PreparedFetch {
        client,
        request,
        url: parsed.url,
    })
}

/// Send a prepared target request and validate the response.
async fn send_remote_fetch(prepared: PreparedFetch) -> Result<RemoteFetch, FetchError> {
    let PreparedFetch {
        client,
        request,
        url,
    } = prepared;

    // The endpoint never follows redirects, even when the command sets -L.
    let response = client
        .execute(request)
        .await
        .map_err(|_| FetchError::BadGateway("Fetch failed"))?;
    if response.status().is_redirection() {
        return Err(FetchError::BadGateway("Target redirect is not allowed"));
    }
    if !response.status().is_success() {
        return Err(FetchError::BadGateway("Target returned an error status"));
    }

    let response_headers = response.headers();
    if response_headers.len() > MAX_RESPONSE_HEADERS {
        return Err(FetchError::BadGateway("Target returned too many headers"));
    }
    let header_bytes: usize = response_headers
        .iter()
        .map(|(name, value)| name.as_str().len() + value.len())
        .sum();
    if header_bytes > MAX_RESPONSE_HEADER_BYTES {
        return Err(FetchError::BadGateway("Target headers are too large"));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES)
    {
        return Err(FetchError::BadGateway("Target body is too large"));
    }

    let total_bytes = response.content_length();
    Ok(RemoteFetch {
        response,
        url,
        total_bytes,
    })
}

/// Make a reader that counts the response bytes and enforces the body limit.
fn response_reader<F>(
    response: reqwest::Response,
    received_bytes: Arc<AtomicU64>,
    body_too_large: Arc<AtomicBool>,
    mut on_progress: F,
) -> AsyncFileReader
where
    F: FnMut(u64) + Send + 'static,
{
    let limit_flag = Arc::clone(&body_too_large);
    let stream = response.bytes_stream().map(move |chunk| match chunk {
        Ok(bytes) => {
            let length = bytes.len() as u64;
            let next = received_bytes.fetch_add(length, Ordering::SeqCst) + length;
            on_progress(next);
            if next > MAX_RESPONSE_BYTES {
                limit_flag.store(true, Ordering::SeqCst);
                Err(io::Error::other("target body is too large"))
            } else {
                Ok(bytes)
            }
        }
        Err(error) => Err(io::Error::other(error)),
    });
    Box::pin(StreamReader::new(Box::pin(stream)))
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
    if let Err(error) = check_totp(&totp, auth.user_id()) {
        return error.into_text();
    }

    let prepared = match prepare_remote_fetch(&req, &body).await {
        Ok(prepared) => prepared,
        Err(error) => return error.into_text(),
    };
    let fetch = match send_remote_fetch(prepared).await {
        Ok(fetch) => fetch,
        Err(error) => return error.into_text(),
    };

    let received_bytes = Arc::new(AtomicU64::new(0));
    let body_too_large = Arc::new(AtomicBool::new(false));
    let reader = response_reader(
        fetch.response,
        Arc::clone(&received_bytes),
        Arc::clone(&body_too_large),
        |_| {},
    );

    let randomized_path = generate_random_path(&derive_filename(&fetch.url));
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
        Err(error) => {
            if body_too_large.load(Ordering::SeqCst) {
                HttpResponse::BadGateway().body("Target body is too large")
            } else {
                HttpResponse::InternalServerError().body(error.to_string())
            }
        }
    }
}

#[derive(Serialize)]
struct JobResponse<'a> {
    id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    token: Option<&'a str>,
    state: &'a str,
    received_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    total_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a str>,
}

fn bearer_token(req: &HttpRequest) -> Option<String> {
    let value = req.headers().get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("Bearer") || token.is_empty() {
        return None;
    }
    Some(token.to_owned())
}

async fn delete_partial<S: StorageProvider>(storage: &S, path: &Path, ctx: &S::Context) {
    if let Err(error) = storage.delete_partial(path, ctx).await {
        tracing::warn!(%error, "Cannot delete the partial remote upload");
    }
}

/// Send the target request, then read the body into storage and update the job.
async fn run_remote_job<S: StorageProvider + 'static>(
    job: Arc<Job>,
    storage: Arc<S>,
    ctx: S::Context,
    prepared: PreparedFetch,
    file_url: String,
    mut cancel: watch::Receiver<bool>,
) {
    if *cancel.borrow() {
        return;
    }

    let send = send_remote_fetch(prepared);
    tokio::pin!(send);
    let fetch = tokio::select! {
        result = &mut send => Some(result),
        _ = cancel.changed() => None,
    };
    let Some(fetch) = fetch else {
        delete_partial(&*storage, job.path(), &ctx).await;
        return;
    };
    let fetch = match fetch {
        Ok(fetch) => fetch,
        Err(error) => {
            job.finish_failed(error.message().to_owned());
            return;
        }
    };
    job.set_total_bytes(fetch.total_bytes);

    let received_bytes = Arc::new(AtomicU64::new(0));
    let body_too_large = Arc::new(AtomicBool::new(false));
    let progress_job = Arc::clone(&job);
    let reader = response_reader(
        fetch.response,
        Arc::clone(&received_bytes),
        Arc::clone(&body_too_large),
        move |next| progress_job.mark_received(next),
    );

    let put = storage.put(job.path(), reader, &ctx);
    tokio::pin!(put);
    let result = tokio::select! {
        result = &mut put => Some(result),
        _ = cancel.changed() => None,
    };
    let Some(result) = result else {
        delete_partial(&*storage, job.path(), &ctx).await;
        return;
    };

    match result {
        Ok(_) if !body_too_large.load(Ordering::SeqCst) => job.finish_done(file_url),
        Ok(_) => {
            delete_partial(&*storage, job.path(), &ctx).await;
            job.finish_failed("Target body is too large".to_owned());
        }
        Err(error) => {
            let message = if body_too_large.load(Ordering::SeqCst) {
                "Target body is too large".to_owned()
            } else {
                error.to_string()
            };
            delete_partial(&*storage, job.path(), &ctx).await;
            job.finish_failed(message);
        }
    }
}

/// Create a remote upload job and start the background task.
async fn create_remote_upload<S: StorageProvider + 'static>(
    req: HttpRequest,
    storage: web::Data<Arc<S>>,
    totp: web::Data<TOTP>,
    jobs: web::Data<JobStore>,
    ctx: S::Context,
    body: web::Bytes,
) -> impl Responder {
    let Some(user_id) = basic_user_id(&req) else {
        return json_error(StatusCode::UNAUTHORIZED, "Basic auth is required");
    };
    if let Err(error) = check_totp(&totp, &user_id) {
        return error.into_json();
    }

    let prepared = match prepare_remote_fetch(&req, &body).await {
        Ok(prepared) => prepared,
        Err(error) => return error.into_json(),
    };

    let conn = req.connection_info();
    let origin = format!("{}://{}", conn.scheme(), conn.host());
    let randomized_path = generate_random_path(&derive_filename(&prepared.url));
    let storage_path = PathBuf::from(".").join(&randomized_path);
    let file_url = PathBuf::from(origin)
        .join(&randomized_path)
        .to_string_lossy()
        .to_string();

    let (id, job) = match jobs.create(storage_path, None) {
        Ok(created) => created,
        Err(error) => {
            tracing::error!("Cannot store the remote upload job record: {error}");
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Cannot store the job record",
            );
        }
    };
    let task_job = Arc::clone(&job);
    let task_storage = Arc::clone(storage.get_ref());
    let cancel = job.cancel_receiver();
    let task = tokio::spawn(run_remote_job::<S>(
        task_job,
        task_storage,
        ctx,
        prepared,
        file_url,
        cancel,
    ));
    job.set_task(task);

    HttpResponse::Accepted()
        .insert_header((header::LOCATION, format!("/api/remote-uploads/{id}")))
        .insert_header((header::CACHE_CONTROL, "no-store"))
        .json(JobResponse {
            id: &id,
            token: Some(job.token()),
            state: "fetching",
            received_bytes: 0,
            total_bytes: None,
            url: None,
            error: None,
        })
}

/// Return the status of a remote upload job.
async fn remote_upload_status(
    req: HttpRequest,
    jobs: web::Data<JobStore>,
    path: web::Path<String>,
) -> impl Responder {
    let id = path.into_inner();
    let Some(token) = bearer_token(&req) else {
        return json_error(
            StatusCode::UNAUTHORIZED,
            "Job token is missing or not valid",
        );
    };
    let Some(job) = jobs.get(&id) else {
        return json_error(StatusCode::NOT_FOUND, "Job not found");
    };
    if !job.token_matches(&token) {
        return json_error(
            StatusCode::UNAUTHORIZED,
            "Job token is missing or not valid",
        );
    }

    let status = job.status();
    let (url, error) = match &status.state {
        JobState::Done { url } => (Some(url.as_str()), None),
        JobState::Failed { error } => (None, Some(error.as_str())),
        JobState::Fetching => (None, None),
    };
    let mut response = HttpResponse::Ok();
    response.insert_header((header::CACHE_CONTROL, "no-store"));
    if matches!(&status.state, JobState::Fetching) {
        response.insert_header((header::RETRY_AFTER, "1"));
    }
    response.json(JobResponse {
        id: &id,
        token: None,
        state: status.state.name(),
        received_bytes: status.received_bytes,
        total_bytes: status.total_bytes,
        url,
        error,
    })
}

/// Cancel a remote upload job and delete a partial object.
async fn cancel_remote_upload<S: StorageProvider>(
    req: HttpRequest,
    storage: web::Data<Arc<S>>,
    jobs: web::Data<JobStore>,
    path: web::Path<String>,
) -> impl Responder {
    let id = path.into_inner();
    let Some(token) = bearer_token(&req) else {
        return json_error(
            StatusCode::UNAUTHORIZED,
            "Job token is missing or not valid",
        );
    };
    let Some(job) = jobs.get(&id) else {
        return json_error(StatusCode::NOT_FOUND, "Job not found");
    };
    if !job.token_matches(&token) {
        return json_error(
            StatusCode::UNAUTHORIZED,
            "Job token is missing or not valid",
        );
    }

    jobs.remove(&id);
    job.cancel();
    job.wait_for_task().await;
    if !job.is_done() {
        let storage: &S = &storage;
        delete_partial(storage, job.path(), &S::Context::default()).await;
    }

    HttpResponse::NoContent()
        .insert_header((header::CACHE_CONTROL, "no-store"))
        .finish()
}

pub fn configure<S>(cfg: &mut web::ServiceConfig)
where
    S: StorageProvider + 'static,
    S::Context: Send + Sync + Default + FromRequest,
{
    cfg.service(
        web::resource("/api/remote-uploads").route(web::post().to(create_remote_upload::<S>)),
    )
    .service(
        web::resource("/api/remote-uploads/{id}")
            .route(web::get().to(remote_upload_status))
            .route(web::delete().to(cancel_remote_upload::<S>)),
    )
    .service(web::resource("/api/remote-upload").route(web::post().to(remote_upload::<S>)))
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
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use totp_rs::TOTP;

    use crate::storage::encryption::EncryptedStorage;
    use crate::storage::in_memory::InMemoryStorage;
    use crate::totp::TotpExt;

    fn auth_header() -> (TOTP, String) {
        let totp = TOTP::from_password("password", "salt").expect("create totp");
        let otp = totp.generate_current().expect("generate otp");
        let credentials = format!("{otp}:");
        let encoded = base64ct::Base64::encode_string(credentials.as_bytes());
        (totp, format!("Basic {encoded}"))
    }

    async fn one_shot_http_server(response: Vec<u8>) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let address = listener.local_addr().expect("read test server address");
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept test request");
            let mut request = Vec::new();
            let mut buffer = [0u8; 1024];
            loop {
                let read = stream.read(&mut buffer).await.expect("read test request");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            stream
                .write_all(&response)
                .await
                .expect("write test response");
            stream.shutdown().await.expect("close test response");
        });
        address
    }

    fn prepared_loopback_get(address: SocketAddr, path: &str) -> PreparedFetch {
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("build test client");
        let url = url::Url::parse(&format!("http://{address}{path}")).expect("parse test URL");
        let request = client.get(url.clone()).build().expect("build test request");
        PreparedFetch {
            client,
            request,
            url,
        }
    }

    #[actix_web::test]
    async fn blocks_a_private_target_without_a_network_request() {
        let (totp, auth) = auth_header();
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(Arc::new(InMemoryStorage::new())))
                .app_data(web::Data::new(totp))
                .app_data(web::Data::new(JobStore::new()))
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
                .app_data(web::Data::new(JobStore::new()))
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
                .app_data(web::Data::new(JobStore::new()))
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
    async fn blocks_a_private_target_for_a_job() {
        let (totp, auth) = auth_header();
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(Arc::new(InMemoryStorage::new())))
                .app_data(web::Data::new(totp))
                .app_data(web::Data::new(JobStore::new()))
                .configure(configure::<InMemoryStorage>),
        )
        .await;

        let request = test::TestRequest::post()
            .uri("/api/remote-uploads")
            .insert_header((header::AUTHORIZATION, auth))
            .insert_header((header::CONTENT_TYPE, "text/plain"))
            .set_payload("curl http://127.0.0.1/secret")
            .to_request();
        let response = test::call_service(&app, request).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let body: serde_json::Value = test::read_body_json(response).await;
        assert_eq!(body["error"], "Target address is blocked");
    }

    #[actix_web::test]
    async fn requires_basic_auth_for_a_job() {
        let (totp, _auth) = auth_header();
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(Arc::new(InMemoryStorage::new())))
                .app_data(web::Data::new(totp))
                .app_data(web::Data::new(JobStore::new()))
                .configure(configure::<InMemoryStorage>),
        )
        .await;

        let request = test::TestRequest::post()
            .uri("/api/remote-uploads")
            .insert_header((header::CONTENT_TYPE, "text/plain"))
            .set_payload("curl https://example.com/")
            .to_request();
        let response = test::call_service(&app, request).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let body: serde_json::Value = test::read_body_json(response).await;
        assert_eq!(body["error"], "Basic auth is required");
    }

    #[actix_web::test]
    async fn rejects_an_unknown_curl_option_for_a_job() {
        let (totp, auth) = auth_header();
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(Arc::new(InMemoryStorage::new())))
                .app_data(web::Data::new(totp))
                .app_data(web::Data::new(JobStore::new()))
                .configure(configure::<InMemoryStorage>),
        )
        .await;

        let request = test::TestRequest::post()
            .uri("/api/remote-uploads")
            .insert_header((header::AUTHORIZATION, auth))
            .insert_header((header::CONTENT_TYPE, "text/plain"))
            .set_payload("curl --upload-file x https://example.com")
            .to_request();
        let response = test::call_service(&app, request).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body: serde_json::Value = test::read_body_json(response).await;
        assert!(body["error"].is_string());
    }

    #[actix_web::test]
    async fn checks_a_job_token_and_an_unknown_job() {
        let (totp, _auth) = auth_header();
        let store = JobStore::new();
        let (id, job) = store
            .create(PathBuf::from("./AbCdEfGh/ZmlsZQ"), None)
            .expect("create a job");
        let token = job.token().to_owned();
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(Arc::new(InMemoryStorage::new())))
                .app_data(web::Data::new(totp))
                .app_data(web::Data::new(store))
                .configure(configure::<InMemoryStorage>),
        )
        .await;

        let request = test::TestRequest::get()
            .uri(&format!("/api/remote-uploads/{id}"))
            .to_request();
        let response = test::call_service(&app, request).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let request = test::TestRequest::get()
            .uri(&format!("/api/remote-uploads/{id}"))
            .insert_header((header::AUTHORIZATION, "Bearer wrong-token"))
            .to_request();
        let response = test::call_service(&app, request).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let request = test::TestRequest::get()
            .uri("/api/remote-uploads/unknown")
            .insert_header((header::AUTHORIZATION, format!("Bearer {token}")))
            .to_request();
        let response = test::call_service(&app, request).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[actix_web::test]
    async fn returns_a_fetching_job_status() {
        let (totp, _auth) = auth_header();
        let store = JobStore::new();
        let (id, job) = store
            .create(PathBuf::from("./AbCdEfGh/ZmlsZQ"), Some(100))
            .expect("create a job");
        job.mark_received(10);
        let token = job.token().to_owned();
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(Arc::new(InMemoryStorage::new())))
                .app_data(web::Data::new(totp))
                .app_data(web::Data::new(store))
                .configure(configure::<InMemoryStorage>),
        )
        .await;

        let request = test::TestRequest::get()
            .uri(&format!("/api/remote-uploads/{id}"))
            .insert_header((header::AUTHORIZATION, format!("Bearer {token}")))
            .to_request();
        let response = test::call_service(&app, request).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL),
            Some(&header::HeaderValue::from_static("no-store"))
        );
        assert_eq!(
            response.headers().get(header::RETRY_AFTER),
            Some(&header::HeaderValue::from_static("1"))
        );

        let body: serde_json::Value = test::read_body_json(response).await;
        assert_eq!(body["id"], id);
        assert_eq!(body["state"], "fetching");
        assert_eq!(body["received_bytes"], 10);
        assert_eq!(body["total_bytes"], 100);
        assert!(body.get("url").is_none());
        assert!(body.get("token").is_none());
    }

    #[actix_web::test]
    async fn returns_a_done_job_status() {
        let (totp, _auth) = auth_header();
        let store = JobStore::new();
        let (id, job) = store
            .create(PathBuf::from("./AbCdEfGh/ZmlsZQ"), Some(100))
            .expect("create a job");
        job.mark_received(100);
        job.finish_done("http://localhost:8080/AbCdEfGh/ZmlsZQ".to_owned());
        let token = job.token().to_owned();
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(Arc::new(InMemoryStorage::new())))
                .app_data(web::Data::new(totp))
                .app_data(web::Data::new(store))
                .configure(configure::<InMemoryStorage>),
        )
        .await;

        let request = test::TestRequest::get()
            .uri(&format!("/api/remote-uploads/{id}"))
            .insert_header((header::AUTHORIZATION, format!("Bearer {token}")))
            .to_request();
        let response = test::call_service(&app, request).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers().get(header::RETRY_AFTER).is_none());

        let body: serde_json::Value = test::read_body_json(response).await;
        assert_eq!(body["state"], "done");
        assert_eq!(body["received_bytes"], 100);
        assert_eq!(body["url"], "http://localhost:8080/AbCdEfGh/ZmlsZQ");
        assert!(body.get("error").is_none());
    }

    #[actix_web::test]
    async fn cancel_deletes_a_partial_object() {
        let (totp, _auth) = auth_header();
        let storage = Arc::new(InMemoryStorage::new());
        let store = JobStore::new();
        let path = PathBuf::from("./AbCdEfGh/ZmlsZQ");
        let (id, job) = store.create(path.clone(), None).expect("create a job");
        let token = job.token().to_owned();
        storage
            .put(
                &path,
                Box::pin(std::io::Cursor::new(b"partial".to_vec())),
                &Default::default(),
            )
            .await
            .expect("store a partial object");

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(Arc::clone(&storage)))
                .app_data(web::Data::new(totp))
                .app_data(web::Data::new(store.clone()))
                .configure(configure::<InMemoryStorage>),
        )
        .await;

        let request = test::TestRequest::delete()
            .uri(&format!("/api/remote-uploads/{id}"))
            .insert_header((header::AUTHORIZATION, format!("Bearer {token}")))
            .to_request();
        let response = test::call_service(&app, request).await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        assert!(
            storage
                .get(&path, &Default::default())
                .await
                .expect("read the partial object")
                .is_none()
        );
        assert!(store.get(&id).is_none());
    }

    #[actix_web::test]
    async fn cancel_keeps_a_done_object() {
        let (totp, _auth) = auth_header();
        let storage = Arc::new(InMemoryStorage::new());
        let store = JobStore::new();
        let path = PathBuf::from("./AbCdEfGh/ZmlsZQ");
        let (id, job) = store.create(path.clone(), None).expect("create a job");
        let token = job.token().to_owned();
        storage
            .put(
                &path,
                Box::pin(std::io::Cursor::new(b"complete".to_vec())),
                &Default::default(),
            )
            .await
            .expect("store a complete object");
        job.finish_done("http://localhost:8080/AbCdEfGh/ZmlsZQ".to_owned());

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(Arc::clone(&storage)))
                .app_data(web::Data::new(totp))
                .app_data(web::Data::new(store.clone()))
                .configure(configure::<InMemoryStorage>),
        )
        .await;

        let request = test::TestRequest::delete()
            .uri(&format!("/api/remote-uploads/{id}"))
            .insert_header((header::AUTHORIZATION, format!("Bearer {token}")))
            .to_request();
        let response = test::call_service(&app, request).await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        assert!(
            storage
                .get(&path, &Default::default())
                .await
                .expect("read the complete object")
                .is_some()
        );
        assert!(store.get(&id).is_none());
    }

    #[actix_web::test]
    async fn cancel_returns_not_found_for_an_unknown_job() {
        let (totp, _auth) = auth_header();
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(Arc::new(InMemoryStorage::new())))
                .app_data(web::Data::new(totp))
                .app_data(web::Data::new(JobStore::new()))
                .configure(configure::<InMemoryStorage>),
        )
        .await;

        let request = test::TestRequest::delete()
            .uri("/api/remote-uploads/unknown")
            .insert_header((header::AUTHORIZATION, "Bearer unknown"))
            .to_request();
        let response = test::call_service(&app, request).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[actix_web::test]
    async fn cancel_accepts_only_a_bearer_token() {
        let (totp, _auth) = auth_header();
        let storage = EncryptedStorage::new(InMemoryStorage::new(), "test-salt".to_owned());
        let store = JobStore::new();
        let (id, job) = store
            .create(PathBuf::from("./AbCdEfGh/ZmlsZQ"), None)
            .expect("create a job");
        let token = job.token().to_owned();
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(Arc::new(storage)))
                .app_data(web::Data::new(totp))
                .app_data(web::Data::new(store.clone()))
                .configure(configure::<EncryptedStorage<InMemoryStorage>>),
        )
        .await;

        let request = test::TestRequest::delete()
            .uri(&format!("/api/remote-uploads/{id}"))
            .insert_header((header::AUTHORIZATION, format!("Bearer {token}")))
            .to_request();
        let response = test::call_service(&app, request).await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(store.get(&id).is_none());
    }

    #[actix_web::test]
    async fn cancel_deletes_the_persisted_record() {
        let (totp, _auth) = auth_header();
        let dir = tempfile::tempdir().expect("create the record directory");
        let store = JobStore::open(dir.path()).expect("open the job store");
        let (id, job) = store
            .create(PathBuf::from("./AbCdEfGh/ZmlsZQ"), None)
            .expect("create a job");
        let token = job.token().to_owned();
        let record = dir.path().join(format!("{id}.json"));
        assert!(record.exists());
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(Arc::new(InMemoryStorage::new())))
                .app_data(web::Data::new(totp))
                .app_data(web::Data::new(store.clone()))
                .configure(configure::<InMemoryStorage>),
        )
        .await;

        let request = test::TestRequest::delete()
            .uri(&format!("/api/remote-uploads/{id}"))
            .insert_header((header::AUTHORIZATION, format!("Bearer {token}")))
            .to_request();
        let response = test::call_service(&app, request).await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(!record.exists());
    }

    #[actix_web::test]
    async fn rejects_a_job_when_the_record_cannot_be_written() {
        let (totp, auth) = auth_header();
        let dir = tempfile::tempdir().expect("create the record directory");
        let store = JobStore::open(dir.path()).expect("open the job store");
        std::fs::remove_dir_all(dir.path()).expect("remove the record directory");
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(Arc::new(InMemoryStorage::new())))
                .app_data(web::Data::new(totp))
                .app_data(web::Data::new(store.clone()))
                .configure(configure::<InMemoryStorage>),
        )
        .await;

        let request = test::TestRequest::post()
            .uri("/api/remote-uploads")
            .insert_header((header::AUTHORIZATION, auth))
            .insert_header((header::CONTENT_TYPE, "text/plain"))
            .set_payload("curl https://1.1.1.1/")
            .to_request();
        let response = test::call_service(&app, request).await;
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body: serde_json::Value = test::read_body_json(response).await;
        assert_eq!(body["error"], "Cannot store the job record");
        assert_eq!(store.prune_expired(), 0);
    }

    #[tokio::test]
    async fn background_job_stores_the_target_body() {
        let response =
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello".to_vec();
        let address = one_shot_http_server(response).await;
        let storage = Arc::new(InMemoryStorage::new());
        let store = JobStore::new();
        let (_id, job) = store
            .create(PathBuf::from("./AbCdEfGh/ZmlsZQ"), None)
            .expect("create a job");

        run_remote_job(
            Arc::clone(&job),
            Arc::clone(&storage),
            Default::default(),
            prepared_loopback_get(address, "/file"),
            "http://localhost/file".to_owned(),
            job.cancel_receiver(),
        )
        .await;

        let status = job.status();
        assert_eq!(status.state.name(), "done");
        assert_eq!(status.received_bytes, 5);
        assert_eq!(status.total_bytes, Some(5));

        let mut reader = storage
            .get(job.path(), &Default::default())
            .await
            .expect("read stored file")
            .expect("stored file must exist");
        let mut body = Vec::new();
        reader
            .read_to_end(&mut body)
            .await
            .expect("read stored body");
        assert_eq!(body, b"hello");
    }

    #[tokio::test]
    async fn background_job_records_a_target_failure() {
        let response =
            b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_vec();
        let address = one_shot_http_server(response).await;
        let storage = Arc::new(InMemoryStorage::new());
        let store = JobStore::new();
        let (_id, job) = store
            .create(PathBuf::from("./AbCdEfGh/ZmlsZQ"), None)
            .expect("create a job");

        run_remote_job(
            Arc::clone(&job),
            Arc::clone(&storage),
            Default::default(),
            prepared_loopback_get(address, "/file"),
            "http://localhost/file".to_owned(),
            job.cancel_receiver(),
        )
        .await;

        let status = job.status();
        assert_eq!(status.state.name(), "failed");
        match status.state {
            JobState::Failed { error } => {
                assert_eq!(error, "Target returned an error status");
            }
            _ => panic!("job must be failed"),
        }
        assert_eq!(status.received_bytes, 0);
        assert_eq!(status.total_bytes, None);
        assert!(
            storage
                .get(job.path(), &Default::default())
                .await
                .expect("read stored file")
                .is_none()
        );
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
                .app_data(web::Data::new(JobStore::new()))
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

    #[actix_web::test]
    #[ignore = "requires network and FILEPASTE_LIVE_FETCH_TEST=1"]
    async fn fetches_a_public_url_through_a_job() {
        if std::env::var_os("FILEPASTE_LIVE_FETCH_TEST").is_none() {
            return;
        }

        let (totp, auth) = auth_header();
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(Arc::new(InMemoryStorage::new())))
                .app_data(web::Data::new(totp))
                .app_data(web::Data::new(JobStore::new()))
                .configure(configure::<InMemoryStorage>),
        )
        .await;

        let request = test::TestRequest::post()
            .uri("/api/remote-uploads")
            .insert_header((header::AUTHORIZATION, auth))
            .insert_header((header::CONTENT_TYPE, "text/plain"))
            .set_payload("curl https://example.com/")
            .to_request();
        let response = test::call_service(&app, request).await;
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let location = response
            .headers()
            .get(header::LOCATION)
            .expect("job location")
            .to_str()
            .expect("job location text")
            .to_owned();
        let body: serde_json::Value = test::read_body_json(response).await;
        let id = body["id"].as_str().expect("job id").to_owned();
        let token = body["token"].as_str().expect("job token").to_owned();
        assert_eq!(location, format!("/api/remote-uploads/{id}"));

        let mut file_url = None;
        for _ in 0..80 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let request = test::TestRequest::get()
                .uri(&format!("/api/remote-uploads/{id}"))
                .insert_header((header::AUTHORIZATION, format!("Bearer {token}")))
                .to_request();
            let response = test::call_service(&app, request).await;
            let body: serde_json::Value = test::read_body_json(response).await;
            match body["state"].as_str() {
                Some("done") => {
                    file_url = body["url"].as_str().map(str::to_owned);
                    break;
                }
                Some("failed") => panic!("job failed: {}", body["error"]),
                _ => (),
            }
        }
        let file_url = file_url.expect("job did not finish");
        let parsed = url::Url::parse(&file_url).expect("parse the file URL");

        let request = test::TestRequest::get().uri(parsed.path()).to_request();
        let response = test::call_service(&app, request).await;
        assert!(response.status().is_success(), "download failed");
        let body = test::read_body(response).await;
        assert!(!body.is_empty());
    }
}
