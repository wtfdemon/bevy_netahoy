//! Which transport a client connects over: WebTransport first, WebSocket where it fails. The
//! rules live here, apart from the sockets, so they have tests; the client feeds them what it
//! knows and acts on the answer.
//!
//! A failed handshake says UDP doesn't get through, so WebSocket from then on. A session that
//! ended mostly says nothing about UDP: a restarted server or a kick ends it on a good path, and
//! WebSocket costs a lossy link far more rollbacks. So the next connect asks the server for its
//! offer again (a restarted server has a new certificate) and goes back to WebTransport, unless
//! UDP alone died: while the session heard nothing, the same run of the server answered its offer
//! over HTTP, its tick moved on ([`Silence`], [`Answer`]). A whole link going down can't look like
//! that: the ask goes unanswered too. Nor can a frozen server: its tick stands still.
//!
//! Giving WebTransport up is only ever read when a connect is planned: a WebSocket session runs
//! until it ends, however long, and nothing moves a running session to another transport. So once
//! UDP has died the client reconnects over WebSocket and stays on that session until it ends; a
//! reconnect [`GIVE_UP_SECS`] or more after the WebTransport session ended tries WebTransport
//! again, an earlier one WebSocket.

use super::Offer;

/// Which transports a client connects over.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TransportChoice {
    /// WebTransport first, WebSocket when it fails or UDP alone dies.
    #[default]
    Auto,
    /// WebTransport only: a failure tries it again, so a test never measures the other one.
    WebTransport,
    /// WebSocket only.
    WebSocket,
}

impl TransportChoice {
    /// `TRANSPORT`: `auto` (or unset), `wt` or `ws`, in any case. Panics on anything else.
    #[cfg(not(target_family = "wasm"))]
    pub fn from_env() -> Self {
        Self::from_setting(std::env::var("TRANSPORT").ok().as_deref())
    }

    /// The page's `?transport=`: `auto` (or none), `wt` or `ws`, in any case. Panics on anything
    /// else.
    #[cfg(target_family = "wasm")]
    pub fn from_page() -> Self {
        let search = web_sys::window().and_then(|window| window.location().search().ok());
        Self::from_setting(
            search
                .as_deref()
                .and_then(|search| query_param(search, "transport")),
        )
    }

    /// The choice `value` names, [`Self::Auto`] when unset; panics on anything else.
    fn from_setting(value: Option<&str>) -> Self {
        match value
            .map(|value| value.trim().to_ascii_lowercase())
            .as_deref()
        {
            None | Some("" | "auto") => Self::Auto,
            Some("wt" | "webtransport") => Self::WebTransport,
            Some("ws" | "websocket") => Self::WebSocket,
            Some(other) => panic!("TRANSPORT={other:?}: expected auto, wt or ws"),
        }
    }
}

/// The value of the query parameter `name` in a page's `search` (`?a=1&b=2`), if it has one.
#[cfg(any(target_family = "wasm", test))]
fn query_param<'a>(search: &'a str, name: &str) -> Option<&'a str> {
    search
        .trim_start_matches('?')
        .split('&')
        .find_map(|pair| pair.strip_prefix(name)?.strip_prefix('='))
}

/// An ask for the offer gets this long (s) to be answered; one still out after that counts as no
/// offer, and the client goes on without it at once rather than at its next reconnect attempt.
pub(super) const ASK_WAIT_SECS: f64 = 5.0;

/// After a WebTransport session ends, a server that answers no offer is asked again for this
/// long (s): a restarting server answers nothing yet, and connecting the moment its WebSocket is
/// up would land the client there.
pub(super) const OFFER_WAIT_SECS: f64 = 10.0;

/// UDP alone died, or a handshake failed after one worked: reconnects within this long (s) go over
/// WebSocket, so a link that keeps blacking out doesn't pay a failed handshake on every reconnect.
/// It doesn't end the WebSocket session: the client stays on that until it ends, and only a
/// reconnect after this long tries WebTransport again.
pub(super) const GIVE_UP_SECS: f64 = 60.0;

/// A session that has heard nothing for this long (s) has gone quiet: the client asks the server
/// for its offer, to learn whether HTTP still gets through.
pub(super) const QUIET_SECS: f64 = 2.0;

/// After the server answered over HTTP, the session must hear nothing for this long (s) more
/// before UDP counts as dead: a link back from an outage can answer HTTP a moment before the
/// session hears again.
pub(super) const STILL_QUIET_SECS: f64 = 1.0;

/// An answer's tick must be this far (s of ticks) past the newest one the session heard for the
/// server to count as running. One that runs is a whole [`QUIET_SECS`] on by the time it answers;
/// one that froze, only by the packets lost just before it did.
pub(super) const MOVED_ON_SECS: f64 = 1.0;

/// What the server's answer to a silent session's ask for the offer says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Answer {
    /// None: the ask failed. A whole link going down loses it too, so nothing is learned.
    Unanswered,
    /// Another run of the server answered: it restarted, which ends a session on a good path.
    Restarted,
    /// The same run, its tick short of [`MOVED_ON_SECS`] past the newest the session heard: its
    /// loop stands still, and the silence isn't UDP's doing.
    Frozen,
    /// The same run, its tick moved on: it runs, and HTTP reaches it while the session hears
    /// nothing.
    Running,
}

impl Answer {
    /// Reads `offer` (none: the ask failed) for a session on the server's run `boot` whose newest
    /// tick heard is `heard_tick`, at `tick_hz` ticks a second. The certificates don't count: a
    /// renewal changes them on a server that runs on.
    pub(super) fn read(offer: Option<&Offer>, boot: u64, heard_tick: u64, tick_hz: f64) -> Self {
        let Some(offer) = offer else {
            return Self::Unanswered;
        };
        if offer.boot != boot {
            Self::Restarted
        } else if (offer.tick as f64) < heard_tick as f64 + MOVED_ON_SECS * tick_hz {
            Self::Frozen
        } else {
            Self::Running
        }
    }
}

/// What a WebTransport session's silence says about UDP. Once the session has heard nothing for
/// [`QUIET_SECS`] the client asks the server for its offer; an [`Answer::Running`], and
/// [`STILL_QUIET_SECS`] more of nothing, means UDP alone died. The library ends a silent session
/// only later, so the verdict is in by then.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Silence {
    packets: usize,
    quiet_for: f64,
    ask: SilentAsk,
}

/// The ask for the offer a silence started.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum SilentAsk {
    #[default]
    Unasked,
    /// Out, no answer yet.
    Asking,
    /// Answered, this far (s) into the silence.
    Answered(Answer, f64),
}

impl Silence {
    /// The session has received `packets` in all, `dt` (s) after the last call. True when the
    /// client should ask for the offer now, once per silence. A packet ends the silence, and with
    /// it any ask still out and its verdict.
    pub(super) fn hear(&mut self, packets: usize, dt: f64) -> bool {
        if packets != self.packets {
            *self = Self {
                packets,
                ..Self::default()
            };
            return false;
        }
        self.quiet_for += dt;
        if self.quiet_for >= QUIET_SECS && self.ask == SilentAsk::Unasked {
            self.ask = SilentAsk::Asking;
            return true;
        }
        false
    }

    /// The ask is out, waiting for [`Self::answered`].
    pub(super) fn asking(&self) -> bool {
        self.ask == SilentAsk::Asking
    }

    /// The ask's answer. Only the first counts.
    pub(super) fn answered(&mut self, answer: Answer) {
        if self.asking() {
            self.ask = SilentAsk::Answered(answer, self.quiet_for);
        }
    }

    /// The answer to this silence's ask, once it is in.
    #[cfg(test)]
    fn answer(&self) -> Option<Answer> {
        match self.ask {
            SilentAsk::Answered(answer, _) => Some(answer),
            _ => None,
        }
    }

    /// UDP alone died: the server answered over HTTP, the same run of it with its tick moved on,
    /// and the session has heard nothing since it went quiet, nor for [`STILL_QUIET_SECS`] after
    /// the answer.
    pub(super) fn udp_died(&self) -> bool {
        match self.ask {
            SilentAsk::Answered(Answer::Running, at) => self.quiet_for - at >= STILL_QUIET_SECS,
            _ => false,
        }
    }
}

/// The offer as the client holds it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum OfferState {
    /// Not asked since the last session ended.
    Unasked,
    /// Asked at this time (s), no answer yet.
    Asking(f64),
    /// The server offered its WebTransport listener.
    Offered,
    /// No offer: the server answered none, or the ask failed.
    NoOffer,
}

/// No WebTransport for now.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum GaveUp {
    /// The first handshake failed: UDP doesn't get through here.
    ForTheRun,
    /// Until this time (s): it worked before, so a connect planned after it gets another try.
    Until(f64),
}

impl GaveUp {
    fn active(self, now: f64) -> bool {
        match self {
            Self::ForTheRun => true,
            Self::Until(until) => now < until,
        }
    }
}

/// What a connect does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Plan {
    /// Ask the server for its offer, and connect once it answers.
    Ask,
    /// Open nothing: the answer that decides is on its way.
    Wait,
    /// Connect over WebTransport with the offer held.
    WebTransport,
    /// Connect over WebSocket.
    WebSocket,
}

/// The next connect's plan at `now` (s). `lost` is when the last WebTransport session ended (s),
/// if none has come up since.
pub(super) fn plan(
    now: f64,
    choice: TransportChoice,
    gave_up: Option<GaveUp>,
    lost: Option<f64>,
    offer: OfferState,
) -> Plan {
    match choice {
        TransportChoice::WebSocket => return Plan::WebSocket,
        TransportChoice::WebTransport => {
            return match offer {
                OfferState::Offered => Plan::WebTransport,
                OfferState::Asking(since) if now - since < ASK_WAIT_SECS => Plan::Wait,
                OfferState::Unasked | OfferState::Asking(_) | OfferState::NoOffer => Plan::Ask,
            };
        }
        TransportChoice::Auto => {}
    }
    if gave_up.is_some_and(|gave_up| gave_up.active(now)) {
        return Plan::WebSocket;
    }
    match offer {
        OfferState::Unasked => Plan::Ask,
        OfferState::Asking(since) if now - since < ASK_WAIT_SECS => Plan::Wait,
        // No answer in time is no offer.
        OfferState::Asking(_) | OfferState::NoOffer => match lost {
            Some(lost) if now - lost < OFFER_WAIT_SECS => Plan::Ask,
            _ => Plan::WebSocket,
        },
        OfferState::Offered => Plan::WebTransport,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: f64 = 100.0;
    const AUTO: TransportChoice = TransportChoice::Auto;
    /// A frame's time, exact in binary so the sums land on [`QUIET_SECS`].
    const FRAME: f64 = 1.0 / 64.0;

    /// The session's server: its run, the newest tick the session heard and its tick rate.
    const BOOT: u64 = 7;
    const HEARD: u64 = 1000;
    const HZ: f64 = 20.0;

    /// A session that has heard `packets` and then nothing until its ask went out.
    fn gone_quiet(packets: usize) -> Silence {
        let mut silence = Silence::default();
        assert!(!silence.hear(packets, FRAME));
        assert!(silence.hear(packets, QUIET_SECS));
        silence
    }

    /// An offer from the run `boot` at `tick`.
    fn offer(boot: u64, tick: u64) -> Offer {
        Offer {
            hashes: vec!["a".to_owned()],
            port: 5001,
            boot,
            tick,
        }
    }

    /// What the session reads in `offer`.
    fn read(offer: &Offer) -> Answer {
        Answer::read(Some(offer), BOOT, HEARD, HZ)
    }

    /// The tick a running server answers with: as far on as the silence before the ask.
    const RUNNING: u64 = HEARD + (QUIET_SECS * HZ) as u64;

    #[test]
    fn the_first_connect_asks_and_waits_for_the_answer() {
        assert_eq!(plan(NOW, AUTO, None, None, OfferState::Unasked), Plan::Ask);
        assert_eq!(
            plan(NOW, AUTO, None, None, OfferState::Asking(NOW)),
            Plan::Wait
        );
        assert_eq!(
            plan(NOW, AUTO, None, None, OfferState::Offered),
            Plan::WebTransport
        );
        assert_eq!(
            plan(NOW, AUTO, None, None, OfferState::NoOffer),
            Plan::WebSocket
        );
    }

    #[test]
    fn a_server_that_never_answers_gets_a_websocket() {
        let asked = OfferState::Asking(NOW);
        let almost = NOW + ASK_WAIT_SECS - FRAME;
        assert_eq!(plan(almost, AUTO, None, None, asked), Plan::Wait);
        assert_eq!(
            plan(NOW + ASK_WAIT_SECS, AUTO, None, None, asked),
            Plan::WebSocket
        );
    }

    #[test]
    fn an_unanswered_ask_counts_as_no_offer() {
        let lost = Some(NOW - 1.0);
        let asked = OfferState::Asking(NOW - ASK_WAIT_SECS);
        let none = OfferState::NoOffer;
        assert_eq!(
            plan(NOW, AUTO, None, lost, asked),
            plan(NOW, AUTO, None, lost, none)
        );
        assert_eq!(plan(NOW, AUTO, None, lost, asked), Plan::Ask);
        let late = NOW + OFFER_WAIT_SECS;
        assert_eq!(plan(late, AUTO, None, lost, asked), Plan::WebSocket);
    }

    #[test]
    fn a_failed_handshake_means_websocket_for_the_run() {
        let later = NOW + 1000.0;
        let gave_up = Some(GaveUp::ForTheRun);
        assert_eq!(
            plan(later, AUTO, gave_up, None, OfferState::Offered),
            Plan::WebSocket
        );
        // It never waits on an offer it won't use.
        assert_eq!(
            plan(later, AUTO, gave_up, Some(NOW), OfferState::Unasked),
            Plan::WebSocket
        );
    }

    #[test]
    fn a_session_that_ended_goes_back_to_webtransport() {
        assert_eq!(
            plan(NOW, AUTO, None, Some(NOW), OfferState::Unasked),
            Plan::Ask
        );
        assert_eq!(
            plan(NOW + 0.5, AUTO, None, Some(NOW), OfferState::Offered),
            Plan::WebTransport
        );
    }

    #[test]
    fn a_quiet_session_asks_once() {
        let mut silence = Silence::default();
        // The first call meets the packets so far: no silence yet.
        assert!(!silence.hear(5, 10.0));
        assert!(!silence.hear(5, QUIET_SECS - FRAME));
        assert!(silence.hear(5, FRAME));
        assert!(silence.asking());
        assert!(!silence.hear(5, 10.0));
    }

    #[test]
    fn an_answer_reads_the_run_and_the_tick() {
        assert_eq!(Answer::read(None, BOOT, HEARD, HZ), Answer::Unanswered);
        assert_eq!(read(&offer(BOOT + 1, RUNNING)), Answer::Restarted);
        // A restarted server's tick starts over, far behind the one heard.
        assert_eq!(read(&offer(BOOT + 1, 3)), Answer::Restarted);
        let moved_on = HEARD + (MOVED_ON_SECS * HZ) as u64;
        assert_eq!(read(&offer(BOOT, HEARD)), Answer::Frozen);
        assert_eq!(read(&offer(BOOT, moved_on - 1)), Answer::Frozen);
        assert_eq!(read(&offer(BOOT, moved_on)), Answer::Running);
        assert_eq!(read(&offer(BOOT, RUNNING)), Answer::Running);
        // A faster tick rate needs more ticks for the same time.
        assert_eq!(
            Answer::read(Some(&offer(BOOT, moved_on)), BOOT, HEARD, 60.0),
            Answer::Frozen
        );
    }

    #[test]
    fn udp_alone_died_when_the_server_runs_and_answers_while_the_session_hears_nothing() {
        let mut silence = gone_quiet(5);
        silence.answered(read(&offer(BOOT, RUNNING)));
        assert_eq!(silence.answer(), Some(Answer::Running));
        assert!(!silence.udp_died());
        assert!(!silence.hear(5, STILL_QUIET_SECS - FRAME));
        assert!(!silence.udp_died());
        assert!(!silence.hear(5, FRAME));
        assert!(silence.udp_died());
    }

    #[test]
    fn a_renewed_certificate_is_still_the_same_server() {
        let mut renewed = offer(BOOT, RUNNING);
        renewed.hashes = vec!["b".to_owned(), "c".to_owned()];
        let mut silence = gone_quiet(5);
        silence.answered(read(&renewed));
        silence.hear(5, STILL_QUIET_SECS);
        assert!(silence.udp_died());
    }

    #[test]
    fn a_whole_link_outage_goes_back_to_webtransport() {
        // The ask is lost with everything else, and fails or is still out when the session ends.
        let mut silence = gone_quiet(5);
        silence.answered(Answer::Unanswered);
        silence.hear(5, 10.0);
        assert!(!silence.udp_died());
        assert!(!gone_quiet(5).udp_died());
        // Once the link is back the same server answers at once, and that says nothing.
        assert_eq!(
            plan(NOW + 0.5, AUTO, None, Some(NOW), OfferState::Offered),
            Plan::WebTransport
        );
    }

    #[test]
    fn a_restarted_server_is_not_udp_dying() {
        let mut silence = gone_quiet(5);
        silence.answered(read(&offer(BOOT + 1, 3)));
        silence.hear(5, 10.0);
        assert!(!silence.udp_died());
        // And the verdict is in: a second answer changes nothing.
        silence.answered(Answer::Running);
        assert_eq!(silence.answer(), Some(Answer::Restarted));
        assert!(!silence.udp_died());
        // The reconnect asks the new run for its offer and goes back to WebTransport.
        assert_eq!(
            plan(NOW + 0.5, AUTO, None, Some(NOW), OfferState::Unasked),
            Plan::Ask
        );
    }

    #[test]
    fn a_frozen_server_is_not_udp_dying() {
        // Its loop stopped a few lost packets after the last one heard; HTTP still answers.
        let mut silence = gone_quiet(5);
        silence.answered(read(&offer(BOOT, HEARD + 3)));
        assert_eq!(silence.answer(), Some(Answer::Frozen));
        silence.hear(5, 10.0);
        assert!(!silence.udp_died());
        // The reconnect stays on WebTransport.
        assert_eq!(
            plan(NOW + 0.5, AUTO, None, Some(NOW), OfferState::Offered),
            Plan::WebTransport
        );
    }

    #[test]
    fn hearing_again_ends_the_silence_and_its_verdict() {
        // A link back from an outage: HTTP answers, and the session hears again a moment later.
        let mut silence = gone_quiet(5);
        silence.answered(Answer::Running);
        assert!(!silence.hear(6, FRAME));
        assert_eq!(silence.answer(), None);
        assert!(!silence.udp_died());
        let mut silence = gone_quiet(5);
        silence.answered(Answer::Running);
        silence.hear(5, STILL_QUIET_SECS);
        assert!(!silence.hear(6, FRAME));
        assert!(!silence.udp_died());
        // An answer to an ask from before the packet says nothing about the link now.
        let mut silence = gone_quiet(5);
        assert!(!silence.hear(6, FRAME));
        assert!(!silence.asking());
        silence.answered(Answer::Running);
        assert!(!silence.udp_died());
        // A new silence asks again.
        assert!(silence.hear(6, QUIET_SECS));
    }

    #[test]
    fn udp_alone_dying_means_websocket_for_now() {
        let gave_up = Some(GaveUp::Until(NOW + GIVE_UP_SECS));
        assert_eq!(
            plan(NOW + 0.5, AUTO, gave_up, Some(NOW), OfferState::Unasked),
            Plan::WebSocket
        );
    }

    #[test]
    fn a_restarting_server_is_asked_again() {
        let none = OfferState::NoOffer;
        assert_eq!(plan(NOW + 0.5, AUTO, None, Some(NOW), none), Plan::Ask);
        assert_eq!(
            plan(NOW + OFFER_WAIT_SECS - 0.1, AUTO, None, Some(NOW), none),
            Plan::Ask
        );
        assert_eq!(
            plan(NOW + OFFER_WAIT_SECS, AUTO, None, Some(NOW), none),
            Plan::WebSocket
        );
    }

    #[test]
    fn giving_up_for_now_ends() {
        let until = NOW + GIVE_UP_SECS;
        let gave_up = Some(GaveUp::Until(until));
        assert_eq!(
            plan(until - 0.1, AUTO, gave_up, None, OfferState::Offered),
            Plan::WebSocket
        );
        assert_eq!(
            plan(until, AUTO, gave_up, None, OfferState::Offered),
            Plan::WebTransport
        );
    }

    #[test]
    fn a_pinned_transport_never_falls_back() {
        let wt = TransportChoice::WebTransport;
        let gave_up = Some(GaveUp::ForTheRun);
        assert_eq!(
            plan(NOW, wt, gave_up, Some(NOW), OfferState::Offered),
            Plan::WebTransport
        );
        assert_eq!(plan(NOW, wt, None, None, OfferState::NoOffer), Plan::Ask);
        let asked = OfferState::Asking(NOW);
        assert_eq!(plan(NOW, wt, None, None, asked), Plan::Wait);
        // An ask that hangs is asked again.
        assert_eq!(plan(NOW + ASK_WAIT_SECS, wt, None, None, asked), Plan::Ask);
        let ws = TransportChoice::WebSocket;
        assert_eq!(
            plan(NOW, ws, None, None, OfferState::Offered),
            Plan::WebSocket
        );
    }

    #[test]
    fn the_choice_parses_from_its_setting() {
        assert_eq!(TransportChoice::from_setting(None), TransportChoice::Auto);
        assert_eq!(
            TransportChoice::from_setting(Some(" WT ")),
            TransportChoice::WebTransport
        );
        assert_eq!(
            TransportChoice::from_setting(Some("ws")),
            TransportChoice::WebSocket
        );
        assert_eq!(
            TransportChoice::from_setting(Some("auto")),
            TransportChoice::Auto
        );
    }

    #[test]
    #[should_panic(expected = "expected auto, wt or ws")]
    fn an_unknown_choice_is_refused() {
        TransportChoice::from_setting(Some("udp"));
    }

    #[test]
    fn the_choice_is_read_from_the_page_query() {
        assert_eq!(query_param("?transport=ws", "transport"), Some("ws"));
        assert_eq!(
            query_param("?tick_hz=60&transport=wt&autopilot", "transport"),
            Some("wt")
        );
        assert_eq!(query_param("?transports=ws", "transport"), None);
        assert_eq!(query_param("?autopilot", "transport"), None);
        assert_eq!(query_param("", "transport"), None);
    }
}
