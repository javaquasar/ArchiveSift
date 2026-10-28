use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::error::ThumbnailError;
use crate::fs::thumb_rel_path;
use crate::logging::{
    emit_diagnostic, non_fatal_error_event, DiagnosticCategory, StderrDiagnosticSink,
    UiDiagnosticSink,
};
use crate::ui::ConsoleUi;

const MAX_CONCURRENT_THUMBNAIL_JOBS: usize = 4;

#[derive(Clone)]
struct ThumbnailWorkLimiter {
    permits: Arc<Semaphore>,
}

impl ThumbnailWorkLimiter {
    fn new(max_jobs: usize) -> Self {
        Self {
            permits: Arc::new(Semaphore::new(max_jobs.max(1))),
        }
    }

    async fn acquire(&self) -> OwnedSemaphorePermit {
        self.permits
            .clone()
            .acquire_owned()
            .await
            .expect("thumbnail work limiter is never closed")
    }
}

fn thumbnail_work_limiter() -> &'static ThumbnailWorkLimiter {
    static LIMITER: OnceLock<ThumbnailWorkLimiter> = OnceLock::new();
    LIMITER.get_or_init(|| ThumbnailWorkLimiter::new(MAX_CONCURRENT_THUMBNAIL_JOBS))
}

/// Generates pre-scaled thumbnails for downloaded images.
#[derive(Clone, Debug)]
pub(crate) struct ThumbnailService {
    thumb_dir: PathBuf,
}

impl ThumbnailService {
    pub(crate) fn new(thumb_dir: PathBuf) -> Self {
        Self { thumb_dir }
    }

    #[cfg(test)]
    pub(crate) fn thumb_dir(&self) -> &Path {
        &self.thumb_dir
    }

    pub(crate) async fn ensure_with_diagnostics(
        &self,
        ui: &ConsoleUi,
        image_id: i64,
        source: &Path,
    ) -> Option<PathBuf> {
        let thumb_path = self.thumb_dir.join(thumb_rel_path(image_id));

        if thumb_path.exists() {
            return Some(thumb_path);
        }

        let source_buf = source.to_path_buf();
        let thumb_path_buf = thumb_path.clone();
        let work_permit = thumbnail_work_limiter().acquire().await;

        let gen_result = tokio::task::spawn_blocking(move || -> Result<(), ThumbnailError> {
            let _work_permit = work_permit;
            if let Some(parent) = thumb_path_buf.parent() {
                std::fs::create_dir_all(parent).map_err(|source| {
                    ThumbnailError::CreateParentDir {
                        path: parent.to_path_buf(),
                        source,
                    }
                })?;
            }

            let img =
                image::open(&source_buf).map_err(|source| ThumbnailError::LoadSourceImage {
                    path: source_buf.clone(),
                    source,
                })?;
            let thumb = img.thumbnail(400, 400);
            thumb
                .save(&thumb_path_buf)
                .map_err(|source| ThumbnailError::SaveThumbnail {
                    path: thumb_path_buf.clone(),
                    source,
                })?;
            Ok(())
        })
        .await;

        match gen_result {
            Ok(Ok(())) => Some(thumb_path),
            Ok(Err(error)) => {
                report_thumbnail_warning(ui, "generate thumbnail", &error);
                None
            }
            Err(source) => {
                let error = ThumbnailError::GenerateTaskFailed {
                    path: thumb_path.clone(),
                    source,
                };
                report_thumbnail_warning(ui, "join thumbnail generation task", &error);
                None
            }
        }
    }
}

fn report_thumbnail_warning(ui: &ConsoleUi, context: &str, error: &dyn std::error::Error) {
    let event = non_fatal_error_event(DiagnosticCategory::Thumbnail, context, error);
    let stderr_sink = StderrDiagnosticSink;
    let ui_sink = UiDiagnosticSink::new(ui);
    emit_diagnostic(&event, &[&stderr_sink, &ui_sink]);
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use image::{ImageBuffer, Rgb};

    fn unique_test_root(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock drift")
            .as_nanos();
        std::env::temp_dir().join(format!("archivesift_thumbnail_{label}_{nanos}"))
    }

    fn write_source_image(path: &Path) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create source image dir");
        }
        let image: ImageBuffer<Rgb<u8>, Vec<u8>> =
            ImageBuffer::from_pixel(32, 24, Rgb([12, 34, 56]));
        image.save(path).expect("save source image");
    }

    #[tokio::test]
    async fn thumbnail_limiter_bounds_parallel_cpu_work() {
        let limiter = ThumbnailWorkLimiter::new(2);
        let first = limiter.acquire().await;
        let second = limiter.acquire().await;

        assert!(limiter.permits.clone().try_acquire_owned().is_err());
        drop(first);
        assert!(limiter.permits.clone().try_acquire_owned().is_ok());
        drop(second);
    }

    #[tokio::test]
    async fn thumbnail_limiter_treats_zero_as_one_worker() {
        let limiter = ThumbnailWorkLimiter::new(0);
        let only = limiter.acquire().await;

        assert!(limiter.permits.clone().try_acquire_owned().is_err());
        drop(only);
    }

    #[tokio::test]
    async fn existing_thumbnail_is_reused_and_returned() {
        let root = unique_test_root("reuse");
        fs::create_dir_all(&root).expect("create root");
        let image_id = 41;
        let ui = ConsoleUi::new();
        let thumbnail_service = ThumbnailService::new(root.join("thumbs"));
        let thumb_dir = thumbnail_service.thumb_dir();
        let thumb_path = thumb_dir.join(thumb_rel_path(image_id));

        fs::create_dir_all(thumb_path.parent().expect("thumb parent")).expect("create thumb dir");
        fs::write(&thumb_path, b"existing-thumb").expect("write existing thumbnail");

        let returned = thumbnail_service
            .ensure_with_diagnostics(&ui, image_id, &root.join("unused-source.jpg"))
            .await;

        assert_eq!(returned, Some(thumb_path));

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn missing_thumbnail_is_generated_and_returned() {
        let root = unique_test_root("generate");
        fs::create_dir_all(&root).expect("create root");
        let image_id = 42;
        let ui = ConsoleUi::new();
        let thumbnail_service = ThumbnailService::new(root.join("thumbs"));
        let thumb_dir = thumbnail_service.thumb_dir();
        let source_path = root.join("downloads").join("demo-source.jpg");
        let thumb_path = thumb_dir.join(thumb_rel_path(image_id));

        write_source_image(&source_path);

        let returned = thumbnail_service
            .ensure_with_diagnostics(&ui, image_id, &source_path)
            .await;

        assert!(thumb_path.exists(), "thumbnail should be generated");
        assert_eq!(returned, Some(thumb_path));

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn generation_failure_is_reported_to_ui_log() {
        let root = unique_test_root("warning");
        fs::create_dir_all(&root).expect("create root");
        let image_id = 43;
        let log = crate::ui::UiLog::default();
        let ui = ConsoleUi::from_log(log.clone());
        let thumbnail_service = ThumbnailService::new(root.join("thumbs"));
        let missing_source = root.join("downloads").join("missing-source.jpg");

        let returned = thumbnail_service
            .ensure_with_diagnostics(&ui, image_id, &missing_source)
            .await;

        let snapshot = log.snapshot();
        assert!(
            snapshot
                .iter()
                .any(|line| line.starts_with("[warn] generate thumbnail:")),
            "expected warning in UI log, got: {snapshot:?}"
        );
        assert_eq!(returned, None);

        let _ = fs::remove_dir_all(root);
    }
}
