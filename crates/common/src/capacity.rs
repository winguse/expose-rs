//! Flow control for the tunnel: each direction gates **one** TCP read hop — the side
//! that sends DATA toward the tunnel uses `acquire_permit` → `forget` per chunk, and
//! restores credits only when the peer ACKs after writing to its local TCP.

use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub const DEFAULT_MAX_PENDING_MESSAGES_PER_CONNECTION: usize = 256;

/// Batch ACK frames to reduce tunnel overhead (client and server writers).
///
/// **Constraint:** `CapacityConfig::max_pending_messages_per_connection` must be
/// greater than or equal to `ACK_BATCH_SIZE`.  If the window is smaller than the
/// batch size the sender will exhaust its permits before the receiver accumulates
/// enough writes to send an ACK, causing a deadlock.  The default window (256) is
/// 4× the batch size which gives a comfortable safety margin.
pub const ACK_BATCH_SIZE: u32 = 64;

#[derive(Clone, Copy, Debug)]
pub struct CapacityConfig {
    pub max_pending_messages_per_connection: usize,
}

impl Default for CapacityConfig {
    fn default() -> Self {
        Self {
            max_pending_messages_per_connection: DEFAULT_MAX_PENDING_MESSAGES_PER_CONNECTION,
        }
    }
}

pub fn semaphore_for_limit(limit: usize) -> Option<Arc<Semaphore>> {
    if limit == 0 {
        None
    } else {
        Some(Arc::new(Semaphore::new(limit)))
    }
}

pub async fn acquire_permit(limit: &Option<Arc<Semaphore>>) -> Option<OwnedSemaphorePermit> {
    match limit {
        Some(sem) => sem.clone().acquire_owned().await.ok(),
        None => None,
    }
}

/// Apply a peer ACK: each unit matches one `OwnedSemaphorePermit::forget` on the reader.
pub fn apply_flow_ack(sem: Option<&Semaphore>, ack_count: u32) {
    if ack_count == 0 {
        return;
    }
    if let Some(sem) = sem {
        sem.add_permits(ack_count as usize);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semaphore_for_limit_zero_returns_none() {
        assert!(semaphore_for_limit(0).is_none());
    }

    #[test]
    fn semaphore_for_limit_nonzero_returns_some_with_correct_permits() {
        let sem = semaphore_for_limit(8).expect("should be Some");
        assert_eq!(sem.available_permits(), 8);
    }

    #[tokio::test]
    async fn acquire_permit_with_none_returns_none() {
        let permit = acquire_permit(&None).await;
        assert!(permit.is_none());
    }

    #[tokio::test]
    async fn acquire_permit_with_some_returns_permit_and_decrements() {
        let sem = semaphore_for_limit(3);
        let permit = acquire_permit(&sem).await;
        assert!(permit.is_some());
        // The Arc<Semaphore> inside sem still reflects one acquired permit.
        let available = sem.as_ref().unwrap().available_permits();
        assert_eq!(available, 2);
        drop(permit); // returns the permit
        assert_eq!(sem.as_ref().unwrap().available_permits(), 3);
    }

    #[tokio::test]
    async fn acquire_permit_forget_does_not_return_credit() {
        let sem = semaphore_for_limit(2);
        let permit = acquire_permit(&sem).await.unwrap();
        assert_eq!(sem.as_ref().unwrap().available_permits(), 1);
        permit.forget(); // credit is gone until apply_flow_ack restores it
        assert_eq!(sem.as_ref().unwrap().available_permits(), 1);
    }

    #[test]
    fn apply_flow_ack_zero_is_noop() {
        let sem = Semaphore::new(2);
        apply_flow_ack(Some(&sem), 0);
        assert_eq!(sem.available_permits(), 2);
    }

    #[test]
    fn apply_flow_ack_none_sem_is_noop() {
        // Should not panic.
        apply_flow_ack(None, 5);
    }

    #[test]
    fn apply_flow_ack_restores_permits() {
        let sem = Semaphore::new(0);
        apply_flow_ack(Some(&sem), 4);
        assert_eq!(sem.available_permits(), 4);
    }

    #[tokio::test]
    async fn forget_then_ack_restores_full_capacity() {
        let sem = semaphore_for_limit(2);
        // Acquire both permits and forget them (simulating in-flight DATA frames).
        let p1 = acquire_permit(&sem).await.unwrap();
        let p2 = acquire_permit(&sem).await.unwrap();
        p1.forget();
        p2.forget();
        assert_eq!(sem.as_ref().unwrap().available_permits(), 0);

        // ACK restores 2 credits.
        apply_flow_ack(sem.as_deref(), 2);
        assert_eq!(sem.as_ref().unwrap().available_permits(), 2);
    }
}
