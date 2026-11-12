//! The offer endpoint: plain HTTP, one JSON [`Offer`] per request, on threads of its own so a
//! slow client never holds the game loop.

use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use bevy::log::warn;

use super::certificate::Certificates;
use crate::transport::Offer;

/// How long one request may take, from its accept to the answer. Each has its own thread, so a
/// slow client holds only its own, and only this long.
const OFFER_DEADLINE: Duration = Duration::from_secs(2);

/// Requests answered at once. One more is closed unanswered: a flood of connections that send
/// nothing would otherwise take a thread each, up to every thread the process may have.
const MAX_OFFER_REQUESTS: usize = 16;

/// The longest request read; a GET is a few hundred bytes.
const MAX_REQUEST_BYTES: usize = 8 * 1024;

/// What the offer endpoint answers from.
pub(super) struct Offers {
    pub(super) certificates: Arc<Certificates>,
    /// The WebTransport listener's UDP port.
    pub(super) port: u16,
    /// This run's number.
    pub(super) boot: u64,
    /// The server's tick, written by its loop: a frozen loop leaves it standing.
    pub(super) tick: Arc<AtomicU64>,
}

impl Offers {
    /// The offer now (and a renewal first, when one is due).
    fn offer(&self) -> Offer {
        Offer {
            hashes: self.certificates.offer_hashes(),
            port: self.port,
            boot: self.boot,
            tick: self.tick.load(Ordering::Relaxed),
        }
    }
}

/// Answers every request on `listener` with the [`Offer`] as JSON, on a thread of its own. Open
/// to any origin: none of it is secret, and a page may be served from another port. Each request
/// has a thread of its own, and at most [`MAX_OFFER_REQUESTS`] are answered at once.
pub(super) fn serve(listener: TcpListener, offers: Arc<Offers>) -> std::io::Result<()> {
    let busy = Arc::new(AtomicUsize::new(0));
    let accept = move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            // Past the cap the stream drops here, which closes it unanswered.
            let Some(slot) = OfferSlot::take(&busy) else {
                continue;
            };
            let offers = offers.clone();
            // An error drops the closure, and with it this connection and its slot.
            let answer = move || {
                let _slot = slot;
                answer_offer(stream, &offers);
            };
            if let Err(err) = thread::Builder::new().name("offer".into()).spawn(answer) {
                warn!("webtransport offer: no thread for a request: {err}");
            }
        }
    };
    thread::Builder::new()
        .name("offer listener".into())
        .spawn(accept)
        .map(drop)
}

/// One of the [`MAX_OFFER_REQUESTS`] a request holds while it is answered; dropping it frees it.
struct OfferSlot(Arc<AtomicUsize>);

impl OfferSlot {
    /// A free slot of `busy`'s, if there is one.
    fn take(busy: &Arc<AtomicUsize>) -> Option<Self> {
        let slot = Self(busy.clone());
        // Dropped at once when there was none: that gives the count back.
        (busy.fetch_add(1, Ordering::AcqRel) < MAX_OFFER_REQUESTS).then_some(slot)
    }
}

impl Drop for OfferSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Reads the request whole, then answers the offer.
fn answer_offer(mut stream: TcpStream, offers: &Offers) {
    let deadline = Instant::now() + OFFER_DEADLINE;
    // The whole request first: closing on unread bytes resets the connection, which can lose
    // the answer.
    let mut request = Vec::new();
    let mut chunk = [0; 1024];
    while !request.ends_with(b"\r\n\r\n") && request.len() < MAX_REQUEST_BYTES {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() || stream.set_read_timeout(Some(left)).is_err() {
            break;
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => request.extend_from_slice(&chunk[..n]),
        }
    }
    let body = serde_json::to_string(&offers.offer()).expect("an offer serializes");
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slot_is_given_back_when_its_request_is_done() {
        let busy = Arc::new(AtomicUsize::new(0));
        let slots: Vec<_> = (0..MAX_OFFER_REQUESTS)
            .map(|_| OfferSlot::take(&busy).expect("a free slot"))
            .collect();
        assert!(OfferSlot::take(&busy).is_none());
        assert_eq!(busy.load(Ordering::Acquire), MAX_OFFER_REQUESTS);
        drop(slots);
        assert_eq!(busy.load(Ordering::Acquire), 0);
        assert!(OfferSlot::take(&busy).is_some());
    }

    /// The endpoint and a native client's ask, end to end over loopback.
    #[cfg(feature = "transport-client")]
    #[test]
    fn a_client_reads_the_offer_the_endpoint_answers() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let offers = Arc::new(Offers {
            certificates: Arc::new(Certificates::new()),
            port: 5001,
            boot: 7,
            tick: Arc::new(AtomicU64::new(1234)),
        });
        serve(listener, offers.clone()).unwrap();
        let url = format!("http://{addr}/");
        let offer =
            bevy::tasks::block_on(crate::transport::client::fetch::fetch_offer(url)).unwrap();
        assert_eq!(offer, offers.offer());
        assert_eq!((offer.port, offer.boot, offer.tick), (5001, 7, 1234));
        assert_eq!(offer.hashes.len(), 1);
    }
}
