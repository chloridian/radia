use assets::{MaterialDescription, MaterialTexture, Sampler, TextureMapping};
use math::UvTransform;

use super::{AlphaBehavior, FaceMaterial};
use crate::texture::{EmissiveImage, MaterialTextures, TextureKey};

const UNTRANSFORMED_UV: [[f32; 4]; 2] = [[1.0, 0.0, 0.0, 0.0], [0.0, -1.0, 1.0, 0.0]];

#[derive(Clone, Copy, PartialEq)]
pub(crate) struct FaceParams {
    pub(crate) color: [f32; 4],
    pub(crate) uv: [[f32; 4]; 2],
    pub(crate) params: [f32; 4],
    pub(crate) emissive: [f32; 3],
    pub(crate) emissive_uv: [[f32; 4]; 2],
    pub(crate) textures: MaterialTextures,
    pub(crate) blend: bool,
    pub(crate) double_sided: bool,
    pub(crate) fullbright: bool,
}

impl FaceParams {
    pub(crate) fn is_visible(&self) -> bool {
        !(self.blend && self.color[3] <= 0.0)
    }

    pub(crate) fn new(face: Option<&FaceMaterial>) -> Self {
        Self::with_texture_entry_transform(face, None)
    }

    pub(crate) fn with_texture_entry_transform(face: Option<&FaceMaterial>, transform: Option<UvTransform>) -> Self {
        let mut result = Self {
            color: [0.38, 0.38, 0.38, 1.0],
            uv: UNTRANSFORMED_UV,
            params: [0.0; 4],
            emissive: [0.0; 3],
            emissive_uv: UNTRANSFORMED_UV,
            textures: MaterialTextures::default(),
            blend: false,
            double_sided: false,
            fullbright: false,
        };
        let Some(face) = face else { return result };
        result.color = face.base_color_factor();
        result.double_sided = face.double_sided();
        match face.alpha {
            AlphaBehavior::Mask(cutoff) => {
                result.params[1] = 1.0;
                result.params[2] = cutoff;
            }
            AlphaBehavior::Blend => {
                result.params[1] = 2.0;
                result.blend = true;
            }
            AlphaBehavior::Opaque | AlphaBehavior::Unknown => {}
        }
        let (matrix, sampler, supported) = match face.material {
            MaterialDescription::Legacy(material) => {
                result.params[0] = f32::from(u8::from(material.fullbright));
                result.fullbright = material.fullbright;
                result.params[3] = f32::from(u8::from(material.mapping == TextureMapping::Planar));
                (
                    legacy_uv(transform.unwrap_or(material.diffuse.transform)),
                    Sampler::default(),
                    !matches!(material.mapping, TextureMapping::Unsupported(_)),
                )
            }
            MaterialDescription::Pbr(material) => {
                result.emissive = material.emissive_factor;
                if let Some(uv) = pbr_texture_uv(material.emissive, transform) {
                    result.emissive_uv = uv;
                    result.textures.emissive = face.emissive_texture.as_ref().map_or(EmissiveImage::Absent, |binding| {
                        binding.image().map_or(EmissiveImage::Pending, |image| {
                            EmissiveImage::Ready(TextureKey { id: image.id(), sampler: material.emissive.sampler })
                        })
                    });
                }
                (pbr_matrix(material.base_color.transform, transform), material.base_color.sampler, material.base_color.tex_coord == 0)
            }
        };
        if supported && finite(&matrix) {
            result.uv = matrix;
            if let Some(texture) = face.base_color_texture.as_ref().and_then(|binding| binding.image()) {
                result.textures.base = Some(TextureKey { id: texture.id(), sampler });
            }
        }
        result
    }
}

fn pbr_texture_uv(texture: MaterialTexture, texture_entry: Option<UvTransform>) -> Option<[[f32; 4]; 2]> {
    let matrix = pbr_matrix(texture.transform, texture_entry);
    (texture.tex_coord == 0 && finite(&matrix)).then_some(matrix)
}

fn pbr_matrix(material: UvTransform, texture_entry: Option<UvTransform>) -> [[f32; 4]; 2] {
    texture_entry.map_or_else(|| pbr_uv(material), |texture_entry| pbr_with_texture_entry(material, texture_entry))
}

fn finite(matrix: &[[f32; 4]; 2]) -> bool {
    matrix.iter().flatten().all(|value| value.is_finite())
}

fn legacy_uv(transform: UvTransform) -> [[f32; 4]; 2] {
    let (s, c) = f64::from(transform.rotation).sin_cos();
    let [x, y] = transform.scale.map(f64::from);
    let [u, v] = transform.offset.map(f64::from);
    [
        [x * c, x * s, u + 0.5 - 0.5 * x * (c + s), 0.0].map(|n| n as f32),
        [y * s, -y * c, 0.5 - v + 0.5 * y * (c - s), 0.0].map(|n| n as f32),
    ]
}

fn pbr_uv(transform: UvTransform) -> [[f32; 4]; 2] {
    let (s, c) = f64::from(transform.rotation).sin_cos();
    let [x, y] = transform.scale.map(f64::from);
    let [u, v] = transform.offset.map(f64::from);
    [[c * x, s * y, u - s * y, 0.0].map(|n| n as f32), [s * x, -c * y, v + c * y, 0.0].map(|n| n as f32)]
}

fn pbr_with_texture_entry(material: UvTransform, texture_entry: UvTransform) -> [[f32; 4]; 2] {
    let texture_entry = legacy_uv(texture_entry);
    let unflipped = [texture_entry[0], [-texture_entry[1][0], -texture_entry[1][1], 1.0 - texture_entry[1][2], 0.0]];
    compose_uv(pbr_uv(material), unflipped)
}

fn compose_uv(first: [[f32; 4]; 2], second: [[f32; 4]; 2]) -> [[f32; 4]; 2] {
    first.map(|row| {
        [
            row[0] * second[0][0] + row[1] * second[1][0],
            row[0] * second[0][1] + row[1] * second[1][1],
            row[0] * second[0][2] + row[1] * second[1][2] + row[2],
            0.0,
        ]
    })
}

#[cfg(test)]
mod tests {
    use std::f32::consts::FRAC_PI_2;

    use assets::{LegacyMaterial, MaterialTexture, PbrMaterial};
    use utils::AssetId;
    use uuid::Uuid;

    use super::*;
    use crate::material::{AssetState, Resolution};

    fn mapped(matrix: [[f32; 4]; 2], uv: [f32; 2]) -> [f32; 2] {
        matrix.map(|row| row[0] * uv[0] + row[1] * uv[1] + row[2])
    }

    #[test]
    fn legacy_transform_order() {
        let matrix = legacy_uv(UvTransform { offset: [0.125, -0.25], scale: [2.0, 3.0], rotation: FRAC_PI_2 });
        let actual = mapped(matrix, [0.75, 0.625]);
        for (actual, expected) in actual.into_iter().zip([0.875, 1.5]) {
            assert!((actual - expected).abs() < 1e-6, "legacy UVs rotate about center before scaling, then flip V");
        }
        assert_eq!(mapped(legacy_uv(UvTransform::default()), [0.0, 0.0]), [0.0, 1.0]);
    }

    #[test]
    fn pbr_transform_order() {
        let matrix = pbr_uv(UvTransform { offset: [0.125, -0.25], scale: [2.0, 3.0], rotation: FRAC_PI_2 });
        let actual = mapped(matrix, [0.75, 0.625]);
        for (actual, expected) in actual.into_iter().zip([-1.0, 1.25]) {
            assert!((actual - expected).abs() < 1e-6, "glTF UVs flip V before scale, origin rotation, and translation");
        }
        assert_eq!(mapped(pbr_uv(UvTransform::default()), [0.0, 1.0]), [0.0, 0.0]);
    }

    #[test]
    fn pbr_texture_transforms_compose() {
        let material_transform = UvTransform { offset: [0.25, -0.125], scale: [2.0, 3.0], rotation: FRAC_PI_2 };
        let texture_entry_transform = UvTransform { offset: [-0.25, 0.125], scale: [0.5, 1.5], rotation: FRAC_PI_2 };
        let face = FaceMaterial {
            slot: 0,
            material: MaterialDescription::Pbr(PbrMaterial {
                base_color: MaterialTexture { transform: material_transform, ..MaterialTexture::default() },
                ..PbrMaterial::default()
            }),
            legacy_state: None,
            pbr_state: None,
            alpha: AlphaBehavior::Opaque,
            base_color_texture: None,
            emissive_texture: None,
        };

        let params = FaceParams::with_texture_entry_transform(Some(&face), Some(texture_entry_transform));
        let expected = [[-4.5, 0.0, 1.375, 0.0], [0.0, 1.0, -0.125, 0.0]];
        for (actual, expected) in params.uv.iter().flatten().zip(expected.iter().flatten()) {
            assert!((actual - expected).abs() < 1e-6, "UV matrix coefficient {actual} differed from {expected}");
        }
    }

    #[test]
    fn fullbright_comes_from_legacy_only() {
        let face = |material| FaceMaterial {
            slot: 0,
            material,
            legacy_state: None,
            pbr_state: None,
            alpha: AlphaBehavior::Opaque,
            base_color_texture: None,
            emissive_texture: None,
        };
        for (material, fullbright, unlit_shading) in [
            (MaterialDescription::Legacy(LegacyMaterial { fullbright: true, ..LegacyMaterial::default() }), true, 1.0),
            (MaterialDescription::Legacy(LegacyMaterial::default()), false, 0.0),
            (MaterialDescription::Pbr(PbrMaterial::default()), false, 0.0),
        ] {
            let params = FaceParams::new(Some(&face(material)));
            assert_eq!(params.fullbright, fullbright, "{material:?} metering class");
            assert_eq!(params.params[0], unlit_shading, "{material:?} unlit shading");
        }
    }

    #[test]
    fn fallback_preserves_material() {
        let id = AssetId::new(Uuid::from_u128(42));
        let mut material = PbrMaterial {
            base_color_factor: [0.2, 0.3, 0.4, 0.5],
            double_sided: true,
            base_color: MaterialTexture { tex_coord: 1, ..Default::default() },
            ..Default::default()
        };
        let mut face = FaceMaterial {
            slot: 0,
            material: MaterialDescription::Pbr(material),
            legacy_state: None,
            pbr_state: None,
            alpha: AlphaBehavior::Mask(0.7),
            base_color_texture: None,
            emissive_texture: None,
        };
        face.resolve_base_color(AssetState { asset: id, resolution: Resolution::Pending }, None);
        let params = FaceParams::new(Some(&face));
        assert_eq!(params.color, material.base_color_factor);
        assert_eq!(params.params, [0.0, 1.0, 0.7, 0.0]);
        assert!(params.double_sided);
        assert!(!params.blend);
        assert_eq!(params.textures.base, None, "unsupported UV sets use a neutral image");
        material.base_color.tex_coord = 0;
        face.material = MaterialDescription::Pbr(material);
        face.alpha = AlphaBehavior::Blend;
        let params = FaceParams::new(Some(&face));
        assert_eq!(params.textures.base, None, "pending assets do not own a GPU texture");
        assert!(params.blend);
        assert_eq!(params.params[1], 2.0);
    }
}
