use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Arc, LazyLock, Mutex as StdMutex, PoisonError};

use tokio::sync::{Mutex, OwnedMutexGuard};

/// The registry the file tools use unless handed another one.
///
/// Exclusion only holds between users of the *same* registry, and the resource
/// it protects — the filesystem — is process-wide, so this default is correct
/// by construction: forget to wire it up and the tools still serialize. Pass
/// your own through `BuildContext` to isolate (tests do).
pub fn shared_file_locks() -> Arc<KeyedLock<String>> {
    static SHARED: LazyLock<Arc<KeyedLock<String>>> = LazyLock::new(|| Arc::new(KeyedLock::new()));
    SHARED.clone()
}

/// Mutual exclusion keyed by an arbitrary value: holders of the same key run
/// one at a time, different keys never wait on each other. Entries are created
/// on demand and reaped once nobody holds or wants them, so the registry costs
/// only what is currently contended.
///
/// In-process and advisory — it says nothing about another process touching the
/// same resource.
///
/// **Deadlock rule:** hold at most one key at a time; a caller that needs two
/// must sort them and acquire in that order.
pub struct KeyedLock<K = String> {
    // Held only across HashMap operations, never across an await.
    registry: StdMutex<HashMap<K, Arc<Mutex<()>>>>,
}

impl<K: Eq + Hash + Clone> KeyedLock<K> {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        KeyedLock {
            registry: StdMutex::new(HashMap::new()),
        }
    }

    /// Acquire the lock for `key`, waiting for the current holder. Released
    /// when the returned guard drops.
    ///
    /// Cancel-safe: dropping this future before it resolves — the runtime does
    /// that to in-flight tools on abort — leaves nothing behind.
    pub async fn lock(&self, key: K) -> KeyedGuard<'_, K> {
        let entry = {
            let mut registry = self.lock_registry();
            match registry.get(&key) {
                Some(entry) => entry.clone(),
                None => {
                    let entry = Arc::new(Mutex::new(()));
                    registry.insert(key.clone(), entry.clone());
                    entry
                }
            }
        };
        // Built before the await, already holding the reference: giving up half
        // way then cleans up through the same drop path as a lock that was
        // taken. The waiter has to do it — whoever released while it was queued
        // saw its reference and rightly kept the entry.
        let mut acquiring = KeyedGuard {
            key,
            entry: Some(entry.clone()),
            guard: None,
            owner: self,
        };
        acquiring.guard = Some(entry.lock_owned().await);
        acquiring
    }

    /// Give up `entry`, dropping the key's registration if that leaves the
    /// registry as its only owner.
    fn release(&self, key: &K, entry: Arc<Mutex<()>>) {
        let mut registry = self.lock_registry();
        drop(entry);
        if let Some(entry) = registry.get(key) {
            // Holders and waiters alike take their reference under this lock,
            // so a count of 1 — the registry's own — means nobody is left.
            if Arc::strong_count(entry) == 1 {
                registry.remove(key);
            }
        }
    }

    fn lock_registry(&self) -> std::sync::MutexGuard<'_, HashMap<K, Arc<Mutex<()>>>> {
        // No critical section can panic, so poisoning is unreachable; recovering
        // keeps a panic elsewhere from wedging every later lock attempt.
        self.registry.lock().unwrap_or_else(PoisonError::into_inner)
    }

    #[cfg(test)]
    fn tracked_keys(&self) -> usize {
        self.lock_registry().len()
    }
}

/// Releases the key when dropped. Doubles as an acquisition still in progress,
/// where `guard` is `None` — see [`KeyedLock::lock`].
pub struct KeyedGuard<'a, K: Eq + Hash + Clone> {
    key: K,
    /// The registered entry, handed back on drop. `Option` only so `Drop` can
    /// pass ownership to `release`.
    entry: Option<Arc<Mutex<()>>>,
    /// `None` until the lock is acquired.
    guard: Option<OwnedMutexGuard<()>>,
    owner: &'a KeyedLock<K>,
}

impl<K: Eq + Hash + Clone> Drop for KeyedGuard<'_, K> {
    fn drop(&mut self) {
        // Unlock before reaping, so an entry is only removed while unlocked.
        // Anyone slipping in between takes their reference under the registry
        // lock, which `release` then sees.
        drop(self.guard.take());
        if let Some(entry) = self.entry.take() {
            self.owner.release(&self.key, entry);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[tokio::test]
    async fn same_key_serializes() {
        let locks = Arc::new(KeyedLock::<String>::new());
        let in_section = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));

        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..8 {
            let locks = locks.clone();
            let in_section = in_section.clone();
            let max_seen = max_seen.clone();
            tasks.spawn(async move {
                let _guard = locks.lock("same".to_string()).await;
                let now = in_section.fetch_add(1, Ordering::SeqCst) + 1;
                max_seen.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(5)).await;
                in_section.fetch_sub(1, Ordering::SeqCst);
            });
        }
        tasks.join_all().await;

        assert_eq!(max_seen.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn different_keys_do_not_block_each_other() {
        let locks = KeyedLock::<String>::new();
        let held = locks.lock("a".to_string()).await;
        // Would hang if distinct keys shared a mutex.
        let _other = locks.lock("b".to_string()).await;
        drop(held);
    }

    /// The registry must not grow without bound over a long-lived process.
    #[tokio::test]
    async fn entries_are_reaped_once_uncontended() {
        let locks = KeyedLock::<String>::new();
        {
            let _a = locks.lock("a".to_string()).await;
            let _b = locks.lock("b".to_string()).await;
            assert_eq!(locks.tracked_keys(), 2);
        }
        assert_eq!(locks.tracked_keys(), 0);
    }

    /// A waiter that gives up before acquiring — the runtime drops in-flight
    /// tool futures on abort — must reap its own entry: the holder released
    /// while it was still queued, saw its reference, and rightly kept the
    /// entry, so nobody else is left to.
    #[tokio::test]
    async fn abandoned_waiter_reaps_its_own_entry() {
        let locks = KeyedLock::<String>::new();
        let held = locks.lock("k".to_string()).await;

        // Poll once so it registers as a waiter, then never poll it again.
        let mut waiter = Box::pin(locks.lock("k".to_string()));
        let polled =
            std::future::poll_fn(|cx| std::task::Poll::Ready(waiter.as_mut().poll(cx))).await;
        assert!(polled.is_pending(), "waiter should be queued");
        assert_eq!(locks.tracked_keys(), 1);

        drop(held);
        drop(waiter);

        assert_eq!(locks.tracked_keys(), 0);
    }

    /// Reaping must not drop an entry another task is waiting on: that task
    /// would get a fresh mutex and both would run at once.
    #[tokio::test]
    async fn entry_survives_while_a_waiter_is_queued() {
        let locks = Arc::new(KeyedLock::<String>::new());
        let held = locks.lock("k".to_string()).await;

        let waiter_locks = locks.clone();
        let waiter = tokio::spawn(async move {
            let _guard = waiter_locks.lock("k".to_string()).await;
            tokio::time::sleep(Duration::from_millis(20)).await;
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(!waiter.is_finished(), "waiter should be blocked");

        drop(held);
        // The waiter now owns the same entry rather than a replacement.
        tokio::time::sleep(Duration::from_millis(5)).await;
        assert_eq!(locks.tracked_keys(), 1);
        waiter.await.unwrap();
        assert_eq!(locks.tracked_keys(), 0);
    }
}
