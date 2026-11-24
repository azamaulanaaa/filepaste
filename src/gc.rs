use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tracing::{error, info, warn};

use crate::storage::{Resource, StorageProvider};

#[async_trait]
trait StoragePrune: StorageProvider {
    /// Recursively walks the storage and attempts to delete everything.
    /// Relies on the underlying delete() implementation to enforce logic.
    async fn prune(&self, path: &Path, ctx: &Self::Context) -> io::Result<()> {
        let items = self.list(path, ctx).await?;

        for item in items {
            match item {
                Resource::File { path, .. } => match self.delete(&path, ctx).await {
                    Ok(_) => tracing::info!("Pruned: {:?}", path),
                    Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                        tracing::debug!("Skipping locked resource: {:?}", path);
                    }
                    Err(e) => tracing::error!("Prune failed for {:?}: {}", path, e),
                },
                Resource::Directory { path, .. } => {
                    // Tail-call recursion for directories
                    Box::pin(self.prune(&path, ctx)).await?;
                }
            }
        }
        Ok(())
    }
}

impl<T: StorageProvider> StoragePrune for T {}

pub fn spawn_gc<S>(storage: Arc<S>, interval: Duration) -> tokio::task::JoinHandle<()>
where
    S: StorageProvider + 'static,
{
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(interval);
        let ctx = S::Context::default();

        loop {
            interval.tick().await;
            info!("Start pruning files");
            if let Err(e) = storage.prune(Path::new(""), &ctx).await {
                error!("Pruning failed: {e}");
            }
            match storage.cleanup_stale_multipart_uploads(&ctx).await {
                Ok(0) => {}
                Ok(aborted) => info!("Aborted {aborted} stale multipart uploads"),
                Err(e) => warn!("Multipart upload cleanup failed: {e}"),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::sync::Notify;

    use super::*;
    use crate::storage::in_memory::{InMemoryContext, InMemoryStorage};
    use crate::storage::{AsyncFileReader, FileMetadata};

    struct CleanupFailureStorage {
        inner: InMemoryStorage,
        list_calls: Arc<AtomicUsize>,
        cleanup_calls: Arc<AtomicUsize>,
        notify: Arc<Notify>,
    }

    #[async_trait]
    impl StorageProvider for CleanupFailureStorage {
        type Context = InMemoryContext;

        async fn put(
            &self,
            path: &Path,
            content: AsyncFileReader,
            ctx: &Self::Context,
        ) -> io::Result<u64> {
            self.inner.put(path, content, ctx).await
        }

        async fn get(
            &self,
            path: &Path,
            ctx: &Self::Context,
        ) -> io::Result<Option<AsyncFileReader>> {
            self.inner.get(path, ctx).await
        }

        async fn delete(&self, path: &Path, ctx: &Self::Context) -> io::Result<()> {
            self.inner.delete(path, ctx).await
        }

        async fn metadata(
            &self,
            path: &Path,
            ctx: &Self::Context,
        ) -> io::Result<Option<FileMetadata>> {
            self.inner.metadata(path, ctx).await
        }

        async fn list(&self, path: &Path, ctx: &Self::Context) -> io::Result<Vec<Resource>> {
            self.list_calls.fetch_add(1, Ordering::SeqCst);
            self.inner.list(path, ctx).await
        }

        async fn cleanup_stale_multipart_uploads(&self, _ctx: &Self::Context) -> io::Result<u64> {
            self.cleanup_calls.fetch_add(1, Ordering::SeqCst);
            self.notify.notify_waiters();
            Err(io::Error::other("cleanup failed"))
        }
    }

    #[tokio::test]
    async fn spawn_gc_continues_after_a_cleanup_error() {
        let list_calls = Arc::new(AtomicUsize::new(0));
        let cleanup_calls = Arc::new(AtomicUsize::new(0));
        let notify = Arc::new(Notify::new());
        let storage = Arc::new(CleanupFailureStorage {
            inner: InMemoryStorage::new(),
            list_calls: Arc::clone(&list_calls),
            cleanup_calls: Arc::clone(&cleanup_calls),
            notify: Arc::clone(&notify),
        });

        let handle = spawn_gc(Arc::clone(&storage), Duration::from_millis(1));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while cleanup_calls.load(Ordering::SeqCst) < 2 {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            let _ = tokio::time::timeout(remaining, notify.notified()).await;
        }
        handle.abort();

        assert!(list_calls.load(Ordering::SeqCst) >= 2);
        assert!(cleanup_calls.load(Ordering::SeqCst) >= 2);
    }
}
