//! Player events, each passed on once per timeline.
//!
//! A command can be stepped more than once: the client replays its unacknowledged commands
//! after a correction, and the server steps a late command's slot again (input rollback). Every
//! run emits its events again, and a rerun can make an event no earlier run made (a fire on a
//! command that arrived late) or move one (a guessed direct hit that the real command misses
//! blasts at its fuse instead).
//!
//! State follows the last run, events follow the first: each timeline (a server command buffer,
//! a client session) keeps an [`Outbox`], which writes an event as a plain Bevy message the
//! first time a run emits it and never again on that timeline.

use std::{
    collections::VecDeque,
    fmt::Debug,
    mem::{self, Discriminant},
};

use bevy::{ecs::message::Messages, platform::collections::HashSet, prelude::*};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::protocol::PlayerId;

/// The player and command an event began on: the same on every rerun, so the event is passed
/// on once.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EventId {
    /// The player whose step made it.
    pub owner: PlayerId,
    /// The command it began on.
    pub sequence: u32,
}

/// Something a step makes happen, and the message the game reads it as.
///
/// State follows the last run, events follow the first: once an event has gone out on a
/// timeline (which remembers its newest 1024), no rerun sends it again, even one that moves it to
/// a later command or changes its payload. A rerun that makes an event no earlier run made does
/// send it. Read it with a plain `MessageReader`, every frame or at least every other update, as
/// any Bevy message.
///
/// An event's key is its [`StepEvent::id`] and its outermost enum variant
/// ([`mem::discriminant`]): one event per id and outermost variant. Two variants may share an id
/// (a shot's fire and its hit), but a step emits at most one event per variant per id, e.g. one
/// hit listing every player a shot hit; a second is a bug: it is dropped, with a warning. Give
/// each kind of event its own top-level variant, don't nest kinds: in `enum E { Rocket(Shot) }`
/// a rocket's fire and hit have one key, so only the first goes out. A struct event type gets
/// one event per id.
pub trait StepEvent: Message + Copy + Debug + Serialize + DeserializeOwned {
    /// The player and command the event began on, the same on every rerun.
    fn id(&self) -> EventId;
}

/// The [`PlayerGame::Event`](crate::PlayerGame::Event) of a game whose steps make nothing
/// happen. It has no values, so there is never one to emit or send.
#[derive(Message, Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NoEvents {}

impl StepEvent for NoEvents {
    fn id(&self) -> EventId {
        match *self {}
    }
}

/// An event's identity: its id and its outermost variant.
type EventKey<E> = (EventId, Discriminant<E>);

fn key<E: StepEvent>(event: &E) -> EventKey<E> {
    (event.id(), mem::discriminant(event))
}

/// How many keys an outbox keeps, the newest. A key has to outlive every rerun that could still
/// emit it: its command's rerun window (at most 64 commands) plus the longest an event can trail
/// its command (a rocket's fuse). That is far fewer events than this for one player (the
/// example, firing as fast as it can, makes ten a second), and one player's events never push
/// out another's.
const SEEN_PER_TIMELINE: usize = 1024;

/// One timeline's events on their way out. A timeline is one run of a player's commands, its
/// sequences counted from the start: the server keeps one per command buffer, the client one
/// for the local player's session. A new one has passed nothing on.
///
/// It remembers the newest 1024 events it passed on and drops their repeats: state follows the
/// last run, events follow the first. Only input rollback reruns a server step, so with it off
/// the server's outboxes drop nothing but a step's own repeats.
#[derive(Resource, Clone, Debug)]
pub struct Outbox<E> {
    /// What the step running now emitted.
    pub(crate) pending: Vec<E>,
    /// The keys passed on, oldest first.
    order: VecDeque<EventKey<E>>,
    /// The same keys, to look up.
    sent: HashSet<EventKey<E>>,
    /// Whether a step's own repeat has been warned about.
    warned: bool,
}

impl<E> Default for Outbox<E> {
    fn default() -> Self {
        Self {
            pending: Vec::new(),
            order: VecDeque::new(),
            sent: HashSet::default(),
            warned: false,
        }
    }
}

impl<E: StepEvent> Outbox<E> {
    /// Writes what the step that just ran emitted to `messages`, in order, but only the events
    /// this timeline never passed on: a rerun's repeats stop here. A repeat within the step is
    /// the game's bug: dropped, and warned about once per outbox (a flood would bury the log).
    pub(crate) fn send(&mut self, messages: &mut Messages<E>) {
        let Self {
            pending,
            order,
            sent,
            warned,
        } = self;
        for (index, event) in pending.iter().enumerate() {
            let event_key = key(event);
            if pending[..index]
                .iter()
                .any(|earlier| key(earlier) == event_key)
            {
                if !*warned {
                    *warned = true;
                    warn!(
                        "a step emitted a second {event:?} under {:?}: dropped, as one event per \
                         variant per id is all a step may emit (later repeats on this timeline \
                         are dropped unwarned)",
                        event_key.0
                    );
                }
                continue;
            }
            if sent.contains(&event_key) {
                continue;
            }
            if order.len() == SEEN_PER_TIMELINE
                && let Some(oldest) = order.pop_front()
            {
                sent.remove(&oldest);
            }
            order.push_back(event_key);
            sent.insert(event_key);
            messages.write(*event);
        }
        pending.clear();
    }
}

/// A player event as the server sends it to every client, once: the first time a server step
/// makes it.
///
/// State follows the last run, events follow the first: a rollback that moves or changes the
/// event sends nothing more, so this is the first run's verdict (e.g. a confirmed hit), and the
/// owner snapshot, the last run's state, can later disagree with it.
///
/// For the client whose player made it (the [`StepEvent::id`] owner is its
/// [`OwnPlayerId`](crate::OwnPlayerId)) it is the verdict on its prediction, and can be the only
/// copy: a guessed hit the server kept, or a hit on a pose the client only extrapolated, was
/// never predicted. Play the predicted copy at once and use this one to confirm it, or to play
/// what the prediction missed; playing both plays the effect twice.
#[derive(Event, Clone, Copy, Debug, Serialize, Deserialize)]
pub struct RemoteEvent<E>(pub E);

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use bevy::log::{
        tracing::{
            self, Subscriber,
            field::{Field, Visit},
        },
        tracing_subscriber::{
            Layer, Registry,
            layer::{Context, SubscriberExt},
        },
    };

    use super::*;
    use crate::game::test_game::TestEvent;

    fn id(sequence: u32) -> EventId {
        EventId {
            owner: PlayerId(1),
            sequence,
        }
    }

    /// `events` emitted by one step through `outbox`: the ones it wrote.
    fn send(outbox: &mut Outbox<TestEvent>, events: &[TestEvent]) -> Vec<TestEvent> {
        let mut messages = Messages::default();
        outbox.pending.extend_from_slice(events);
        outbox.send(&mut messages);
        messages.drain().collect()
    }

    #[test]
    fn an_outbox_writes_each_event_once() {
        let (fired, hit) = (TestEvent::Fired(id(7)), TestEvent::Hit(id(7)));
        let mut outbox = Outbox::default();
        assert_eq!(
            send(&mut outbox, &[fired, hit]),
            [fired, hit],
            "a hit under its fire's id is another event"
        );
        assert_eq!(send(&mut outbox, &[fired, hit]), [], "a rerun's drop");
        assert_eq!(
            send(&mut Outbox::default(), &[fired]),
            [fired],
            "another timeline's 7 is new"
        );
    }

    /// A key stays while fewer than [`SEEN_PER_TIMELINE`] newer ones have gone out on its
    /// timeline, however many commands later a rerun emits it again: a rocket's blast keeps the
    /// id of the command that fired it.
    #[test]
    fn an_outbox_keeps_its_newest_keys() {
        let blast = TestEvent::Hit(id(10));
        let mut outbox = Outbox::default();
        assert_eq!(send(&mut outbox, &[blast]), [blast]);
        for sequence in 11..11 + SEEN_PER_TIMELINE as u32 - 1 {
            send(&mut outbox, &[TestEvent::Fired(id(sequence))]);
        }
        assert_eq!(
            send(&mut outbox, &[blast]),
            [],
            "1023 newer keys: still a repeat"
        );

        send(&mut outbox, &[TestEvent::Fired(id(5000))]);
        assert_eq!(outbox.order.len(), SEEN_PER_TIMELINE);
        assert_eq!(outbox.sent.len(), SEEN_PER_TIMELINE);
        assert_eq!(
            send(&mut outbox, &[blast]),
            [blast],
            "pushed out by 1024 newer keys, it may recur"
        );
    }

    /// Each timeline's keys are its own: another timeline passing on any number of events, its
    /// own copies of this one's keys included, pushes none of them out.
    #[test]
    fn a_timelines_keys_outlive_any_other_timelines_events() {
        let blast = TestEvent::Hit(id(10));
        let (mut mine, mut theirs) = (Outbox::default(), Outbox::default());
        assert_eq!(send(&mut mine, &[blast]), [blast]);
        assert_eq!(
            send(&mut theirs, &[blast]),
            [blast],
            "another timeline's 10"
        );
        for sequence in 0..4 * SEEN_PER_TIMELINE as u32 {
            let other = EventId {
                owner: PlayerId(2),
                sequence,
            };
            send(
                &mut theirs,
                &[TestEvent::Fired(other), TestEvent::Hit(other)],
            );
        }
        assert_eq!(theirs.order.len(), SEEN_PER_TIMELINE);
        assert_eq!(
            send(&mut theirs, &[blast]),
            [blast],
            "pushed out of their own"
        );
        assert_eq!(send(&mut mine, &[blast]), [], "still a repeat on mine");
        assert_eq!(mine.order.len(), 1);
    }

    /// Every warning's message, as logged.
    struct Warnings(Arc<Mutex<Vec<String>>>);

    impl<S: Subscriber> Layer<S> for Warnings {
        fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
            struct Message<'a>(&'a mut String);

            impl Visit for Message<'_> {
                fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
                    if field.name() == "message" {
                        *self.0 = format!("{value:?}");
                    }
                }
            }

            if *event.metadata().level() == tracing::Level::WARN {
                let mut message = String::new();
                event.record(&mut Message(&mut message));
                self.0.lock().unwrap().push(message);
            }
        }
    }

    /// Eight pellets under one id are a game bug: the first goes out, the rest are dropped,
    /// and the outbox warns once, naming the id, however often it happens again. A rerun's
    /// repeat is no bug and warns nothing.
    #[test]
    fn a_steps_own_repeat_is_dropped_with_one_warning() {
        let warnings = Arc::new(Mutex::new(Vec::new()));
        let subscriber = Registry::default().with(Warnings(warnings.clone()));
        tracing::subscriber::with_default(subscriber, || {
            let mut outbox = Outbox::default();
            let (fired, hit) = (TestEvent::Fired(id(7)), TestEvent::Hit(id(7)));
            assert_eq!(send(&mut outbox, &[fired, hit]), [fired, hit]);
            assert_eq!(send(&mut outbox, &[fired, hit]), [], "a rerun's");

            let (fired, pellet) = (TestEvent::Fired(id(8)), TestEvent::Hit(id(8)));
            let mut shot = vec![fired];
            shot.extend([pellet; 8]);
            assert_eq!(send(&mut outbox, &shot), [fired, pellet]);
            assert_eq!(send(&mut outbox, &shot), [], "the shot's rerun");
            let pellet = TestEvent::Hit(id(9));
            assert_eq!(send(&mut outbox, &[pellet; 8]), [pellet]);

            let pellet = TestEvent::Hit(id(10));
            assert_eq!(send(&mut Outbox::default(), &[pellet; 2]), [pellet]);
        });
        let warnings = warnings.lock().unwrap();
        assert_eq!(warnings.len(), 2, "one per outbox: {warnings:?}");
        assert!(warnings[0].contains("sequence: 8"), "{}", warnings[0]);
        assert!(warnings[1].contains("sequence: 10"), "{}", warnings[1]);
    }
}
