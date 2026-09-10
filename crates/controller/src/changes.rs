//! Coalesce ordinary watch events without delaying endpoint availability.

use std::time::Duration;

use tokio::sync::mpsc;

/// Wait for one rebuild. Endpoint changes interrupt an ordinary debounce;
/// they also remain pending when the reconcile loop is busy applying state.
/// All notices consumed here precede the next cache snapshot. Notices arriving
/// after this function returns remain queued for the following rebuild.
pub async fn next(
    changes: &mut mpsc::Receiver<()>,
    endpoints: &mut mpsc::Receiver<()>,
    debounce: Duration,
) -> Option<()> {
    tokio::select! {
        changed = endpoints.recv() => { changed?; }
        changed = changes.recv() => {
            changed?;
            tokio::select! {
                changed = endpoints.recv() => { changed?; }
                _ = tokio::time::sleep(debounce) => {}
            }
        }
    }

    drain_pending(changes);
    drain_pending(endpoints);
    Some(())
}

fn drain_pending(rx: &mut mpsc::Receiver<()>) {
    // Bound the drain even if producers keep filling the channel. Rebuilding
    // the latest cache includes these changes; draining an unbounded stream
    // could instead prevent any rebuild from starting.
    for _ in 0..rx.len() {
        if rx.try_recv().is_err() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::poll;
    use tokio::time::{advance, timeout, Instant};

    const DEBOUNCE: Duration = Duration::from_millis(500);

    #[tokio::test(start_paused = true)]
    async fn ordinary_changes_keep_the_debounce() {
        let (tx, mut rx) = mpsc::channel(64);
        let (_endpoint_tx, mut endpoint_rx) = mpsc::channel(1);
        tx.try_send(()).unwrap();
        let waiting = next(&mut rx, &mut endpoint_rx, DEBOUNCE);
        tokio::pin!(waiting);
        assert!(poll!(waiting.as_mut()).is_pending());
        advance(Duration::from_millis(499)).await;
        assert!(poll!(waiting.as_mut()).is_pending());
        advance(Duration::from_millis(1)).await;
        assert_eq!(waiting.await, Some(()));
    }

    #[tokio::test(start_paused = true)]
    async fn endpoints_wake_immediately_with_a_full_ordinary_queue() {
        let (tx, mut rx) = mpsc::channel(64);
        let (endpoint_tx, mut endpoint_rx) = mpsc::channel(1);
        for _ in 0..64 {
            tx.try_send(()).unwrap();
        }
        assert!(tx.try_send(()).is_err());
        endpoint_tx.try_send(()).unwrap();
        let started = Instant::now();
        assert_eq!(next(&mut rx, &mut endpoint_rx, DEBOUNCE).await, Some(()));
        assert_eq!(started.elapsed(), Duration::ZERO);
        assert!(rx.is_empty());
        assert!(endpoint_rx.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn endpoints_interrupt_an_in_progress_debounce() {
        let (tx, mut rx) = mpsc::channel(64);
        let (endpoint_tx, mut endpoint_rx) = mpsc::channel(1);
        tx.try_send(()).unwrap();
        let started = Instant::now();
        let waiting = next(&mut rx, &mut endpoint_rx, DEBOUNCE);
        tokio::pin!(waiting);
        assert!(poll!(waiting.as_mut()).is_pending());
        advance(Duration::from_millis(120)).await;
        endpoint_tx.try_send(()).unwrap();
        assert_eq!(waiting.await, Some(()));
        assert_eq!(started.elapsed(), Duration::from_millis(120));
    }

    #[tokio::test(start_paused = true)]
    async fn endpoint_bursts_coalesce_and_do_not_spin_afterward() {
        let (_tx, mut rx) = mpsc::channel(64);
        let (endpoint_tx, mut endpoint_rx) = mpsc::channel(1);
        endpoint_tx.try_send(()).unwrap();
        for _ in 0..100 {
            assert!(endpoint_tx.try_send(()).is_err());
        }
        assert_eq!(next(&mut rx, &mut endpoint_rx, DEBOUNCE).await, Some(()));
        assert!(
            timeout(DEBOUNCE * 2, next(&mut rx, &mut endpoint_rx, DEBOUNCE))
                .await
                .is_err()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn endpoint_change_during_apply_survives_for_the_next_rebuild() {
        let (tx, mut rx) = mpsc::channel(64);
        let (endpoint_tx, mut endpoint_rx) = mpsc::channel(1);
        tx.try_send(()).unwrap();
        assert_eq!(next(&mut rx, &mut endpoint_rx, DEBOUNCE).await, Some(()));

        // The loop has taken its cache snapshot and is busy applying it.
        // A later endpoint change cannot be drained as part of that snapshot.
        endpoint_tx.try_send(()).unwrap();
        advance(Duration::from_millis(200)).await;
        let applied_at = Instant::now();
        assert_eq!(next(&mut rx, &mut endpoint_rx, DEBOUNCE).await, Some(()));
        assert_eq!(applied_at.elapsed(), Duration::ZERO);
        assert!(endpoint_rx.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn ordinary_bursts_do_not_extend_the_deadline() {
        let (tx, mut rx) = mpsc::channel(64);
        let (_endpoint_tx, mut endpoint_rx) = mpsc::channel(1);
        tx.try_send(()).unwrap();
        let started = Instant::now();
        let waiting = next(&mut rx, &mut endpoint_rx, DEBOUNCE);
        tokio::pin!(waiting);
        assert!(poll!(waiting.as_mut()).is_pending());
        for _ in 0..10 {
            advance(Duration::from_millis(49)).await;
            tx.try_send(()).unwrap();
            assert!(poll!(waiting.as_mut()).is_pending());
        }
        advance(Duration::from_millis(10)).await;
        assert_eq!(waiting.await, Some(()));
        assert_eq!(started.elapsed(), DEBOUNCE);
    }

    #[tokio::test(start_paused = true)]
    async fn the_outer_loop_can_interrupt_a_debounce() {
        let (tx, mut rx) = mpsc::channel(64);
        let (endpoint_tx, mut endpoint_rx) = mpsc::channel(1);
        tx.try_send(()).unwrap();
        // Resync and shutdown remain selectable while ordinary events settle.
        tokio::select! {
            _ = next(&mut rx, &mut endpoint_rx, DEBOUNCE) => panic!("debounce finished too soon"),
            _ = tokio::time::sleep(Duration::from_millis(100)) => {}
        }
        endpoint_tx.try_send(()).unwrap();
        let started = Instant::now();
        assert_eq!(next(&mut rx, &mut endpoint_rx, DEBOUNCE).await, Some(()));
        assert_eq!(started.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn a_closed_channel_ends_the_wait() {
        let (tx, mut rx) = mpsc::channel(64);
        let (endpoint_tx, mut endpoint_rx) = mpsc::channel(1);
        drop(tx);
        drop(endpoint_tx);
        assert_eq!(next(&mut rx, &mut endpoint_rx, DEBOUNCE).await, None);
    }
}
