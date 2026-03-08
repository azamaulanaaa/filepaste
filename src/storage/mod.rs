pub mod config;
pub mod encryption;
#[cfg(test)]
pub mod in_memory;
pub mod local;
pub mod retention;

use std::future::{Ready, ready};
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::SystemTime;

use actix_web::{Error, FromRequest, HttpRequest, dev::Payload};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncRead;

pub type AsyncFileReader = Pin<Box<dyn AsyncRead + Send>>;

#[derive(Debug, Clone)]
pub struct FileMetadata {
    pub size: u64,
    pub modified: SystemTime,
}

#[derive(Debug, Clone)]
pub struct DirMetadata {
    pub modified: SystemTime,
}

#[derive(Debug, Clone)]
pub enum Resource {
    File {
        path: PathBuf,
        metadata: FileMetadata,
    },
    Directory {
        path: PathBuf,
        metadata: DirMetadata,
    },
}

impl Resource {
    pub fn path(&self) -> &Path {
        match self {
            Resource::File { path, .. } => path,
            Resource::Directory { path, .. } => path,
        }
    }
}

#[async_trait]
pub trait StorageProvider: Send + Sync {
    type Context: Default + Send + Sync + Clone + FromRequest + 'static;

    async fn put(
        &self,
        path: &Path,
        mut content: AsyncFileReader,
        ctx: &Self::Context,
    ) -> io::Result<u64>;
    async fn get(&self, path: &Path, ctx: &Self::Context) -> io::Result<Option<AsyncFileReader>>;
    async fn delete(&self, path: &Path, ctx: &Self::Context) -> io::Result<()>;
    async fn metadata(&self, path: &Path, ctx: &Self::Context) -> io::Result<Option<FileMetadata>>;
    async fn list(&self, path: &Path, ctx: &Self::Context) -> io::Result<Vec<Resource>>;
}

macro_rules! register_storage_system {
    ($($(#[$meta:meta])* $variant:ident => $storage_ty:ty),* $(,)?) => {

        pub enum Storage {
            $(
                $(#[$meta])*
                $variant($storage_ty)
            ),*
        }

        #[derive(Serialize, Deserialize, Clone, Debug)]
        #[serde(tag = "type", rename_all = "lowercase")]
        pub enum Context {
            $(
                $(#[$meta])*
                $variant( <$storage_ty as StorageProvider>::Context )
            ),*
        }

        impl Default for Context {
            fn default() -> Self {
                $(
                    $(#[$meta])*
                    #[allow(unreachable_code)]
                    return Self::$variant(Default::default());
                )*
            }
        }

        impl FromRequest for Context {
            type Error = Error;
            type Future = Ready<Result<Self, Self::Error>>;

            fn from_request(_req: &HttpRequest, _payload: &mut Payload) -> Self::Future {
                ready(Ok(Self::default()))
            }
        }

        #[async_trait]
        impl StorageProvider for Storage {
            type Context = Context;

            async fn put(&self, path: &Path, content: AsyncFileReader, ctx: &Self::Context) -> io::Result<u64> {
                #[allow(unreachable_patterns)]
                match (self, ctx) {
                    $(
                        $(#[$meta])*
                        (Self::$variant(s), Context::$variant(c)) => s.put(path, content, c).await,
                    )*
                    _ => Err(io::Error::new(io::ErrorKind::InvalidInput, "Storage and Context variant mismatch")),
                }
            }

            async fn get(&self, path: &Path, ctx: &Self::Context) -> io::Result<Option<AsyncFileReader>> {
                #[allow(unreachable_patterns)]
                match (self, ctx) {
                    $(
                        $(#[$meta])*
                        (Self::$variant(s), Context::$variant(c)) => s.get(path, c).await,
                    )*
                    _ => Err(io::Error::new(io::ErrorKind::InvalidInput, "Storage and Context variant mismatch")),
                }
            }

            async fn delete(&self, path: &Path, ctx: &Self::Context) -> io::Result<()> {
                #[allow(unreachable_patterns)]
                match (self, ctx) {
                    $(
                        $(#[$meta])*
                        (Self::$variant(s), Context::$variant(c)) => s.delete(path, c).await,
                    )*
                    _ => Err(io::Error::new(io::ErrorKind::InvalidInput, "Storage and Context variant mismatch")),
                }
            }

            async fn metadata(&self, path: &Path, ctx: &Self::Context) -> io::Result<Option<FileMetadata>> {
                #[allow(unreachable_patterns)]
                match (self, ctx) {
                    $(
                        $(#[$meta])*
                        (Self::$variant(s), Context::$variant(c)) => s.metadata(path, c).await,
                    )*
                    _ => Err(io::Error::new(io::ErrorKind::InvalidInput, "Storage and Context variant mismatch")),
                }
            }

            async fn list(&self, path: &Path, ctx: &Self::Context) -> io::Result<Vec<Resource>> {
                #[allow(unreachable_patterns)]
                match (self, ctx) {
                    $(
                        $(#[$meta])*
                        (Self::$variant(s), Context::$variant(c)) => s.list(path, c).await,
                    )*
                    _ => Err(io::Error::new(io::ErrorKind::InvalidInput, "Storage and Context variant mismatch")),
                }
            }
        }

        $(
            $(#[$meta])*
            impl From<<$storage_ty as StorageProvider>::Context> for Context {
                fn from(ctx: <$storage_ty as StorageProvider>::Context) -> Self {
                    Self::$variant(ctx)
                }
            }
        )*
    };
}

register_storage_system! {
    Local => local::LocalStorage,
    #[cfg(test)]
    InMemory => in_memory::InMemoryStorage,
}

impl Storage {
    pub async fn init(cfg: config::StorageConfig) -> io::Result<Self> {
        Ok(match cfg {
            config::StorageConfig::Local { root } => Self::Local(local::LocalStorage::new(root)?),
            #[cfg(test)]
            config::StorageConfig::InMemory => Self::InMemory(in_memory::InMemoryStorage::new()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::{io::Cursor, time::Duration};

    use tokio::io::AsyncReadExt;

    async fn run_consistency_test_suite<S: StorageProvider>(storage: S, ctx: &S::Context) {
        let path = Path::new("consistency_test.txt");
        let content = b"universal data".to_vec();

        // 1. Test Put & Get
        // We wrap the slice in Box::pin because the trait expects Pin<Box<dyn AsyncRead>>
        let reader = Box::pin(Cursor::new(content.clone()));
        storage.put(path, reader, ctx).await.expect("Put failed");

        let mut retrieved_reader = storage
            .get(path, ctx)
            .await
            .expect("Get failed")
            .expect("Should return Some reader");

        let mut retrieved_buffer = Vec::new();
        retrieved_reader
            .read_to_end(&mut retrieved_buffer)
            .await
            .expect("Read failed");

        assert_eq!(retrieved_buffer, content, "Data mismatch after Put/Get");

        // 2. Test Metadata
        let meta = storage
            .metadata(path, ctx)
            .await
            .expect("Metadata failed")
            .expect("Metadata should be Some after put");
        assert_eq!(meta.size, 14);

        // 3. Test Listing
        let dir = Path::new("test_dir");
        let file1 = dir.join("file1.tmp");
        let file2 = dir.join("file2.tmp");

        storage.put(&file1, Box::pin(&b"1"[..]), ctx).await.unwrap();
        storage.put(&file2, Box::pin(&b"2"[..]), ctx).await.unwrap();

        let list = storage.list(dir, ctx).await.expect("List failed");
        assert!(list.iter().any(|r| r.path() == file1));
        assert!(list.iter().any(|r| r.path() == file2));

        // 4. Test Delete & Not Found
        storage.delete(path, ctx).await.expect("Delete failed");
        let after_delete = storage
            .get(path, ctx)
            .await
            .expect("Get after delete failed");
        assert!(
            after_delete.is_none(),
            "Resource should be gone after delete"
        );
    }

    #[tokio::test]
    async fn test_in_memory_consistency() {
        let storage = in_memory::InMemoryStorage::new();
        let context = in_memory::InMemoryContext::default();
        run_consistency_test_suite(storage, &context).await;
    }

    #[tokio::test]
    async fn test_local_consistency() {
        let temp_dir = std::env::temp_dir().join("storage_test");
        let storage = local::LocalStorage::new(&temp_dir).expect("Failed to create local storage");
        let context = local::LocalContext::default();

        run_consistency_test_suite(storage, &context).await;

        // Cleanup
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[tokio::test]
    async fn test_storage_enum_consistency() {
        let storage_enum = Storage::init(config::StorageConfig::InMemory)
            .await
            .expect("Failed to create in memory storage");
        let context_enum = Context::InMemory(in_memory::InMemoryContext::default());

        run_consistency_test_suite(storage_enum, &context_enum).await;
    }

    #[tokio::test]
    async fn test_encryption_consistency() {
        let inner_storage = in_memory::InMemoryStorage::new();
        let password_salt = "randomsalty".to_string();
        let storage = encryption::EncryptedStorage::new(inner_storage, password_salt);
        let context = encryption::EncryptedContext::<in_memory::InMemoryContext>::default();

        run_consistency_test_suite(storage, &context).await;
    }

    #[tokio::test]
    async fn test_retention_consistency() {
        let inner_storage = in_memory::InMemoryStorage::new();
        let storage = retention::RetentionStorage::new(inner_storage, Duration::from_hours(0));
        let context = retention::RetentionContext::<in_memory::InMemoryContext>::default();

        run_consistency_test_suite(storage, &context).await;
    }
}
