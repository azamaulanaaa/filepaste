use std::future::{Ready, ready};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use actix_web::{Error, FromRequest, HttpRequest, dev::Payload};
use async_trait::async_trait;
use aws_credential_types::Credentials;
use aws_credential_types::provider::{self, ProvideCredentials, error::CredentialsError, future};
use aws_sdk_s3::Client;
use aws_sdk_s3::error::{DisplayErrorContext, SdkError};
use aws_sdk_s3::operation::get_object::GetObjectError;
use aws_sdk_s3::operation::head_object::HeadObjectError;
use aws_sdk_s3::primitives::{ByteStream, DateTime};
use aws_sdk_s3::types::{CommonPrefix, CompletedMultipartUpload, CompletedPart, Object};
use aws_types::region::Region;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;

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

/// Stores file content in an Amazon S3 bucket.
pub struct S3Storage {
    client: Client,
    bucket: String,
    prefix: String,
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
        })
    }
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

/// The part size for multipart uploads.
///
/// One part is the largest buffer that an upload holds in memory.
/// S3 accepts at most 10000 parts, so the largest object is about 78 GiB.
const PART_SIZE: usize = 8 * 1024 * 1024;

/// Smallest part size that S3 accepts, except for the last part.
const MIN_PART_SIZE: usize = 5 * 1024 * 1024;

/// Largest part size that S3 accepts.
const MAX_PART_SIZE: usize = 16 * 1024 * 1024;

const _: () = assert!(PART_SIZE >= MIN_PART_SIZE && PART_SIZE <= MAX_PART_SIZE);

/// Largest number of parts that S3 accepts in one multipart upload.
const MAX_PARTS: i32 = 10_000;

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
        mut content: AsyncFileReader,
        _ctx: &Self::Context,
    ) -> io::Result<u64> {
        let key = object_key(&self.prefix, path)?;
        let sink = S3UploadSink { storage: self };
        upload_reader(&sink, &key, &mut content, PART_SIZE).await
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
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll};

    use tokio::io::{AsyncRead, AsyncWriteExt, ReadBuf};
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

    /// A mock S3 endpoint that records requests and answers the multipart calls.
    struct MockS3 {
        endpoint: String,
        requests: Arc<Mutex<Vec<RecordedRequest>>>,
        fail_parts: Arc<AtomicBool>,
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
            let state = MockState {
                requests: Arc::clone(&requests),
                fail_parts: Arc::clone(&fail_parts),
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
                task,
            }
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
            return http_response("204 No Content", &[], "");
        }
        if request.method == "PUT" {
            return http_response("200 OK", &[("ETag", "\"single\"")], "");
        }
        http_response("404 Not Found", &[], "")
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
            prefix: "files".to_owned(),
        }
    }

    #[tokio::test]
    async fn put_uploads_large_input_with_multipart_requests() {
        let mock = MockS3::start().await;
        let storage = mock_storage(&mock);
        let length = PART_SIZE as u64 + 1;
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
        assert_eq!(parts[0].body.len(), PART_SIZE);
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
        let content: AsyncFileReader = Box::pin(tokio::io::repeat(b'x').take(PART_SIZE as u64));

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
