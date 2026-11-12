//! The pistol's hits, shown as the netcode judged them. A hitscan shot is checked against the
//! other players as the shooter saw them: at the command's seen time, a whole server tick and a
//! fraction into the next, on the curve through each player's history
//! ([`PlayerPoses::pose_at_time`]). Each hit this player makes leaves three clay ghosts of its
//! victim:
//!
//! - the hit itself, solid: the pose the shot met, at the seen tick plus the subtick fraction;
//! - the two ticks either side of it, faint: where the server had the victim at those ticks.
//!
//! So a hit between ticks is drawn between them. A label gives the tick and the fraction, and
//! then the server's verdict: the hit appears the moment this client predicts it, and the
//! server's own copy of the shot ([`RemoteEvent`]) confirms it, or doesn't.
//!
//! Every pistol shot also draws its tracer, and every hit, anyone's, makes its victim flinch.

use std::collections::VecDeque;

use bevy::prelude::*;
use bevy_msdf_text::{MsdfText, MsdfTextMode};
use bevy_netahoy::*;

use playground::{Fired, Hit, Shot, Weapon, bots::player_name};

use crate::{
    demon::{
        Demon, DemonAssets, DemonFlinch, DemonPose, clay, flinch, model_transform,
        spawn_frozen_demon,
    },
    text::Font,
};

/// How long a hit's ghosts stay, the last [`FADE_SECONDS`] of it fading.
const GHOST_SECONDS: f32 = 4.0;
const FADE_SECONDS: f32 = 1.0;
/// A newer hit hides an older one's label and leaves it this long to fade (s), so the newest
/// reads clearly however fast the shots come.
const SUPERSEDED_SECONDS: f32 = 0.6;
/// How long after the server's copy of a shot it may still send the hit (s): past it, with no
/// hit, the server missed.
const VERDICT_GRACE: f32 = 0.3;
/// How long a tracer stays (s).
const TRACER_SECONDS: f32 = 0.18;
/// The hit's ghost: solid red clay. The ticks': faint light, added to what is behind them, so
/// they never hide the hit or each other.
const HIT_COLOR: Color = Color::srgb(0.92, 0.16, 0.12);
const TICK_COLOR: Color = Color::srgb(0.30, 0.48, 0.80);

/// Ghosts for this player's pistol hits, tracers for everyone's shots, and the hit counts.
pub struct HitGhostsPlugin;

impl Plugin for HitGhostsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<PistolHits>()
            .add_observer(server_shot)
            .add_systems(
                Update,
                (predicted_shots, judge_ghosts, fade_ghosts, draw_tracers)
                    .chain()
                    .after(NetAhoyClientSystems::Interpolate),
            );
    }
}

/// This player's pistol hits: as its prediction made them, and as the server's step did.
#[derive(Resource, Default)]
pub struct PistolHits {
    pub predicted: u32,
    pub confirmed: u32,
    /// The newest predicted hits, so the server's copy of one is known for it after its ghost
    /// has gone.
    recent: VecDeque<EventId>,
}

impl PistolHits {
    /// Twenty seconds of hits at the pistol's refire: past any round trip.
    const RECENT: usize = 64;

    fn note(&mut self, id: EventId) {
        self.predicted += 1;
        if self.recent.len() == Self::RECENT {
            self.recent.pop_front();
        }
        self.recent.push_back(id);
    }
}

/// What the server made of a hit this client predicted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verdict {
    /// Predicted; the server's copy of the shot hasn't come yet.
    Waiting,
    /// The server's step hit too.
    Confirmed,
    /// The server's copy of the shot came, and no hit with it.
    Missed,
    /// The server hit where this client's prediction didn't.
    ServerOnly,
}

/// A hit's ghosts: the root of the three demons and the label.
#[derive(Component)]
struct HitGhost {
    id: EventId,
    age: f32,
    verdict: Verdict,
    /// The verdict the label shows.
    shown: Verdict,
    /// When the server's copy of the shot came, on the ghost's clock.
    server_fired_at: Option<f32>,
    label: Entity,
    /// The first line of the label: the tick and the fraction.
    heading: String,
    hit_material: Handle<StandardMaterial>,
    tick_material: Handle<StandardMaterial>,
    /// The three positions (chest high), tick, hit, next tick, for the line through them.
    path: [Vec3; 3],
}

/// A shot's line, from the muzzle to what it met.
#[derive(Component)]
struct Tracer {
    from: Vec3,
    to: Vec3,
    age: f32,
}

/// What a hit's ghosts are drawn from.
#[derive(bevy::ecs::system::SystemParam)]
struct GhostWorld<'w, 's> {
    commands: Commands<'w, 's>,
    font: Res<'w, Font>,
    assets: Res<'w, DemonAssets>,
    shape: Res<'w, PlayerShape>,
    rate: Res<'w, TickRate>,
    materials: ResMut<'w, Assets<StandardMaterial>>,
    histories:
        Query<'w, 's, (&'static PlayerId, &'static History<PlayerSnapshot>), With<RemotePlayer>>,
    visuals: Query<'w, 's, (Entity, &'static RemoteVisual)>,
    demons: Query<'w, 's, &'static Demon>,
    children: Query<'w, 's, &'static Children>,
    bones: Query<'w, 's, (&'static Name, &'static Transform)>,
    ghosts: Query<'w, 's, (Entity, &'static mut HitGhost)>,
}

impl GhostWorld<'_, '_> {
    /// The ghosts of `hit`, judged `verdict`: the victim at the hit's seen time and the ticks
    /// either side.
    fn spawn(&mut self, hit: &Hit, verdict: Verdict) {
        let Some(victim) = hit.direct_hit else {
            return;
        };
        let poses = PlayerPoses::new(self.histories.iter(), *self.rate);
        let seen = RemoteRenderTime::new(hit.seen_tick, hit.seen_alpha);
        let at = |time| poses.pose_at_time(victim, time);
        let (Some(before), Some(struck), Some(after)) = (
            at(RemoteRenderTime::new(seen.tick, 0.0)),
            at(seen),
            at(RemoteRenderTime::new(seen.tick + 1, 0.0)),
        ) else {
            return;
        };
        // The victim's limbs as drawn now: the frame the shot was fired from.
        let pose = self
            .visuals
            .iter()
            .find(|(_, visual)| visual.player_id == victim)
            .and_then(|(entity, _)| {
                DemonPose::of(entity, &self.demons, &self.children, &self.bones)
            })
            .unwrap_or_default();

        for (_, mut older) in &mut self.ghosts {
            older.age = older.age.max(GHOST_SECONDS - SUPERSEDED_SECONDS);
            self.commands.entity(older.label).insert(Visibility::Hidden);
        }

        let hit_material = self.materials.add(clay(HIT_COLOR));
        let tick_material = self.materials.add(StandardMaterial {
            base_color: TICK_COLOR,
            unlit: true,
            alpha_mode: AlphaMode::Add,
            ..default()
        });

        let heading = format!(
            "{} hit at tick {}, subtick {:.2}",
            player_name(victim),
            seen.tick,
            seen.alpha
        );
        let chest = Vec3::Y * self.shape.half_height * 0.4;
        let root = self
            .commands
            .spawn((
                Name::new(format!("hit ghosts {}", hit.id.sequence)),
                Transform::default(),
                Visibility::default(),
            ))
            .id();
        for (snapshot, material, tick_label) in [
            (before, &tick_material, Some(seen.tick)),
            (struck, &hit_material, None),
            (after, &tick_material, Some(seen.tick + 1)),
        ] {
            let body = self
                .commands
                .spawn((
                    Transform::from_translation(snapshot.position),
                    Visibility::default(),
                    ChildOf(root),
                ))
                .id();
            spawn_frozen_demon(
                &mut self.commands,
                &self.assets,
                pose.clone(),
                material.clone(),
                model_transform(&self.shape, snapshot.look.x),
                body,
            );
            if let Some(tick) = tick_label {
                // At the feet, clear of the hit's label over the heads.
                self.commands.spawn((
                    self.font
                        .text(format!("tick {tick}"), MsdfTextMode::WorldFixedPx, 12.0)
                        .with_color(Color::WHITE.with_alpha(0.85))
                        .with_anchor(Vec2::new(0.0, 0.5)),
                    Transform::from_xyz(0.0, -self.shape.half_height - 0.05, 0.0),
                    ChildOf(body),
                ));
            }
        }
        let label = self
            .commands
            .spawn((
                label_text(&self.font, &heading, verdict),
                Transform::from_translation(
                    struck.position + Vec3::Y * (self.shape.half_height + 0.45),
                ),
                ChildOf(root),
            ))
            .id();
        self.commands.entity(root).insert(HitGhost {
            id: hit.id,
            age: 0.0,
            verdict,
            shown: verdict,
            server_fired_at: None,
            label,
            heading,
            hit_material,
            tick_material,
            path: [before.position, struck.position, after.position].map(|p| p + chest),
        });
    }
}

/// The hit's label: its tick and fraction, then the verdict.
fn label_text(font: &Font, heading: &str, verdict: Verdict) -> MsdfText {
    let (line, color) = match verdict {
        Verdict::Waiting => ("predicted: waiting for the server", Color::WHITE),
        Verdict::Confirmed => ("the server hit too", Color::srgb(0.55, 1.0, 0.55)),
        Verdict::Missed => ("the server missed", Color::srgb(1.0, 0.45, 0.4)),
        Verdict::ServerOnly => (
            "the server hit; this client missed",
            Color::srgb(1.0, 0.8, 0.35),
        ),
    };
    font.text(
        format!("{heading}\n{line}"),
        MsdfTextMode::WorldFixedPx,
        16.0,
    )
    .with_color(color)
    .with_anchor(Vec2::new(0.0, -0.5))
}

/// This player's predicted shots: a tracer for each, ghosts for each hit.
fn predicted_shots(
    mut shots: MessageReader<Shot>,
    mut hits: ResMut<PistolHits>,
    mut world: GhostWorld,
    mut flinches: Query<(&RemoteVisual, &mut DemonFlinch)>,
) {
    for shot in shots.read() {
        match *shot {
            Shot::Fired(fired) if fired.weapon == Weapon::Pistol => {
                spawn_tracer(&mut world.commands, &fired);
            }
            Shot::Hit(hit) if hit.weapon == Weapon::Pistol => {
                hits.note(hit.id);
                world.spawn(&hit, Verdict::Waiting);
                flinch(&mut flinches, hit.direct_hit);
            }
            _ => {}
        }
    }
}

/// The server's pistol shots: other players' tracers and flinches, and the verdict on this
/// player's.
fn server_shot(
    shot: On<RemoteEvent<Shot>>,
    own: Res<OwnPlayerId>,
    mut hits: ResMut<PistolHits>,
    mut world: GhostWorld,
    mut flinches: Query<(&RemoteVisual, &mut DemonFlinch)>,
) {
    let RemoteEvent(shot) = *shot;
    match shot {
        Shot::Fired(fired) if fired.weapon == Weapon::Pistol => {
            if !own.is_assigned_to(fired.id.owner) {
                spawn_tracer(&mut world.commands, &fired);
                return;
            }
            for (_, mut ghost) in &mut world.ghosts {
                if ghost.id == fired.id && ghost.server_fired_at.is_none() {
                    ghost.server_fired_at = Some(ghost.age);
                }
            }
        }
        Shot::Hit(hit) if hit.weapon == Weapon::Pistol => {
            if !own.is_assigned_to(hit.id.owner) {
                flinch(&mut flinches, hit.direct_hit);
                return;
            }
            hits.confirmed += 1;
            for (_, mut ghost) in &mut world.ghosts {
                if ghost.id == hit.id {
                    ghost.verdict = Verdict::Confirmed;
                }
            }
            if !hits.recent.contains(&hit.id) {
                world.spawn(&hit, Verdict::ServerOnly);
                flinch(&mut flinches, hit.direct_hit);
            }
        }
        _ => {}
    }
}

/// A predicted hit whose shot the server stepped without hitting is a miss; each verdict shows
/// on its label.
fn judge_ghosts(
    font: Res<Font>,
    mut ghosts: Query<&mut HitGhost>,
    mut labels: Query<&mut MsdfText>,
) {
    for mut ghost in &mut ghosts {
        if ghost.verdict == Verdict::Waiting
            && ghost
                .server_fired_at
                .is_some_and(|at| ghost.age - at > VERDICT_GRACE)
        {
            ghost.verdict = Verdict::Missed;
        }
        if ghost.shown == ghost.verdict {
            continue;
        }
        if let Ok(mut label) = labels.get_mut(ghost.label) {
            *label = label_text(&font, &ghost.heading, ghost.verdict);
        }
        ghost.shown = ghost.verdict;
    }
}

/// Ages the ghosts, fades them out and despawns them; draws the line through each one's ticks.
fn fade_ghosts(
    mut commands: Commands,
    time: Res<Time>,
    mut gizmos: Gizmos,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut ghosts: Query<(Entity, &mut HitGhost)>,
) {
    for (entity, mut ghost) in &mut ghosts {
        ghost.age += time.delta_secs();
        if ghost.age >= GHOST_SECONDS {
            commands.entity(entity).despawn();
            continue;
        }
        let left = ((GHOST_SECONDS - ghost.age) / FADE_SECONDS).min(1.0);
        let left = left * left;
        if left < 1.0 {
            // Blended only while it fades: a blended mesh can draw its far side over its near.
            if let Some(mut hit) = materials.get_mut(&ghost.hit_material) {
                hit.alpha_mode = AlphaMode::Blend;
                hit.base_color.set_alpha(left);
            }
            if let Some(mut tick) = materials.get_mut(&ghost.tick_material) {
                tick.base_color = TICK_COLOR.mix(&Color::BLACK, 1.0 - left);
            }
        }
        let [before, struck, after] = ghost.path;
        let line = Color::WHITE.with_alpha(0.7 * left);
        gizmos.line(before, after, line);
        for tick in [before, after] {
            gizmos.sphere(Isometry3d::from_translation(tick), 0.06, line);
        }
        gizmos.sphere(
            Isometry3d::from_translation(struck),
            0.1,
            HIT_COLOR.with_alpha(left),
        );
    }
}

fn spawn_tracer(commands: &mut Commands, fired: &Fired) {
    commands.spawn(Tracer {
        from: fired.start,
        to: fired.start + fired.dir * fired.hit_distance,
        age: 0.0,
    });
}

fn draw_tracers(
    mut commands: Commands,
    time: Res<Time>,
    mut gizmos: Gizmos,
    mut tracers: Query<(Entity, &mut Tracer)>,
) {
    for (entity, mut tracer) in &mut tracers {
        tracer.age += time.delta_secs();
        if tracer.age >= TRACER_SECONDS {
            commands.entity(entity).despawn();
            continue;
        }
        let left = 1.0 - tracer.age / TRACER_SECONDS;
        // Shortened from the muzzle as it fades, so it reads as a shot going out.
        let from = tracer.from.lerp(tracer.to, 1.0 - left);
        gizmos.line(from, tracer.to, Color::srgba(1.0, 0.93, 0.7, left));
    }
}
