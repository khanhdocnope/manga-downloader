//! Lưu ảnh: lọc kích thước, khử trùng lặp SHA-256. Băm + đọc header chạy trên thread pool.
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

pub const MIN_WIDTH: usize = 450;
pub const MIN_HEIGHT: usize = 600;

#[derive(Clone, Debug)]
pub struct SavedImage {
    pub index: usize,
    pub path: PathBuf,
    #[allow(dead_code)]
    pub sha256: String,
    pub width: usize,
    pub height: usize,
    #[allow(dead_code)]
    pub source: String,
    pub size: u64,
}

#[derive(Default, Clone, Copy, Debug)]
pub struct Stats {
    pub small: usize,
    pub duplicate: usize,
    pub invalid: usize,
}

#[derive(Default)]
struct Inner {
    count: usize,
    hashes: HashSet<String>,
    rejected: HashSet<String>,
    stats: Stats,
}

pub struct FileManager {
    out_dir: PathBuf,
    inner: Mutex<Inner>,
}

/// (width, height, ext) - ext = None nếu định dạng không được hỗ trợ.
fn inspect(data: &[u8]) -> Option<(usize, usize, Option<&'static str>)> {
    use imagesize::ImageType;
    let kind = imagesize::image_type(data).ok()?;
    let size = imagesize::blob_size(data).ok()?;
    let ext = match kind {
        ImageType::Jpeg => Some(".jpg"),
        ImageType::Png => Some(".png"),
        ImageType::Webp => Some(".webp"),
        ImageType::Gif => Some(".gif"),
        _ => None,
    };
    Some((size.width, size.height, ext))
}

impl FileManager {
    pub fn new(out_dir: PathBuf) -> Self {
        Self { out_dir, inner: Mutex::new(Inner::default()) }
    }

    pub async fn count(&self) -> usize {
        self.inner.lock().await.count
    }

    pub async fn stats(&self) -> Stats {
        self.inner.lock().await.stats
    }

    /// Lưu ảnh nếu hợp lệ, đủ lớn và chưa trùng. None nếu bị loại.
    pub async fn save(
        &self,
        data: Arc<Vec<u8>>,
        source: &str,
        force_ext: Option<&str>,
    ) -> Option<SavedImage> {
        let d = Arc::clone(&data);
        let (digest, info) = tokio::task::spawn_blocking(move || {
            (format!("{:x}", Sha256::digest(&d[..])), inspect(&d))
        })
        .await
        .ok()?;

        let mut inner = self.inner.lock().await;
        if inner.hashes.contains(&digest) || inner.rejected.contains(&digest) {
            inner.stats.duplicate += 1;
            return None;
        }
        let Some((width, height, fmt_ext)) = info else {
            inner.rejected.insert(digest);
            inner.stats.invalid += 1;
            return None;
        };
        if width < MIN_WIDTH || height < MIN_HEIGHT {
            inner.rejected.insert(digest);
            inner.stats.small += 1;
            return None;
        }
        let Some(ext) = force_ext.or(fmt_ext) else {
            inner.rejected.insert(digest);
            inner.stats.invalid += 1;
            return None;
        };

        let index = inner.count + 1;
        let path = self.out_dir.join(format!("{index:03}{ext}"));
        if tokio::fs::create_dir_all(&self.out_dir).await.is_err()
            || tokio::fs::write(&path, &data[..]).await.is_err()
        {
            return None;
        }
        inner.hashes.insert(digest.clone());
        inner.count = index;
        Some(SavedImage {
            index,
            path,
            sha256: digest,
            width,
            height,
            source: source.to_string(),
            size: data.len() as u64,
        })
    }
}
