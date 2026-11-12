//! Every word the client draws, in one style: [bevy_msdf_text] in Fira Sans Bold, white with a
//! black outline, crisp at any size and distance. The font is in the binary, as the demon is.

use std::path::{Path, PathBuf};

use bevy::{asset::io::embedded::EmbeddedAssetRegistry, prelude::*};
use bevy_msdf_text::{MsdfFont, MsdfText, MsdfTextMode, MsdfTextPlugin, TextAlign};

const FONT_PATH: &str = "playground/FiraSans-Bold.ttf";
/// The outline, in em: as wide as the atlas allows with room to spare.
const OUTLINE: f32 = 0.16;

/// The text renderer and the font.
pub struct TextPlugin;

impl Plugin for TextPlugin {
    fn build(&self, app: &mut App) {
        app.world_mut()
            .resource_mut::<EmbeddedAssetRegistry>()
            .insert_asset(
                PathBuf::new(),
                Path::new(FONT_PATH),
                include_bytes!("../assets/fonts/FiraSans-Bold.ttf").as_slice(),
            );
        app.add_plugins(MsdfTextPlugin);
        let font = app
            .world()
            .resource::<AssetServer>()
            .load::<MsdfFont>(format!("embedded://{FONT_PATH}"));
        app.insert_resource(Font(font));
    }
}

/// The font every text is set in.
#[derive(Resource, Clone, Deref)]
pub struct Font(Handle<MsdfFont>);

impl Font {
    /// `text` in the house style: white, outlined in black, centred, in `mode` at `size`.
    pub fn text(&self, text: impl Into<String>, mode: MsdfTextMode, size: f32) -> MsdfText {
        MsdfText::new(text, self.0.clone())
            .with_mode(mode)
            .with_size(size)
            .with_color(Color::WHITE)
            .with_outline(OUTLINE, Color::BLACK)
    }

    /// HUD text: `size` pixels per em, its `anchor` corner (see [`MsdfText::anchor`]) `offset`
    /// pixels from the viewport point `at` (0 to 1, from the top left), lines aligned `align`.
    pub fn hud(
        &self,
        text: impl Into<String>,
        size: f32,
        at: Vec2,
        offset: Vec2,
        anchor: Vec2,
        align: TextAlign,
    ) -> MsdfText {
        self.text(text, MsdfTextMode::Screen { at, offset }, size)
            .with_anchor(anchor)
            .with_align(align)
    }
}
