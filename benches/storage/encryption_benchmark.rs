use std::hint::black_box;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use criterion::{BenchmarkId, Criterion, Throughput};
use tempfile::TempDir;
use tokio::io::{AsyncRead, ReadBuf};
use tokio::runtime::Runtime;

use filepaste::storage::StorageProvider;
use filepaste::storage::encryption::{EncryptedContext, EncryptedStorage};
use filepaste::storage::local::LocalStorage;

/// A high-performance dummy reader that cycles through a buffer.
struct CyclicDummyReader {
    pool: Arc<Vec<u8>>,
    pos: usize,
    total_to_read: u64,
    bytes_read: u64,
}

impl AsyncRead for CyclicDummyReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<tokio::io::Result<()>> {
        let remaining_in_file = self.total_to_read - self.bytes_read;
        if remaining_in_file == 0 {
            return Poll::Ready(Ok(()));
        }

        // Determine how much we can write into the provided buffer
        let to_read = std::cmp::min(buf.remaining(), remaining_in_file as usize);
        let pool_len = self.pool.len();

        let mut space_filled = 0;
        while space_filled < to_read {
            let pool_offset = self.pos % pool_len;
            let available_in_pool = pool_len - pool_offset;
            let chunk_size = std::cmp::min(available_in_pool, to_read - space_filled);

            buf.put_slice(&self.pool[pool_offset..pool_offset + chunk_size]);

            self.pos = (self.pos + chunk_size) % pool_len;
            space_filled += chunk_size;
        }

        self.bytes_read += space_filled as u64;
        Poll::Ready(Ok(()))
    }
}

pub fn bench_encryption(c: &mut Criterion) {
    let rt = Runtime::new().expect("failed to start tokio runtime");

    // Setup environment
    let temp_dir = TempDir::new().expect("failed to create temp dir");
    let salt_key = "fixed_length_salt_for_benchmarking".to_string();

    let storage = EncryptedStorage::new(
        LocalStorage::new(temp_dir.path()).expect("failed to start local storage"),
        salt_key,
    );
    let ctx = EncryptedContext::new(Default::default(), "secure_password");

    // Pre-generate entropy pool
    let mut pool_data = vec![0u8; 64 * 1024]; // 64KB pool for better L1/L2 cache locality
    rand::fill(&mut pool_data[..]);
    let pool = Arc::new(pool_data);

    let mut group = c.benchmark_group("Storage_IO_Performance");

    for size in [1, 10, 100].map(|m| m * 1024 * 1024) {
        group.throughput(Throughput::Bytes(size as u64));
        let path = PathBuf::from(format!("bench_{}", size));

        // Benchmark: PUT (Write + Encrypt)
        group.bench_with_input(BenchmarkId::new("put", size), &size, |b, &s| {
            b.to_async(&rt).iter(|| {
                let reader = Box::pin(CyclicDummyReader {
                    pool: Arc::clone(&pool),
                    pos: 0,
                    total_to_read: s,
                    bytes_read: 0,
                });
                storage.put(&path, reader, &ctx)
            });
        });

        // Benchmark: GET (Read + Decrypt)
        group.bench_with_input(BenchmarkId::new("get", size), &size, |b, _| {
            b.to_async(&rt).iter(|| async {
                let mut reader = storage
                    .get(&path, &ctx)
                    .await
                    .expect("Storage error")
                    .expect("File not found");

                let mut sink = tokio::io::sink();
                let bytes_processed = tokio::io::copy(&mut reader, &mut sink)
                    .await
                    .expect("Decryption/Read failed");

                black_box(bytes_processed);
            });
        });
    }

    group.finish();
}
