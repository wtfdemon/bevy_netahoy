//! The level's dev textures, made at startup: a flat colour with a grid on it, a heavy line
//! every metre and a light one every quarter, in the style of a Hammer test map. Mipmapped and
//! sampled anisotropically, so the floor's grid stays clean to the horizon. Boxes get their UVs
//! in metres, so the grid is the same size on every face.

use bevy::{
    asset::RenderAssetUsages,
    image::{ImageAddressMode, ImageFilterMode, ImageSampler, ImageSamplerDescriptor},
    mesh::VertexAttributeValues,
    prelude::*,
    render::render_resource::{Extent3d, TextureDimension, TextureFormat},
};

/// Texels per metre, and so per tile.
const SIZE: usize = 256;
/// The heavy line at each metre's edge, and the light one every quarter (texels).
const MAJOR_WIDTH: f32 = 7.0;
const MINOR_WIDTH: f32 = 2.5;
/// How much each line darkens the colour under it.
const MAJOR_SHADE: f32 = 0.70;
const MINOR_SHADE: f32 = 0.86;

/// A dev texture in `color`, a tile a metre square.
pub fn grid_texture(color: Srgba) -> Image {
    let base = LinearRgba::from(color);
    let base = Vec3::new(base.red, base.green, base.blue);
    // The top level, each texel the mean of 4x4 samples, in linear light.
    let mut level: Vec<Vec3> = (0..SIZE * SIZE)
        .map(|index| {
            let (x, y) = ((index % SIZE) as f32, (index / SIZE) as f32);
            let mut sum = 0.0;
            for sample in 0..16 {
                let u = x + (sample % 4) as f32 / 4.0 + 0.125;
                let v = y + (sample / 4) as f32 / 4.0 + 0.125;
                sum += line_shade(u).min(line_shade(v));
            }
            base * (sum / 16.0)
        })
        .collect();

    let mut data = Vec::new();
    let mut size = SIZE;
    let mut levels = 0;
    loop {
        data.extend(level.iter().flat_map(|texel| {
            let srgb = Srgba::from(LinearRgba::rgb(texel.x, texel.y, texel.z));
            srgb.to_u8_array()
        }));
        levels += 1;
        if size == 1 {
            break;
        }
        // The next level down: each texel the mean of the four above it.
        let half = size / 2;
        level = (0..half * half)
            .map(|index| {
                let (x, y) = (2 * (index % half), 2 * (index / half));
                (level[y * size + x]
                    + level[y * size + x + 1]
                    + level[(y + 1) * size + x]
                    + level[(y + 1) * size + x + 1])
                    / 4.0
            })
            .collect();
        size = half;
    }

    let mut image = Image::new_uninit(
        Extent3d {
            width: SIZE as u32,
            height: SIZE as u32,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::RENDER_WORLD,
    );
    image.data = Some(data);
    image.texture_descriptor.mip_level_count = levels;
    image.sampler = ImageSampler::Descriptor(ImageSamplerDescriptor {
        address_mode_u: ImageAddressMode::Repeat,
        address_mode_v: ImageAddressMode::Repeat,
        mag_filter: ImageFilterMode::Linear,
        min_filter: ImageFilterMode::Linear,
        mipmap_filter: ImageFilterMode::Linear,
        anisotropy_clamp: 16,
        ..default()
    });
    image
}

/// How much the grid darkens at `texel` along one axis: the heavy line at the tile's edge, a
/// light one at each quarter, 1 between them.
fn line_shade(texel: f32) -> f32 {
    let quarter = SIZE as f32 / 4.0;
    let from_edge = texel.min(SIZE as f32 - texel);
    if from_edge < MAJOR_WIDTH / 2.0 {
        return MAJOR_SHADE;
    }
    let from_quarter = (texel % quarter).min(quarter - texel % quarter);
    if from_quarter < MINOR_WIDTH / 2.0 {
        MINOR_SHADE
    } else {
        1.0
    }
}

/// A box of `size` whose UVs are its faces' positions in metres, offset by `origin`: each face
/// projected along its normal, so the grid is a metre square on every face, and lines up across
/// neighbouring axis-aligned boxes given their translation as `origin`.
pub fn grid_box(size: Vec3, origin: Vec3) -> Mesh {
    let mut mesh = Mesh::from(Cuboid::from_size(size));
    let Some(VertexAttributeValues::Float32x3(positions)) =
        mesh.attribute(Mesh::ATTRIBUTE_POSITION)
    else {
        return mesh;
    };
    let Some(VertexAttributeValues::Float32x3(normals)) = mesh.attribute(Mesh::ATTRIBUTE_NORMAL)
    else {
        return mesh;
    };
    let uvs: Vec<[f32; 2]> = positions
        .iter()
        .zip(normals)
        .map(|(position, normal)| {
            let p = Vec3::from(*position) + origin;
            let n = Vec3::from(*normal).abs();
            if n.x > 0.5 {
                [p.z, -p.y]
            } else if n.y > 0.5 {
                [p.x, p.z]
            } else {
                [p.x, -p.y]
            }
        })
        .collect();
    mesh.insert_attribute(Mesh::ATTRIBUTE_UV_0, uvs);
    mesh
}
