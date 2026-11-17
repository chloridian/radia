mod json;
pub(crate) mod legacy;
pub(crate) mod pbr;

pub use legacy::{LegacyAlphaMode, LegacyMaterial, TextureMapping};
pub use pbr::{PbrAlphaMode, PbrMaterial};

use math::UvTransform;
use utils::AssetId;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Filter {
    Nearest,
    Linear,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum MinFilter {
    Nearest,
    Linear,
    NearestMipmapNearest,
    LinearMipmapNearest,
    NearestMipmapLinear,
    LinearMipmapLinear,
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum Wrap {
    ClampToEdge,
    MirroredRepeat,
    #[default]
    Repeat,
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct Sampler {
    pub mag_filter: Option<Filter>,
    pub min_filter: Option<MinFilter>,
    pub wrap_s: Wrap,
    pub wrap_t: Wrap,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TextureRef {
    Absent,
    Clear,
    Asset(AssetId),
}

impl From<Uuid> for TextureRef {
    fn from(id: Uuid) -> Self {
        if id.is_nil() || id == Uuid::max() { Self::Clear } else { Self::Asset(AssetId::new(id)) }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MaterialTexture {
    pub source: TextureRef,
    pub transform: UvTransform,
    pub tex_coord: u32,
    pub sampler: Sampler,
}

impl Default for MaterialTexture {
    fn default() -> Self {
        Self { source: TextureRef::Absent, transform: UvTransform::default(), tex_coord: 0, sampler: Sampler::default() }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MaterialDescription {
    Legacy(LegacyMaterial),
    Pbr(PbrMaterial),
}

impl MaterialDescription {
    pub fn base_color_texture(&self) -> &MaterialTexture {
        match self {
            Self::Legacy(material) => &material.diffuse,
            Self::Pbr(material) => &material.base_color,
        }
    }
}
