use std::io;
use std::path::Path;
use std::sync::Mutex;

use async_trait::async_trait;

use super::in_memory::{InMemoryContext, InMemoryStorage};
use super::{AsyncFileReader, FileMetadata, Resource, StorageProvider};

/// Stores content and records each `put_sized` length.
pub(crate) struct RecordingStorage {
    inner: InMemoryStorage,
    lengths: Mutex<Vec<u64>>,
}

impl RecordingStorage {
    pub(crate) fn new() -> Self {
        Self {
            inner: InMemoryStorage::new(),
            lengths: Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn lengths(&self) -> Vec<u64> {
        self.lengths.lock().expect("length lock").clone()
    }
}

#[async_trait]
impl StorageProvider for RecordingStorage {
    type Context = InMemoryContext;

    async fn put(
        &self,
        path: &Path,
        content: AsyncFileReader,
        ctx: &Self::Context,
    ) -> io::Result<u64> {
        self.inner.put(path, content, ctx).await
    }

    async fn put_sized(
        &self,
        path: &Path,
        content: AsyncFileReader,
        length: u64,
        ctx: &Self::Context,
    ) -> io::Result<u64> {
        self.lengths.lock().expect("length lock").push(length);
        self.inner.put_sized(path, content, length, ctx).await
    }

    async fn get(&self, path: &Path, ctx: &Self::Context) -> io::Result<Option<AsyncFileReader>> {
        self.inner.get(path, ctx).await
    }

    async fn delete(&self, path: &Path, ctx: &Self::Context) -> io::Result<()> {
        self.inner.delete(path, ctx).await
    }

    async fn metadata(&self, path: &Path, ctx: &Self::Context) -> io::Result<Option<FileMetadata>> {
        self.inner.metadata(path, ctx).await
    }

    async fn list(&self, path: &Path, ctx: &Self::Context) -> io::Result<Vec<Resource>> {
        self.inner.list(path, ctx).await
    }
}
