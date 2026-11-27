#![expect(clippy::unwrap_used)]

use std::{assert_matches, time::Instant};

use assets::{
    Budget, ColorSpace, Content, Failure, Image, LoadJob, PixelAlpha, PixelFormat, Priority, Request, Status, Store, TextureSize,
};
use proptest::prelude;
use test_support::texture::{GRAY_4096_J2C, RGBA_2048_LOSSY_J2C, RGBA_J2C, RGBA_RAW};
use utils::AssetId;
use uuid::Uuid;

fn decode_size(bytes: &[u8], size: TextureSize) -> Result<Image, Failure> {
    let now = Instant::now();
    let mut store = Store::new(Budget::default());
    let route = store.register_route().unwrap();
    let request = Request::Texture { id: AssetId::new(Uuid::from_u128(1)), size };
    let demand = store.request(route, request, Priority::Visible, now).unwrap();
    let LoadJob::Fetch(job) = store.next_job(now).unwrap() else { panic!("fresh texture request unexpectedly used cached source") };
    let completion = match job.complete(Ok(bytes.to_vec())) {
        Ok(input) => store.start_decode(input).expect("decode admission").run(),
        Err(failure) => failure.into(),
    };
    store.complete(completion, now);
    match demand.status() {
        Status::Ready(Content::Texture(image)) => Ok(image.image().unwrap()),
        Status::Failed { failure, retry_at: None } => Err(failure),
        status => panic!("unexpected texture status: {status:?}"),
    }
}

fn decode(bytes: &[u8]) -> Result<Image, Failure> {
    decode_size(bytes, TextureSize::default())
}

#[test]
fn rgba_output_image() {
    let image = decode(RGBA_J2C).unwrap();
    assert_eq!(image.source_size(), [8, 8]);
    assert_eq!(image.discard_level(), 0);
    assert_eq!(image.size(), [8, 8]);
    assert_eq!(image.format(), PixelFormat::Rgba8);
    assert_eq!(image.alpha(), PixelAlpha::Straight);
    assert_eq!(image.color_space(), ColorSpace::Srgb);
    assert_eq!(image.pixels(), RGBA_RAW);
}

#[test]
fn discard_reduces_large_textures() {
    let gray = decode(GRAY_4096_J2C).unwrap();
    assert_eq!(gray.source_size(), [4096, 4096]);
    assert_eq!(gray.discard_level(), 1);
    assert_eq!(gray.size(), [2048, 2048]);
    assert_eq!(gray.format(), PixelFormat::Gray8);
    assert_eq!(gray.alpha(), PixelAlpha::Opaque);
    assert!(gray.pixels().iter().all(|sample| *sample == 73));

    let rgba = decode_size(RGBA_2048_LOSSY_J2C, TextureSize::new(512)).unwrap();
    assert_eq!(rgba.source_size(), [2048, 2048]);
    assert_eq!(rgba.discard_level(), 2);
    assert_eq!(rgba.size(), [512, 512]);
    assert_eq!(rgba.format(), PixelFormat::Rgba8);
    assert_eq!(rgba.pixels().len(), 512 * 512 * 4);
    assert!(rgba.pixels().windows(2).any(|pair| pair[0] != pair[1]), "fixture has detail");
}

#[test]
fn decode_errors_map_to_failures() {
    let valid = RGBA_J2C;
    let sot = valid.windows(2).position(|bytes| bytes == [0xff, 0x90]).unwrap();
    for length in [0, 4, 40, sot - 1] {
        assert_matches!(decode(&valid[..length]).err(), Some(Failure::Malformed), "truncation at {length}");
    }
    let mut huge = valid.to_vec();
    huge[8..12].copy_from_slice(&u32::MAX.to_be_bytes());
    assert_matches!(decode(&huge).err(), Some(Failure::TooLarge));
    let mut signed = valid.to_vec();
    signed[42] = 0x87;
    assert_matches!(decode(&signed).err(), Some(Failure::Unsupported));
    let mut expensive = valid.to_vec();
    for offset in [8, 12, 24, 28] {
        expensive[offset..offset + 4].copy_from_slice(&2048u32.to_be_bytes());
    }
    let cod = expensive.windows(2).position(|bytes| bytes == [0xff, 0x52]).unwrap();
    expensive[cod + 6..cod + 8].copy_from_slice(&32u16.to_be_bytes());
    expensive[cod + 10] = 0;
    expensive[cod + 11] = 0;
    assert_matches!(decode(&expensive).err(), Some(Failure::TooLarge));
    assert_matches!(decode(b"\x89PNG\r\n\x1a\n").err(), Some(Failure::Malformed));
    assert_matches!(decode(&vec![0; 32 * 1024 * 1024 + 1]).err(), Some(Failure::TooLarge));
}

proptest::proptest! {
    #[test]
    fn mutations_never_publish_bad_images(offset in 0usize..470, byte in prelude::any::<u8>()) {
        let mut encoded = RGBA_J2C.to_vec();
        encoded[offset] = byte;
        if let Ok(image) = decode(&encoded) {
            let [width, height] = image.size();
            proptest::prop_assert!((1..=2048).contains(&width) && (1..=2048).contains(&height));
            proptest::prop_assert_eq!(image.pixels().len(), width as usize * height as usize * image.format().channels());
        }
    }
}
