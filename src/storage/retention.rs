use std::io;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use actix_web::dev::Payload;
use actix_web::{Error, FromRequest, HttpRequest};
use async_trait::async_trait;
use futures::future::LocalBoxFuture;
use futures::{StreamExt, stream};
use tokio::io::AsyncReadExt;
use tokio_util::{bytes::Bytes, io::StreamReader};

use super::{AsyncFileReader, FileMetadata, Resource, StorageProvider};

const MAGIC: &[u8; 4] = b"RETE";
const CURRENT_VERSION: u16 = 1;
const HEADER_V1_SIZE: u16 = 16; // Magic(4) + Ver(2) + Size(2) + Nanos(8)

// --- Retention Header Structure ---

#[derive(Debug, Clone, Copy)]
pub struct RetentionHeader {
    pub version: u16,
    pub header_size: u16,
    pub retain_until: SystemTime,
}

impl RetentionHeader {
    pub fn new(retain_until: SystemTime) -> Self {
        Self {
            version: CURRENT_VERSION,
            header_size: HEADER_V1_SIZE,
            retain_until,
        }
    }

    /// Serializes SystemTime as nanoseconds since Epoch
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(self.header_size as usize);
        buf.extend_from_slice(MAGIC);
        buf.extend_from_slice(&self.version.to_be_bytes());
        buf.extend_from_slice(&self.header_size.to_be_bytes());

        let nanos = self
            .retain_until
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_nanos() as u64;

        buf.extend_from_slice(&nanos.to_be_bytes());
        buf
    }

    /// Deserializes and reconstructs SystemTime
    pub async fn decode(reader: &mut AsyncFileReader) -> io::Result<Self> {
        let mut fixed_part = [0u8; 8];
        reader.read_exact(&mut fixed_part).await?;

        if &fixed_part[0..4] != MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Invalid magic number",
            ));
        }

        let version = u16::from_be_bytes([fixed_part[4], fixed_part[5]]);
        let header_size = u16::from_be_bytes([fixed_part[6], fixed_part[7]]);

        let mut nanos_buf = [0u8; 8];
        reader.read_exact(&mut nanos_buf).await?;
        let nanos = u64::from_be_bytes(nanos_buf);

        let retain_until = UNIX_EPOCH + Duration::from_nanos(nanos);

        // Forward compatibility skip
        let bytes_read = 16u16;
        if header_size > bytes_read {
            let to_skip = (header_size - bytes_read) as u64;
            tokio::io::copy(&mut reader.as_mut().take(to_skip), &mut tokio::io::sink()).await?;
        }

        Ok(Self {
            version,
            header_size,
            retain_until,
        })
    }
}

#[derive(Clone, Debug, Default)]
pub struct RetentionContext<C> {
    pub inner: C,
    pub retain_until: Option<SystemTime>,
}

impl<C> RetentionContext<C> {
    pub fn new(inner: C, hours: Option<u64>) -> Self {
        let retain_until = hours.map(|v| SystemTime::now() + Duration::from_hours(v));
        Self {
            inner,
            retain_until,
        }
    }
}

impl<C> FromRequest for RetentionContext<C>
where
    C: FromRequest + 'static,
    C::Error: Into<Error>,
{
    type Error = Error;
    type Future = LocalBoxFuture<'static, Result<Self, Self::Error>>;

    fn from_request(req: &HttpRequest, payload: &mut Payload) -> Self::Future {
        // 1. Extract the inner context
        let c_future = C::from_request(req, payload);

        // 2. Look for the X-Retention-Hour header
        let hours: Option<u64> = req
            .headers()
            .get("X-Retention-Hour")
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.parse().ok());

        Box::pin(async move {
            let c_instance = c_future.await.map_err(|e| e.into())?;

            // Create the context with the calculated SystemTime

            Ok(RetentionContext::new(c_instance, hours))
        })
    }
}

pub struct RetentionStorage<S: StorageProvider> {
    inner: S,
    default_duration: Duration,
}

impl<S: StorageProvider> RetentionStorage<S> {
    pub fn new(inner: S, default_duration: Duration) -> Self {
        Self {
            inner,
            default_duration,
        }
    }
}

#[async_trait]
impl<S: StorageProvider> StorageProvider for RetentionStorage<S> {
    type Context = RetentionContext<S::Context>;

    async fn put(
        &self,
        path: &Path,
        content: AsyncFileReader,
        ctx: &Self::Context,
    ) -> io::Result<u64> {
        // Enforce lock on overwrite
        if let Ok(Some(mut existing)) = self.inner.get(path, &ctx.inner).await {
            if let Ok(hdr) = RetentionHeader::decode(&mut existing).await {
                if hdr.retain_until > SystemTime::now() {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "File is locked via SystemTime retention",
                    ));
                }
            }
        }

        let header = RetentionHeader::new(
            ctx.retain_until
                .unwrap_or(SystemTime::now() + self.default_duration),
        );
        let header_bytes = header.encode();
        let header_len = header_bytes.len() as u64;

        let header_stream = stream::once(async move { Ok(Bytes::from(header_bytes)) });
        let content_stream = tokio_util::io::ReaderStream::new(content);
        let combined = header_stream.chain(content_stream);

        let written = self
            .inner
            .put(path, Box::pin(StreamReader::new(combined)), &ctx.inner)
            .await?;

        Ok(written.saturating_sub(header_len))
    }

    async fn get(&self, path: &Path, ctx: &Self::Context) -> io::Result<Option<AsyncFileReader>> {
        let raw_opt = self.inner.get(path, &ctx.inner).await?;
        let mut reader = match raw_opt {
            Some(r) => r,
            None => return Ok(None),
        };

        // Advance cursor past the header
        let _ = RetentionHeader::decode(&mut reader).await?;
        Ok(Some(reader))
    }

    async fn delete(&self, path: &Path, ctx: &Self::Context) -> io::Result<()> {
        if let Ok(Some(mut reader)) = self.inner.get(path, &ctx.inner).await {
            if let Ok(hdr) = RetentionHeader::decode(&mut reader).await {
                if hdr.retain_until > SystemTime::now() {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "Retention active",
                    ));
                }
            }
        }
        self.inner.delete(path, &ctx.inner).await
    }

    async fn metadata(&self, path: &Path, ctx: &Self::Context) -> io::Result<Option<FileMetadata>> {
        let mut meta_opt = self.inner.metadata(path, &ctx.inner).await?;
        if let Some(meta) = &mut meta_opt {
            meta.size = meta.size.saturating_sub(HEADER_V1_SIZE as u64);
        }
        Ok(meta_opt)
    }

    async fn list(&self, path: &Path, ctx: &Self::Context) -> io::Result<Vec<Resource>> {
        let mut resources = self.inner.list(path, &ctx.inner).await?;
        for resource in &mut resources {
            if let Resource::File { metadata, .. } = resource {
                metadata.size = metadata.size.saturating_sub(HEADER_V1_SIZE as u64);
            }
        }
        Ok(resources)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    use crate::storage::in_memory;

    #[tokio::test]
    async fn test_retention_delete_locking() {
        let inner_storage = in_memory::InMemoryStorage::new();
        let storage = RetentionStorage::new(inner_storage, Duration::from_hours(5));
        let context = RetentionContext::<in_memory::InMemoryContext>::default();

        let path = Path::new("locked_file.txt");
        let content = b"cannot touch this";

        // 2. Put the file with retention
        storage
            .put(path, Box::pin(Cursor::new(content)), &context)
            .await
            .expect("Initial put should succeed");

        // 3. Attempt to delete the file using the context
        let delete_result = storage.delete(path, &context).await;

        // 4. Assert that it failed with PermissionDenied
        match delete_result {
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                // Success: the retention policy blocked the deletion
                assert_eq!(e.to_string(), "Retention active");
            }
            Err(e) => panic!("Expected PermissionDenied, got: {:?}", e),
            Ok(_) => panic!("Delete should have failed but succeeded"),
        }

        // 5. Verify the file still exists and is readable
        let exists = storage.get(path, &context).await.unwrap();
        assert!(
            exists.is_some(),
            "File should still exist after failed delete attempt"
        );
    }
}
