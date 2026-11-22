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
use aws_sdk_s3::types::{CommonPrefix, Object};
use aws_types::region::Region;
use serde::{Deserialize, Serialize};

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

        // The SDK ByteStream has no stream constructor for this reader type.
        let mut buffer = Vec::new();
        let bytes_written = tokio::io::copy(&mut content, &mut buffer).await?;
        let length = i64::try_from(bytes_written).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "S3 object is too large to upload",
            )
        })?;

        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(ByteStream::from(buffer))
            .content_length(length)
            .send()
            .await
            .map_err(|err| sdk_io_error("PutObject", err))?;

        Ok(bytes_written)
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
}
