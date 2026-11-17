use std::{collections::BTreeMap, num::NonZeroU32};

use llsd::{Limits, Map, Value, notation};
use protocol::messages::{GenericStreamingMessage, generic_streaming_message::MAX_DATA_BYTES};
use utils::AssetId;
use uuid::Uuid;
use world::{
    FieldUpdate, MAX_SURFACE_FACES, PbrAlphaMode, PbrMaterialOverride, PbrMaterialOverrides, PbrTextureOverride,
    PbrTextureTransformOverride,
};

const METHOD: u16 = 0x4175;
const LLSD_LIMITS: Limits = Limits {
    max_bytes: MAX_DATA_BYTES,
    max_depth: 8,
    max_values: 2048,
    max_container_items: 256,
    max_string_bytes: 512,
    max_map_key_bytes: 16,
    max_binary_bytes: 0,
};

pub(in crate::session) struct Decoded {
    pub(in crate::session) local_id: NonZeroU32,
    pub(in crate::session) overrides: PbrMaterialOverrides,
}

pub(super) fn decode_message(message: &GenericStreamingMessage) -> Option<Result<Decoded, ()>> {
    if message.method != METHOD {
        return None;
    }

    Some(decode_payload(&message.data))
}

pub(in crate::session) fn decode_payload(bytes: &[u8]) -> Result<Decoded, ()> {
    let bytes = bytes.strip_suffix(b"\0").unwrap_or(bytes);
    let payload: Value = notation::from_slice_with_limits(bytes, LLSD_LIMITS).map_err(|_| ())?;
    let Value::Map(payload) = payload else { return Err(()) };
    let local_id = match payload.get("id") {
        Some(Value::Integer(value)) => NonZeroU32::new(*value as u32).ok_or(())?,
        _ => return Err(()),
    };
    let faces = match payload.get("te") {
        Some(Value::Array(faces)) => faces,
        _ => return Err(()),
    };
    let materials = match payload.get("od") {
        Some(Value::Array(materials)) if materials.len() == faces.len() => materials,
        _ => return Err(()),
    };

    let mut overrides = BTreeMap::new();
    for (face, material) in faces.iter().zip(materials).take(MAX_SURFACE_FACES) {
        let face = face_index(face)?;
        let material = match material {
            Value::Map(material) => decode_material(material)?,
            Value::Undefined => PbrMaterialOverride::default(),
            _ => return Err(()),
        };
        overrides.insert(face, material);
    }
    let overrides = PbrMaterialOverrides::new(overrides.into_iter().collect()).map_err(|_| ())?;
    Ok(Decoded { local_id, overrides })
}

pub(in crate::session) fn encode_payload(local_id: NonZeroU32, overrides: &PbrMaterialOverrides) -> Option<Vec<u8>> {
    let (faces, materials) =
        overrides.faces().iter().map(|(face, material)| (Value::Integer(i32::from(*face)), encode_material(material))).unzip();
    let payload = Map::from([
        ("id".to_owned(), Value::Integer(local_id.get() as i32)),
        ("te".to_owned(), Value::Array(faces)),
        ("od".to_owned(), Value::Array(materials)),
    ]);
    notation::to_vec(&Value::Map(payload)).ok().filter(|bytes| bytes.len() <= MAX_DATA_BYTES)
}

fn face_index(value: &Value) -> Result<u8, ()> {
    match value {
        Value::Integer(index) if (0..MAX_SURFACE_FACES as i32).contains(index) => Ok(*index as u8),
        _ => Err(()),
    }
}

fn decode_material(fields: &Map) -> Result<PbrMaterialOverride, ()> {
    let mut material = PbrMaterialOverride::default();
    if let Some(value) = defined(fields, "tex") {
        let Value::Array(textures) = value else { return Err(()) };
        let slots = texture_slots(&mut material);
        if textures.len() > slots.len() {
            return Err(());
        }
        for (slot, texture) in slots.into_iter().zip(textures) {
            slot.texture = match texture {
                Value::Undefined => FieldUpdate::Keep,
                Value::Uuid(texture) if texture.is_nil() => FieldUpdate::Keep,
                Value::Uuid(texture) if *texture == Uuid::max() => FieldUpdate::Clear,
                Value::Uuid(texture) => FieldUpdate::Set(AssetId::new(*texture)),
                _ => return Err(()),
            };
        }
    }
    if let Some(value) = defined(fields, "bc") {
        material.base_color_factor = Some(vector(value)?);
    }
    if let Some(value) = defined(fields, "ec") {
        material.emissive_factor = Some(vector(value)?);
    }
    if let Some(value) = defined(fields, "mf") {
        material.metallic = Some(number(value)?);
    }
    if let Some(value) = defined(fields, "rf") {
        material.roughness = Some(number(value)?);
    }
    if let Some(value) = defined(fields, "am") {
        material.alpha_mode = Some(match value {
            Value::Integer(0) => PbrAlphaMode::Opaque,
            Value::Integer(1) => PbrAlphaMode::Blend,
            Value::Integer(2) => PbrAlphaMode::Mask,
            _ => return Err(()),
        });
    }
    if let Some(value) = defined(fields, "ac") {
        material.alpha_cutoff = Some(number(value)?);
    }
    if let Some(value) = defined(fields, "ds") {
        material.double_sided = Some(match value {
            Value::Boolean(value) => *value,
            _ => return Err(()),
        });
    }
    if let Some(value) = defined(fields, "ti") {
        let Value::Array(transforms) = value else { return Err(()) };
        let slots = texture_slots(&mut material);
        if transforms.len() > slots.len() {
            return Err(());
        }
        for (slot, transform) in slots.into_iter().zip(transforms) {
            slot.transform = match transform {
                Value::Undefined => PbrTextureTransformOverride::default(),
                Value::Map(transform) => decode_transform(transform)?,
                _ => return Err(()),
            };
        }
    }
    Ok(material)
}

fn decode_transform(fields: &Map) -> Result<PbrTextureTransformOverride, ()> {
    let mut transform = PbrTextureTransformOverride::default();
    if let Some(value) = defined(fields, "o") {
        transform.offset = Some(vector(value)?);
    }
    if let Some(value) = defined(fields, "s") {
        transform.scale = Some(vector(value)?);
    }
    if let Some(value) = defined(fields, "r") {
        transform.rotation = Some(number(value)?);
    }
    Ok(transform)
}

fn defined<'a>(fields: &'a Map, key: &str) -> Option<&'a Value> {
    fields.get(key).filter(|value| !matches!(value, Value::Undefined))
}

fn encode_material(material: &PbrMaterialOverride) -> Value {
    let mut fields = Map::new();
    let slots = [&material.base_color, &material.normal, &material.metallic_roughness, &material.emissive];
    if slots.iter().any(|slot| slot.texture != FieldUpdate::Keep) {
        let textures = slots.iter().map(|slot| {
            Value::Uuid(match slot.texture {
                FieldUpdate::Keep => Uuid::nil(),
                FieldUpdate::Clear => Uuid::max(),
                FieldUpdate::Set(texture) => texture.uuid(),
            })
        });
        fields.insert("tex".to_owned(), Value::Array(textures.collect()));
    }
    if let Some(value) = material.base_color_factor {
        fields.insert("bc".to_owned(), reals(value));
    }
    if let Some(value) = material.emissive_factor {
        fields.insert("ec".to_owned(), reals(value));
    }
    if let Some(value) = material.metallic {
        fields.insert("mf".to_owned(), real(value));
    }
    if let Some(value) = material.roughness {
        fields.insert("rf".to_owned(), real(value));
    }
    if let Some(value) = material.alpha_mode {
        let mode = match value {
            PbrAlphaMode::Opaque => 0,
            PbrAlphaMode::Blend => 1,
            PbrAlphaMode::Mask => 2,
        };
        fields.insert("am".to_owned(), Value::Integer(mode));
    }
    if let Some(value) = material.alpha_cutoff {
        fields.insert("ac".to_owned(), real(value));
    }
    if let Some(value) = material.double_sided {
        fields.insert("ds".to_owned(), Value::Boolean(value));
    }
    if slots.iter().any(|slot| slot.transform != PbrTextureTransformOverride::default()) {
        let transforms = slots.iter().map(|slot| {
            let mut transform = Map::new();
            if let Some(value) = slot.transform.offset {
                transform.insert("o".to_owned(), reals(value));
            }
            if let Some(value) = slot.transform.scale {
                transform.insert("s".to_owned(), reals(value));
            }
            if let Some(value) = slot.transform.rotation {
                transform.insert("r".to_owned(), real(value));
            }
            Value::Map(transform)
        });
        fields.insert("ti".to_owned(), Value::Array(transforms.collect()));
    }
    Value::Map(fields)
}

fn reals<const N: usize>(values: [f32; N]) -> Value {
    Value::Array(values.into_iter().map(real).collect())
}

fn real(value: f32) -> Value {
    Value::Real(f64::from(value))
}

fn texture_slots(material: &mut PbrMaterialOverride) -> [&mut PbrTextureOverride; 4] {
    [&mut material.base_color, &mut material.normal, &mut material.metallic_roughness, &mut material.emissive]
}

fn vector<const N: usize>(value: &Value) -> Result<[f32; N], ()> {
    let Value::Array(values) = value else { return Err(()) };
    let values: &[Value; N] = values.as_slice().try_into().map_err(|_| ())?;
    let mut result = [0.0; N];
    for (component, value) in result.iter_mut().zip(values) {
        *component = number(value)?;
    }
    Ok(result)
}

fn number(value: &Value) -> Result<f32, ()> {
    value.as_finite_f32().ok_or(())
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, num::NonZeroU32};

    use llsd::{Value, notation};
    use protocol::messages::generic_streaming_message::MAX_DATA_BYTES;
    use utils::AssetId;
    use uuid::Uuid;
    use world::{FieldUpdate, PbrAlphaMode, PbrMaterialOverride, PbrMaterialOverrides, PbrTextureOverride, PbrTextureTransformOverride};

    use super::{decode_payload, encode_payload};

    fn payload(id: i32, faces: Vec<Value>, materials: Vec<Value>) -> Vec<u8> {
        let mut map = BTreeMap::new();
        map.insert("id".to_owned(), Value::Integer(id));
        map.insert("te".to_owned(), Value::Array(faces));
        map.insert("od".to_owned(), Value::Array(materials));
        notation::to_vec(&Value::Map(map)).expect("serialize LLSD payload")
    }

    fn material(fields: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
        Value::Map(fields.into_iter().map(|(key, value)| (key.to_owned(), value)).collect())
    }

    #[test]
    fn decodes_material_fields() {
        let data = material([
            ("bc", Value::Array(vec![Value::Real(0.1), Value::Real(0.2), Value::Real(0.3), Value::Integer(1)])),
            ("ec", Value::Array(vec![Value::Real(0.4), Value::Real(0.5), Value::Real(0.6)])),
            ("am", Value::Integer(2)),
            (
                "tex",
                Value::Array(vec![
                    Value::Uuid(Uuid::nil()),
                    Value::Uuid(Uuid::max()),
                    Value::Uuid(Uuid::from_u128(3)),
                    Value::Uuid(Uuid::from_u128(4)),
                ]),
            ),
            (
                "ti",
                Value::Array(vec![
                    material([("r", Value::Real(0.25))]),
                    material([("s", Value::Array(vec![Value::Real(2.0), Value::Real(3.0)]))]),
                    material([("o", Value::Array(vec![Value::Real(-0.5), Value::Real(0.5)]))]),
                    material([]),
                ]),
            ),
        ]);
        let decoded = decode_payload(&payload(7, vec![Value::Integer(4)], vec![data])).expect("valid override");

        assert_eq!(decoded.local_id.get(), 7);
        assert_eq!(decoded.overrides.face(4).unwrap().alpha_mode, Some(PbrAlphaMode::Mask));
        assert_eq!(decoded.overrides.face(4).unwrap().base_color_factor, Some([0.1, 0.2, 0.3, 1.0]));
        let decoded = decoded.overrides.face(4).unwrap();
        assert_eq!(decoded.emissive_factor, Some([0.4, 0.5, 0.6]));
        assert_eq!(
            decoded.base_color,
            PbrTextureOverride {
                texture: FieldUpdate::Keep,
                transform: PbrTextureTransformOverride { rotation: Some(0.25), ..Default::default() },
            }
        );
        assert_eq!(
            decoded.normal,
            PbrTextureOverride {
                texture: FieldUpdate::Clear,
                transform: PbrTextureTransformOverride { scale: Some([2.0, 3.0]), ..Default::default() },
            }
        );
        assert_eq!(
            decoded.metallic_roughness,
            PbrTextureOverride {
                texture: FieldUpdate::Set(AssetId::new(Uuid::from_u128(3))),
                transform: PbrTextureTransformOverride { offset: Some([-0.5, 0.5]), ..Default::default() },
            }
        );
        assert_eq!(
            decoded.emissive,
            PbrTextureOverride { texture: FieldUpdate::Set(AssetId::new(Uuid::from_u128(4))), ..Default::default() }
        );
    }

    #[test]
    fn empty_faces_clear_snapshot() {
        let decoded = decode_payload(&payload(7, Vec::new(), Vec::new())).expect("empty snapshot");

        assert!(decoded.overrides.is_empty());
    }

    #[test]
    fn decodes_nul_terminated_payload() {
        let decoded = decode_payload(b"{'id':i1863579,'od':[{'ds':1},{'ds':1}],'te':[i0,i1]}\0").expect("simulator payload");

        assert_eq!(decoded.local_id.get(), 1863579);
        assert_eq!(decoded.overrides.face(0).unwrap().double_sided, Some(true));
        assert_eq!(decoded.overrides.face(1).unwrap().double_sided, Some(true));
    }

    #[test]
    fn duplicate_face_uses_last_value() {
        let decoded = decode_payload(&payload(
            7,
            vec![Value::Integer(4), Value::Integer(4)],
            vec![material([("mf", Value::Real(0.1))]), material([("mf", Value::Real(0.8))])],
        ))
        .expect("last face value wins");

        assert_eq!(decoded.overrides.faces().len(), 1);
        assert_eq!(decoded.overrides.face(4).unwrap().metallic, Some(0.8));
    }

    #[test]
    fn rejects_invalid_payloads() {
        let face = |material| payload(7, vec![Value::Integer(4)], vec![material]);
        let mut cases = vec![
            ("face out of range".to_owned(), payload(7, vec![Value::Integer(45)], vec![material([])])),
            ("unaligned faces".to_owned(), payload(7, vec![Value::Integer(4)], Vec::new())),
            ("non-finite value".to_owned(), face(material([("rf", Value::Real(f64::MAX))]))),
            ("oversized".to_owned(), vec![b' '; MAX_DATA_BYTES + 1]),
            ("zero local id".to_owned(), payload(0, Vec::new(), Vec::new())),
        ];
        for values in [vec![Value::Real(1.0)], vec![Value::Real(1.0); 5], vec![Value::Boolean(true); 4]] {
            cases.push((format!("bad bc vector {values:?}"), face(material([("bc", Value::Array(values))]))));
        }
        for (key, element) in [("tex", Value::Uuid(Uuid::nil())), ("ti", material([]))] {
            cases.push((format!("too many {key} entries"), face(material([(key, Value::Array(vec![element; 5]))]))));
        }
        for (key, value) in [
            ("tex", Value::Array(vec![Value::Integer(1)])),
            ("ti", Value::Array(vec![Value::Integer(1)])),
            ("ti", Value::Array(vec![material([("s", Value::Integer(1))])])),
            ("bc", Value::Integer(1)),
        ] {
            cases.push((format!("wrong {key} type {value:?}"), face(material([(key, value)]))));
        }
        for (case, bytes) in cases {
            assert!(decode_payload(&bytes).is_err(), "{case}");
        }
    }

    #[test]
    fn preserves_signed_local_id_bits() {
        for (wire_id, expected) in [(i32::MAX, i32::MAX as u32), (i32::MIN, 0x8000_0000), (-1, u32::MAX)] {
            let decoded = decode_payload(&payload(wire_id, Vec::new(), Vec::new())).expect("valid local id");

            assert_eq!(decoded.local_id.get(), expected);
        }
    }

    #[test]
    fn ignores_unsupported_material_keys() {
        let decoded = decode_payload(&payload(7, vec![Value::Integer(4)], vec![material([("future", Value::Integer(3))])]))
            .expect("unknown keys are forward-compatible");

        assert_eq!(decoded.overrides.face(4), Some(&PbrMaterialOverride::default()));
    }

    #[test]
    fn undefined_material_is_default() {
        let decoded = decode_payload(&payload(7, vec![Value::Integer(4)], vec![Value::Undefined])).expect("undefined material is valid");

        assert_eq!(decoded.overrides.face(4), Some(&PbrMaterialOverride::default()));
    }

    #[test]
    fn undefined_entries_are_not_overridden() {
        let decoded = decode_payload(
            b"{'id':i7,'od':[{'tex':[!,!,!,uffffffff-ffff-ffff-ffff-ffffffffffff],'ti':[!,{'s':[r5.7,r3.8],'o':!}],'bc':!}],'te':[i4]}",
        )
        .expect("undefined entries are valid");

        assert_eq!(
            decoded.overrides.face(4),
            Some(&PbrMaterialOverride {
                normal: PbrTextureOverride {
                    texture: FieldUpdate::Keep,
                    transform: PbrTextureTransformOverride { scale: Some([5.7, 3.8]), ..Default::default() },
                },
                emissive: PbrTextureOverride { texture: FieldUpdate::Clear, ..Default::default() },
                ..Default::default()
            })
        );
    }

    #[test]
    fn caps_faces_at_limit() {
        let faces = (0..46).map(Value::Integer).collect();
        let materials = (0..46).map(|_| material([])).collect();
        let decoded = decode_payload(&payload(7, faces, materials)).expect("excess faces are ignored");

        assert_eq!(decoded.overrides.faces().len(), 45);
    }

    fn full_material(seed: u8) -> PbrMaterialOverride {
        let seed_f = f32::from(seed);
        let texture = |offset: u128| FieldUpdate::Set(AssetId::new(Uuid::from_u128(u128::from(seed) * 16 + offset)));
        let transform =
            PbrTextureTransformOverride { offset: Some([0.1 + seed_f, -0.3]), scale: Some([1.5, 2.0 / 3.0]), rotation: Some(0.7) };
        PbrMaterialOverride {
            base_color: PbrTextureOverride { texture: texture(1), transform },
            normal: PbrTextureOverride { texture: FieldUpdate::Clear, transform },
            metallic_roughness: PbrTextureOverride { texture: FieldUpdate::Keep, transform },
            emissive: PbrTextureOverride { texture: texture(4), transform },
            base_color_factor: Some([0.1, 0.2, 1.0 / 3.0, 1.0]),
            emissive_factor: Some([0.4, 0.5, f32::MIN_POSITIVE]),
            metallic: Some(0.25),
            roughness: Some(0.75),
            alpha_mode: Some(PbrAlphaMode::Blend),
            alpha_cutoff: Some(0.5),
            double_sided: Some(false),
        }
    }

    fn full_overrides(faces: u8) -> PbrMaterialOverrides {
        PbrMaterialOverrides::new((0..faces).map(|face| (face, full_material(face))).collect()).expect("valid overrides")
    }

    #[test]
    fn encoded_overrides_round_trip() {
        let local_id = NonZeroU32::new(u32::MAX).unwrap();
        let mut faces = full_overrides(2).faces().to_vec();
        faces.push((7, PbrMaterialOverride { double_sided: Some(true), ..Default::default() }));
        faces.push((9, PbrMaterialOverride::default()));
        let overrides = PbrMaterialOverrides::new(faces).expect("valid overrides");

        let decoded = decode_payload(&encode_payload(local_id, &overrides).expect("fits")).expect("decodes");

        assert_eq!(decoded.local_id, local_id);
        assert_eq!(decoded.overrides, overrides);
    }

    #[test]
    fn encoding_over_message_bound_is_rejected() {
        let local_id = NonZeroU32::new(7).unwrap();
        let encodings: Vec<_> = (1..=45).map(|faces| encode_payload(local_id, &full_overrides(faces))).collect();
        let fitting = encodings.iter().take_while(|encoding| encoding.is_some()).count();

        assert!((1..45).contains(&fitting));
        assert!(encodings[fitting..].iter().all(Option::is_none));
        let largest = encodings[fitting - 1].as_ref().expect("largest fitting encoding");
        assert!(largest.len() <= MAX_DATA_BYTES);
        assert_eq!(decode_payload(largest).expect("decodes").overrides, full_overrides(fitting as u8));
    }
}
