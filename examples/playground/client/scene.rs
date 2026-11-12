//! The world the client draws: daylight, a sky, the level in dev textures, its signs, and the
//! camera. Purely presentation; the level's colliders come from `playground::level`, the same
//! on both peers.

use bevy::{
    asset::RenderAssetUsages,
    camera::Exposure,
    light::{CascadeShadowConfigBuilder, NotShadowCaster, NotShadowReceiver},
    mesh::{Indices, PrimitiveTopology},
    pbr::{DistanceFog, FogFalloff},
    prelude::*,
};
use bevy_msdf_text::MsdfTextMode;
use playground::level::{self, SPAWN_POINT, Surface};

use crate::{dev_texture, text::Font};

/// The sky straight up, and at the horizon, where the fog meets it.
const ZENITH: Color = Color::srgb(0.24, 0.50, 0.88);
const HORIZON: Color = Color::srgb(0.76, 0.85, 0.95);
/// The sun's light: a little warm.
const SUNLIGHT: Color = Color::srgb(1.0, 0.95, 0.86);
/// The dev textures' colours.
const ORANGE: Srgba = Srgba::rgb(0.89, 0.49, 0.20);
const GREY: Srgba = Srgba::rgb(0.66, 0.66, 0.66);
const BLUE: Srgba = Srgba::rgb(0.33, 0.58, 0.86);
/// The sky dome's radius: inside the camera's far plane.
const SKY_RADIUS: f32 = 900.0;

/// The world, lit and textured, and the camera.
pub struct ScenePlugin;

impl Plugin for ScenePlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(ClearColor(HORIZON))
            .insert_resource(GlobalAmbientLight {
                color: Color::srgb(0.80, 0.87, 1.0),
                brightness: 1_100.0,
                ..default()
            })
            .add_systems(Startup, (load_materials, setup_scene).chain())
            .add_systems(PostUpdate, follow_sky.before(TransformSystems::Propagate));
    }
}

/// A material per [`Surface`], shared by every block painted with it.
#[derive(Resource)]
pub struct DevMaterials {
    orange: Handle<StandardMaterial>,
    grey: Handle<StandardMaterial>,
    /// The moving platform's: the one thing in the level that moves.
    pub blue: Handle<StandardMaterial>,
}

impl DevMaterials {
    pub fn get(&self, surface: Surface) -> Handle<StandardMaterial> {
        match surface {
            Surface::Orange => self.orange.clone(),
            Surface::Grey => self.grey.clone(),
        }
    }
}

fn load_materials(
    mut commands: Commands,
    mut images: ResMut<Assets<Image>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let mut dev = |color: Srgba| {
        materials.add(StandardMaterial {
            base_color_texture: Some(images.add(dev_texture::grid_texture(color))),
            perceptual_roughness: 0.92,
            reflectance: 0.25,
            ..default()
        })
    };
    commands.insert_resource(DevMaterials {
        orange: dev(ORANGE),
        grey: dev(GREY),
        blue: dev(BLUE),
    });
}

/// Marks the sky dome, which stays centred on the camera.
#[derive(Component)]
struct Sky;

fn setup_scene(
    mut commands: Commands,
    font: Res<Font>,
    dev: Res<DevMaterials>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    // The sun, high behind the spawn and to its right, so the faces the spawn looks at are lit.
    commands.spawn((
        DirectionalLight {
            color: SUNLIGHT,
            illuminance: 7_000.0,
            shadow_maps_enabled: true,
            ..default()
        },
        Transform::from_xyz(18.0, 40.0, 30.0).looking_at(Vec3::ZERO, Vec3::Y),
        CascadeShadowConfigBuilder {
            num_cascades: 3,
            first_cascade_far_bound: 14.0,
            maximum_distance: 140.0,
            ..default()
        }
        .build(),
    ));

    commands.spawn((
        Name::new("sky"),
        Sky,
        Mesh3d(meshes.add(sky_dome())),
        MeshMaterial3d(materials.add(StandardMaterial {
            unlit: true,
            fog_enabled: false,
            cull_mode: None,
            ..default()
        })),
        NotShadowCaster,
        NotShadowReceiver,
    ));

    for block in level::blocks() {
        // Axis-aligned blocks share one grid in world space; a turned one keeps its own.
        let origin = if block.rotation == Quat::IDENTITY {
            block.translation
        } else {
            Vec3::ZERO
        };
        let mut entity = commands.spawn((
            Name::new(block.name),
            Mesh3d(meshes.add(dev_texture::grid_box(block.size, origin))),
            MeshMaterial3d(dev.get(block.surface)),
            block.transform(),
        ));
        if block.name == "floor" {
            entity.insert(NotShadowCaster);
        }
    }
    level::spawn_colliders(&mut commands);

    for sign in level::signs() {
        let down = sign.rotation * Vec3::NEG_Y;
        commands.spawn((
            Name::new(format!("sign: {}", sign.title)),
            font.text(sign.title, MsdfTextMode::Plane, sign.size),
            Transform::from_translation(sign.at).with_rotation(sign.rotation),
        ));
        commands.spawn((
            Name::new(format!("sign: {} (text)", sign.title)),
            font.text(sign.body, MsdfTextMode::Plane, sign.size * 0.36)
                .with_anchor(Vec2::new(0.0, 0.5)),
            Transform::from_translation(sign.at + down * sign.size * 0.68)
                .with_rotation(sign.rotation),
        ));
    }

    commands.spawn((
        Camera3d::default(),
        Projection::Perspective(PerspectiveProjection {
            fov: 80.0_f32.to_radians(),
            ..default()
        }),
        Exposure::default(),
        DistanceFog {
            color: HORIZON,
            directional_light_color: Color::srgba(1.0, 0.92, 0.78, 0.35),
            directional_light_exponent: 24.0,
            falloff: FogFalloff::Linear {
                start: 30.0,
                end: 420.0,
            },
        },
        Transform::from_translation(SPAWN_POINT + Vec3::new(0.0, 3.0, 7.0))
            .looking_at(SPAWN_POINT, Vec3::Y),
    ));
}

/// A sphere inside out, its vertex colours fading from [`HORIZON`] at and below the horizon to
/// [`ZENITH`] overhead.
fn sky_dome() -> Mesh {
    const RINGS: usize = 24;
    const SEGMENTS: usize = 48;
    let (horizon, zenith) = (HORIZON.to_linear(), ZENITH.to_linear());
    let mut positions = Vec::new();
    let mut colors = Vec::new();
    for ring in 0..=RINGS {
        // From straight up to straight down.
        let polar = ring as f32 / RINGS as f32 * std::f32::consts::PI;
        let height = polar.cos();
        let blend = height.max(0.0).powf(0.6);
        let color = horizon.mix(&zenith, blend);
        for segment in 0..=SEGMENTS {
            let azimuth = segment as f32 / SEGMENTS as f32 * std::f32::consts::TAU;
            positions.push(
                Vec3::new(
                    polar.sin() * azimuth.cos(),
                    height,
                    polar.sin() * azimuth.sin(),
                ) * SKY_RADIUS,
            );
            colors.push(color.to_f32_array());
        }
    }
    let mut indices = Vec::new();
    let row = SEGMENTS as u32 + 1;
    for ring in 0..RINGS as u32 {
        for segment in 0..SEGMENTS as u32 {
            let a = ring * row + segment;
            let b = a + row;
            indices.extend([a, b, a + 1, a + 1, b, b + 1]);
        }
    }
    Mesh::new(
        PrimitiveTopology::TriangleList,
        RenderAssetUsages::RENDER_WORLD,
    )
    .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, positions)
    .with_inserted_attribute(Mesh::ATTRIBUTE_COLOR, colors)
    .with_inserted_indices(Indices::U32(indices))
}

fn follow_sky(
    camera: Single<&Transform, (With<Camera3d>, Without<Sky>)>,
    mut sky: Single<&mut Transform, With<Sky>>,
) {
    sky.translation = camera.translation;
}
