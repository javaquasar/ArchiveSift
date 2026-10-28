use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};

type Entry<T> = ([u8; 32], Arc<OnceLock<Result<Arc<T>, String>>>);

/// A small LRU of compilation cells. Different keys compile without the map lock;
/// concurrent requests share initialization, including errors, while the key is resident.
pub(super) struct CompilationCache<T> {
    entries: Mutex<VecDeque<Entry<T>>>,
    capacity: usize,
}

impl<T> CompilationCache<T> {
    pub const fn new(capacity: usize) -> Self {
        Self {
            entries: Mutex::new(VecDeque::new()),
            capacity,
        }
    }

    pub fn get_or_compile(
        &self,
        key: [u8; 32],
        compile: impl FnOnce() -> Result<T, String>,
    ) -> Result<Arc<T>, String> {
        let cell = {
            let mut entries = self
                .entries
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if let Some(index) = entries.iter().position(|(cached, _)| *cached == key) {
                let entry = entries.remove(index).unwrap();
                let cell = Arc::clone(&entry.1);
                entries.push_front(entry);
                cell
            } else {
                let cell = Arc::new(OnceLock::new());
                entries.push_front((key, Arc::clone(&cell)));
                while entries.len() > self.capacity {
                    entries.pop_back();
                }
                cell
            }
        };
        cell.get_or_init(|| compile().map(Arc::new)).clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn concurrent_loads_share_a_compilation_without_blocking_other_keys() {
        let cache = Arc::new(CompilationCache::new(4));
        let calls = Arc::new(AtomicUsize::new(0));
        let start = Arc::new(std::sync::Barrier::new(8));
        let threads = (0..8)
            .map(|_| {
                let cache = Arc::clone(&cache);
                let calls = Arc::clone(&calls);
                let start = Arc::clone(&start);
                std::thread::spawn(move || {
                    start.wait();
                    cache
                        .get_or_compile([1; 32], || {
                            calls.fetch_add(1, Ordering::Relaxed);
                            // Recursing on another key would deadlock if compilation held the map lock.
                            assert_eq!(*cache.get_or_compile([2; 32], || Ok(22)).unwrap(), 22);
                            Ok(11)
                        })
                        .unwrap()
                })
            })
            .collect::<Vec<_>>();
        let artifacts = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert!(artifacts
            .iter()
            .all(|artifact| Arc::ptr_eq(artifact, &artifacts[0])));
    }

    #[test]
    fn eviction_preserves_live_artifacts_and_recompiles_only_the_evicted_key() {
        let cache = CompilationCache::new(2);
        let first = cache.get_or_compile([1; 32], || Ok(1)).unwrap();
        let second = cache.get_or_compile([2; 32], || Ok(2)).unwrap();
        assert!(Arc::ptr_eq(
            &first,
            &cache
                .get_or_compile([1; 32], || panic!("unexpected recompile"))
                .unwrap()
        ));
        cache.get_or_compile([3; 32], || Ok(3)).unwrap();
        assert_eq!(
            *second, 2,
            "an active Store/backend must outlive cache eviction"
        );
        let replacement = cache.get_or_compile([2; 32], || Ok(20)).unwrap();
        assert!(!Arc::ptr_eq(&second, &replacement));
        assert_eq!(*replacement, 20);
        assert_eq!(cache.entries.lock().unwrap().len(), 2);
    }

    #[test]
    fn failed_contract_checks_are_reused_until_the_binary_changes() {
        let cache = CompilationCache::<usize>::new(2);
        assert_eq!(
            cache
                .get_or_compile([1; 32], || Err("bad ABI".into()))
                .unwrap_err(),
            "bad ABI"
        );
        assert_eq!(
            cache
                .get_or_compile([1; 32], || panic!("recompiled a rejected binary"))
                .unwrap_err(),
            "bad ABI"
        );
        assert_eq!(*cache.get_or_compile([2; 32], || Ok(2)).unwrap(), 2);
    }
}
