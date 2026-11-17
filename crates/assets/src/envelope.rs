use std::{
    collections::HashMap,
    io::{Read, Write},
};

use flate2::{Compression, read::ZlibDecoder, write::ZlibEncoder};
use llsd::{Binary, Limits, Value, binary, xml};
use utils::AssetId;
use uuid::Uuid;

use crate::{Failure, Kind, Resource};

const MAX_ENVELOPE_BYTES: usize = 4 * 1024 * 1024;

fn limits(input: usize) -> Limits {
    Limits {
        max_bytes: input,
        max_depth: 16,
        max_values: 4096,
        max_container_items: 512,
        max_string_bytes: 2 * 1024 * 1024,
        max_map_key_bytes: 128,
        max_binary_bytes: 2 * 1024 * 1024,
    }
}

pub fn legacy_request(ids: &[AssetId]) -> Result<Value, Failure> {
    let request = Value::Array(ids.iter().map(|id| Value::Binary(Binary::new(id.uuid().as_bytes().to_vec()))).collect());
    let bytes = binary::to_vec_headerless(&request).map_err(|_| Failure::Malformed)?;
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&bytes).map_err(|_| Failure::Malformed)?;
    let zipped = encoder.finish().map_err(|_| Failure::Malformed)?;
    Ok(Value::Map([("Zipped".into(), Value::Binary(Binary::new(zipped)))].into()))
}

pub fn split_legacy_response(bytes: Vec<u8>) -> Result<HashMap<AssetId, Vec<u8>>, Failure> {
    if bytes.len() > Kind::LegacyMaterial.max_response_bytes() {
        return Err(Failure::TooLarge);
    }
    let Value::Map(mut fields) = xml::from_slice_with_limits(&bytes, limits(bytes.len())).map_err(|_| Failure::Malformed)? else {
        return Err(Failure::Malformed);
    };
    let Some(Value::Binary(zipped)) = fields.remove("Zipped") else { return Err(Failure::Malformed) };
    let mut decoder = ZlibDecoder::new(zipped.as_slice());
    let mut inflated = Vec::new();
    let mut chunk = [0; 16 * 1024];
    loop {
        let count = decoder.read(&mut chunk).map_err(|_| Failure::Malformed)?;
        if count == 0 {
            break;
        }
        if inflated.len() + count > MAX_ENVELOPE_BYTES {
            return Err(Failure::TooLarge);
        }
        inflated.extend_from_slice(&chunk[..count]);
    }
    let Value::Array(entries) = binary::from_slice_with_limits(&inflated, limits(MAX_ENVELOPE_BYTES)).map_err(|_| Failure::Malformed)?
    else {
        return Err(Failure::Malformed);
    };
    let mut materials = HashMap::with_capacity(entries.len());
    for entry in entries {
        let Value::Map(mut entry) = entry else { continue };
        let Some(Value::Binary(id)) = entry.get("ID") else { continue };
        let Ok(id) = Uuid::from_slice(id.as_slice()) else { continue };
        let Some(Value::Map(material)) = entry.remove("Material") else { return Err(Failure::Malformed) };
        let material = binary::to_vec(&Value::Map(material)).map_err(|_| Failure::Malformed)?;
        if material.len() > MAX_ENVELOPE_BYTES {
            return Err(Failure::TooLarge);
        }
        materials.insert(AssetId::new(id), material);
    }
    Ok(materials)
}

pub(crate) fn extract(resource: Resource, bytes: Vec<u8>) -> Result<Vec<u8>, Failure> {
    if bytes.len() > resource.kind.max_response_bytes() {
        return Err(Failure::TooLarge);
    }
    match resource.kind {
        Kind::Texture | Kind::LegacyMaterial | Kind::Environment | Kind::Mesh => {
            if bytes.is_empty() {
                Err(Failure::Malformed)
            } else {
                Ok(bytes)
            }
        }
        Kind::PbrMaterial => {
            let (Value::Map(mut fields), _) =
                binary::from_slice_prefix_with_limits(&bytes, limits(bytes.len())).map_err(|_| Failure::Malformed)?
            else {
                return Err(Failure::Malformed);
            };
            if !matches!(fields.get("version"), Some(Value::String(version)) if matches!(version.as_str(), "1.0" | "1.1"))
                || !matches!(fields.get("type"), Some(Value::String(kind)) if kind == "GLTF 2.0")
            {
                return Err(Failure::Unsupported);
            }
            match fields.remove("data") {
                Some(Value::String(json)) => Ok(json.into_bytes()),
                _ => Err(Failure::Malformed),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resource(kind: Kind) -> Resource {
        Resource { id: AssetId::new(Uuid::from_u128(1)), kind }
    }

    #[test]
    fn pbr_literal_requires_version() {
        let bytes = b"{\0\0\0\x03k\0\0\0\x07versions\0\0\0\x031.1k\0\0\0\x04types\0\0\0\x08GLTF 2.0k\0\0\0\x04datas\0\0\0\x02{}}";
        assert_eq!(extract(resource(Kind::PbrMaterial), bytes.to_vec()), Ok(b"{}".to_vec()));
        let invalid = bytes.windows(3).position(|window| window == b"1.1").unwrap();
        let mut unsupported = bytes.to_vec();
        unsupported[invalid..invalid + 3].copy_from_slice(b"9.9");
        assert_eq!(extract(resource(Kind::PbrMaterial), unsupported), Err(Failure::Unsupported));
        assert_eq!(extract(resource(Kind::PbrMaterial), bytes[..bytes.len() - 1].to_vec()), Err(Failure::Malformed));
    }

    #[test]
    fn pbr_ignores_trailing_bytes() {
        let mut bytes = b"<? LLSD/Binary ?>\n{\0\0\0\x03k\0\0\0\x04datas\0\0\0\x02{}k\0\0\0\x04types\0\0\0\x08GLTF 2.0k\0\0\0\x07versions\0\0\0\x031.1}".to_vec();
        bytes.push(0);
        assert_eq!(extract(resource(Kind::PbrMaterial), bytes), Ok(b"{}".to_vec()));
    }

    fn stored_zlib(bytes: &[u8]) -> Vec<u8> {
        let length = u16::try_from(bytes.len()).unwrap();
        let mut encoded = vec![0x78, 0x01, 0x01];
        encoded.extend(length.to_le_bytes());
        encoded.extend((!length).to_le_bytes());
        encoded.extend_from_slice(bytes);
        let (mut a, mut b) = (1u32, 0u32);
        for byte in bytes {
            a = (a + u32::from(*byte)) % 65521;
            b = (b + a) % 65521;
        }
        encoded.extend(((b << 16) | a).to_be_bytes());
        encoded
    }

    fn outer(zipped: Vec<u8>) -> Vec<u8> {
        xml::to_vec(&Value::Map([("Zipped".into(), Value::Binary(Binary::new(zipped)))].into())).unwrap()
    }

    fn id(value: u128) -> AssetId {
        AssetId::new(Uuid::from_u128(value))
    }

    fn zipped(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn request_lists_ids_in_order() {
        let ids = [id(1), id(2), id(3)];
        let Value::Map(request) = legacy_request(&ids).unwrap() else { unreachable!() };
        let Some(Value::Binary(zipped)) = request.get("Zipped") else { unreachable!() };
        let mut decoded = Vec::new();
        ZlibDecoder::new(zipped.as_slice()).read_to_end(&mut decoded).unwrap();
        let mut expected = b"[\0\0\0\x03".to_vec();
        for id in ids {
            expected.extend_from_slice(b"b\0\0\0\x10");
            expected.extend_from_slice(id.uuid().as_bytes());
        }
        expected.push(b']');
        assert_eq!(decoded, expected);
    }

    #[test]
    fn split_keeps_listed_materials() {
        let mut record = b"[\0\0\0\x03".to_vec();
        for (value, name) in [(1, b"oak"), (2, b"elm")] {
            record.extend_from_slice(b"{\0\0\0\x02k\0\0\0\x02IDb\0\0\0\x10");
            record.extend_from_slice(id(value).uuid().as_bytes());
            record.extend_from_slice(b"k\0\0\0\x08Material{\0\0\0\x01k\0\0\0\x04Names\0\0\0\x03");
            record.extend_from_slice(name);
            record.extend_from_slice(b"}}");
        }
        record.extend_from_slice(b"{\0\0\0\x01k\0\0\0\x02IDb\0\0\0\x02xy}]");
        let materials = split_legacy_response(outer(stored_zlib(&record))).unwrap();
        let name = |value| binary::from_slice::<Value>(&materials[&id(value)]).unwrap();
        assert_eq!(materials.len(), 2, "the entry with a short ID is skipped");
        assert_eq!(name(1), Value::Map([("Name".into(), Value::String("oak".into()))].into()));
        assert_eq!(name(2), Value::Map([("Name".into(), Value::String("elm".into()))].into()));
        assert!(!materials.contains_key(&id(3)));
        assert_eq!(split_legacy_response(outer(vec![0, 1, 2])), Err(Failure::Malformed));
    }

    #[test]
    fn full_batch_fits_limits() {
        let fields = |seed: i32| {
            let mut material: llsd::Map = [
                ("NormMap".into(), Value::Uuid(Uuid::from_u128(7))),
                ("SpecMap".into(), Value::Uuid(Uuid::from_u128(8))),
                ("SpecColor".into(), Value::Array([51, 102, 153, 204].map(Value::Integer).into())),
            ]
            .into();
            for name in [
                "NormOffsetX",
                "NormOffsetY",
                "NormRepeatX",
                "NormRepeatY",
                "NormRotation",
                "SpecOffsetX",
                "SpecOffsetY",
                "SpecRepeatX",
                "SpecRepeatY",
                "SpecRotation",
                "SpecExp",
                "EnvIntensity",
                "AlphaMaskCutoff",
                "DiffuseAlphaMode",
            ] {
                material.insert(name.into(), Value::Integer(seed));
            }
            material
        };
        let entries = (1..=50)
            .map(|value| {
                Value::Map(
                    [
                        ("ID".into(), Value::Binary(Binary::new(id(value).uuid().as_bytes().to_vec()))),
                        ("Material".into(), Value::Map(fields(value as i32))),
                    ]
                    .into(),
                )
            })
            .collect();
        let materials = split_legacy_response(outer(zipped(&binary::to_vec(&Value::Array(entries)).unwrap()))).unwrap();
        assert_eq!(materials.len(), 50);
        assert_eq!(binary::from_slice::<Value>(&materials[&id(50)]).unwrap(), Value::Map(fields(50)));
    }

    #[test]
    fn legacy_material_passes_through() {
        assert_eq!(extract(resource(Kind::LegacyMaterial), b"material".to_vec()), Ok(b"material".to_vec()));
        assert_eq!(extract(resource(Kind::LegacyMaterial), Vec::new()), Err(Failure::Malformed));
    }

    #[test]
    fn oversized_input_is_too_large() {
        assert_eq!(split_legacy_response(outer(zipped(&vec![b' '; MAX_ENVELOPE_BYTES + 1]))), Err(Failure::TooLarge));
        assert_eq!(split_legacy_response(vec![0; Kind::LegacyMaterial.max_response_bytes() + 1]), Err(Failure::TooLarge));
        assert_eq!(extract(resource(Kind::PbrMaterial), vec![0; Kind::PbrMaterial.max_response_bytes() + 1]), Err(Failure::TooLarge));
    }
}
