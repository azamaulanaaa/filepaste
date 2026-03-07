pub mod config;
#[cfg(test)]
pub mod in_memory;
pub mod local;

use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::SystemTime;

use async_trait::async_trait;
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
pub trait FileStorage: Send + Sync {
    type Context: Default + Send + Sync + Clone;

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

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::Cursor;

    use tokio::io::AsyncReadExt;

    async fn run_consistency_test_suite<S: FileStorage>(storage: S, ctx: &S::Context) {
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
}
