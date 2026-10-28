use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;

const BACKGROUND_WORKERS: usize = 4;
const BACKGROUND_QUEUE_CAPACITY: usize = 64;

type BackgroundJob = Box<dyn FnOnce() + Send + 'static>;

struct BackgroundExecutor {
    sender: SyncSender<BackgroundJob>,
}

impl BackgroundExecutor {
    fn new(worker_count: usize, queue_capacity: usize) -> Self {
        let (sender, receiver) = mpsc::sync_channel(queue_capacity.max(1));
        let receiver = Arc::new(Mutex::new(receiver));
        for worker_index in 0..worker_count.max(1) {
            let receiver = Arc::clone(&receiver);
            thread::Builder::new()
                .name(format!("ui-background-{worker_index}"))
                .spawn(move || run_worker(receiver))
                .expect("background worker should start");
        }
        Self { sender }
    }

    fn execute(&self, job: BackgroundJob) {
        // A bounded channel applies backpressure instead of allowing UI actions
        // to create an unbounded number of operating-system threads.
        self.sender
            .send(job)
            .expect("background worker pool remains available");
    }
}

fn run_worker(receiver: Arc<Mutex<Receiver<BackgroundJob>>>) {
    loop {
        let job = {
            let receiver = receiver.lock().unwrap_or_else(|poison| poison.into_inner());
            receiver.recv()
        };
        let Ok(job) = job else {
            return;
        };
        job();
    }
}

pub(super) fn spawn_background(job: impl FnOnce() + Send + 'static) {
    static EXECUTOR: OnceLock<BackgroundExecutor> = OnceLock::new();
    EXECUTOR
        .get_or_init(|| BackgroundExecutor::new(BACKGROUND_WORKERS, BACKGROUND_QUEUE_CAPACITY))
        .execute(Box::new(job));
}

pub(super) fn shared_async_runtime() -> Result<&'static tokio::runtime::Runtime, String> {
    static RUNTIME: OnceLock<Result<tokio::runtime::Runtime, String>> = OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            let worker_threads = std::thread::available_parallelism()
                .map(usize::from)
                .unwrap_or(2)
                .clamp(2, 4);
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(worker_threads)
                .enable_all()
                .thread_name("archivesift-async")
                .build()
                .map_err(|error| error.to_string())
        })
        .as_ref()
        .map_err(Clone::clone)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[test]
    fn executor_never_exceeds_configured_worker_count() {
        let executor = BackgroundExecutor::new(2, 8);
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let (done_tx, done_rx) = mpsc::channel();

        for _ in 0..6 {
            let active = Arc::clone(&active);
            let peak = Arc::clone(&peak);
            let done_tx = done_tx.clone();
            executor.execute(Box::new(move || {
                let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                thread::sleep(Duration::from_millis(10));
                active.fetch_sub(1, Ordering::SeqCst);
                done_tx.send(()).unwrap();
            }));
        }
        for _ in 0..6 {
            done_rx.recv().unwrap();
        }

        assert!(peak.load(Ordering::SeqCst) <= 2);
    }

    #[test]
    fn async_runtime_is_reused() {
        let first = shared_async_runtime().expect("runtime");
        let second = shared_async_runtime().expect("runtime");

        assert!(std::ptr::eq(first, second));
    }
}
