use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

struct PendingJobs<K, J> {
    order: VecDeque<K>,
    jobs: HashMap<K, J>,
}

impl<K, J> Default for PendingJobs<K, J> {
    fn default() -> Self {
        Self {
            order: VecDeque::new(),
            jobs: HashMap::new(),
        }
    }
}

impl<K, J> PendingJobs<K, J>
where
    K: Clone + Eq + Hash,
{
    fn push_latest(&mut self, key: K, job: J) {
        if !self.jobs.contains_key(&key) {
            self.order.push_back(key.clone());
        }
        self.jobs.insert(key, job);
    }

    fn pop_front(&mut self) -> Option<J> {
        while let Some(key) = self.order.pop_front() {
            if let Some(job) = self.jobs.remove(&key) {
                return Some(job);
            }
        }
        None
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.jobs.len()
    }
}

struct WorkerState<K, J> {
    pending: PendingJobs<K, J>,
    closed: bool,
}

impl<K, J> Default for WorkerState<K, J> {
    fn default() -> Self {
        Self {
            pending: PendingJobs::default(),
            closed: false,
        }
    }
}

/// A single background worker that retains only the newest pending job for
/// each key. Work already being processed is allowed to finish; repeated
/// pending requests for the same key are replaced instead of spawning more
/// threads or growing an unbounded queue.
pub(super) struct CoalescingWorker<K, J> {
    state: Arc<(Mutex<WorkerState<K, J>>, Condvar)>,
}

impl<K, J> CoalescingWorker<K, J>
where
    K: Clone + Eq + Hash + Send + 'static,
    J: Send + 'static,
{
    pub(super) fn new(handler: impl Fn(J) + Send + 'static) -> Self {
        let state = Arc::new((Mutex::new(WorkerState::default()), Condvar::new()));
        let worker_state = Arc::clone(&state);

        thread::spawn(move || loop {
            let job = {
                let (lock, wakeup) = &*worker_state;
                let mut state = lock.lock().unwrap_or_else(|error| error.into_inner());
                loop {
                    if state.closed {
                        return;
                    }
                    if let Some(job) = state.pending.pop_front() {
                        break job;
                    }
                    state = wakeup
                        .wait(state)
                        .unwrap_or_else(|error| error.into_inner());
                }
            };

            handler(job);
        });

        Self { state }
    }

    pub(super) fn submit(&self, key: K, job: J) {
        let (lock, wakeup) = &*self.state;
        let mut state = lock.lock().unwrap_or_else(|error| error.into_inner());
        if state.closed {
            return;
        }
        state.pending.push_latest(key, job);
        wakeup.notify_one();
    }
}

impl<K, J> Drop for CoalescingWorker<K, J> {
    fn drop(&mut self) {
        let (lock, wakeup) = &*self.state;
        let mut state = lock.lock().unwrap_or_else(|error| error.into_inner());
        state.closed = true;
        wakeup.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn pending_jobs_keep_only_latest_value_per_key() {
        let mut pending = PendingJobs::default();
        pending.push_latest("tag", "a");
        pending.push_latest("tag", "ab");
        pending.push_latest("tag", "abc");

        assert_eq!(pending.len(), 1);
        assert_eq!(pending.pop_front(), Some("abc"));
        assert_eq!(pending.pop_front(), None);
    }

    #[test]
    fn pending_jobs_preserve_distinct_keys() {
        let mut pending = PendingJobs::default();
        pending.push_latest("tag", "tag query");
        pending.push_latest("actor", "actor query");
        pending.push_latest("tag", "new tag query");

        assert_eq!(pending.len(), 2);
        assert_eq!(pending.pop_front(), Some("new tag query"));
        assert_eq!(pending.pop_front(), Some("actor query"));
    }

    #[test]
    fn worker_coalesces_requests_submitted_while_busy() {
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        let release_rx = Arc::new(Mutex::new(release_rx));

        let worker = CoalescingWorker::new(move |job: u32| {
            if job == 1 {
                started_tx.send(()).unwrap();
                release_rx.lock().unwrap().recv().unwrap();
            }
            result_tx.send(job).unwrap();
        });

        worker.submit("tag", 1);
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        worker.submit("tag", 2);
        worker.submit("tag", 3);
        release_tx.send(()).unwrap();

        assert_eq!(result_rx.recv_timeout(Duration::from_secs(2)).unwrap(), 1);
        assert_eq!(result_rx.recv_timeout(Duration::from_secs(2)).unwrap(), 3);
        assert!(result_rx.recv_timeout(Duration::from_millis(100)).is_err());
    }
}
