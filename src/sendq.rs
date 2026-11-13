use std::sync::Arc;

use tokio::sync::{Semaphore, mpsc};

#[derive(Clone)]
pub struct SendQueue {
    tx: mpsc::UnboundedSender<String>,
    budget: Arc<Semaphore>,
    cap: usize,
}

pub struct SendQueueRx {
    rx: mpsc::UnboundedReceiver<String>,
    budget: Arc<Semaphore>,
    cap: usize,
}

pub fn send_queue(cap_bytes: usize) -> (SendQueue, SendQueueRx) {
    let (tx, rx) = mpsc::unbounded_channel();
    let budget = Arc::new(Semaphore::new(cap_bytes));
    (
        SendQueue {
            tx,
            budget: budget.clone(),
            cap: cap_bytes,
        },
        SendQueueRx {
            rx,
            budget,
            cap: cap_bytes,
        },
    )
}

fn cost(msg: &str, cap: usize) -> u32 {
    msg.len().clamp(1, cap).min(u32::MAX as usize) as u32
}

impl SendQueue {
    pub fn try_send(&self, msg: String) -> Result<(), String> {
        let permits = cost(&msg, self.cap);
        match self.budget.try_acquire_many(permits) {
            Ok(permit) => self.push(permit, msg),
            Err(_) => Err(msg),
        }
    }

    pub async fn send(&self, msg: String) -> Result<(), String> {
        let permits = cost(&msg, self.cap);
        match self.budget.acquire_many(permits).await {
            Ok(permit) => self.push(permit, msg),
            Err(_) => Err(msg),
        }
    }

    fn push(&self, permit: tokio::sync::SemaphorePermit<'_>, msg: String) -> Result<(), String> {
        let permits = permit.num_permits();
        permit.forget();
        match self.tx.send(msg) {
            Ok(()) => {
                crate::metrics::add_send_queue_bytes(permits);
                Ok(())
            }
            Err(e) => {
                self.budget.add_permits(permits);
                Err(e.0)
            }
        }
    }

    pub fn queued_bytes(&self) -> usize {
        self.cap - self.budget.available_permits()
    }
}

impl SendQueueRx {
    pub async fn recv(&mut self) -> Option<String> {
        let msg = self.rx.recv().await?;
        self.release(&msg);
        Some(msg)
    }

    pub fn try_recv(&mut self) -> Result<String, mpsc::error::TryRecvError> {
        let msg = self.rx.try_recv()?;
        self.release(&msg);
        Ok(msg)
    }

    fn release(&self, msg: &str) {
        let permits = cost(msg, self.cap) as usize;
        self.budget.add_permits(permits);
        crate::metrics::sub_send_queue_bytes(permits);
    }
}

impl Drop for SendQueueRx {
    fn drop(&mut self) {
        self.rx.close();
        let mut left = 0;
        while let Ok(msg) = self.rx.try_recv() {
            left += cost(&msg, self.cap) as usize;
        }
        crate::metrics::sub_send_queue_bytes(left);
        self.budget.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bounds_queued_bytes_not_message_count() {
        let (tx, mut rx) = send_queue(10_000);
        let big = "x".repeat(4_000);
        assert!(tx.try_send(big.clone()).is_ok());
        assert!(tx.try_send(big.clone()).is_ok());
        assert!(tx.try_send(big.clone()).is_err());
        assert_eq!(tx.queued_bytes(), 8_000);
        for _ in 0..3_000 {
            let _ = tx.try_send("y".into());
        }
        assert_eq!(tx.queued_bytes(), 10_000);

        assert_eq!(rx.recv().await.unwrap().len(), 4_000);
        assert_eq!(tx.queued_bytes(), 6_000);
        assert!(tx.try_send(big.clone()).is_ok());
        assert!(tx.try_send(big).is_err());
    }

    #[tokio::test]
    async fn send_waits_for_room() {
        let (tx, mut rx) = send_queue(100);
        tx.try_send("a".repeat(100)).unwrap();
        let waiter = tokio::spawn({
            let tx = tx.clone();
            async move { tx.send("b".repeat(50)).await }
        });
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());
        rx.recv().await.unwrap();
        waiter.await.unwrap().unwrap();
        assert_eq!(tx.queued_bytes(), 50);
    }

    #[tokio::test]
    async fn oversized_message_still_goes_through_an_empty_queue() {
        let (tx, mut rx) = send_queue(100);
        tx.send("z".repeat(1_000)).await.unwrap();
        assert_eq!(rx.recv().await.unwrap().len(), 1_000);
    }

    #[tokio::test]
    async fn closed_receiver_fails_senders() {
        let (tx, rx) = send_queue(100);
        drop(rx);
        assert!(tx.try_send("a".into()).is_err());
        assert!(tx.send("a".into()).await.is_err());
    }
}
