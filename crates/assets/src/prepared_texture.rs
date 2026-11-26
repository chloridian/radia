use crate::{MAX_SOURCE_EDGE, PixelFormat, TextureSize, texture::Decoded};

pub const PREPARED_TEXTURE_FORMAT_VERSION: u32 = 2;
const MAGIC: [u8; 4] = *b"RTEX";
const KEY_DOMAIN: &[u8] = b"radia-prepared-texture-v1";
const MIP_COUNT: u8 = 1;
pub(crate) const PREPARED_TEXTURE_FOOTER_BYTES: usize = 68;
pub(crate) const MAX_PREPARED_TEXTURE_BYTES: usize = storage::MAX_VALUE_BYTES;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PreparedTextureKey {
    pub source_sha256: [u8; 32],
    pub format_version: u32,
    pub target_edge: u32,
}

impl PreparedTextureKey {
    pub fn new(source_sha256: [u8; 32], target: TextureSize) -> Self {
        Self { source_sha256, format_version: PREPARED_TEXTURE_FORMAT_VERSION, target_edge: target.edge() }
    }

    pub const fn with_version(source_sha256: [u8; 32], format_version: u32, target_edge: u32) -> Self {
        Self { source_sha256, format_version, target_edge }
    }

    pub(crate) fn storage_name(self) -> [u8; 16] {
        const DOMAIN_END: usize = KEY_DOMAIN.len();
        const HASH_END: usize = DOMAIN_END + 32;
        const VERSION_END: usize = HASH_END + 4;
        const EDGE_END: usize = VERSION_END + 4;
        let mut input = [0u8; EDGE_END];
        input[..DOMAIN_END].copy_from_slice(KEY_DOMAIN);
        input[DOMAIN_END..HASH_END].copy_from_slice(&self.source_sha256);
        input[HASH_END..VERSION_END].copy_from_slice(&self.format_version.to_le_bytes());
        input[VERSION_END..EDGE_END].copy_from_slice(&self.target_edge.to_le_bytes());
        let digest = storage::sha256(&input);
        let mut name = [0u8; 16];
        name.copy_from_slice(&digest[..16]);
        name
    }
}

pub(crate) fn encode_prepared_texture(key: PreparedTextureKey, decoded: &Decoded) -> Option<Vec<u8>> {
    let pixels = decoded.pixels();
    let total_len = PREPARED_TEXTURE_FOOTER_BYTES.checked_add(pixels.len())?;
    if total_len > MAX_PREPARED_TEXTURE_BYTES {
        return None;
    }
    let pixel_len = u32::try_from(pixels.len()).ok()?;
    let format_tag = match decoded.format() {
        PixelFormat::Gray8 => 0u8,
        PixelFormat::GrayAlpha8 => 1u8,
        PixelFormat::Rgb8 => 2u8,
        PixelFormat::Rgba8 => 3u8,
    };
    let source_size = decoded.source_size();
    let size = decoded.size();

    let mut out = Vec::new();
    out.try_reserve_exact(total_len).ok()?;
    out.extend_from_slice(pixels);
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&key.format_version.to_le_bytes());
    out.extend_from_slice(&key.source_sha256);
    out.extend_from_slice(&key.target_edge.to_le_bytes());
    out.extend_from_slice(&source_size[0].to_le_bytes());
    out.extend_from_slice(&source_size[1].to_le_bytes());
    out.push(decoded.discard());
    out.push(format_tag);
    out.push(MIP_COUNT);
    out.push(0);
    out.extend_from_slice(&size[0].to_le_bytes());
    out.extend_from_slice(&size[1].to_le_bytes());
    out.extend_from_slice(&pixel_len.to_le_bytes());
    Some(out)
}

pub(crate) fn decode_prepared_texture(mut bytes: Vec<u8>, expected: PreparedTextureKey) -> Option<Decoded> {
    if bytes.len() < PREPARED_TEXTURE_FOOTER_BYTES || bytes.len() > MAX_PREPARED_TEXTURE_BYTES {
        return None;
    }
    let footer_start = bytes.len() - PREPARED_TEXTURE_FOOTER_BYTES;
    let footer = bytes.get(footer_start..)?;
    if footer.get(0..4)? != MAGIC {
        return None;
    }
    let format_version = read_u32(footer, 4)?;
    if format_version != expected.format_version || format_version != PREPARED_TEXTURE_FORMAT_VERSION {
        return None;
    }
    let source_sha256 = footer.get(8..40)?;
    if source_sha256 != expected.source_sha256 {
        return None;
    }
    let target_edge = read_u32(footer, 40)?;
    if target_edge != expected.target_edge {
        return None;
    }
    let source_width = read_u32(footer, 44)?;
    let source_height = read_u32(footer, 48)?;
    if source_width == 0 || source_height == 0 || source_width > MAX_SOURCE_EDGE || source_height > MAX_SOURCE_EDGE {
        return None;
    }
    let source_size = [source_width, source_height];
    let discard = *footer.get(52)?;
    let expected_discard = TextureSize::new(target_edge).discard_level(source_size);
    if discard != expected_discard {
        return None;
    }
    let format = match *footer.get(53)? {
        0 => PixelFormat::Gray8,
        1 => PixelFormat::GrayAlpha8,
        2 => PixelFormat::Rgb8,
        3 => PixelFormat::Rgba8,
        _ => return None,
    };
    let mip_count = *footer.get(54)?;
    let reserved = *footer.get(55)?;
    if mip_count != MIP_COUNT || reserved != 0 {
        return None;
    }
    let width = read_u32(footer, 56)?;
    let height = read_u32(footer, 60)?;
    let desired = source_size.map(|edge| edge.div_ceil(1 << discard));
    if width == 0 || height == 0 || width > MAX_SOURCE_EDGE || height > MAX_SOURCE_EDGE || [width, height] != desired {
        return None;
    }
    let pixel_len = usize::try_from(read_u32(footer, 64)?).ok()?;
    let expected_len = usize::try_from(width).ok()?.checked_mul(usize::try_from(height).ok()?)?.checked_mul(format.channels())?;
    if pixel_len != expected_len || footer_start != pixel_len {
        return None;
    }
    bytes.truncate(pixel_len);
    Some(Decoded::from_prepared(source_size, discard, [width, height], format, bytes))
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let end = offset.checked_add(size_of::<u32>())?;
    let value = bytes.get(offset..end)?.try_into().ok()?;
    Some(u32::from_le_bytes(value))
}

#[cfg(test)]
mod tests {
    use std::{assert_matches, fs, path::Path, sync::Arc, time::Instant};

    use tempfile::tempdir;
    use test_support::texture::{GRAY_4096_J2C, RGBA_J2C};
    use utils::AssetId;
    use uuid::Uuid;

    use super::*;
    use crate::{
        Budget, Content, Image, Kind, LoadJob, Priority, Request, Resource, Status, Store,
        cache::{AssetCache, AssetCacheError, AssetLookup},
        texture,
    };

    fn load_warm_texture(cache: &Arc<AssetCache>, id: AssetId, size: TextureSize) -> Image {
        let mut store = Store::with_cache(Budget::default(), Arc::clone(cache));
        let route = store.register_route().expect("register route");
        let now = Instant::now();
        let demand = store.request(route, Request::Texture { id, size }, Priority::Visible, now).expect("request texture");
        let Some(LoadJob::ReadCache(read)) = store.next_job(now) else {
            panic!("warm texture starts with cache read");
        };
        let Some(LoadJob::Decode(input)) = store.complete_cache_read(read.run(), now) else {
            panic!("cache hit supplies decode input");
        };
        let decode = store.start_decode(input).expect("start decode");
        let writes = store.complete(decode.run(), now);
        for write in writes {
            write.run();
        }
        let status = demand.status();
        let Status::Ready(Content::Texture(handle)) = status else {
            panic!("expected ready texture, got {status:?}");
        };
        handle.image().expect("decoded image installed")
    }

    #[test]
    fn cache_hit_matches_pixels() {
        let directory = tempdir().expect("tempdir");
        let cache = Arc::new(AssetCache::open(directory.path(), 64 * 1024 * 1024).expect("open cache"));
        let id = AssetId::new(Uuid::from_u128(10));
        let size = TextureSize::new(512);
        cache.put_complete(Request::Texture { id, size }, GRAY_4096_J2C.to_vec()).expect("seed encoded texture");

        let first = load_warm_texture(&cache, id, size);
        let stats_after_first = cache.stats();
        assert_eq!(stats_after_first.prepared_misses, 1);
        assert_eq!(stats_after_first.prepared_hits, 0);
        assert_eq!(stats_after_first.prepared_entries, 1);

        let second = load_warm_texture(&cache, id, size);
        let stats_after_second = cache.stats();
        assert_eq!(stats_after_second.prepared_hits, 1);
        assert_eq!(stats_after_second.prepared_misses, 1);
        assert_eq!(first.source_size(), second.source_size());
        assert_eq!(first.discard_level(), second.discard_level());
        assert_eq!(first.size(), second.size());
        assert_eq!(first.format(), second.format());
        assert_eq!(first.pixels(), second.pixels());

        Arc::try_unwrap(cache).expect("release cache").shutdown().expect("shutdown cache");
    }

    #[test]
    fn key_change_invalidates_cache() {
        let decoded = texture::decode(RGBA_J2C, TextureSize::default(), || false).expect("decode fixture");
        let source_hash = storage::sha256(RGBA_J2C);
        let key = PreparedTextureKey::new(source_hash, TextureSize::default());
        let encoded = encode_prepared_texture(key, &decoded).expect("encode prepared");

        assert!(decode_prepared_texture(encoded.clone(), key).is_some());

        let mut other_hash = source_hash;
        other_hash[0] ^= 0xff;
        assert!(decode_prepared_texture(encoded.clone(), PreparedTextureKey::new(other_hash, TextureSize::default())).is_none());

        let other_version = PreparedTextureKey::with_version(source_hash, PREPARED_TEXTURE_FORMAT_VERSION + 1, key.target_edge);
        assert!(decode_prepared_texture(encoded.clone(), other_version).is_none());

        let other_edge = PreparedTextureKey::new(source_hash, TextureSize::new(64));
        assert!(decode_prepared_texture(encoded, other_edge).is_none());
    }

    fn corrupt_first_prep_file(dir: &Path) {
        let mut stack = vec![dir.to_path_buf()];
        while let Some(current) = stack.pop() {
            for entry in fs::read_dir(&current).expect("read prepared dir") {
                let path = entry.expect("dir entry").path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().and_then(|ext| ext.to_str()) == Some("prep") {
                    let mut bytes = fs::read(&path).expect("read prep file");
                    let last = bytes.len() - 1;
                    bytes[last] ^= 0x5a;
                    fs::write(&path, bytes).expect("write corrupt prep file");
                    return;
                }
            }
        }
        panic!("expected a .prep file under {}", dir.display());
    }

    #[test]
    fn corrupt_entry_falls_back() {
        let directory = tempdir().expect("tempdir");
        let cache = Arc::new(AssetCache::open(directory.path(), 64 * 1024 * 1024).expect("open cache"));
        let id = AssetId::new(Uuid::from_u128(11));
        let size = TextureSize::default();
        let request = Request::Texture { id, size };
        cache.put_complete(request, RGBA_J2C.to_vec()).expect("seed encoded texture");

        let source_hash = storage::sha256(RGBA_J2C);
        let key = PreparedTextureKey::new(source_hash, size);
        let decoded = texture::decode(RGBA_J2C, size, || false).expect("decode fixture");
        let encoded = encode_prepared_texture(key, &decoded).expect("encode prepared");
        cache.put_prepared_texture_bytes(key, encoded).expect("seed prepared");
        assert_eq!(cache.stats().prepared_entries, 1);

        corrupt_first_prep_file(&directory.path().join("prepared"));

        let image = load_warm_texture(&cache, id, size);
        assert_eq!(image.pixels(), decoded.pixels());
        let stats = cache.stats();
        assert_eq!(stats.prepared_corrupt_entries, 1);
        assert_eq!(stats.prepared_hits, 0);
        assert_eq!(stats.prepared_entries, 1);

        let image_again = load_warm_texture(&cache, id, size);
        assert_eq!(image_again.pixels(), decoded.pixels());
        assert_eq!(cache.stats().prepared_hits, 1);

        Arc::try_unwrap(cache).expect("release cache").shutdown().expect("shutdown cache");
    }

    #[test]
    fn truncated_entry_falls_back() {
        let directory = tempdir().expect("tempdir");
        let cache = Arc::new(AssetCache::open(directory.path(), 64 * 1024 * 1024).expect("open cache"));
        let id = AssetId::new(Uuid::from_u128(13));
        let size = TextureSize::default();
        let request = Request::Texture { id, size };
        cache.put_complete(request, RGBA_J2C.to_vec()).expect("seed encoded texture");

        let source_hash = storage::sha256(RGBA_J2C);
        let key = PreparedTextureKey::new(source_hash, size);
        let decoded = texture::decode(RGBA_J2C, size, || false).expect("decode fixture");
        let mut truncated = encode_prepared_texture(key, &decoded).expect("encode prepared");
        truncated.pop();
        cache.put_prepared_texture_bytes(key, truncated).expect("seed truncated prepared");
        assert_eq!(cache.stats().prepared_entries, 1);

        let image = load_warm_texture(&cache, id, size);
        assert_eq!(image.pixels(), decoded.pixels());
        let stats = cache.stats();
        assert_eq!(stats.prepared_corrupt_entries, 1);
        assert_eq!(stats.prepared_hits, 0);
        assert_eq!(stats.prepared_entries, 1);

        let image_again = load_warm_texture(&cache, id, size);
        assert_eq!(image_again.pixels(), decoded.pixels());
        assert_eq!(cache.stats().prepared_hits, 1);

        Arc::try_unwrap(cache).expect("release cache").shutdown().expect("shutdown cache");
    }

    #[test]
    fn quota_eviction_keeps_encoded() {
        let directory = tempdir().expect("tempdir");
        let cache = Arc::new(
            AssetCache::open_with_prepared_limit(directory.path(), 64 * 1024 * 1024, storage::MIN_SIZE_LIMIT)
                .expect("open cache with tight prepared limit"),
        );
        let encoded_id = AssetId::new(Uuid::from_u128(12));
        let encoded_request = Request::new(Resource { id: encoded_id, kind: Kind::Texture });
        cache.put_complete(encoded_request, RGBA_J2C.to_vec()).expect("seed encoded texture");

        let filler = vec![0xab; 16 * 1024];
        for index in 0..8u8 {
            let key = PreparedTextureKey::new([index; 32], TextureSize::default());
            if let Err(error) = cache.put_prepared_texture_bytes(key, filler.clone()) {
                assert_matches!(error, AssetCacheError::Storage(_));
            }
        }

        assert_matches!(cache.lookup(encoded_request), AssetLookup::Hit(value) if value.bytes == RGBA_J2C);
        assert_eq!(cache.stats().entries_by_kind.textures, 1);

        Arc::try_unwrap(cache).expect("release cache").shutdown().expect("shutdown cache");
    }
}
