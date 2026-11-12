//! The players drawn as demons: `assets/demon.glb` (made by `assets/trim_demon.py`) on every
//! remote player's visual and on this player's own, all alike: matte clay, flat shaded, like
//! figurines. Played with Bevy's animation graph: three clips and plain crossfades, the run while
//! on the ground and moving, at a speed that follows the player's, the jump while in the air,
//! and the pistol pose standing.
//!
//! A demon's pose can be copied onto a frozen one ([`DemonPose`], [`spawn_frozen_demon`]): the
//! hit ghosts are those.
//!
//! A remote player animates from what the library samples for its visual
//! ([`RemoteSampledFrame`]: the pose at the remote render time with its move flags, and the
//! velocity of the curve there); this player from its prediction. Both face their look's yaw,
//! turning to it at [`TURN_RATE`].

use std::{
    f32::consts::PI,
    path::{Path, PathBuf},
    time::Duration,
};

use bevy::{
    asset::io::embedded::EmbeddedAssetRegistry, ecs::system::SystemParam, gltf::GltfMaterialName,
    platform::collections::HashMap, prelude::*, world_serialization::WorldInstanceReady,
};
use bevy_netahoy::*;

use playground::Arsenal;

/// The model in the binary's embedded assets, so every way the example runs (native from any
/// directory, `just web`, a preview slot) finds it.
const MODEL_PATH: &str = "playground/demon.glb";
/// The clay: an off-white body and a grey for the hair.
pub const CLAY: Color = Color::srgb(0.87, 0.86, 0.83);
const CLAY_DARK: Color = Color::srgb(0.46, 0.46, 0.47);
/// The model is 3 m tall; the players are 1.5 m ([`PlayerShape`]'s default).
const MODEL_SCALE: f32 = 0.5;
/// The model faces +Z, a player's look -Z.
const MODEL_TURN: f32 = PI;

/// The clips, in the order `trim_demon.py` writes them.
const RUN_CLIP: usize = 0;
const JUMP_CLIP: usize = 1;
const STAND_CLIP: usize = 2;

/// Horizontal speed (m/s) the run plays at its own pace; faster runs play it faster.
const RUN_SPEED_AT_1X: f32 = 3.5;
/// The run's playback speed stays in this range, so a slow walk still steps.
const RUN_PLAYBACK: (f32, f32) = (0.6, 2.5);
/// Below this horizontal speed (m/s) a player on the ground stands.
const STAND_BELOW: f32 = 0.5;
/// Time (s) on the ground before a jump gives way to the run or the stand: a bunnyhop touches
/// down for one tick, and the jump plays again from the takeoff instead of flashing the run.
const LANDING_SECS: f32 = 0.1;
/// Time (s) below [`STAND_BELOW`] before a run gives way to the stand: a remote player's sampled
/// velocity is zero for a frame when its playback stalls, and a turn can slow it for a moment.
const STOPPING_SECS: f32 = 0.15;
/// Crossfade between clips.
const FADE: Duration = Duration::from_millis(150);
/// How fast a demon turns to its look's yaw (rad/s): a look that flicks (a strafe-jumper's at
/// each landing) turns the body in a few frames rather than one.
const TURN_RATE: f32 = 14.0;

/// Loads the demon and puts one on every player's visual.
pub struct DemonPlugin;

impl Plugin for DemonPlugin {
    fn build(&self, app: &mut App) {
        app.world_mut()
            .resource_mut::<EmbeddedAssetRegistry>()
            .insert_asset(
                PathBuf::new(),
                Path::new(MODEL_PATH),
                include_bytes!("../assets/demon.glb").as_slice(),
            );
        app.init_resource::<FlatMeshes>()
            .add_systems(Startup, load_demon)
            .add_systems(
                Update,
                (attach_demons, animate_demons, fade_flinches)
                    .chain()
                    .after(NetAhoyClientSystems::Interpolate),
            );
    }
}

/// The model and its animation graph, shared by every demon.
#[derive(Resource)]
pub struct DemonAssets {
    scene: Handle<WorldAsset>,
    graph: Handle<AnimationGraph>,
    /// The graph's node for each clip: run, jump, stand.
    nodes: [AnimationNodeIndex; 3],
    /// The hair's clay, shared.
    hair: Handle<StandardMaterial>,
}

fn load_demon(
    mut commands: Commands,
    asset_server: Res<AssetServer>,
    mut graphs: ResMut<Assets<AnimationGraph>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let path = format!("embedded://{MODEL_PATH}");
    let clip = |index| asset_server.load(GltfAssetLabel::Animation(index).from_asset(path.clone()));
    let (graph, nodes) =
        AnimationGraph::from_clips([clip(RUN_CLIP), clip(JUMP_CLIP), clip(STAND_CLIP)]);
    commands.insert_resource(DemonAssets {
        scene: asset_server.load(GltfAssetLabel::Scene(0).from_asset(path)),
        graph: graphs.add(graph),
        nodes: nodes.try_into().expect("three clips, three nodes"),
        hair: materials.add(clay(CLAY_DARK)),
    });
}

/// Matte clay in `color`: rough, with almost no highlight.
pub fn clay(color: Color) -> StandardMaterial {
    StandardMaterial {
        base_color: color,
        perceptual_roughness: 1.0,
        reflectance: 0.08,
        ..default()
    }
}

/// Each of the model's meshes with flat normals, one face one normal, made the first time a
/// demon dresses: the faceted look of a clay figurine.
#[derive(Resource, Default)]
struct FlatMeshes(HashMap<AssetId<Mesh>, Handle<Mesh>>);

/// The flat-shaded copies of the model's meshes.
#[derive(SystemParam)]
struct FlatShading<'w> {
    copies: ResMut<'w, FlatMeshes>,
    meshes: ResMut<'w, Assets<Mesh>>,
}

impl FlatShading<'_> {
    /// `mesh`'s flat-shaded copy, or `mesh` itself if its data has gone to the GPU.
    fn of(&mut self, mesh: &Handle<Mesh>) -> Handle<Mesh> {
        if let Some(flat) = self.copies.0.get(&mesh.id()) {
            return flat.clone();
        }
        let Some(mut flat) = self.meshes.get(mesh).cloned() else {
            return mesh.clone();
        };
        if flat.try_duplicate_vertices().is_err() || flat.try_compute_flat_normals().is_err() {
            return mesh.clone();
        }
        let flat = self.meshes.add(flat);
        self.copies.0.insert(mesh.id(), flat.clone());
        flat
    }
}

/// On a player's visual: its demon's body, which flinches red for a moment when hit.
#[derive(Component)]
pub struct DemonFlinch {
    skin: Handle<StandardMaterial>,
    /// Seconds of the flinch left.
    left: f32,
}

impl DemonFlinch {
    const SECONDS: f32 = 0.3;
    const COLOR: Color = Color::srgb(1.0, 0.18, 0.12);

    /// Starts the flinch over.
    pub fn start(&mut self) {
        self.left = Self::SECONDS;
    }
}

/// `victim`'s demon flinches, if it is a remote player's.
pub fn flinch(visuals: &mut Query<(&RemoteVisual, &mut DemonFlinch)>, victim: Option<PlayerId>) {
    for (visual, mut flinch) in visuals {
        if Some(visual.player_id) == victim {
            flinch.start();
        }
    }
}

/// Fades each flinch from red back to clay.
fn fade_flinches(
    time: Res<Time>,
    mut flinches: Query<&mut DemonFlinch>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    for mut flinch in &mut flinches {
        if flinch.left <= 0.0 {
            continue;
        }
        flinch.left = (flinch.left - time.delta_secs()).max(0.0);
        if let Some(mut skin) = materials.get_mut(&flinch.skin) {
            let red = flinch.left / DemonFlinch::SECONDS;
            skin.base_color = CLAY.mix(&DemonFlinch::COLOR, red);
        }
    }
}

/// What a demon is doing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Gait {
    #[default]
    Stand,
    Run,
    Jump,
}

impl Gait {
    fn node(self, assets: &DemonAssets) -> AnimationNodeIndex {
        assets.nodes[match self {
            Self::Run => RUN_CLIP,
            Self::Jump => JUMP_CLIP,
            Self::Stand => STAND_CLIP,
        }]
    }
}

/// On a player's visual: its demon and what it was doing last frame.
#[derive(Component)]
pub struct Demon {
    /// The model's root, a child of the visual.
    model: Entity,
    /// The model's [`AnimationPlayer`], once its scene has spawned.
    player: Option<Entity>,
    gait: Gait,
    grounded: bool,
    /// Seconds on the ground since the last landing.
    grounded_secs: f32,
    /// Seconds below [`STAND_BELOW`] since it last moved faster.
    slow_secs: f32,
}

/// On a demon's root: the visual it stands on, and its body's material.
#[derive(Component)]
struct DemonModel {
    visual: Entity,
    skin: Handle<StandardMaterial>,
}

/// The model's transform under a player's centre, facing `yaw`: its feet on the ground, scaled
/// to the player.
pub fn model_transform(shape: &PlayerShape, yaw: f32) -> Transform {
    Transform::from_xyz(0.0, -shape.half_height, 0.0)
        .with_rotation(Quat::from_rotation_y(yaw + MODEL_TURN))
        .with_scale(Vec3::splat(MODEL_SCALE))
}

/// A clay demon on every new visual, this player's and the others' alike.
fn attach_demons(
    mut commands: Commands,
    assets: Res<DemonAssets>,
    shape: Res<PlayerShape>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    visuals: Query<Entity, Or<(Added<OwnVisual>, Added<RemoteVisual>)>>,
) {
    for visual in &visuals {
        // Its own, so a hit can tint this one alone.
        let skin = materials.add(clay(CLAY));
        let model = commands
            .spawn((
                Name::new("demon"),
                DemonModel {
                    visual,
                    skin: skin.clone(),
                },
                WorldAssetRoot(assets.scene.clone()),
                model_transform(&shape, 0.0),
                ChildOf(visual),
            ))
            .observe(dress_demon)
            .id();
        commands
            .entity(visual)
            .insert(DemonFlinch { skin, left: 0.0 });
        commands.entity(visual).insert(Demon {
            model,
            player: None,
            gait: Gait::default(),
            grounded: true,
            grounded_secs: LANDING_SECS,
            slow_secs: STOPPING_SECS,
        });
    }
}

/// Once a demon's scene is in: clay on it, flat shaded, and its animation player hooked to the
/// graph, standing.
fn dress_demon(
    ready: On<WorldInstanceReady>,
    mut commands: Commands,
    assets: Res<DemonAssets>,
    models: Query<&DemonModel>,
    children: Query<&Children>,
    parts: Query<(&GltfMaterialName, &Mesh3d)>,
    mut flat: FlatShading,
    mut players: Query<&mut AnimationPlayer>,
    mut demons: Query<&mut Demon>,
) {
    let root = ready.event_target();
    let Ok(model) = models.get(root) else {
        return;
    };
    for entity in children.iter_descendants(root) {
        if let Ok((material, mesh)) = parts.get(entity) {
            let skin = if material.0 == "hair" {
                assets.hair.clone()
            } else {
                model.skin.clone()
            };
            commands
                .entity(entity)
                .insert((MeshMaterial3d(skin), Mesh3d(flat.of(&mesh.0))));
        }
        if let Ok(mut player) = players.get_mut(entity) {
            let mut transitions = AnimationTransitions::new();
            transitions
                .play(&mut player, Gait::Stand.node(&assets), Duration::ZERO)
                .repeat();
            commands
                .entity(entity)
                .insert((AnimationGraphHandle(assets.graph.clone()), transitions));
            if let Ok(mut demon) = demons.get_mut(model.visual) {
                demon.player = Some(entity);
            }
        }
    }
}

/// How a player moves this frame, from what the library gives its visual.
struct Motion {
    grounded: bool,
    velocity: Vec3,
    yaw: f32,
}

/// Faces each demon along its look and picks and paces its clip.
fn animate_demons(
    time: Res<Time>,
    assets: Res<DemonAssets>,
    mut demons: Query<(&mut Demon, Option<&RemoteSampledFrame>, Option<&OwnVisual>)>,
    predictions: Query<&PlayerState<Arsenal>, With<PredictedPlayer>>,
    mut models: Query<&mut Transform, With<DemonModel>>,
    mut players: Query<(&mut AnimationPlayer, &mut AnimationTransitions)>,
) {
    for (mut demon, remote, own) in &mut demons {
        let motion = if let Some(frame) = remote {
            Motion {
                grounded: frame.0.move_flags.grounded,
                velocity: frame.0.velocity,
                yaw: frame.0.look.x,
            }
        } else if let Some(state) = own.and_then(|own| predictions.get(own.prediction_entity).ok())
        {
            Motion {
                grounded: state.moves.ground.is_some(),
                velocity: state.moves.velocity,
                yaw: state.moves.orientation.to_euler(EulerRot::YXZ).0,
            }
        } else {
            continue;
        };

        if let Ok(mut transform) = models.get_mut(demon.model) {
            let facing = Quat::from_rotation_y(motion.yaw + MODEL_TURN);
            transform.rotation = transform
                .rotation
                .rotate_towards(facing, TURN_RATE * time.delta_secs());
        }

        let took_off = demon.grounded && !motion.grounded;
        demon.grounded_secs = if motion.grounded {
            demon.grounded_secs + time.delta_secs()
        } else {
            0.0
        };
        demon.grounded = motion.grounded;
        let speed = motion.velocity.xz().length();
        demon.slow_secs = if speed < STAND_BELOW {
            demon.slow_secs + time.delta_secs()
        } else {
            0.0
        };
        let gait = if !motion.grounded
            || (demon.gait == Gait::Jump && demon.grounded_secs < LANDING_SECS)
        {
            Gait::Jump
        } else if demon.slow_secs < STOPPING_SECS {
            Gait::Run
        } else {
            Gait::Stand
        };

        let Some(Ok((mut player, mut transitions))) = demon.player.map(|e| players.get_mut(e))
        else {
            continue;
        };
        if gait != demon.gait || took_off {
            let animation = transitions
                .play(&mut player, gait.node(&assets), FADE)
                .set_weight(1.0);
            // The jump plays once from the takeoff and holds its last pose; the others loop.
            if gait != Gait::Jump {
                animation.repeat();
            }
            demon.gait = gait;
        }
        if gait == Gait::Run
            && let Some(run) = player.animation_mut(Gait::Run.node(&assets))
        {
            run.set_speed((speed / RUN_SPEED_AT_1X).clamp(RUN_PLAYBACK.0, RUN_PLAYBACK.1));
        }
    }
}

/// A demon's pose: every bone's local transform, by name.
#[derive(Clone, Debug, Default)]
pub struct DemonPose(Vec<(Name, Transform)>);

impl DemonPose {
    /// The pose `visual`'s demon is drawn in now, if it has one.
    pub fn of(
        visual: Entity,
        demons: &Query<&Demon>,
        children: &Query<&Children>,
        bones: &Query<(&Name, &Transform)>,
    ) -> Option<Self> {
        let demon = demons.get(visual).ok()?;
        let pose: Vec<_> = children
            .iter_descendants(demon.model)
            .filter_map(|entity| bones.get(entity).ok())
            .map(|(name, transform)| (name.clone(), *transform))
            .collect();
        (!pose.is_empty()).then_some(Self(pose))
    }
}

/// On a frozen demon's root: the pose and material it takes once its scene is in.
#[derive(Component)]
struct Frozen {
    pose: DemonPose,
    material: Handle<StandardMaterial>,
}

/// A demon that never moves, in `pose` (the model's rest pose if empty) and `material` all over,
/// with `transform` from [`model_transform`], under `parent`.
pub fn spawn_frozen_demon(
    commands: &mut Commands,
    assets: &DemonAssets,
    pose: DemonPose,
    material: Handle<StandardMaterial>,
    transform: Transform,
    parent: Entity,
) {
    commands
        .spawn((
            Name::new("frozen demon"),
            Frozen { pose, material },
            WorldAssetRoot(assets.scene.clone()),
            transform,
            ChildOf(parent),
        ))
        .observe(freeze_demon);
}

/// Once a frozen demon's scene is in: its pose on the bones and its material on the meshes. No
/// animation graph, so its animation player plays nothing and the pose holds.
fn freeze_demon(
    ready: On<WorldInstanceReady>,
    mut commands: Commands,
    frozen: Query<&Frozen>,
    children: Query<&Children>,
    mut bones: Query<(&Name, &mut Transform)>,
    parts: Query<&Mesh3d>,
    mut flat: FlatShading,
) {
    let root = ready.event_target();
    let Ok(frozen) = frozen.get(root) else {
        return;
    };
    let pose: HashMap<&Name, &Transform> = frozen.pose.0.iter().map(|(n, t)| (n, t)).collect();
    for entity in children.iter_descendants(root) {
        if let Ok((name, mut transform)) = bones.get_mut(entity)
            && let Some(posed) = pose.get(name)
        {
            *transform = **posed;
        }
        if let Ok(mesh) = parts.get(entity) {
            commands.entity(entity).insert((
                MeshMaterial3d(frozen.material.clone()),
                Mesh3d(flat.of(&mesh.0)),
            ));
        }
    }
}
