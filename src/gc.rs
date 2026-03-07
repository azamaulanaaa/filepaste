use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tracing::info;

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

pub fn spawn_gc<S>(storage: Arc<S>, interval: Duration)
where
    S: StorageProvider + 'static,
{
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(interval);

        loop {
            interval.tick().await;
            info!("Start pruning files");
            let _ = storage.prune(Path::new(""), &Default::default()).await;
        }
    });
}
