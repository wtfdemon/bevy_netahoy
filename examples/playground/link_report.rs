//! The link conditioner's numbers in the log, for the client and the server alike.

use bevy::prelude::*;

/// Logs what each conditioned link has done, every 30 s of real time.
pub fn report_link_conditions(
    time: Res<Time<Real>>,
    mut window: Local<f32>,
    links: Query<(Entity, &bevy_link_conditioner::ConditionedLink)>,
) {
    *window += time.delta_secs();
    if *window < 30.0 {
        return;
    }
    *window = 0.0;
    for (entity, link) in &links {
        info!(
            "link conditioner {entity}: up {} | down {}",
            link.up().stats(),
            link.down().stats()
        );
    }
}
