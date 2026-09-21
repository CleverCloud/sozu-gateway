//! Coalesce ordinary watch events without delaying endpoint availability.

use std::time::Duration;

use tokio::sync::mpsc;

/// Settle a rebuild after its first notice has already been received. An
/// ordinary change debounces (an endpoint notice cuts the debounce short); an
/// endpoint notice does not. Then drain whatever else is queued.
///
/// The caller receives that first notice **in its own `select!`** —
/// `mpsc::Receiver::recv` is cancellation-safe, so a competing timer that wins
/// the race removes no message — and calls this only once it has committed to a
/// rebuild. Composing the recv and the debounce into one future awaited as a
/// single `select!` arm instead lets a sibling timer cancel it *after* it
/// consumed a notice but during the debounce, dropping that notice; the run
/// loop's Sōzu-liveness tick would then never reconcile it.
pub async fn settle(
    changes: &mut mpsc::Receiver<()>,
    endpoints: &mut mpsc::Receiver<()>,
    debounce: Duration,
    first_was_endpoint: bool,
) {
    if !first_was_endpoint {
        tokio::select! {
            _ = endpoints.recv() => {}
            _ = tokio::time::sleep(debounce) => {}
        }
    }
    drain_pending(changes);
    drain_pending(endpoints);
}

/// Wait for one rebuild on a single channel, for loops with no endpoint
/// priority. Await this *in* a `select!` head so resync and shutdown stay
/// selectable while ordinary events settle.
pub async fn debounced(changes: &mut mpsc::Receiver<()>, debounce: Duration) -> Option<()> {
    changes.recv().await?;
    tokio::time::sleep(debounce).await;
    drain_pending(changes);
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

    /// The run loop's recv-then-`settle` composition, as one awaitable, so the
    /// debounce behaviour can be tested as a unit. Production code inlines this
    /// in its `select!` so the first recv stays cancellation-safe.
    async fn next(
        changes: &mut mpsc::Receiver<()>,
        endpoints: &mut mpsc::Receiver<()>,
        debounce: Duration,
    ) -> Option<()> {
        let first_was_endpoint = tokio::select! {
            changed = endpoints.recv() => { changed?; true }
            changed = changes.recv() => { changed?; false }
        };
        settle(changes, endpoints, debounce, first_was_endpoint).await;
        Some(())
    }

    /// The regression guard for the run loop's cancel-safety: when a competing
    /// timer (the Sōzu-liveness or resync tick) wins the race against the raw
    /// `recv`, the pending change must stay in the channel for the next
    /// iteration, not be consumed and dropped. `mpsc::Receiver::recv` guarantees
    /// this; composing recv+debounce into one cancelled future did not.
    #[tokio::test(start_paused = true)]
    async fn a_timer_winning_the_race_does_not_consume_a_pending_change() {
        let (tx, mut rx) = mpsc::channel::<()>(64);
        tx.try_send(()).unwrap();
        // A ready timer competes with the change recv, biased to the timer so
        // it always wins — exactly the loss window the old composed future had.
        tokio::select! {
            biased;
            _ = tokio::time::sleep(Duration::ZERO) => {}
            changed = rx.recv() => panic!("recv should have lost the race: {changed:?}"),
        }
        // The change survived the cancelled recv and is still deliverable.
        assert_eq!(rx.try_recv(), Ok(()));
    }

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
    async fn a_single_channel_debounce_bounds_its_drain_and_ends_when_closed() {
        let (tx, mut rx) = mpsc::channel(64);
        for _ in 0..64 {
            tx.try_send(()).unwrap();
        }
        let started = Instant::now();
        assert_eq!(debounced(&mut rx, DEBOUNCE).await, Some(()));
        assert_eq!(started.elapsed(), DEBOUNCE);
        assert!(rx.is_empty());
        drop(tx);
        assert_eq!(debounced(&mut rx, DEBOUNCE).await, None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_single_channel_debounce_stays_interruptible() {
        let (tx, mut rx) = mpsc::channel(64);
        tx.try_send(()).unwrap();
        // Shutdown and resync must not wait out the debounce.
        tokio::select! {
            _ = debounced(&mut rx, DEBOUNCE) => panic!("debounce finished too soon"),
            _ = tokio::time::sleep(Duration::from_millis(100)) => {}
        }
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
