//! A resizable multi-producer, single-consumer queue for sending values between asynchronous tasks.

use std::sync::Arc;
use tokio::sync::mpsc::error::{SendError, TrySendError};
use tokio::sync::{mpsc, OwnedSemaphorePermit, Semaphore, TryAcquireError};

/// Send values to the associated `Receiver`.
#[derive(Debug)]
pub struct Sender<T> {
    inner: mpsc::UnboundedSender<(T, OwnedSemaphorePermit)>,
    semaphore: Arc<Semaphore>,
}

/// Receive values from the associated `Sender`.
#[derive(Debug)]
pub struct Receiver<T> {
    inner: mpsc::UnboundedReceiver<(T, OwnedSemaphorePermit)>,
    semaphore: Arc<Semaphore>,
    size: usize,
    debt: usize,
}

/// Creates a resizable mpsc channel for communicating between asynchronous tasks with backpressure.
pub fn resizable<T>(size: usize) -> (Sender<T>, Receiver<T>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let semaphore = Arc::new(Semaphore::new(size));

    let sender = Sender {
        inner: tx,
        semaphore: semaphore.clone(),
    };

    let receiver = Receiver {
        inner: rx,
        semaphore,
        size,
        debt: 0,
    };

    (sender, receiver)
}

impl<T> Sender<T> {
    /// Sends a value, waiting until there is capacity.
    pub async fn send(&self, value: T) -> Result<(), SendError<T>> {
        let permit = self.semaphore.clone().acquire_owned().await.unwrap();
        self.inner
            .send((value, permit))
            .map_err(|SendError((value, _))| SendError(value))
    }

    /// Attempts to immediately send a message on this `Sender`.
    pub fn try_send(&self, value: T) -> Result<(), TrySendError<T>> {
        let permit = match self.semaphore.clone().try_acquire_owned() {
            Ok(permit) => permit,

            Err(TryAcquireError::NoPermits) => {
                return Err(TrySendError::Full(value));
            }

            Err(TryAcquireError::Closed) => panic!("Semaphore unexpectedly closed"),
        };

        self.inner
            .send((value, permit))
            .map_err(|SendError((value, _))| TrySendError::Closed(value))
    }
}

impl<T> Receiver<T> {
    /// Receives the next value for this receiver.
    pub async fn recv(&mut self) -> Option<T> {
        let (value, permit) = self.inner.recv().await?;

        if self.debt > 0 {
            permit.forget();
            self.debt -= 1;
        }

        Some(value)
    }

    /// Returns the current target capacity of the channel.
    pub fn size(&self) -> usize {
        self.size
    }

    /// Resizes the channel to the target capacity.
    ///
    /// If there are currently more messages in the channel than the desired total, then the channel
    /// must be drained below the new target size before new messages can enter the channel.
    pub fn resize(&mut self, new_size: usize) {
        if new_size == self.size {
            return;
        }

        let total_permits = self.size + self.debt;

        if new_size > total_permits {
            self.semaphore.add_permits(new_size - total_permits);
            self.debt = 0;
        } else if new_size < total_permits {
            // Drain any leftover
            let excess = total_permits - new_size;
            let to_acquire = std::cmp::min(self.semaphore.available_permits(), excess);
            let permit = self.semaphore.try_acquire_many(to_acquire as u32).unwrap();
            permit.forget();

            // Set new debt
            self.debt = excess - to_acquire;
        } else {
            self.debt = 0;
        }

        self.size = new_size;
    }
}

#[cfg(test)]
mod tests {
    use super::resizable;

    #[tokio::test]
    async fn push_pop_two() {
        let (tx, mut rx) = resizable(2);
        tx.send(1).await.unwrap();
        tx.send(2).await.unwrap();

        assert_eq!(1, rx.recv().await.unwrap());
        assert_eq!(2, rx.recv().await.unwrap());
    }

    #[tokio::test]
    async fn try_push_pop_two() {
        let (tx, mut rx) = resizable(2);
        tx.try_send(1).unwrap();
        tx.try_send(2).unwrap();

        assert_eq!(1, rx.recv().await.unwrap());
        assert_eq!(2, rx.recv().await.unwrap());
    }

    #[tokio::test]
    #[should_panic]
    async fn test_limit() {
        let (tx, rx) = resizable(1);
        tx.try_send(1).unwrap();
        tx.try_send(2).unwrap();
    }

    #[tokio::test]
    async fn test_bounded() {
        let (tx, mut rx) = resizable(1);

        tokio::spawn(async move {
            assert_eq!(1, rx.recv().await.unwrap());
            assert_eq!(2, rx.recv().await.unwrap());
        });

        tx.send(1).await.unwrap();
        tx.send(2).await.unwrap();
    }

    #[tokio::test]
    async fn test_shrink() {
        let (tx, mut rx) = resizable(3);

        tx.try_send(1).unwrap();
        tx.try_send(2).unwrap();
        tx.try_send(3).unwrap();

        rx.resize(2);
        assert_eq!(2, rx.size);
        assert_eq!(1, rx.debt);

        assert_eq!(1, rx.recv().await.unwrap());
        assert_eq!(0, rx.debt); // Debt now repaid

        assert_eq!(2, rx.recv().await.unwrap());

        tx.try_send(4).unwrap();

        assert_eq!(3, rx.recv().await.unwrap());
        assert_eq!(4, rx.recv().await.unwrap());
    }

    #[tokio::test]
    #[should_panic]
    async fn test_shrink_new_limit() {
        let (tx, mut rx) = resizable(3);

        tx.try_send(1).unwrap();
        tx.try_send(2).unwrap();
        tx.try_send(3).unwrap();

        rx.resize(2);

        assert_eq!(1, rx.recv().await.unwrap());
        assert_eq!(2, rx.recv().await.unwrap());

        tx.try_send(4).unwrap();
        tx.try_send(5).unwrap();
    }

    #[tokio::test]
    async fn test_repay_debt() {
        let (tx, mut rx) = resizable(3);
        tx.try_send(1).unwrap();
        tx.try_send(2).unwrap();
        tx.try_send(3).unwrap();

        rx.resize(2);
        assert_eq!(2, rx.size);
        assert_eq!(1, rx.debt);
    }
}
