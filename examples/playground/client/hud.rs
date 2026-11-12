//! The on-screen text: the connection, what the prediction is doing, the move state on both
//! sides, the weapon and its hits, and the keys. All of it is what the library reports, read
//! from its components each frame.

use bevy::prelude::*;
use bevy_msdf_text::{MsdfText, TextAlign};
use bevy_netahoy::{
    debug::{DebugTimeScale, PredictionStatus},
    transport::Transport,
    *,
};
use bevy_replicon::prelude::*;

use playground::{Arsenal, Weapon};

use crate::{hit_ghosts::PistolHits, presentation::RemoteGhostDebug, text::Font};

/// The HUD's sizes, pixels per em.
const STATUS_SIZE: f32 = 17.0;
const KEYS_SIZE: f32 = 15.0;
/// Its margin from the window's edges (px).
const MARGIN: f32 = 18.0;

const KEYS: &str = "WASD  move     SPACE  jump (hold to bunny hop)     CTRL  crouch\n\
                    1  rockets     2  pistol     RIGHT MOUSE / F  fire\n\
                    F2  network conditions     F3  the server's copy     ESC  release the mouse";

/// Marks the status block [`update_status`] writes.
#[derive(Component)]
pub struct StatusText;

/// The status block, the keys, the title and the crosshair.
pub fn setup_hud(mut commands: Commands, font: Res<Font>) {
    let top_left = Vec2::new(-0.5, 0.5);
    commands.spawn((
        font.hud(
            "connecting",
            STATUS_SIZE,
            Vec2::ZERO,
            Vec2::splat(MARGIN),
            top_left,
            TextAlign::Left,
        ),
        StatusText,
    ));
    commands.spawn(font.hud(
        KEYS,
        KEYS_SIZE,
        Vec2::new(0.0, 1.0),
        Vec2::new(MARGIN, -MARGIN),
        Vec2::new(-0.5, -0.5),
        TextAlign::Left,
    ));
    commands.spawn(font.hud(
        "bevy_netahoy playground",
        STATUS_SIZE,
        Vec2::new(1.0, 0.0),
        Vec2::new(-MARGIN, MARGIN),
        Vec2::new(0.5, 0.5),
        TextAlign::Right,
    ));

    commands
        .spawn(Node {
            position_type: PositionType::Absolute,
            width: Val::Percent(100.0),
            height: Val::Percent(100.0),
            justify_content: JustifyContent::Center,
            align_items: AlignItems::Center,
            ..default()
        })
        .with_children(|parent| {
            for (width, height) in [(16.0, 2.0), (2.0, 16.0)] {
                parent.spawn((
                    Node {
                        position_type: PositionType::Absolute,
                        width: px(width + 2.0),
                        height: px(height + 2.0),
                        ..default()
                    },
                    BackgroundColor(Color::BLACK.with_alpha(0.6)),
                ));
                parent.spawn((
                    Node {
                        position_type: PositionType::Absolute,
                        width: px(width),
                        height: px(height),
                        ..default()
                    },
                    BackgroundColor(Color::WHITE.with_alpha(0.9)),
                ));
            }
        });
}

/// Everything the status block reads.
#[derive(bevy::ecs::system::SystemParam)]
pub struct Status<'w, 's> {
    own: Res<'w, OwnPlayerId>,
    state: Res<'w, State<ClientState>>,
    time_scale: Res<'w, DebugTimeScale>,
    debug_ghosts: Res<'w, RemoteGhostDebug>,
    hits: Res<'w, PistolHits>,
    playback: Option<Res<'w, DemoPlayback>>,
    reconnect: Option<Res<'w, Reconnect>>,
    sessions: Query<'w, 's, &'static Transport, With<aeronet::io::Session>>,
    predicted: Query<
        'w,
        's,
        (&'static PlayerState<Arsenal>, &'static PredictionStatus),
        With<PredictedPlayer>,
    >,
    server: Query<'w, 's, &'static OwnerSnapshot<Arsenal>, With<OwnPlayer>>,
}

impl Status<'_, '_> {
    /// Who this client is and how it is connected, or where the demo is.
    fn connection(&self) -> String {
        let mut line = if let Some(playback) = &self.playback {
            format!(
                "demo of {} - {:.1} s / {:.1} s - {}x",
                playback
                    .recorded_player
                    .map_or_else(|| "?".to_string(), |id| format!("player {}", id.0)),
                playback.clock.min(playback.duration()),
                playback.duration(),
                playback.speed,
            )
        } else {
            let player = self
                .own
                .0
                .map_or_else(|| "joining".to_string(), |id| format!("player {}", id.0));
            // The library drops `Reconnect` when the server refuses this client's protocol.
            if self.reconnect.is_none() && *self.state.get() == ClientState::Disconnected {
                return "refused by the server: it runs another build or another tick rate".into();
            }
            let mut line = format!("{player} - {:?}", self.state.get());
            for transport in &self.sessions {
                line.push_str(&format!(" over {}", transport.name()));
            }
            line
        };
        if self.time_scale.is_scaled() {
            line.push_str(&format!(" - slow motion {:.2}x", self.time_scale.factor));
        }
        if self.debug_ghosts.visible {
            line.push_str(" - showing the server's copy");
        }
        line
    }

    /// What the last reconcile did, and how far behind the acks are.
    fn prediction(&self) -> String {
        let Ok((_, prediction)) = self.predicted.single() else {
            return "prediction: waiting".into();
        };
        format!(
            "prediction: {:?} - off by {:.3} m - acked #{} - {} ahead - {} replayed",
            prediction.mode(),
            prediction.last_error(),
            prediction.last_ack_sequence(),
            prediction.unacked(),
            prediction.replayed_commands(),
        )
    }

    /// The predicted player's move state beside the server's.
    fn moves(&self) -> String {
        let move_state = |flags: MoveFlags| {
            if flags.crouching {
                "crouching"
            } else if flags.grounded {
                "on the ground"
            } else {
                "in the air"
            }
        };
        let predicted = self.predicted.single().map_or("waiting", |(state, _)| {
            move_state(MoveFlags::from_moves(&state.moves))
        });
        let server = self
            .server
            .single()
            .ok()
            .filter(|snapshot| snapshot.server_tick != 0)
            .map_or("waiting", |snapshot| move_state(snapshot.move_flags()));
        format!("you: {predicted} - the server: {server}")
    }

    /// The weapon in hand and the pistol's hits.
    fn weapon(&self) -> String {
        let weapon = match self.predicted.single().map(|(state, _)| state.game.weapon) {
            Ok(Weapon::Rockets) => "rockets",
            Ok(Weapon::Pistol) => "pistol",
            Err(_) => "-",
        };
        format!(
            "{weapon} - pistol hits: {} predicted, {} by the server",
            self.hits.predicted, self.hits.confirmed
        )
    }
}

/// Rewrites the status block when what it says changes.
pub fn update_status(status: Status, mut text: Single<&mut MsdfText, With<StatusText>>) {
    let lines = [
        status.connection(),
        status.prediction(),
        status.moves(),
        status.weapon(),
    ]
    .join("\n");
    if text.text != lines {
        text.text = lines;
    }
}
