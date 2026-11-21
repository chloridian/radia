use std::hint::black_box;

use divan::counter::ItemsCount;

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
fn texture_compression_bc1_with_mips(bencher: divan::Bencher, edge: u32) {
    bench_mipmaps(bencher, edge, false, CompressionQuality::Fast);
}

#[divan::bench(args = EDGES)]
fn texture_compression_bc3_with_mips(bencher: divan::Bencher, edge: u32) {
    bench_mipmaps(bencher, edge, true, CompressionQuality::Fast);
}

#[divan::bench(args = EDGES)]
fn texture_compression_bc7_opaque_with_mips(bencher: divan::Bencher, edge: u32) {
    bench_mipmaps(bencher, edge, false, CompressionQuality::High);
}

#[divan::bench(args = EDGES)]
fn texture_compression_bc7_alpha_with_mips(bencher: divan::Bencher, edge: u32) {
    bench_mipmaps(bencher, edge, true, CompressionQuality::High);
}
