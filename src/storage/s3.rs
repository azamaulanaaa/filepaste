use std::future::{Ready, ready};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::sync::Mutex;
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime};

use actix_web::{Error, FromRequest, HttpRequest, dev::Payload};
use async_trait::async_trait;
use aws_credential_types::Credentials;
use aws_credential_types::provider::{self, ProvideCredentials, error::CredentialsError, future};
use aws_sdk_s3::Client;
use aws_sdk_s3::error::{DisplayErrorContext, SdkError};
use aws_sdk_s3::operation::abort_multipart_upload::AbortMultipartUploadError;
use aws_sdk_s3::operation::get_object::GetObjectError;
use aws_sdk_s3::operation::head_object::HeadObjectError;
use aws_sdk_s3::primitives::{ByteStream, DateTime};
use aws_sdk_s3::types::{CommonPrefix, CompletedMultipartUpload, CompletedPart, Object};
use aws_types::region::Region;
use base64ct::{Base64UrlUnpadded, Encoding};
use bytes::Bytes;
use http_body::{Body, Frame, SizeHint};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, ReadBuf};
use tracing::{debug, info, warn};

use super::{AsyncFileReader, DirMetadata, FileMetadata, Resource, StorageProvider};

/// Reads AWS credentials from the process environment at request time.
///
/// This provider supports environment variables only. It does not read
/// shared credential files or the instance metadata service.
#[derive(Debug, Default, Clone, Copy)]
struct EnvCredentialsProvider;

impl EnvCredentialsProvider {
    async fn load_credentials(&self) -> provider::Result {
        let access_key = std::env::var("AWS_ACCESS_KEY_ID")
            .map_err(|_| CredentialsError::provider_error("AWS_ACCESS_KEY_ID is not set"))?;
        let secret_key = std::env::var("AWS_SECRET_ACCESS_KEY")
            .map_err(|_| CredentialsError::provider_error("AWS_SECRET_ACCESS_KEY is not set"))?;
        let session_token = std::env::var("AWS_SESSION_TOKEN").ok();
        Ok(Credentials::new(
            access_key,
            secret_key,
            session_token,
            None,
            "Environment",
        ))
    }
}

impl ProvideCredentials for EnvCredentialsProvider {
    fn provide_credentials<'a>(&'a self) -> future::ProvideCredentials<'a>
    where
        Self: 'a,
    {
        future::ProvideCredentials::new(self.load_credentials())
    }
}

/// Request context for S3 storage.
#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub struct S3Context {}

impl FromRequest for S3Context {
    type Error = Error;
    type Future = Ready<Result<Self, Self::Error>>;

    fn from_request(_req: &HttpRequest, _payload: &mut Payload) -> Self::Future {
        ready(Ok(Self::default()))
    }
}

/// Selects the S3 request strategy for an upload.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum S3UploadMode {
    /// Use one PutObject when the length is known and at most 5 GiB.
    #[default]
    PreferSingle,
    /// Use one PutObject and reject an unknown or large length.
    RequireSingle,
    /// Use the multipart upload path.
    Multipart,
}

/// The largest object that one S3 PutObject call accepts.
pub const MAX_SINGLE_PUT_SIZE: u64 = 5 * 1024 * 1024 * 1024;

/// The buffer size for one streaming PutObject frame.
const SINGLE_PUT_BUFFER_SIZE: usize = 64 * 1024;

/// Stores file content in an Amazon S3 bucket.
pub struct S3Storage {
    client: Client,
    bucket: String,
    prefix: String,
    multipart_expiry: Option<Duration>,
    upload_mode: S3UploadMode,
    multipart_part_size: usize,
}

impl S3Storage {
    /// Creates an S3 storage provider from the bucket, region, key prefix, and optional endpoint.
    /// Environment variables supply the access keys.
    pub async fn new(
        bucket: String,
        region: String,
        key_prefix: String,
        endpoint_url: Option<String>,
    ) -> io::Result<Self> {
        Self::new_with_multipart_expiry(
            bucket,
            region,
            key_prefix,
            endpoint_url,
            DEFAULT_MULTIPART_UPLOAD_EXPIRY_HOURS,
        )
        .await
    }

    /// Creates an S3 storage provider with an incomplete-upload expiry in hours.
    ///
    /// A value of zero disables cleanup.
    /// A positive value is at least one hour.
    pub async fn new_with_multipart_expiry(
        bucket: String,
        region: String,
        key_prefix: String,
        endpoint_url: Option<String>,
        multipart_upload_expiry_hours: u64,
    ) -> io::Result<Self> {
        Self::new_with_options(
            bucket,
            region,
            key_prefix,
            endpoint_url,
            multipart_upload_expiry_hours,
            S3UploadMode::default(),
            DEFAULT_MULTIPART_PART_SIZE_MB,
        )
        .await
    }

    /// Creates an S3 storage provider with the expiry, upload mode, and part size.
    ///
    /// A value of zero disables cleanup.
    /// A positive value is at least one hour.
    pub async fn new_with_options(
        bucket: String,
        region: String,
        key_prefix: String,
        endpoint_url: Option<String>,
        multipart_upload_expiry_hours: u64,
        upload_mode: S3UploadMode,
        multipart_part_size_mb: u64,
    ) -> io::Result<Self> {
        if bucket.trim().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "S3 bucket must not be empty",
            ));
        }
        if region.trim().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "S3 region must not be empty",
            ));
        }
        let prefix = normalize_prefix(&key_prefix)?;
        let multipart_part_size = multipart_part_size(multipart_part_size_mb)?;

        let mut builder = aws_sdk_s3::config::Builder::new()
            .behavior_version_latest()
            .region(Region::new(region))
            .credentials_provider(EnvCredentialsProvider)
            .force_path_style(endpoint_url.is_some());
        if let Some(url) = endpoint_url.as_deref() {
            if url.trim().is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "S3 endpoint URL must not be empty",
                ));
            }
            builder = builder.endpoint_url(url.to_owned());
        }

        Ok(Self {
            client: Client::from_conf(builder.build()),
            bucket,
            prefix,
            multipart_expiry: multipart_expiry(multipart_upload_expiry_hours),
            upload_mode,
            multipart_part_size,
        })
    }

    /// Returns true when the key belongs to this service.
    fn key_belongs_to_service(&self, key: &str) -> bool {
        if self.prefix.is_empty() {
            return is_app_key(key);
        }
        key.strip_prefix(&self.prefix)
            .is_some_and(|rest| rest.starts_with('/'))
    }

    /// Streams one object with one PutObject call.
    async fn put_single(
        &self,
        key: &str,
        content: AsyncFileReader,
        length: u64,
    ) -> io::Result<u64> {
        let content_length = i64::try_from(length).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "S3 object is too large to upload",
            )
        })?;
        let body = SinglePutBody::new(content, length);

        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(ByteStream::from_body_1_x(body))
            .content_length(content_length)
            .send()
            .await
            .map_err(|err| sdk_io_error("PutObject", err))?;

        Ok(length)
    }

    /// Uploads one object with the multipart path.
    async fn put_multipart(&self, key: &str, mut content: AsyncFileReader) -> io::Result<u64> {
        let sink = S3UploadSink { storage: self };
        upload_reader(&sink, key, &mut content, self.multipart_part_size).await
    }
}

/// The request strategy for one upload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UploadStrategy {
    SinglePut,
    Multipart,
}

/// Selects the request strategy for a known input length.
fn select_upload_strategy(mode: S3UploadMode, length: u64) -> io::Result<UploadStrategy> {
    match mode {
        S3UploadMode::RequireSingle => {
            if length > MAX_SINGLE_PUT_SIZE {
                Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "S3 object is larger than the 5 GiB single PutObject limit",
                ))
            } else {
                Ok(UploadStrategy::SinglePut)
            }
        }
        S3UploadMode::PreferSingle => {
            if length <= MAX_SINGLE_PUT_SIZE {
                Ok(UploadStrategy::SinglePut)
            } else {
                Ok(UploadStrategy::Multipart)
            }
        }
        S3UploadMode::Multipart => Ok(UploadStrategy::Multipart),
    }
}

/// Default expiry for incomplete multipart uploads, in hours.
pub const DEFAULT_MULTIPART_UPLOAD_EXPIRY_HOURS: u64 = 24;

/// Converts an expiry in hours into a cleanup window.
///
/// A value of zero disables cleanup.
/// A positive value is at least one hour.
fn multipart_expiry(hours: u64) -> Option<Duration> {
    if hours == 0 {
        None
    } else {
        let seconds = hours.max(1).saturating_mul(60 * 60);
        Some(Duration::from_secs(seconds))
    }
}

/// Returns true when the key uses the service key shape.
///
/// The service uses an 8-character alphanumeric directory and a
/// Base64Url filename.
fn is_app_key(key: &str) -> bool {
    let Some((directory, filename)) = key.split_once('/') else {
        return false;
    };
    if directory.len() != 8 || !directory.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        return false;
    }
    if filename.is_empty() {
        return false;
    }
    let Ok(decoded) = Base64UrlUnpadded::decode_vec(filename) else {
        return false;
    };
    std::str::from_utf8(&decoded).is_ok()
}

/// Converts a configured key prefix into slash-separated parts.
fn normalize_prefix(raw: &str) -> io::Result<String> {
    let parts = collect_parts(Path::new(raw))?;
    Ok(parts.join("/"))
}

/// Collects the normal parts of a path and rejects the other components.
fn collect_parts(path: &Path) -> io::Result<Vec<&str>> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(name) => {
                let part = name.to_str().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "Path must use UTF-8")
                })?;
                parts.push(part);
            }
            Component::CurDir => {}
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Path must be relative and contain no parent directory",
                ));
            }
        }
    }
    Ok(parts)
}

/// Joins a key prefix with path parts by using the S3 key separator.
fn join_key(prefix: &str, parts: &[&str]) -> String {
    let tail = parts.join("/");
    match (prefix.is_empty(), tail.is_empty()) {
        (true, _) => tail,
        (false, true) => prefix.to_owned(),
        (false, false) => format!("{prefix}/{tail}"),
    }
}

/// Builds an object key from the configured prefix and a storage path.
fn object_key(prefix: &str, path: &Path) -> io::Result<String> {
    let parts = collect_parts(path)?;
    if parts.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Storage path must name an object",
        ));
    }
    Ok(join_key(prefix, &parts))
}

/// Builds a listing prefix that ends at a directory boundary.
fn list_key_prefix(prefix: &str, path: &Path) -> io::Result<String> {
    let parts = collect_parts(path)?;
    let mut key = join_key(prefix, &parts);
    if !key.is_empty() {
        key.push('/');
    }
    Ok(key)
}

/// Removes the configured prefix from an object key.
fn relative_key<'a>(key: &'a str, prefix: &str) -> Option<&'a str> {
    if prefix.is_empty() {
        Some(key)
    } else {
        key.strip_prefix(prefix)?.strip_prefix('/')
    }
}

/// Converts a stored size into a file size.
fn object_size(size: Option<i64>) -> io::Result<u64> {
    u64::try_from(size.unwrap_or_default())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "S3 object has a negative size"))
}

/// Converts a stored time into a system time.
fn object_modified(modified: Option<&DateTime>) -> io::Result<SystemTime> {
    let Some(modified) = modified else {
        return Ok(SystemTime::UNIX_EPOCH);
    };
    SystemTime::try_from(*modified)
        .map_err(|err| io::Error::other(format!("S3 last-modified time is invalid: {err}")))
}

/// Maps an SDK error into an I/O error with the operation name and cause.
fn sdk_io_error<E, R>(operation: &str, err: SdkError<E, R>) -> io::Error
where
    E: std::error::Error + 'static,
    R: std::fmt::Debug,
{
    io::Error::other(format!(
        "S3 {operation} failed: {}",
        DisplayErrorContext(&err)
    ))
}

/// Converts one page of S3 listing results into storage resources.
fn convert_listing(
    prefix: &str,
    objects: &[Object],
    common_prefixes: &[CommonPrefix],
) -> io::Result<Vec<Resource>> {
    let mut resources = Vec::new();

    for object in objects {
        let Some(key) = object.key() else {
            continue;
        };
        let Some(relative) = relative_key(key, prefix) else {
            continue;
        };
        if relative.is_empty() || relative.ends_with('/') {
            continue;
        }
        resources.push(Resource::File {
            path: PathBuf::from(relative),
            metadata: FileMetadata {
                size: object_size(object.size())?,
                modified: object_modified(object.last_modified())?,
            },
        });
    }

    for common in common_prefixes {
        let Some(raw) = common.prefix() else {
            continue;
        };
        let Some(relative) = relative_key(raw.trim_end_matches('/'), prefix) else {
            continue;
        };
        if relative.is_empty() {
            continue;
        }
        // S3 does not report a modification time for a common prefix.
        resources.push(Resource::Directory {
            path: PathBuf::from(relative),
            metadata: DirMetadata {
                modified: SystemTime::UNIX_EPOCH,
            },
        });
    }

    Ok(resources)
}

/// The default multipart part size in MiB.
pub const DEFAULT_MULTIPART_PART_SIZE_MB: u64 = 8;

/// The smallest multipart part size in MiB.
pub const MIN_MULTIPART_PART_SIZE_MB: u64 = 5;

/// The largest multipart part size in MiB.
pub const MAX_MULTIPART_PART_SIZE_MB: u64 = 128;

/// The default multipart part size in bytes.
///
/// The tests use this value. Production uploads use the configured part size.
#[cfg(test)]
const DEFAULT_PART_SIZE: usize = 8 * 1024 * 1024;

/// Largest number of parts that S3 accepts in one multipart upload.
const MAX_PARTS: i32 = 10_000;

/// Validates a configured multipart part size and converts it to bytes.
///
/// The server holds one part in memory, so the upper bound limits peak memory.
fn multipart_part_size(megabytes: u64) -> io::Result<usize> {
    if !(MIN_MULTIPART_PART_SIZE_MB..=MAX_MULTIPART_PART_SIZE_MB).contains(&megabytes) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "S3 multipart part size must be between 5 and 128 MiB",
        ));
    }
    let megabytes = usize::try_from(megabytes).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "S3 multipart part size is too large",
        )
    })?;
    Ok(megabytes * 1024 * 1024)
}

/// Stores the parts of one S3 upload.
///
/// The tests use this trait to check the multipart sequence without a
/// real bucket.
#[async_trait]
trait UploadSink {
    /// Stores an object that fits in one part.
    async fn put_object(&self, key: &str, body: Vec<u8>) -> io::Result<()>;

    /// Starts a multipart upload and returns the upload ID.
    async fn start_multipart(&self, key: &str) -> io::Result<String>;

    /// Stores one part and returns its ETag.
    async fn put_part(
        &self,
        key: &str,
        upload_id: &str,
        part_number: i32,
        body: Vec<u8>,
    ) -> io::Result<String>;

    /// Completes the multipart upload with the part ETags.
    async fn complete_multipart(
        &self,
        key: &str,
        upload_id: &str,
        etags: Vec<String>,
    ) -> io::Result<()>;

    /// Aborts the multipart upload.
    async fn abort_multipart(&self, key: &str, upload_id: &str) -> io::Result<()>;
}

/// Uploads the reader to the sink and returns the number of bytes read.
///
/// The helper holds one part in memory at a time.
/// A short first read uses a single PutObject call.
/// A multipart upload is aborted when the reader or an upload call fails.
async fn upload_reader<S: UploadSink + Sync>(
    sink: &S,
    key: &str,
    reader: &mut AsyncFileReader,
    part_size: usize,
) -> io::Result<u64> {
    if part_size == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Upload part size must be greater than zero",
        ));
    }

    let mut buffer = vec![0_u8; part_size];
    let filled = read_part(reader, &mut buffer).await?;

    if filled < part_size {
        buffer.truncate(filled);
        sink.put_object(key, buffer).await?;
        return Ok(filled as u64);
    }

    let upload_id = sink.start_multipart(key).await?;
    let result = upload_parts(sink, key, reader, &upload_id, buffer, part_size).await;
    if result.is_err() {
        // Keep the original error. An abort failure must not hide it.
        let _ = sink.abort_multipart(key, &upload_id).await;
    }
    result
}

/// Uploads the first full part and every later part.
///
/// The caller aborts the multipart upload on error.
async fn upload_parts<S: UploadSink + Sync>(
    sink: &S,
    key: &str,
    reader: &mut AsyncFileReader,
    upload_id: &str,
    mut buffer: Vec<u8>,
    part_size: usize,
) -> io::Result<u64> {
    let mut written = 0_u64;
    let mut part_number = 1_i32;
    let mut etags = Vec::new();

    loop {
        written += buffer.len() as u64;
        etags.push(sink.put_part(key, upload_id, part_number, buffer).await?);

        buffer = vec![0_u8; part_size];
        let filled = read_part(reader, &mut buffer).await?;
        buffer.truncate(filled);

        if filled == 0 {
            break;
        }
        if part_number == MAX_PARTS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "S3 object needs more than 10000 parts",
            ));
        }
        part_number += 1;
        if filled < part_size {
            written += filled as u64;
            etags.push(sink.put_part(key, upload_id, part_number, buffer).await?);
            break;
        }
    }

    sink.complete_multipart(key, upload_id, etags).await?;
    Ok(written)
}

/// Fills the buffer from the reader and returns the number of bytes read.
///
/// A short result occurs only at end of input.
async fn read_part(reader: &mut AsyncFileReader, buffer: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..]).await {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
    Ok(filled)
}

/// Converts a part length into the SDK content length.
fn body_length(body: &[u8]) -> io::Result<i64> {
    i64::try_from(body.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "S3 object is too large to upload",
        )
    })
}

/// Streams a fixed number of bytes for one S3 PutObject call.
///
/// The body holds a fixed buffer and stops after the declared length.
/// A short reader returns an `UnexpectedEof` error.
struct SinglePutBody {
    reader: Mutex<AsyncFileReader>,
    remaining: u64,
    buffer: Vec<u8>,
}

impl SinglePutBody {
    fn new(reader: AsyncFileReader, length: u64) -> Self {
        Self {
            reader: Mutex::new(reader),
            remaining: length,
            buffer: vec![0_u8; SINGLE_PUT_BUFFER_SIZE],
        }
    }
}

impl Body for SinglePutBody {
    type Data = Bytes;
    type Error = io::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        if this.remaining == 0 {
            return Poll::Ready(None);
        }

        let wanted = this.remaining.min(this.buffer.len() as u64) as usize;
        let read = {
            let mut read_buf = ReadBuf::new(&mut this.buffer[..wanted]);
            let poll = match this.reader.lock() {
                Ok(mut reader) => reader.as_mut().poll_read(cx, &mut read_buf),
                Err(_) => {
                    return Poll::Ready(Some(Err(io::Error::other(
                        "S3 upload reader lock is poisoned",
                    ))));
                }
            };
            match poll {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(err)) => return Poll::Ready(Some(Err(err))),
                Poll::Ready(Ok(())) => read_buf.filled().len(),
            }
        };

        if read == 0 {
            return Poll::Ready(Some(Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "S3 upload reader ended before the declared length",
            ))));
        }

        this.remaining -= read as u64;
        Poll::Ready(Some(Ok(Frame::data(Bytes::copy_from_slice(
            &this.buffer[..read],
        )))))
    }

    fn is_end_stream(&self) -> bool {
        self.remaining == 0
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.remaining)
    }
}

/// Stores upload parts in one S3 bucket.
struct S3UploadSink<'a> {
    storage: &'a S3Storage,
}

#[async_trait]
impl UploadSink for S3UploadSink<'_> {
    async fn put_object(&self, key: &str, body: Vec<u8>) -> io::Result<()> {
        let length = body_length(&body)?;
        self.storage
            .client
            .put_object()
            .bucket(&self.storage.bucket)
            .key(key)
            .body(ByteStream::from(body))
            .content_length(length)
            .send()
            .await
            .map_err(|err| sdk_io_error("PutObject", err))?;
        Ok(())
    }

    async fn start_multipart(&self, key: &str) -> io::Result<String> {
        let output = self
            .storage
            .client
            .create_multipart_upload()
            .bucket(&self.storage.bucket)
            .key(key)
            .send()
            .await
            .map_err(|err| sdk_io_error("CreateMultipartUpload", err))?;
        output
            .upload_id()
            .map(str::to_owned)
            .ok_or_else(|| io::Error::other("S3 CreateMultipartUpload returned no upload ID"))
    }

    async fn put_part(
        &self,
        key: &str,
        upload_id: &str,
        part_number: i32,
        body: Vec<u8>,
    ) -> io::Result<String> {
        let length = body_length(&body)?;
        let output = self
            .storage
            .client
            .upload_part()
            .bucket(&self.storage.bucket)
            .key(key)
            .upload_id(upload_id)
            .part_number(part_number)
            .body(ByteStream::from(body))
            .content_length(length)
            .send()
            .await
            .map_err(|err| sdk_io_error("UploadPart", err))?;
        output
            .e_tag()
            .map(str::to_owned)
            .ok_or_else(|| io::Error::other("S3 UploadPart returned no ETag"))
    }

    async fn complete_multipart(
        &self,
        key: &str,
        upload_id: &str,
        etags: Vec<String>,
    ) -> io::Result<()> {
        let parts: Vec<CompletedPart> = etags
            .into_iter()
            .enumerate()
            .map(|(index, etag)| {
                CompletedPart::builder()
                    .part_number(i32::try_from(index + 1).expect("part count is bounded"))
                    .e_tag(etag)
                    .build()
            })
            .collect();
        let upload = CompletedMultipartUpload::builder()
            .set_parts(Some(parts))
            .build();

        self.storage
            .client
            .complete_multipart_upload()
            .bucket(&self.storage.bucket)
            .key(key)
            .upload_id(upload_id)
            .multipart_upload(upload)
            .send()
            .await
            .map_err(|err| sdk_io_error("CompleteMultipartUpload", err))?;
        Ok(())
    }

    async fn abort_multipart(&self, key: &str, upload_id: &str) -> io::Result<()> {
        self.storage
            .client
            .abort_multipart_upload()
            .bucket(&self.storage.bucket)
            .key(key)
            .upload_id(upload_id)
            .send()
            .await
            .map_err(|err| sdk_io_error("AbortMultipartUpload", err))?;
        Ok(())
    }
}

#[async_trait]
impl StorageProvider for S3Storage {
    type Context = S3Context;

    async fn put(
        &self,
        path: &Path,
        content: AsyncFileReader,
        _ctx: &Self::Context,
    ) -> io::Result<u64> {
        let key = object_key(&self.prefix, path)?;
        if self.upload_mode == S3UploadMode::RequireSingle {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "S3 upload mode require_single needs a known content length",
            ));
        }
        self.put_multipart(&key, content).await
    }

    async fn put_sized(
        &self,
        path: &Path,
        content: AsyncFileReader,
        length: u64,
        _ctx: &Self::Context,
    ) -> io::Result<u64> {
        let key = object_key(&self.prefix, path)?;
        match select_upload_strategy(self.upload_mode, length)? {
            UploadStrategy::SinglePut => self.put_single(&key, content, length).await,
            UploadStrategy::Multipart => self.put_multipart(&key, content).await,
        }
    }

    async fn get(&self, path: &Path, _ctx: &Self::Context) -> io::Result<Option<AsyncFileReader>> {
        let key = object_key(&self.prefix, path)?;

        match self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
        {
            Ok(output) => Ok(Some(Box::pin(output.body.into_async_read()))),
            Err(err)
                if err
                    .as_service_error()
                    .is_some_and(GetObjectError::is_no_such_key) =>
            {
                Ok(None)
            }
            Err(err) => Err(sdk_io_error("GetObject", err)),
        }
    }

    async fn delete(&self, path: &Path, _ctx: &Self::Context) -> io::Result<()> {
        let key = object_key(&self.prefix, path)?;

        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|err| sdk_io_error("DeleteObject", err))?;

        Ok(())
    }

    async fn metadata(
        &self,
        path: &Path,
        _ctx: &Self::Context,
    ) -> io::Result<Option<FileMetadata>> {
        let key = object_key(&self.prefix, path)?;

        match self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
        {
            Ok(output) => Ok(Some(FileMetadata {
                size: object_size(output.content_length())?,
                modified: object_modified(output.last_modified())?,
            })),
            Err(err)
                if err
                    .as_service_error()
                    .is_some_and(HeadObjectError::is_not_found) =>
            {
                Ok(None)
            }
            Err(err) => Err(sdk_io_error("HeadObject", err)),
        }
    }

    async fn list(&self, path: &Path, _ctx: &Self::Context) -> io::Result<Vec<Resource>> {
        let prefix = list_key_prefix(&self.prefix, path)?;
        let mut resources = Vec::new();
        let mut continuation_token: Option<String> = None;

        loop {
            let mut request = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(&prefix)
                .delimiter("/");
            if let Some(token) = continuation_token.as_deref() {
                request = request.continuation_token(token);
            }

            let output = request
                .send()
                .await
                .map_err(|err| sdk_io_error("ListObjectsV2", err))?;
            resources.extend(convert_listing(
                &self.prefix,
                output.contents(),
                output.common_prefixes(),
            )?);

            if output.is_truncated() == Some(true) {
                if let Some(token) = output.next_continuation_token() {
                    continuation_token = Some(token.to_owned());
                    continue;
                }
            }
            break;
        }

        Ok(resources)
    }

    async fn cleanup_stale_multipart_uploads(&self, _ctx: &Self::Context) -> io::Result<u64> {
        let Some(expiry) = self.multipart_expiry else {
            return Ok(0);
        };
        let Some(cutoff) = SystemTime::now().checked_sub(expiry) else {
            return Ok(0);
        };

        let mut key_marker: Option<String> = None;
        let mut upload_id_marker: Option<String> = None;
        let mut aborted = 0_u64;

        loop {
            let mut request = self.client.list_multipart_uploads().bucket(&self.bucket);
            if !self.prefix.is_empty() {
                request = request.prefix(&self.prefix);
            }
            if let Some(marker) = key_marker.as_deref() {
                request = request.key_marker(marker);
            }
            if let Some(marker) = upload_id_marker.as_deref() {
                request = request.upload_id_marker(marker);
            }

            let output = request
                .send()
                .await
                .map_err(|err| sdk_io_error("ListMultipartUploads", err))?;

            for upload in output.uploads() {
                let (Some(upload_id), Some(key), Some(initiated)) =
                    (upload.upload_id(), upload.key(), upload.initiated())
                else {
                    debug!("Skipping multipart upload entry with missing fields");
                    continue;
                };
                if !self.key_belongs_to_service(key) {
                    debug!("Skipping multipart upload for foreign key: {key}");
                    continue;
                }
                let initiated = match SystemTime::try_from(*initiated) {
                    Ok(initiated) => initiated,
                    Err(err) => {
                        debug!("Skipping multipart upload {upload_id} with invalid time: {err}");
                        continue;
                    }
                };
                if initiated >= cutoff {
                    continue;
                }

                match self
                    .client
                    .abort_multipart_upload()
                    .bucket(&self.bucket)
                    .key(key)
                    .upload_id(upload_id)
                    .send()
                    .await
                {
                    Ok(_) => {
                        aborted += 1;
                        info!("Aborted stale multipart upload: key={key} upload_id={upload_id}");
                    }
                    Err(err)
                        if err
                            .as_service_error()
                            .is_some_and(AbortMultipartUploadError::is_no_such_upload) =>
                    {
                        debug!(
                            "Stale multipart upload is already gone: key={key} upload_id={upload_id}"
                        );
                    }
                    Err(err) => {
                        warn!(
                            "Failed to abort stale multipart upload: key={key} upload_id={upload_id}: {}",
                            DisplayErrorContext(&err)
                        );
                    }
                }
            }

            if output.is_truncated() != Some(true) {
                break;
            }
            let next_key_marker = output.next_key_marker().map(str::to_owned);
            let next_upload_id_marker = output.next_upload_id_marker().map(str::to_owned);
            if next_key_marker.is_none() && next_upload_id_marker.is_none() {
                warn!("ListMultipartUploads returned a truncated page without markers");
                break;
            }
            key_marker = next_key_marker;
            upload_id_marker = next_upload_id_marker;
        }

        Ok(aborted)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use tokio::io::{AsyncRead, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    use super::*;

    #[test]
    fn object_key_joins_prefix_with_normal_parts() {
        assert_eq!(
            object_key("files", Path::new("a/b.txt")).expect("object key"),
            "files/a/b.txt"
        );
        assert_eq!(
            object_key("", Path::new("a/b.txt")).expect("object key"),
            "a/b.txt"
        );
        assert_eq!(
            object_key("files/inner", Path::new("a.txt")).expect("object key"),
            "files/inner/a.txt"
        );
    }

    #[test]
    fn object_key_accepts_current_directory_component() {
        assert_eq!(
            object_key("files", Path::new("./a.txt")).expect("object key"),
            "files/a.txt"
        );
    }

    #[test]
    fn object_key_rejects_absolute_path() {
        let err =
            object_key("files", Path::new("/etc/passwd")).expect_err("absolute path must fail");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn object_key_rejects_parent_directory_component() {
        let err = object_key("files", Path::new("a/../b.txt")).expect_err("parent path must fail");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn object_key_rejects_empty_path() {
        let err = object_key("files", Path::new("")).expect_err("empty path must fail");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn list_key_prefix_ends_with_key_separator() {
        assert_eq!(
            list_key_prefix("files", Path::new("dir")).expect("list prefix"),
            "files/dir/"
        );
        assert_eq!(
            list_key_prefix("files", Path::new("")).expect("list prefix"),
            "files/"
        );
        assert_eq!(list_key_prefix("", Path::new("")).expect("list prefix"), "");
    }

    #[test]
    fn normalize_prefix_keeps_relative_parts() {
        assert_eq!(
            normalize_prefix("files/inner/").expect("key prefix"),
            "files/inner"
        );
        assert!(normalize_prefix("../files").is_err());
        assert!(normalize_prefix("/files").is_err());
    }

    #[tokio::test]
    async fn new_rejects_empty_bucket() {
        let err = S3Storage::new(String::new(), "region".to_owned(), String::new(), None)
            .await
            .err()
            .expect("empty bucket must fail");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[tokio::test]
    async fn new_rejects_empty_region() {
        let err = S3Storage::new("bucket".to_owned(), String::new(), String::new(), None)
            .await
            .err()
            .expect("empty region must fail");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[tokio::test]
    async fn new_rejects_empty_endpoint_url() {
        let err = S3Storage::new(
            "bucket".to_owned(),
            "region".to_owned(),
            String::new(),
            Some("  ".to_owned()),
        )
        .await
        .err()
        .expect("empty endpoint must fail");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn convert_listing_maps_files_and_directories() {
        let objects = vec![
            Object::builder()
                .key("files/a.txt")
                .size(4)
                .last_modified(DateTime::from_secs(1_000))
                .build(),
            Object::builder().key("files/dir/").build(),
            Object::builder().key("other/b.txt").size(2).build(),
        ];
        let prefixes = vec![CommonPrefix::builder().prefix("files/dir/").build()];

        let resources = convert_listing("files", &objects, &prefixes).expect("listing");

        assert_eq!(resources.len(), 2);
        assert!(resources.iter().any(|resource| matches!(
            resource,
            Resource::File { path, metadata }
                if path == Path::new("a.txt") && metadata.size == 4
        )));
        assert!(resources.iter().any(|resource| matches!(
            resource,
            Resource::Directory { path, .. } if path == Path::new("dir")
        )));
    }

    /// Records the calls that the upload helper makes.
    #[derive(Default)]
    struct RecordingSink {
        single: Mutex<Vec<Vec<u8>>>,
        parts: Mutex<Vec<(i32, Vec<u8>)>>,
        completed: Mutex<Option<Vec<String>>>,
        aborted: AtomicBool,
        fail_part: Option<i32>,
    }

    #[async_trait]
    impl UploadSink for RecordingSink {
        async fn put_object(&self, _key: &str, body: Vec<u8>) -> io::Result<()> {
            self.single.lock().expect("single lock").push(body);
            Ok(())
        }

        async fn start_multipart(&self, _key: &str) -> io::Result<String> {
            Ok("upload-id".to_owned())
        }

        async fn put_part(
            &self,
            _key: &str,
            _upload_id: &str,
            part_number: i32,
            body: Vec<u8>,
        ) -> io::Result<String> {
            if self.fail_part == Some(part_number) {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionReset,
                    "part upload failed",
                ));
            }
            self.parts
                .lock()
                .expect("parts lock")
                .push((part_number, body));
            Ok(format!("etag-{part_number}"))
        }

        async fn complete_multipart(
            &self,
            _key: &str,
            _upload_id: &str,
            etags: Vec<String>,
        ) -> io::Result<()> {
            *self.completed.lock().expect("completed lock") = Some(etags);
            Ok(())
        }

        async fn abort_multipart(&self, _key: &str, _upload_id: &str) -> io::Result<()> {
            self.aborted.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    /// Reads some bytes and then returns an error.
    struct FailingReader {
        data: Vec<u8>,
        position: usize,
        fail_at: usize,
    }

    impl AsyncRead for FailingReader {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            let this = self.get_mut();
            if this.position >= this.fail_at {
                return Poll::Ready(Err(io::Error::other("reader failed")));
            }
            let end = this.fail_at.min(this.data.len());
            let count = (end - this.position).min(buf.remaining());
            buf.put_slice(&this.data[this.position..this.position + count]);
            this.position += count;
            Poll::Ready(Ok(()))
        }
    }

    fn reader_from(bytes: &[u8]) -> AsyncFileReader {
        Box::pin(std::io::Cursor::new(bytes.to_vec()))
    }

    #[tokio::test]
    async fn upload_reader_rejects_zero_part_size() {
        let sink = RecordingSink::default();
        let mut reader = reader_from(b"data");

        let err = upload_reader(&sink, "key", &mut reader, 0)
            .await
            .expect_err("zero part size");

        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[tokio::test]
    async fn upload_reader_stores_empty_input_as_one_object() {
        let sink = RecordingSink::default();
        let mut reader = reader_from(b"");

        let written = upload_reader(&sink, "key", &mut reader, 4)
            .await
            .expect("upload empty input");

        assert_eq!(written, 0);
        assert_eq!(
            *sink.single.lock().expect("single lock"),
            vec![Vec::<u8>::new()]
        );
        assert!(sink.parts.lock().expect("parts lock").is_empty());
        assert!(sink.completed.lock().expect("completed lock").is_none());
        assert!(!sink.aborted.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn upload_reader_stores_short_input_as_one_object() {
        let sink = RecordingSink::default();
        let mut reader = reader_from(b"abc");

        let written = upload_reader(&sink, "key", &mut reader, 4)
            .await
            .expect("upload short input");

        assert_eq!(written, 3);
        assert_eq!(
            *sink.single.lock().expect("single lock"),
            vec![b"abc".to_vec()]
        );
        assert!(sink.parts.lock().expect("parts lock").is_empty());
        assert!(sink.completed.lock().expect("completed lock").is_none());
        assert!(!sink.aborted.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn upload_reader_stores_large_input_as_multiple_parts() {
        let sink = RecordingSink::default();
        let mut reader = reader_from(b"abcdefghij");

        let written = upload_reader(&sink, "key", &mut reader, 4)
            .await
            .expect("upload large input");

        assert_eq!(written, 10);
        assert_eq!(
            *sink.parts.lock().expect("parts lock"),
            vec![
                (1, b"abcd".to_vec()),
                (2, b"efgh".to_vec()),
                (3, b"ij".to_vec()),
            ]
        );
        assert_eq!(
            *sink.completed.lock().expect("completed lock"),
            Some(vec![
                "etag-1".to_owned(),
                "etag-2".to_owned(),
                "etag-3".to_owned(),
            ])
        );
        assert!(!sink.aborted.load(Ordering::SeqCst));
        assert!(sink.single.lock().expect("single lock").is_empty());
    }

    #[tokio::test]
    async fn upload_reader_counts_exact_part_multiple() {
        let sink = RecordingSink::default();
        let mut reader = reader_from(b"01234567");

        let written = upload_reader(&sink, "key", &mut reader, 4)
            .await
            .expect("upload exact multiple");

        assert_eq!(written, 8);
        assert_eq!(
            *sink.parts.lock().expect("parts lock"),
            vec![(1, b"0123".to_vec()), (2, b"4567".to_vec())]
        );
    }

    #[tokio::test]
    async fn upload_reader_aborts_multipart_on_reader_error() {
        let sink = RecordingSink::default();
        let mut reader: AsyncFileReader = Box::pin(FailingReader {
            data: b"abcdefghij".to_vec(),
            position: 0,
            fail_at: 6,
        });

        let err = upload_reader(&sink, "key", &mut reader, 4)
            .await
            .expect_err("reader error");

        assert_eq!(err.kind(), io::ErrorKind::Other);
        assert_eq!(err.to_string(), "reader failed");
        assert_eq!(sink.parts.lock().expect("parts lock").len(), 1);
        assert!(sink.completed.lock().expect("completed lock").is_none());
        assert!(sink.aborted.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn upload_reader_aborts_multipart_on_part_error() {
        let sink = RecordingSink {
            fail_part: Some(2),
            ..RecordingSink::default()
        };
        let mut reader = reader_from(b"abcdefghij");

        let err = upload_reader(&sink, "key", &mut reader, 4)
            .await
            .expect_err("part error");

        assert_eq!(err.kind(), io::ErrorKind::ConnectionReset);
        assert_eq!(sink.parts.lock().expect("parts lock").len(), 1);
        assert!(sink.completed.lock().expect("completed lock").is_none());
        assert!(sink.aborted.load(Ordering::SeqCst));
    }

    /// One HTTP request that the mock S3 server received.
    struct RecordedRequest {
        method: String,
        target: String,
        body: Vec<u8>,
    }

    /// One incomplete multipart upload in a mock listing.
    struct MockUpload {
        key: Option<String>,
        upload_id: Option<String>,
        initiated: Option<String>,
    }

    /// One page of mock multipart upload listings.
    #[derive(Default)]
    struct MockListPage {
        uploads: Vec<MockUpload>,
        next_key_marker: Option<String>,
        next_upload_id_marker: Option<String>,
    }

    /// A mock S3 endpoint that records requests and answers the multipart calls.
    struct MockS3 {
        endpoint: String,
        requests: Arc<Mutex<Vec<RecordedRequest>>>,
        fail_parts: Arc<AtomicBool>,
        list_pages: Arc<Mutex<Vec<MockListPage>>>,
        fail_aborts: Arc<Mutex<HashSet<String>>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl MockS3 {
        async fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind mock S3");
            let address = listener.local_addr().expect("mock S3 address");
            let requests = Arc::new(Mutex::new(Vec::new()));
            let fail_parts = Arc::new(AtomicBool::new(false));
            let list_pages = Arc::new(Mutex::new(Vec::new()));
            let fail_aborts = Arc::new(Mutex::new(HashSet::new()));
            let state = MockState {
                requests: Arc::clone(&requests),
                fail_parts: Arc::clone(&fail_parts),
                list_pages: Arc::clone(&list_pages),
                fail_aborts: Arc::clone(&fail_aborts),
            };
            let task = tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        return;
                    };
                    let state = state.clone();
                    tokio::spawn(async move {
                        let _ = handle_connection(stream, state).await;
                    });
                }
            });
            Self {
                endpoint: format!("http://{address}"),
                requests,
                fail_parts,
                list_pages,
                fail_aborts,
                task,
            }
        }

        fn set_list_pages(&self, pages: Vec<MockListPage>) {
            *self.list_pages.lock().expect("list pages lock") = pages;
        }

        fn fail_abort(&self, upload_id: &str) {
            self.fail_aborts
                .lock()
                .expect("abort lock")
                .insert(upload_id.to_owned());
        }
    }

    impl Drop for MockS3 {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    /// The state that the mock S3 server shares with the test.
    #[derive(Clone)]
    struct MockState {
        requests: Arc<Mutex<Vec<RecordedRequest>>>,
        fail_parts: Arc<AtomicBool>,
        list_pages: Arc<Mutex<Vec<MockListPage>>>,
        fail_aborts: Arc<Mutex<HashSet<String>>>,
    }

    async fn handle_connection(mut stream: TcpStream, state: MockState) -> io::Result<()> {
        let request = read_request(&mut stream).await?;
        let response = respond_to(&request, &state);
        state.requests.lock().expect("request lock").push(request);
        stream.write_all(&response).await?;
        stream.shutdown().await
    }

    async fn read_request(stream: &mut TcpStream) -> io::Result<RecordedRequest> {
        let mut received = Vec::new();
        let mut chunk = [0_u8; 8192];
        let header_end = loop {
            if let Some(index) = find_bytes(&received, b"\r\n\r\n") {
                break index;
            }
            let count = stream.read(&mut chunk).await?;
            if count == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "mock S3 received no request",
                ));
            }
            received.extend_from_slice(&chunk[..count]);
        };

        let header_text = String::from_utf8_lossy(&received[..header_end]).into_owned();
        let mut lines = header_text.split("\r\n");
        let request_line = lines.next().unwrap_or_default();
        let mut fields = request_line.split_whitespace();
        let method = fields.next().unwrap_or_default().to_owned();
        let target = fields.next().unwrap_or_default().to_owned();
        let headers: Vec<(String, String)> = lines
            .filter_map(|line| {
                let (name, value) = line.split_once(':')?;
                Some((name.trim().to_ascii_lowercase(), value.trim().to_owned()))
            })
            .collect();

        if header_value(&headers, "expect")
            .is_some_and(|value| value.eq_ignore_ascii_case("100-continue"))
        {
            stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").await?;
        }

        let mut body = received[header_end + 4..].to_vec();
        if header_value(&headers, "transfer-encoding")
            .is_some_and(|value| value.to_ascii_lowercase().contains("chunked"))
        {
            body = read_chunked(stream, body).await?;
        } else if let Some(value) = header_value(&headers, "content-length") {
            let length: usize = value.parse().map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "invalid Content-Length")
            })?;
            while body.len() < length {
                let count = stream.read(&mut chunk).await?;
                if count == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "mock S3 received a short body",
                    ));
                }
                body.extend_from_slice(&chunk[..count]);
            }
            body.truncate(length);
        }

        if header_value(&headers, "content-encoding")
            .is_some_and(|value| contains_token(value, "aws-chunked"))
        {
            body = decode_aws_chunked(&body)?;
        }

        Ok(RecordedRequest {
            method,
            target,
            body,
        })
    }

    fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
        headers
            .iter()
            .find(|(header, _)| header.as_str() == name)
            .map(|(_, value)| value.as_str())
    }

    fn contains_token(value: &str, token: &str) -> bool {
        value
            .split(',')
            .any(|part| part.trim().eq_ignore_ascii_case(token))
    }

    fn find_bytes(data: &[u8], needle: &[u8]) -> Option<usize> {
        data.windows(needle.len())
            .position(|window| window == needle)
    }

    async fn read_chunked(stream: &mut TcpStream, mut buffered: Vec<u8>) -> io::Result<Vec<u8>> {
        let mut body = Vec::new();
        loop {
            let line = read_line(stream, &mut buffered).await?;
            let size_text = line.split(';').next().unwrap_or_default().trim();
            let size = usize::from_str_radix(size_text, 16)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid chunk size"))?;
            if size == 0 {
                loop {
                    let trailer = read_line(stream, &mut buffered).await?;
                    if trailer.is_empty() {
                        return Ok(body);
                    }
                }
            }
            while buffered.len() < size + 2 {
                let mut chunk = [0_u8; 8192];
                let count = stream.read(&mut chunk).await?;
                if count == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "mock S3 received a short chunk",
                    ));
                }
                buffered.extend_from_slice(&chunk[..count]);
            }
            body.extend_from_slice(&buffered[..size]);
            buffered.drain(..size + 2);
        }
    }

    async fn read_line(stream: &mut TcpStream, buffered: &mut Vec<u8>) -> io::Result<String> {
        loop {
            if let Some(index) = find_bytes(buffered, b"\r\n") {
                let line = String::from_utf8_lossy(&buffered[..index]).into_owned();
                buffered.drain(..index + 2);
                return Ok(line);
            }
            let mut chunk = [0_u8; 8192];
            let count = stream.read(&mut chunk).await?;
            if count == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "mock S3 received no line",
                ));
            }
            buffered.extend_from_slice(&chunk[..count]);
        }
    }

    fn decode_aws_chunked(data: &[u8]) -> io::Result<Vec<u8>> {
        let mut decoded = Vec::new();
        let mut offset = 0;
        loop {
            let line_end = find_bytes(&data[offset..], b"\r\n").ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "invalid aws-chunked body")
            })?;
            let line = String::from_utf8_lossy(&data[offset..offset + line_end]);
            let size_text = line.split(';').next().unwrap_or_default().trim();
            let size = usize::from_str_radix(size_text, 16).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "invalid aws-chunked size")
            })?;
            offset += line_end + 2;
            if size == 0 {
                return Ok(decoded);
            }
            if data.len() < offset + size {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "short aws-chunked body",
                ));
            }
            decoded.extend_from_slice(&data[offset..offset + size]);
            offset += size;
            if data[offset..].starts_with(b"\r\n") {
                offset += 2;
            }
        }
    }

    fn respond_to(request: &RecordedRequest, state: &MockState) -> Vec<u8> {
        let target = request.target.as_str();
        if request.method == "GET" && target.contains("uploads") {
            return http_response("200 OK", &[], &list_uploads_xml(state, target));
        }
        if request.method == "POST" && target.contains("uploads") {
            return http_response("200 OK", &[], INITIATE_XML);
        }
        if request.method == "PUT" && target.contains("partNumber=") {
            if state.fail_parts.load(Ordering::SeqCst) {
                return http_response("403 Forbidden", &[], ACCESS_DENIED_XML);
            }
            let part_number = query_value(target, "partNumber").unwrap_or_default();
            let etag = format!("\"etag-{part_number}\"");
            return http_response("200 OK", &[("ETag", etag.as_str())], "");
        }
        if request.method == "POST" && target.contains("uploadId=") {
            return http_response("200 OK", &[], COMPLETE_XML);
        }
        if request.method == "DELETE" && target.contains("uploadId=") {
            let upload_id = query_value_decoded(target, "uploadId").unwrap_or_default();
            if state
                .fail_aborts
                .lock()
                .expect("abort lock")
                .contains(&upload_id)
            {
                return http_response("403 Forbidden", &[], ACCESS_DENIED_XML);
            }
            return http_response("204 No Content", &[], "");
        }
        if request.method == "PUT" {
            return http_response("200 OK", &[("ETag", "\"single\"")], "");
        }
        http_response("404 Not Found", &[], "")
    }

    fn list_uploads_xml(state: &MockState, target: &str) -> String {
        let pages = state.list_pages.lock().expect("list pages lock");
        let key_marker = query_value_decoded(target, "key-marker");
        let page_index = match key_marker.as_deref() {
            None => 0,
            Some(marker) => pages
                .iter()
                .position(|page| page.next_key_marker.as_deref() == Some(marker))
                .map_or(pages.len(), |index| index + 1),
        };
        let Some(page) = pages.get(page_index) else {
            return empty_list_xml();
        };

        let truncated = page.next_key_marker.is_some() || page.next_upload_id_marker.is_some();
        let mut xml = String::from(
            r#"<?xml version="1.0" encoding="UTF-8"?><ListMultipartUploadsResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Bucket>test-bucket</Bucket>"#,
        );
        if let Some(marker) = &page.next_key_marker {
            xml.push_str(&format!("<NextKeyMarker>{marker}</NextKeyMarker>"));
        }
        if let Some(marker) = &page.next_upload_id_marker {
            xml.push_str(&format!(
                "<NextUploadIdMarker>{marker}</NextUploadIdMarker>"
            ));
        }
        xml.push_str(&format!("<IsTruncated>{truncated}</IsTruncated>"));
        for upload in &page.uploads {
            xml.push_str("<Upload>");
            if let Some(key) = &upload.key {
                xml.push_str(&format!("<Key>{key}</Key>"));
            }
            if let Some(upload_id) = &upload.upload_id {
                xml.push_str(&format!("<UploadId>{upload_id}</UploadId>"));
            }
            if let Some(initiated) = &upload.initiated {
                xml.push_str(&format!("<Initiated>{initiated}</Initiated>"));
            }
            xml.push_str("</Upload>");
        }
        xml.push_str("</ListMultipartUploadsResult>");
        xml
    }

    fn empty_list_xml() -> String {
        String::from(
            r#"<?xml version="1.0" encoding="UTF-8"?><ListMultipartUploadsResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Bucket>test-bucket</Bucket><IsTruncated>false</IsTruncated></ListMultipartUploadsResult>"#,
        )
    }

    fn query_value_decoded(target: &str, name: &str) -> Option<String> {
        query_value(target, name).map(percent_decode)
    }

    fn percent_decode(value: &str) -> String {
        let bytes = value.as_bytes();
        let mut decoded = Vec::with_capacity(bytes.len());
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index] == b'%' && index + 2 < bytes.len() {
                if let Ok(text) = std::str::from_utf8(&bytes[index + 1..index + 3]) {
                    if let Ok(byte) = u8::from_str_radix(text, 16) {
                        decoded.push(byte);
                        index += 3;
                        continue;
                    }
                }
            }
            decoded.push(bytes[index]);
            index += 1;
        }
        String::from_utf8_lossy(&decoded).into_owned()
    }

    fn query_value<'a>(target: &'a str, name: &str) -> Option<&'a str> {
        let query = target.split_once('?')?.1;
        query.split('&').find_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            (key == name).then_some(value)
        })
    }

    fn http_response(status: &str, headers: &[(&str, &str)], body: &str) -> Vec<u8> {
        let mut response = format!("HTTP/1.1 {status}\r\n");
        if status != "204 No Content" {
            response.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        response.push_str("Connection: close\r\n");
        for (name, value) in headers {
            response.push_str(&format!("{name}: {value}\r\n"));
        }
        response.push_str("\r\n");
        response.push_str(body);
        response.into_bytes()
    }

    const INITIATE_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?><InitiateMultipartUploadResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Bucket>test-bucket</Bucket><Key>files/large.bin</Key><UploadId>test-upload-id</UploadId></InitiateMultipartUploadResult>"#;

    const COMPLETE_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?><CompleteMultipartUploadResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Location>http://127.0.0.1/test-bucket/files/large.bin</Location><Bucket>test-bucket</Bucket><Key>files/large.bin</Key><ETag>"etag-1"</ETag></CompleteMultipartUploadResult>"#;

    const ACCESS_DENIED_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?><Error><Code>AccessDenied</Code><Message>part upload denied</Message><RequestId>test</RequestId><HostId>test</HostId></Error>"#;

    fn mock_storage(mock: &MockS3) -> S3Storage {
        mock_storage_with(mock, "files", DEFAULT_MULTIPART_UPLOAD_EXPIRY_HOURS)
    }

    fn mock_storage_with(mock: &MockS3, prefix: &str, expiry_hours: u64) -> S3Storage {
        mock_storage_with_mode(mock, prefix, expiry_hours, S3UploadMode::Multipart)
    }

    fn mock_storage_with_mode(
        mock: &MockS3,
        prefix: &str,
        expiry_hours: u64,
        upload_mode: S3UploadMode,
    ) -> S3Storage {
        mock_storage_with_settings(
            mock,
            prefix,
            expiry_hours,
            upload_mode,
            DEFAULT_MULTIPART_PART_SIZE_MB,
        )
    }

    fn mock_storage_with_settings(
        mock: &MockS3,
        prefix: &str,
        expiry_hours: u64,
        upload_mode: S3UploadMode,
        multipart_part_size_mb: u64,
    ) -> S3Storage {
        let config = aws_sdk_s3::config::Builder::new()
            .behavior_version_latest()
            .region(Region::new("us-east-1"))
            .credentials_provider(Credentials::new(
                "test-key",
                "test-secret",
                None,
                None,
                "test",
            ))
            .endpoint_url(mock.endpoint.clone())
            .force_path_style(true)
            .build();
        S3Storage {
            client: Client::from_conf(config),
            bucket: "test-bucket".to_owned(),
            prefix: prefix.to_owned(),
            multipart_expiry: multipart_expiry(expiry_hours),
            upload_mode,
            multipart_part_size: multipart_part_size(multipart_part_size_mb)
                .expect("multipart part size"),
        }
    }

    const OLD_INITIATED: &str = "2000-01-01T00:00:00.000Z";
    const RECENT_INITIATED: &str = "2999-01-01T00:00:00.000Z";

    fn stale_upload(key: &str, upload_id: &str) -> MockUpload {
        MockUpload {
            key: Some(key.to_owned()),
            upload_id: Some(upload_id.to_owned()),
            initiated: Some(OLD_INITIATED.to_owned()),
        }
    }

    fn recent_upload(key: &str, upload_id: &str) -> MockUpload {
        MockUpload {
            key: Some(key.to_owned()),
            upload_id: Some(upload_id.to_owned()),
            initiated: Some(RECENT_INITIATED.to_owned()),
        }
    }

    fn list_page(uploads: Vec<MockUpload>) -> MockListPage {
        MockListPage {
            uploads,
            next_key_marker: None,
            next_upload_id_marker: None,
        }
    }

    #[test]
    fn multipart_expiry_disables_zero_and_clamps_positive_values() {
        assert_eq!(multipart_expiry(0), None);
        assert_eq!(multipart_expiry(1), Some(Duration::from_hours(1)));
        assert_eq!(multipart_expiry(24), Some(Duration::from_hours(24)));
    }

    #[test]
    fn upload_strategy_selects_the_single_put_limit() {
        assert_eq!(
            select_upload_strategy(S3UploadMode::PreferSingle, MAX_SINGLE_PUT_SIZE)
                .expect("strategy"),
            UploadStrategy::SinglePut
        );
        assert_eq!(
            select_upload_strategy(S3UploadMode::PreferSingle, MAX_SINGLE_PUT_SIZE + 1)
                .expect("strategy"),
            UploadStrategy::Multipart
        );
        assert_eq!(
            select_upload_strategy(S3UploadMode::Multipart, 0).expect("strategy"),
            UploadStrategy::Multipart
        );
        let err = select_upload_strategy(S3UploadMode::RequireSingle, MAX_SINGLE_PUT_SIZE + 1)
            .expect_err("large require_single upload");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn multipart_part_size_accepts_the_valid_range() {
        assert_eq!(multipart_part_size(5).expect("5 MiB"), 5 * 1024 * 1024);
        assert_eq!(multipart_part_size(8).expect("8 MiB"), DEFAULT_PART_SIZE);
        assert_eq!(multipart_part_size(16).expect("16 MiB"), 16 * 1024 * 1024);
        assert_eq!(
            multipart_part_size(128).expect("128 MiB"),
            128 * 1024 * 1024
        );
    }

    #[test]
    fn multipart_part_size_rejects_invalid_values() {
        for value in [0, 4, 129] {
            let err = multipart_part_size(value).expect_err("invalid part size");
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
            assert!(err.to_string().contains("5 and 128 MiB"));
        }
    }

    #[tokio::test]
    async fn new_rejects_an_invalid_multipart_part_size() {
        let err = match S3Storage::new_with_options(
            "bucket".to_owned(),
            "region".to_owned(),
            String::new(),
            None,
            DEFAULT_MULTIPART_UPLOAD_EXPIRY_HOURS,
            S3UploadMode::PreferSingle,
            4,
        )
        .await
        {
            Ok(_) => panic!("invalid part size must fail"),
            Err(err) => err,
        };

        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn single_put_body_reports_the_exact_size_hint() {
        let body = SinglePutBody::new(Box::pin(tokio::io::empty()), 1234);
        assert_eq!(body.size_hint().exact(), Some(1234));
    }

    #[tokio::test]
    async fn single_put_body_stops_at_the_declared_length() {
        let mut body = SinglePutBody::new(Box::pin(tokio::io::repeat(b'x').take(10)), 4);
        let mut total = 0_u64;

        loop {
            let frame = std::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await;
            match frame {
                None => break,
                Some(Ok(frame)) => {
                    total += frame.data_ref().expect("data frame").len() as u64;
                }
                Some(Err(err)) => panic!("body error: {err}"),
            }
        }

        assert_eq!(total, 4);
    }

    #[tokio::test]
    async fn single_put_body_reports_a_short_reader() {
        let mut body = SinglePutBody::new(Box::pin(tokio::io::empty()), 5);
        let frame = std::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await;

        let err = frame.expect("body frame").expect_err("short reader error");
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
        assert!(err.to_string().contains("ended before the declared length"));
    }

    fn known_reader(length: u64) -> AsyncFileReader {
        Box::pin(tokio::io::repeat(b'x').take(length))
    }

    #[tokio::test]
    async fn single_put_modes_use_one_request() {
        let length = 9 * 1024 * 1024;

        for mode in [S3UploadMode::PreferSingle, S3UploadMode::RequireSingle] {
            let mock = MockS3::start().await;
            let storage =
                mock_storage_with_mode(&mock, "files", DEFAULT_MULTIPART_UPLOAD_EXPIRY_HOURS, mode);

            let written = storage
                .put_sized(
                    Path::new("large.bin"),
                    known_reader(length),
                    length,
                    &Default::default(),
                )
                .await
                .expect("put");

            assert_eq!(written, length, "mode {mode:?}");
            let requests = mock.requests.lock().expect("request lock");
            let puts: Vec<&RecordedRequest> = requests
                .iter()
                .filter(|request| request.method == "PUT")
                .collect();
            assert_eq!(puts.len(), 1, "mode {mode:?}");
            assert_eq!(puts[0].body.len() as u64, length, "mode {mode:?}");
            assert!(
                !requests.iter().any(|request| request.method == "POST"),
                "mode {mode:?} must not start or complete a multipart upload"
            );
        }
    }

    #[tokio::test]
    async fn require_single_rejects_an_unknown_length() {
        let mock = MockS3::start().await;
        let storage = mock_storage_with_mode(
            &mock,
            "files",
            DEFAULT_MULTIPART_UPLOAD_EXPIRY_HOURS,
            S3UploadMode::RequireSingle,
        );

        let err = storage
            .put(
                Path::new("unknown.bin"),
                Box::pin(tokio::io::empty()),
                &Default::default(),
            )
            .await
            .expect_err("unknown length");

        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("known content length"));
        assert!(mock.requests.lock().expect("request lock").is_empty());
    }

    #[tokio::test]
    async fn require_single_rejects_a_length_above_the_limit() {
        let mock = MockS3::start().await;
        let storage = mock_storage_with_mode(
            &mock,
            "files",
            DEFAULT_MULTIPART_UPLOAD_EXPIRY_HOURS,
            S3UploadMode::RequireSingle,
        );

        let err = storage
            .put_sized(
                Path::new("large.bin"),
                Box::pin(tokio::io::empty()),
                MAX_SINGLE_PUT_SIZE + 1,
                &Default::default(),
            )
            .await
            .expect_err("large object");

        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("5 GiB"));
        assert!(mock.requests.lock().expect("request lock").is_empty());
    }

    #[tokio::test]
    async fn multipart_mode_uses_multipart_for_a_known_length() {
        let mock = MockS3::start().await;
        let storage = mock_storage_with_mode(
            &mock,
            "files",
            DEFAULT_MULTIPART_UPLOAD_EXPIRY_HOURS,
            S3UploadMode::Multipart,
        );
        let length = 9 * 1024 * 1024;

        let written = storage
            .put_sized(
                Path::new("large.bin"),
                known_reader(length),
                length,
                &Default::default(),
            )
            .await
            .expect("put");

        assert_eq!(written, length);
        let requests = mock.requests.lock().expect("request lock");
        assert!(
            requests
                .iter()
                .any(|request| request.method == "POST" && request.target.contains("uploads"))
        );
        let parts: Vec<&RecordedRequest> = requests
            .iter()
            .filter(|request| request.method == "PUT" && request.target.contains("partNumber="))
            .collect();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].body.len(), DEFAULT_PART_SIZE);
        assert_eq!(
            parts[1].body.len() as u64,
            length - DEFAULT_PART_SIZE as u64
        );
        assert!(
            requests
                .iter()
                .any(|request| request.method == "POST" && request.target.contains("uploadId="))
        );
    }

    #[tokio::test]
    async fn multipart_upload_uses_the_configured_part_size() {
        let mock = MockS3::start().await;
        let storage = mock_storage_with_settings(
            &mock,
            "files",
            DEFAULT_MULTIPART_UPLOAD_EXPIRY_HOURS,
            S3UploadMode::Multipart,
            5,
        );
        let length = 12 * 1024 * 1024;

        let written = storage
            .put_sized(
                Path::new("large.bin"),
                known_reader(length),
                length,
                &Default::default(),
            )
            .await
            .expect("put");

        assert_eq!(written, length);
        let requests = mock.requests.lock().expect("request lock");
        let parts: Vec<&RecordedRequest> = requests
            .iter()
            .filter(|request| request.method == "PUT" && request.target.contains("partNumber="))
            .collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0].body.len(), 5 * 1024 * 1024);
        assert_eq!(parts[1].body.len(), 5 * 1024 * 1024);
        assert_eq!(parts[2].body.len(), 2 * 1024 * 1024);
    }

    #[tokio::test]
    async fn single_put_ignores_the_configured_part_size() {
        let mock = MockS3::start().await;
        let storage = mock_storage_with_settings(
            &mock,
            "files",
            DEFAULT_MULTIPART_UPLOAD_EXPIRY_HOURS,
            S3UploadMode::PreferSingle,
            16,
        );
        let length = 9 * 1024 * 1024;

        let written = storage
            .put_sized(
                Path::new("large.bin"),
                known_reader(length),
                length,
                &Default::default(),
            )
            .await
            .expect("put");

        assert_eq!(written, length);
        let requests = mock.requests.lock().expect("request lock");
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.method == "PUT")
                .count(),
            1
        );
        assert!(!requests.iter().any(|request| request.method == "POST"));
    }

    #[tokio::test]
    async fn prefer_single_falls_back_to_multipart_for_an_unknown_length() {
        let mock = MockS3::start().await;
        let storage = mock_storage_with_mode(
            &mock,
            "files",
            DEFAULT_MULTIPART_UPLOAD_EXPIRY_HOURS,
            S3UploadMode::PreferSingle,
        );
        let length = 9 * 1024 * 1024;

        let written = storage
            .put(
                Path::new("large.bin"),
                known_reader(length),
                &Default::default(),
            )
            .await
            .expect("put");

        assert_eq!(written, length);
        let requests = mock.requests.lock().expect("request lock");
        assert!(
            requests
                .iter()
                .any(|request| request.method == "POST" && request.target.contains("uploads"))
        );
        assert!(
            requests
                .iter()
                .any(|request| request.method == "POST" && request.target.contains("uploadId="))
        );
    }

    #[tokio::test]
    async fn zero_length_input_uses_one_put() {
        let mock = MockS3::start().await;
        let storage = mock_storage_with_mode(
            &mock,
            "files",
            DEFAULT_MULTIPART_UPLOAD_EXPIRY_HOURS,
            S3UploadMode::PreferSingle,
        );

        let written = storage
            .put_sized(
                Path::new("empty.bin"),
                Box::pin(tokio::io::empty()),
                0,
                &Default::default(),
            )
            .await
            .expect("put");

        assert_eq!(written, 0);
        let requests = mock.requests.lock().expect("request lock");
        let puts: Vec<&RecordedRequest> = requests
            .iter()
            .filter(|request| request.method == "PUT")
            .collect();
        assert_eq!(puts.len(), 1);
        assert!(puts[0].body.is_empty());
        assert!(!requests.iter().any(|request| request.method == "POST"));
    }

    #[tokio::test]
    async fn short_reader_returns_an_error() {
        let mock = MockS3::start().await;
        let storage = mock_storage_with_mode(
            &mock,
            "files",
            DEFAULT_MULTIPART_UPLOAD_EXPIRY_HOURS,
            S3UploadMode::PreferSingle,
        );
        let length = 9 * 1024 * 1024;

        let err = storage
            .put_sized(
                Path::new("short.bin"),
                Box::pin(tokio::io::repeat(b'x').take(1)),
                length,
                &Default::default(),
            )
            .await
            .expect_err("short reader");

        assert!(err.to_string().contains("ended before the declared length"));
    }

    #[tokio::test]
    async fn cleanup_aborts_only_old_uploads() {
        let mock = MockS3::start().await;
        mock.set_list_pages(vec![list_page(vec![
            stale_upload("files/old.bin", "old-id"),
            recent_upload("files/new.bin", "new-id"),
        ])]);
        let storage = mock_storage(&mock);

        let aborted = storage
            .cleanup_stale_multipart_uploads(&Default::default())
            .await
            .expect("cleanup");

        assert_eq!(aborted, 1);
        let requests = mock.requests.lock().expect("request lock");
        let aborts: Vec<&RecordedRequest> = requests
            .iter()
            .filter(|request| request.method == "DELETE")
            .collect();
        assert_eq!(aborts.len(), 1);
        assert!(aborts[0].target.contains("uploadId=old-id"));
        assert!(requests.iter().any(|request| {
            request.method == "GET"
                && request.target.contains("uploads")
                && request.target.contains("prefix=files")
        }));
    }

    #[tokio::test]
    async fn cleanup_follows_multipart_upload_pagination() {
        let mock = MockS3::start().await;
        mock.set_list_pages(vec![
            MockListPage {
                uploads: vec![recent_upload("files/recent.bin", "recent-id")],
                next_key_marker: Some("marker-1".to_owned()),
                next_upload_id_marker: Some("upload-1".to_owned()),
            },
            list_page(vec![stale_upload("files/old.bin", "old-id")]),
        ]);
        let storage = mock_storage(&mock);

        let aborted = storage
            .cleanup_stale_multipart_uploads(&Default::default())
            .await
            .expect("cleanup");

        assert_eq!(aborted, 1);
        let requests = mock.requests.lock().expect("request lock");
        let lists: Vec<&RecordedRequest> = requests
            .iter()
            .filter(|request| request.method == "GET" && request.target.contains("uploads"))
            .collect();
        assert_eq!(lists.len(), 2);
        assert!(lists[1].target.contains("key-marker=marker-1"));
        assert!(lists[1].target.contains("upload-id-marker=upload-1"));
        assert!(requests.iter().any(|request| {
            request.method == "DELETE" && request.target.contains("uploadId=old-id")
        }));
    }

    #[tokio::test]
    async fn cleanup_skips_uploads_with_missing_fields() {
        let mock = MockS3::start().await;
        mock.set_list_pages(vec![list_page(vec![
            MockUpload {
                key: None,
                upload_id: Some("missing-key-id".to_owned()),
                initiated: Some(OLD_INITIATED.to_owned()),
            },
            MockUpload {
                key: Some("files/missing-id.bin".to_owned()),
                upload_id: None,
                initiated: Some(OLD_INITIATED.to_owned()),
            },
            MockUpload {
                key: Some("files/missing-time.bin".to_owned()),
                upload_id: Some("missing-time-id".to_owned()),
                initiated: None,
            },
            stale_upload("files/old.bin", "old-id"),
        ])]);
        let storage = mock_storage(&mock);

        let aborted = storage
            .cleanup_stale_multipart_uploads(&Default::default())
            .await
            .expect("cleanup");

        assert_eq!(aborted, 1);
        let requests = mock.requests.lock().expect("request lock");
        let aborts: Vec<&RecordedRequest> = requests
            .iter()
            .filter(|request| request.method == "DELETE")
            .collect();
        assert_eq!(aborts.len(), 1);
        assert!(aborts[0].target.contains("uploadId=old-id"));
    }

    #[tokio::test]
    async fn cleanup_continues_after_an_abort_failure() {
        let mock = MockS3::start().await;
        mock.set_list_pages(vec![list_page(vec![
            stale_upload("files/fail.bin", "fail-id"),
            stale_upload("files/ok.bin", "ok-id"),
        ])]);
        mock.fail_abort("fail-id");
        let storage = mock_storage(&mock);

        let aborted = storage
            .cleanup_stale_multipart_uploads(&Default::default())
            .await
            .expect("cleanup");

        assert_eq!(aborted, 1);
        let requests = mock.requests.lock().expect("request lock");
        let aborts: Vec<&RecordedRequest> = requests
            .iter()
            .filter(|request| request.method == "DELETE")
            .collect();
        assert_eq!(aborts.len(), 2);
        assert!(
            aborts
                .iter()
                .any(|request| request.target.contains("uploadId=fail-id"))
        );
        assert!(
            aborts
                .iter()
                .any(|request| request.target.contains("uploadId=ok-id"))
        );
    }

    #[tokio::test]
    async fn cleanup_skips_keys_outside_the_configured_prefix() {
        let mock = MockS3::start().await;
        mock.set_list_pages(vec![list_page(vec![
            stale_upload("other/old.bin", "foreign-id"),
            stale_upload("files/old.bin", "own-id"),
        ])]);
        let storage = mock_storage(&mock);

        let aborted = storage
            .cleanup_stale_multipart_uploads(&Default::default())
            .await
            .expect("cleanup");

        assert_eq!(aborted, 1);
        let requests = mock.requests.lock().expect("request lock");
        let aborts: Vec<&RecordedRequest> = requests
            .iter()
            .filter(|request| request.method == "DELETE")
            .collect();
        assert_eq!(aborts.len(), 1);
        assert!(aborts[0].target.contains("uploadId=own-id"));
    }

    #[tokio::test]
    async fn cleanup_skips_keys_outside_the_app_key_shape() {
        let mock = MockS3::start().await;
        mock.set_list_pages(vec![list_page(vec![
            stale_upload("AbCd1234/ZG9j", "own-id"),
            stale_upload("AbCd1234/file.txt", "dot-id"),
            stale_upload("short/ZG9j", "short-dir-id"),
            stale_upload("AbCd1234/", "empty-name-id"),
            stale_upload("AbCd12345/ZG9j", "long-dir-id"),
        ])]);
        let storage = mock_storage_with(&mock, "", DEFAULT_MULTIPART_UPLOAD_EXPIRY_HOURS);

        let aborted = storage
            .cleanup_stale_multipart_uploads(&Default::default())
            .await
            .expect("cleanup");

        assert_eq!(aborted, 1);
        let requests = mock.requests.lock().expect("request lock");
        let aborts: Vec<&RecordedRequest> = requests
            .iter()
            .filter(|request| request.method == "DELETE")
            .collect();
        assert_eq!(aborts.len(), 1);
        assert!(aborts[0].target.contains("uploadId=own-id"));
        assert!(
            requests
                .iter()
                .filter(|request| request.method == "GET" && request.target.contains("uploads"))
                .all(|request| !request.target.contains("prefix="))
        );
    }

    #[tokio::test]
    async fn cleanup_is_disabled_when_expiry_is_zero() {
        let mock = MockS3::start().await;
        mock.set_list_pages(vec![list_page(vec![stale_upload(
            "files/old.bin",
            "old-id",
        )])]);
        let storage = mock_storage_with(&mock, "files", 0);

        let aborted = storage
            .cleanup_stale_multipart_uploads(&Default::default())
            .await
            .expect("cleanup");

        assert_eq!(aborted, 0);
        assert!(mock.requests.lock().expect("request lock").is_empty());
    }

    #[tokio::test]
    async fn put_uploads_large_input_with_multipart_requests() {
        let mock = MockS3::start().await;
        let storage = mock_storage(&mock);
        let length = DEFAULT_PART_SIZE as u64 + 1;
        let content: AsyncFileReader = Box::pin(tokio::io::repeat(b'x').take(length));

        let written = storage
            .put(Path::new("large.bin"), content, &Default::default())
            .await
            .expect("put large object");

        assert_eq!(written, length);

        let requests = mock.requests.lock().expect("request lock");
        let creates = requests
            .iter()
            .filter(|request| request.method == "POST" && request.target.contains("uploads"))
            .count();
        assert_eq!(creates, 1);

        let parts: Vec<&RecordedRequest> = requests
            .iter()
            .filter(|request| request.method == "PUT" && request.target.contains("partNumber="))
            .collect();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].body.len(), DEFAULT_PART_SIZE);
        assert_eq!(parts[1].body.len(), 1);

        let complete = requests
            .iter()
            .find(|request| request.method == "POST" && request.target.contains("uploadId="))
            .expect("complete request");
        let complete_body = String::from_utf8_lossy(&complete.body);
        assert!(complete_body.contains("<PartNumber>1</PartNumber>"));
        assert!(complete_body.contains("<PartNumber>2</PartNumber>"));
        assert!(complete_body.contains("etag-1"));
        assert!(complete_body.contains("etag-2"));

        assert!(!requests.iter().any(|request| request.method == "DELETE"));
    }

    #[tokio::test]
    async fn put_aborts_multipart_request_when_a_part_fails() {
        let mock = MockS3::start().await;
        mock.fail_parts.store(true, Ordering::SeqCst);
        let storage = mock_storage(&mock);
        let content: AsyncFileReader =
            Box::pin(tokio::io::repeat(b'x').take(DEFAULT_PART_SIZE as u64));

        let err = storage
            .put(Path::new("large.bin"), content, &Default::default())
            .await
            .expect_err("part failure");

        assert!(err.to_string().contains("UploadPart"));

        let requests = mock.requests.lock().expect("request lock");
        let aborts = requests
            .iter()
            .filter(|request| request.method == "DELETE" && request.target.contains("uploadId="))
            .count();
        assert_eq!(aborts, 1);
        assert!(
            !requests
                .iter()
                .any(|request| request.method == "POST" && request.target.contains("uploadId="))
        );
    }
}
