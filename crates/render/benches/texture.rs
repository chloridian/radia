use std::hint::black_box;

use block_compression::{BC7Settings, CompressionVariant, encode::compress_rgba8};
use divan::counter::ItemsCount;
use intel_tex_2::{RgbaSurface, bc1, bc3, bc7};

#[path = "../src/texture/compression.rs"]
#[allow(unused_imports)]
mod texture_compression;

use self::texture_compression::{CompressionFormat, CompressionQuality};

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

fn bench_variant(bencher: divan::Bencher, edge: u32, variant: CompressionVariant, alpha: bool) {
    let pixels = pixels(edge, alpha);
    let mut blocks = vec![0; variant.blocks_byte_size(edge, edge)];
    bencher.counter(ItemsCount::new(u64::from(edge).pow(2))).bench_local(|| {
        compress_rgba8(variant, black_box(&pixels), black_box(&mut blocks), edge, edge, edge * 4);
        black_box(&blocks);
    });
}

fn bench_ispc(bencher: divan::Bencher, edge: u32, format: CompressionFormat, alpha: bool) {
    let pixels = pixels(edge, alpha);
    let surface = RgbaSurface { data: &pixels, width: edge, height: edge, stride: edge * 4 };
    let mut blocks = vec![0; (edge / 4) as usize * (edge / 4) as usize * format.block_bytes()];
    bencher.counter(ItemsCount::new(u64::from(edge).pow(2))).bench_local(|| {
        match format {
            CompressionFormat::Bc1 => bc1::compress_blocks_into(black_box(&surface), black_box(&mut blocks)),
            CompressionFormat::Bc3 => bc3::compress_blocks_into(black_box(&surface), black_box(&mut blocks)),
            CompressionFormat::Bc7 => unreachable!(),
        }
        black_box(&blocks);
    });
}

fn bench_ispc_bc7(bencher: divan::Bencher, edge: u32, settings: bc7::EncodeSettings, alpha: bool) {
    let pixels = pixels(edge, alpha);
    let surface = RgbaSurface { data: &pixels, width: edge, height: edge, stride: edge * 4 };
    let mut blocks = vec![0; bc7::calc_output_size(edge, edge)];
    bencher.counter(ItemsCount::new(u64::from(edge).pow(2))).bench_local(|| {
        bc7::compress_blocks_into(&settings, black_box(&surface), black_box(&mut blocks));
        black_box(&blocks);
    });
}

mod benches {
    use super::*;

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

    #[divan::bench(args = EDGES)]
    fn texture_compression_bc1(bencher: divan::Bencher, edge: u32) {
        bench_variant(bencher, edge, CompressionVariant::BC1, false);
    }

    #[divan::bench(args = EDGES)]
    fn texture_compression_bc3(bencher: divan::Bencher, edge: u32) {
        bench_variant(bencher, edge, CompressionVariant::BC3, true);
    }

    #[divan::bench(args = EDGES)]
    fn texture_compression_bc7_opaque_ultrafast(bencher: divan::Bencher, edge: u32) {
        bench_variant(bencher, edge, CompressionVariant::BC7(BC7Settings::opaque_ultra_fast()), false);
    }

    #[divan::bench(args = EDGES)]
    fn texture_compression_bc7_alpha_ultrafast(bencher: divan::Bencher, edge: u32) {
        bench_variant(bencher, edge, CompressionVariant::BC7(BC7Settings::alpha_ultrafast()), true);
    }

    #[divan::bench(args = EDGES)]
    fn texture_compression_bc7_alpha_fast(bencher: divan::Bencher, edge: u32) {
        bench_variant(bencher, edge, CompressionVariant::BC7(BC7Settings::alpha_fast()), true);
    }

    #[divan::bench(args = EDGES)]
    fn texture_compression_ispc_bc1(bencher: divan::Bencher, edge: u32) {
        bench_ispc(bencher, edge, CompressionFormat::Bc1, false);
    }

    #[divan::bench(args = EDGES)]
    fn texture_compression_ispc_bc3(bencher: divan::Bencher, edge: u32) {
        bench_ispc(bencher, edge, CompressionFormat::Bc3, true);
    }

    #[divan::bench(args = EDGES)]
    fn texture_compression_ispc_bc7_opaque_ultrafast(bencher: divan::Bencher, edge: u32) {
        bench_ispc_bc7(bencher, edge, bc7::opaque_ultra_fast_settings(), false);
    }

    #[divan::bench(args = EDGES)]
    fn texture_compression_ispc_bc7_alpha_ultrafast(bencher: divan::Bencher, edge: u32) {
        bench_ispc_bc7(bencher, edge, bc7::alpha_ultra_fast_settings(), true);
    }
}
