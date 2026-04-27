//! Flow control for the tunnel: each direction gates **one** TCP read hop — the side
//! that sends DATA toward the tunnel uses `acquire_permit` → `forget` per chunk, and
//! restores credits only when the peer ACKs after writing to its local TCP.

use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub const DEFAULT_MAX_PENDING_MESSAGES_PER_CONNECTION: usize = 256;

/// Batch ACK frames to reduce tunnel overhead (client and server writers).
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
