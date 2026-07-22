use dashmap::DashMap;
use std::{
    hash::Hash,
    sync::{Arc, OnceLock},
};
use tokio::sync::{Mutex, OwnedMutexGuard};

/// Per-key async mutex. Only one task at a time may hold the lock for a given key.
/// The map entry is removed automatically when the guard is dropped.
pub(crate) struct KeyedLock<K: Eq + Hash + Clone + Send + Sync + 'static> {
    map: OnceLock<Arc<DashMap<K, Arc<Mutex<()>>>>>,
}

impl<K: Eq + Hash + Clone + Send + Sync + 'static> KeyedLock<K> {
    pub const fn new() -> Self {
        Self {
            map: OnceLock::new(),
        }
    }

    fn inner(&self) -> Arc<DashMap<K, Arc<Mutex<()>>>> {
        self.map
            .get_or_init(|| Arc::new(DashMap::new()))
            .clone()
    }

    /// Acquire the lock for `key`, inserting an entry if none exists.
    pub async fn lock(&self, key: K) -> KeyedLockGuard<K> {
        let map = self.inner();
        let mutex = map
            .entry(key.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _guard = mutex
            .lock_owned()
            .await;
        KeyedLockGuard { map, key, _guard }
    }

    pub fn contains_key(&self, key: &K) -> bool {
        self.map
            .get()
            .map_or(false, |m| m.contains_key(key))
    }

    /// Acquire the lock only if an entry already exists (someone else is working).
    /// Returns `None` immediately if no entry is found.
    pub async fn lock_if_exists(&self, key: &K) -> Option<KeyedLockGuard<K>> {
        let map = self.inner();
        let mutex = map
            .get(key)
            .map(|e| Arc::clone(&e))?;
        let _guard = mutex
            .lock_owned()
            .await;
        Some(KeyedLockGuard {
            map,
            key: key.clone(),
            _guard,
        })
    }
}

pub(crate) struct KeyedLockGuard<K: Eq + Hash + Clone + Send + Sync + 'static> {
    map: Arc<DashMap<K, Arc<Mutex<()>>>>,
    key: K,
    _guard: OwnedMutexGuard<()>,
}

impl<K: Eq + Hash + Clone + Send + Sync + 'static> Drop for KeyedLockGuard<K> {
    fn drop(&mut self) {
        // The map and this owned guard are the final two strong references only
        // when nobody is queued for the key. Removing an entry while waiters
        // still hold the old mutex lets a new caller create a second mutex and
        // breaks mutual exclusion.
        let can_remove = self
            .map
            .get(&self.key)
            .is_some_and(|mutex| Arc::strong_count(&mutex) == 2);
        if can_remove {
            self.map
                .remove(&self.key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::KeyedLock;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use tokio::time::{Duration, sleep};

    #[tokio::test]
    async fn queued_and_late_callers_never_overlap_for_the_same_key() {
        let lock = Arc::new(KeyedLock::<u8>::new());
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();

        for delay_ms in [0, 1, 2, 8, 9, 10] {
            let lock = Arc::clone(&lock);
            let active = Arc::clone(&active);
            let peak = Arc::clone(&peak);
            tasks.push(tokio::spawn(async move {
                sleep(Duration::from_millis(delay_ms)).await;
                let _guard = lock
                    .lock(7)
                    .await;
                let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                sleep(Duration::from_millis(5)).await;
                active.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for task in tasks {
            task.await
                .unwrap();
        }
        assert_eq!(peak.load(Ordering::SeqCst), 1);
    }
}
