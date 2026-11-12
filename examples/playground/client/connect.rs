//! What the playground adds to the library's transport client: a WebSocket link's loss model
//! from the link conditioner, and the window title.

use bevy::prelude::*;
use bevy_link_conditioner::{DefaultLinkConditioner, Mode};
use bevy_netahoy::{transport::Transport, *};

/// Datagram only when the conditioner drops packets for good. A WebSocket never loses one (TCP
/// redelivers), so the stream model sends only the newest command; over datagrams the resent
/// unacked tail fills a lost command's slot.
fn websocket_loss_model(conditioner: &DefaultLinkConditioner) -> LinkLossModel {
    if conditioner.enabled && conditioner.mode == Mode::Datagram {
        LinkLossModel::Datagram
    } else {
        LinkLossModel::Stream
    }
}

/// A WebSocket session starts on the conditioner's loss model: the library tags it
/// [`LinkLossModel::Stream`], which a conditioner dropping datagrams makes wrong.
pub fn condition_new_session(
    session: On<Add, Transport>,
    conditioner: Res<DefaultLinkConditioner>,
    mut models: Query<(&mut LinkLossModel, &Transport)>,
) {
    if let Ok((mut model, Transport::WebSocket)) = models.get_mut(session.event_target()) {
        model.set_if_neq(websocket_loss_model(&conditioner));
    }
}

/// The F2 panel can switch a WebSocket link to datagrams and back. WebTransport is datagrams
/// whatever the panel says.
pub fn follow_link_mode(
    conditioner: Res<DefaultLinkConditioner>,
    mut models: Query<(&mut LinkLossModel, &Transport)>,
) {
    if conditioner.is_changed() {
        for (mut model, transport) in &mut models {
            if *transport == Transport::WebSocket {
                model.set_if_neq(websocket_loss_model(&conditioner));
            }
        }
    }
}

/// Titles the window with the player the server gave this client, or the one a demo recorded.
pub fn title_window(
    own: Res<OwnPlayerId>,
    playback: Option<Res<DemoPlayback>>,
    mut windows: Query<&mut Window>,
    mut titled: Local<Option<PlayerId>>,
) {
    let player = own
        .0
        .or_else(|| playback.and_then(|playback| playback.recorded_player));
    let Some(id) = player.filter(|_| player != *titled) else {
        return;
    };
    *titled = player;
    for mut window in &mut windows {
        window.title = format!("bevy_netahoy playground: player {}", id.0);
    }
}
