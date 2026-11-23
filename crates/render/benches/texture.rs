use std::{hint::black_box, time::Instant};

use assets::{Budget, Content, Kind, LoadJob, Priority, Request, Resource, Status, Store};
use divan::counter::ItemsCount;
use test_support::texture::RGBA_J2C;
use utils::AssetId;
use uuid::Uuid;

#[path = "../src/texture/compression.rs"]
#[allow(unused_imports)]
mod texture_compression;

use self::texture_compression::CompressionQuality;

#[cfg(not(codspeed))]
const EDGES: [u32; 3] = [512, 1024, 2048];
#[cfg(codspeed)]
const EDGES: [u32; 2] = [512, 1024];

fn pixels(edge: u32, alpha: bool) -> Vec<u8> {
    let mut pixels = Vec::with_capacity(edge as usize * edge as usize * 4);
    for y in 0..edge {
        for x in 0..edge {
            pixels.extend_from_slice(&[
                (x % 256) as u8,
                (y % 256) as u8,
                ((x * 13 + y * 7) % 256) as u8,
                if alpha { ((x + y * 3) % 256) as u8 } else { 255 },
            ]);
        }
    }
    pixels
}

fn bench_mipmaps(bencher: divan::Bencher, edge: u32, alpha: bool, quality: CompressionQuality) {
    assert!(texture_compression::supported([edge; 2]));
    let pixels = pixels(edge, alpha);
    let _ = black_box(texture_compression::work_bytes([edge; 2]));
    bencher.counter(ItemsCount::new(u64::from(edge).pow(2))).bench_local(|| {
        let image = texture_compression::encode(black_box(&pixels), [edge; 2], quality, || false).unwrap();
        black_box(image.format.gpu());
        black_box(image.levels[0].size);
        black_box(image.levels[0].bytes.len());
        black_box(image);
    });
}

#[divan::bench(args = EDGES)]
fn texture_compression_bc1(bencher: divan::Bencher, edge: u32) {
    bench_mipmaps(bencher, edge, false, CompressionQuality::Fast);
}

#[divan::bench(args = EDGES)]
fn texture_compression_bc3(bencher: divan::Bencher, edge: u32) {
    bench_mipmaps(bencher, edge, true, CompressionQuality::Fast);
}

#[divan::bench(args = EDGES)]
fn texture_compression_bc7_opaque(bencher: divan::Bencher, edge: u32) {
    bench_mipmaps(bencher, edge, false, CompressionQuality::High);
}

#[divan::bench(args = EDGES)]
fn texture_compression_bc7_alpha(bencher: divan::Bencher, edge: u32) {
    bench_mipmaps(bencher, edge, true, CompressionQuality::High);
}

#[divan::bench]
fn streaming_shared_pool(bencher: divan::Bencher) {
    const POOL_SIZE: usize = 16;
    bencher.counter(ItemsCount::new(POOL_SIZE as u64)).bench_local(|| {
        let mut store = Store::new(Budget::default());
        let route = store.register_route().expect("benchmark route");
        let now = Instant::now();
        let mut demands = Vec::with_capacity(POOL_SIZE);
        for index in 0..POOL_SIZE {
            let id = AssetId::new(Uuid::from_u128(200_000 + index as u128));
            let demand = store
                .request(route, Request::new(Resource { id, kind: Kind::Texture }), Priority::Visible, now)
                .expect("benchmark texture request");
            demands.push(demand);
        }
        let mut pending = Vec::new();
        while demands.iter().any(|demand| !matches!(demand.status(), Status::Ready(_))) {
            while let Some(job) = store.next_job(now) {
                if let LoadJob::Fetch(fetch) = job {
                    pending.push(fetch.complete(Ok(RGBA_J2C.to_vec())).expect("fixture fits"));
                }
            }
            let mut active = Vec::new();
            let mut remaining = Vec::new();
            for input in pending.drain(..) {
                match store.start_decode(input) {
                    Ok(job) => active.push(job),
                    Err(input) => remaining.push(input),
                }
            }
            pending = remaining;
            for job in active {
                store.complete(job.run(), now);
            }
        }
        let mut encoded_bytes = 0usize;
        for demand in &demands {
            let Status::Ready(Content::Texture(handle)) = demand.status() else { panic!("texture decoded") };
            let image = handle.image().expect("decoded image");
            let encoded = texture_compression::encode(image.pixels(), image.size(), CompressionQuality::Fast, || false).expect("encode");
            encoded_bytes += encoded.levels[0].bytes.len();
        }
        black_box(encoded_bytes);
    });
}
