use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use rustic_core::{Progress, ProgressBars, ProgressType, RusticProgress};

/// What a progress counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressKind {
    /// Indeterminate, only started and finished.
    Activity,
    /// Counts items, e.g. index files.
    Items,
    /// Counts bytes.
    Bytes,
}

/// A snapshot of one running progress, reported to the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgressUpdate {
    pub kind: ProgressKind,
    pub title: String,
    pub position: u64,
    /// `0` if unknown.
    pub length: u64,
    pub finished: bool,
}

impl ProgressUpdate {
    /// Completed fraction in `0.0..=1.0`, if the length is known.
    pub fn fraction(&self) -> Option<f64> {
        (self.length > 0).then(|| (self.position as f64 / self.length as f64).min(1.0))
    }
}

pub(crate) type ProgressCallback = Arc<dyn Fn(ProgressUpdate) + Send + Sync>;

/// The `ProgressBars` of a repository. rustic fixes them when the repository is
/// created, so operations attach their callback here for their duration.
#[derive(Clone, Default)]
pub(crate) struct ProgressRouter {
    callback: Arc<Mutex<Option<ProgressCallback>>>,
}

impl std::fmt::Debug for ProgressRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProgressRouter").finish_non_exhaustive()
    }
}

impl ProgressRouter {
    /// Routes progress to `callback` until the returned guard is dropped.
    pub fn attach(&self, callback: ProgressCallback) -> AttachGuard<'_> {
        *self.callback.lock().unwrap() = Some(callback);
        AttachGuard { router: self }
    }
}

pub(crate) struct AttachGuard<'a> {
    router: &'a ProgressRouter,
}

impl Drop for AttachGuard<'_> {
    fn drop(&mut self) {
        *self.router.callback.lock().unwrap() = None;
    }
}

impl ProgressBars for ProgressRouter {
    fn progress(&self, progress_type: ProgressType, prefix: &str) -> Progress {
        let Some(callback) = self.callback.lock().unwrap().clone() else {
            return Progress::hidden();
        };
        let kind = match progress_type {
            ProgressType::Spinner => ProgressKind::Activity,
            ProgressType::Counter => ProgressKind::Items,
            ProgressType::Bytes => ProgressKind::Bytes,
        };
        Progress::new(CallbackProgress {
            callback,
            kind,
            title: Mutex::new(prefix.to_owned()),
            position: AtomicU64::new(0),
            length: AtomicU64::new(0),
            last_report: Mutex::new(None),
        })
    }
}

/// Minimum time between two reports, `inc` is called per blob.
const REPORT_INTERVAL: Duration = Duration::from_millis(50);

struct CallbackProgress {
    callback: ProgressCallback,
    kind: ProgressKind,
    title: Mutex<String>,
    position: AtomicU64,
    length: AtomicU64,
    last_report: Mutex<Option<Instant>>,
}

impl std::fmt::Debug for CallbackProgress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CallbackProgress")
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

impl CallbackProgress {
    fn report(&self, finished: bool, force: bool) {
        {
            let mut last = self.last_report.lock().unwrap();
            let now = Instant::now();
            if !force && last.is_some_and(|last| now - last < REPORT_INTERVAL) {
                return;
            }
            *last = Some(now);
        }
        (self.callback)(ProgressUpdate {
            kind: self.kind,
            title: self.title.lock().unwrap().clone(),
            position: self.position.load(Ordering::Relaxed),
            length: self.length.load(Ordering::Relaxed),
            finished,
        });
    }
}

impl RusticProgress for CallbackProgress {
    fn is_hidden(&self) -> bool {
        false
    }

    fn set_length(&self, len: u64) {
        self.length.store(len, Ordering::Relaxed);
        self.report(false, true);
    }

    fn set_title(&self, title: &str) {
        title.clone_into(&mut self.title.lock().unwrap());
        self.report(false, true);
    }

    fn inc(&self, inc: u64) {
        self.position.fetch_add(inc, Ordering::Relaxed);
        self.report(false, false);
    }

    fn finish(&self) {
        self.report(true, true);
    }
}
