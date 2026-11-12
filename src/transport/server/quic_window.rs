//! The server's congestion controller for WebTransport: one fixed window.
//!
//! The server sends each player a few kB/s of state that is stale within a tick, so holding a
//! datagram back to be polite to a link is never what it wants: the datagram arrives late, and
//! quinn drops the oldest once its send buffer is full. So the window never reacts to loss, ECN or
//! RTT. What it does bound is how much can be unacknowledged, and with it quinn's pacer
//! (1.25 x window / RTT).
//!
//! quinn's BBR was close to this, but only until a burst: an app-limited sender stays in its
//! startup phase, and the first ack quinn sees as not app-limited (a join burst) starts its RTT
//! probe, which drops the window to about 4 packets with far more than that in flight, and the
//! player gets nothing until enough of it is acked. Measured at 16 players joining in waves: 99
//! such drops in 9 runs, none with this window.
//!
//! This does not protect a link too slow for the game: nothing here slows down for one.

use std::{any::Any, sync::Arc, time::Instant};

use wtransport::quinn::congestion::{Controller, ControllerFactory};

/// 200 packets of 1200 B, BBR's startup window. At a busy game's 14 kB/s per player it takes 17 s
/// to fill, past the 10 s idle timeout, so a blackout never parks fresh state behind it.
pub(super) const WINDOW: u64 = 200 * 1200;

/// [`Controller`] and its factory in one: the state is one number.
#[derive(Clone, Copy, Debug)]
pub(super) struct FixedWindow {
    window: u64,
}

impl FixedWindow {
    /// A window of `window` bytes.
    pub(super) fn new(window: u64) -> Self {
        Self { window }
    }
}

impl Default for FixedWindow {
    /// [`WINDOW`].
    fn default() -> Self {
        Self::new(WINDOW)
    }
}

impl ControllerFactory for FixedWindow {
    fn build(self: Arc<Self>, _now: Instant, _current_mtu: u16) -> Box<dyn Controller> {
        Box::new(*self)
    }
}

impl Controller for FixedWindow {
    fn on_congestion_event(
        &mut self,
        _now: Instant,
        _sent: Instant,
        _is_persistent_congestion: bool,
        _lost_bytes: u64,
    ) {
    }

    fn on_mtu_update(&mut self, _new_mtu: u16) {}

    fn window(&self) -> u64 {
        self.window
    }

    fn clone_box(&self) -> Box<dyn Controller> {
        Box::new(*self)
    }

    fn initial_window(&self) -> u64 {
        self.window
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn controller() -> Box<dyn Controller> {
        Arc::new(FixedWindow::default()).build(Instant::now(), 1200)
    }

    #[test]
    fn nothing_moves_the_window() {
        let mut cc = controller();
        let start = Instant::now();
        for packet in 0..10_000u64 {
            let now = start + Duration::from_millis(packet);
            cc.on_sent(now, 1200, packet);
            cc.on_end_acks(now, packet * 1200, packet % 2 == 0, Some(packet));
            // Every kind of bad news, a persistent congestion event too (Cubic and NewReno fall
            // to their minimum on one).
            cc.on_congestion_event(now, now, packet % 7 == 0, 1200 * (packet % 50));
            cc.on_mtu_update(1200 + (packet % 300) as u16);
            assert_eq!(cc.window(), WINDOW);
        }
        assert_eq!(cc.initial_window(), WINDOW);
    }

    /// quinn clones a controller when a connection changes path.
    #[test]
    fn a_clone_keeps_the_window() {
        assert_eq!(controller().clone_box().window(), WINDOW);
    }

    #[test]
    fn the_window_is_the_one_asked_for() {
        let cc = Arc::new(FixedWindow::new(64_000)).build(Instant::now(), 1200);
        assert_eq!(cc.window(), 64_000);
        assert_eq!(cc.initial_window(), 64_000);
    }
}
