use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

/// A live allocation budget. A lease follows the buffer, including queued deliveries.
#[derive(Clone, Debug)]
pub struct BufferBudget {
    capacity: u32,
    permits: Arc<Semaphore>,
}

#[derive(Debug)]
pub struct BufferLease {
    _permit: OwnedSemaphorePermit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BufferLimitError {
    TooLarge,
    Cancelled,
}

impl BufferBudget {
    pub fn new(capacity: u32) -> Self {
        Self {
            capacity,
            permits: Arc::new(Semaphore::new(capacity as usize)),
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity as usize
    }

    pub fn available(&self) -> usize {
        self.permits.available_permits()
    }

    /// Acquire the entire allocation before building it. Oversized requests never wait.
    pub async fn reserve(
        &self,
        bytes: usize,
        cancel: &CancellationToken,
    ) -> Result<BufferLease, BufferLimitError> {
        if bytes > self.capacity() {
            return Err(BufferLimitError::TooLarge);
        }
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(BufferLimitError::Cancelled),
            permit = self.permits.clone().acquire_many_owned(bytes as u32) => {
                Ok(BufferLease { _permit: permit.expect("budget semaphore is never closed") })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn released_buffers_do_not_consume_a_cumulative_allowance() {
        let budget = BufferBudget::new(1024);
        for _ in 0..100 {
            let lease = budget
                .reserve(1024, &CancellationToken::new())
                .await
                .unwrap();
            assert_eq!(budget.available(), 0);
            drop(lease);
        }
        assert_eq!(budget.available(), 1024);
    }

    #[tokio::test]
    async fn pending_allocation_waits_for_release_and_observes_cancellation() {
        let budget = BufferBudget::new(64);
        let cancel = CancellationToken::new();
        let held = budget.reserve(64, &cancel).await.unwrap();
        let mut pending = Box::pin(budget.reserve(1, &cancel));
        assert!(futures::poll!(&mut pending).is_pending());
        drop(held);
        let one = pending.await.unwrap();
        assert_eq!(budget.available(), 63);
        cancel.cancel();
        assert!(matches!(
            budget.reserve(64, &cancel).await,
            Err(BufferLimitError::Cancelled)
        ));
        drop(one);
        assert_eq!(budget.available(), 64);
    }

    #[tokio::test]
    async fn impossible_allocations_fail_without_waiting() {
        let budget = BufferBudget::new(8);
        assert!(matches!(
            budget.reserve(9, &CancellationToken::new()).await,
            Err(BufferLimitError::TooLarge)
        ));
    }
}
