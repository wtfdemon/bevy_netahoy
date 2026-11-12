//! When the server's WebTransport certificate is replaced, apart from the certificate itself so the
//! timing has tests.
//!
//! Whether a certificate is due is read on the wall clock, by which every client checks its end:
//! an [`Instant`] stands still while the host sleeps, and a server woken after its certificate
//! ended would go on offering it.
//!
//! A client asks for the offer, then connects, trusting the certificates the offer listed. So a
//! new certificate isn't presented the moment it is made: for [`OVERLAP`] handshakes still get the
//! old one while offers list both, and a client that asked just before the renewal still meets the
//! certificate it was told of. That wait is time passed, on an [`Instant`].

use std::time::{Duration, Instant, SystemTime};

/// A certificate with this little left is replaced when a client next asks for the offer.
pub(super) const RENEW_BEFORE: Duration = Duration::from_secs(24 * 3600);

/// How long handshakes still get the old certificate after a renewal, while offers list both: far
/// longer than a client takes from its ask to its handshake (5 s for the answer at most, then 5 s
/// for the handshake).
pub(super) const OVERLAP: Duration = Duration::from_secs(60);

/// The certificate handshakes get, and after a renewal the one that takes over from it.
#[derive(Debug)]
pub(super) struct Rotation<C> {
    presented: Dated<C>,
    /// The next one, and when handshakes start getting it.
    next: Option<(Dated<C>, Instant)>,
}

#[derive(Debug)]
struct Dated<C> {
    certificate: C,
    ends: SystemTime,
}

impl<C> Rotation<C> {
    /// Starts on `certificate`, valid until `ends`.
    pub(super) fn new(certificate: C, ends: SystemTime) -> Self {
        Self {
            presented: Dated { certificate, ends },
            next: None,
        }
    }

    /// The certificates an offer lists at `now`, the wall clock reading `wall`: the presented one,
    /// and the next while it waits to take over. A presented one with [`RENEW_BEFORE`] or less
    /// left first gets its successor from `make` (the certificate and its end). One already past
    /// its end is replaced at once: no handshake can use it.
    pub(super) fn offered(
        &mut self,
        wall: SystemTime,
        now: Instant,
        make: impl FnOnce() -> (C, SystemTime),
    ) -> impl Iterator<Item = &C> {
        self.settle(now);
        if self.next.is_none() && wall + RENEW_BEFORE >= self.presented.ends {
            let (certificate, ends) = make();
            let from = if wall >= self.presented.ends {
                now
            } else {
                now + OVERLAP
            };
            self.next = Some((Dated { certificate, ends }, from));
            self.settle(now);
        }
        std::iter::once(&self.presented.certificate)
            .chain(self.next.as_ref().map(|(next, _)| &next.certificate))
    }

    /// The certificate a handshake at `now` gets.
    pub(super) fn presented(&mut self, now: Instant) -> &C {
        self.settle(now);
        &self.presented.certificate
    }

    /// The next certificate takes over once its time has come.
    fn settle(&mut self, now: Instant) {
        if self.next.as_ref().is_some_and(|(_, from)| now >= *from)
            && let Some((next, _)) = self.next.take()
        {
            self.presented = next;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: Duration = Duration::from_secs(14 * 24 * 3600);
    const SECOND: Duration = Duration::from_secs(1);

    /// Both clocks, `after` from a start shared by every test's first certificate.
    #[derive(Clone, Copy)]
    struct Clock {
        wall: SystemTime,
        now: Instant,
    }

    impl Clock {
        fn start() -> Self {
            Self {
                wall: SystemTime::UNIX_EPOCH + VALID,
                now: Instant::now(),
            }
        }

        fn after(self, passed: Duration) -> Self {
            Self {
                wall: self.wall + passed,
                now: self.now + passed,
            }
        }

        fn before(self, passed: Duration) -> Self {
            Self {
                wall: self.wall - passed,
                now: self.now - passed,
            }
        }
    }

    fn rotation() -> (Rotation<&'static str>, Clock) {
        let start = Clock::start();
        (Rotation::new("old", start.wall + VALID), start)
    }

    /// What an ask at `at` is offered; a renewal makes "new".
    fn offered(rotation: &mut Rotation<&'static str>, at: Clock) -> Vec<&'static str> {
        let make = || ("new", at.wall + VALID);
        rotation.offered(at.wall, at.now, make).copied().collect()
    }

    fn not_made() -> (&'static str, SystemTime) {
        panic!("nothing is due for renewal")
    }

    #[test]
    fn a_fresh_certificate_is_offered_alone() {
        let (mut rotation, start) = rotation();
        let offer: Vec<_> = rotation
            .offered(start.wall, start.now, not_made)
            .copied()
            .collect();
        assert_eq!(offer, ["old"]);
        let almost = start.after(VALID - RENEW_BEFORE - SECOND);
        assert_eq!(
            rotation.offered(almost.wall, almost.now, not_made).count(),
            1
        );
        assert_eq!(*rotation.presented(almost.now), "old");
    }

    #[test]
    fn a_renewal_offers_both_while_handshakes_still_get_the_old_one() {
        let (mut rotation, start) = rotation();
        let renewal = start.after(VALID - RENEW_BEFORE);
        assert_eq!(offered(&mut rotation, renewal), ["old", "new"]);
        assert_eq!(*rotation.presented(renewal.now), "old");
        let late = renewal.after(OVERLAP - SECOND);
        assert_eq!(*rotation.presented(late.now), "old");
        // One renewal at a time: a second ask in the overlap gets the same two.
        let again: Vec<_> = rotation
            .offered(late.wall, late.now, not_made)
            .copied()
            .collect();
        assert_eq!(again, ["old", "new"]);
    }

    #[test]
    fn after_the_overlap_the_new_one_alone() {
        let (mut rotation, start) = rotation();
        let renewal = start.after(VALID - RENEW_BEFORE);
        offered(&mut rotation, renewal);
        let after = renewal.after(OVERLAP);
        assert_eq!(*rotation.presented(after.now), "new");
        let offer: Vec<_> = rotation
            .offered(after.wall, after.now, not_made)
            .copied()
            .collect();
        assert_eq!(offer, ["new"]);
    }

    #[test]
    fn a_client_that_asked_just_before_a_renewal_meets_its_certificate() {
        let (mut rotation, start) = rotation();
        let renewal = start.after(VALID - RENEW_BEFORE);
        let first = offered(&mut rotation, renewal.before(SECOND));
        // Another client's ask renews it, a moment before the first one's handshake.
        let second = offered(&mut rotation, renewal);
        assert!(first.contains(rotation.presented(renewal.after(SECOND).now)));
        assert!(second.contains(rotation.presented(renewal.after(SECOND).now)));
        // The second client's handshake after the overlap meets one of its two too.
        let later = renewal.after(OVERLAP + SECOND);
        assert!(second.contains(rotation.presented(later.now)));
    }

    #[test]
    fn one_past_its_end_is_replaced_at_once() {
        let (mut rotation, start) = rotation();
        let expired = start.after(VALID + SECOND);
        assert_eq!(offered(&mut rotation, expired), ["new"]);
        assert_eq!(*rotation.presented(expired.now), "new");
    }

    #[test]
    fn the_renewal_reads_the_wall_clock() {
        // The host slept through the certificate's life: the wall clock moved on, the Instant
        // didn't.
        let (mut slept, start) = rotation();
        let woken = Clock {
            wall: start.wall + VALID + SECOND,
            now: start.now,
        };
        assert_eq!(offered(&mut slept, woken), ["new"]);
        // And a day before its end by the wall clock, however little time the Instant counted.
        let (mut due, start) = rotation();
        let day_left = Clock {
            wall: start.wall + VALID - RENEW_BEFORE,
            now: start.now,
        };
        assert_eq!(offered(&mut due, day_left), ["old", "new"]);
    }
}
