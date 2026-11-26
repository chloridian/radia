use std::io::Write;

use flate2::{Compression, write::ZlibEncoder};
use wgpu::TextureFormat;

const PNG_SIGNATURE: [u8; 8] = [137, 80, 78, 71, 13, 10, 26, 10];

pub fn encode_rgba_png(pixels: &[u8], width: u32, height: u32, bytes_per_row: u32, format: TextureFormat) -> Result<Vec<u8>, String> {
    if width == 0 || height == 0 {
        return Err("cannot encode empty image".to_owned());
    }
    let row_bytes = usize::try_from(width).ok().and_then(|w| w.checked_mul(4)).ok_or_else(|| "image width overflow".to_owned())?;
    let stride = usize::try_from(bytes_per_row).map_err(|_| "invalid row stride".to_owned())?;
    let rows = usize::try_from(height).map_err(|_| "invalid image height".to_owned())?;
    if stride < row_bytes || pixels.len() < stride.saturating_mul(rows) {
        return Err("pixel buffer smaller than image dimensions".to_owned());
    }
    let swap_rb = matches!(format, TextureFormat::Bgra8Unorm | TextureFormat::Bgra8UnormSrgb);

    let mut raw = Vec::with_capacity(rows.saturating_mul(row_bytes + 1));
    for row_stride in pixels.chunks_exact(stride).take(rows) {
        raw.push(0);
        let row = row_stride.get(..row_bytes).expect("stride is at least row_bytes");
        if swap_rb {
            for px in row.as_chunks::<4>().0 {
                raw.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
            }
        } else {
            raw.extend_from_slice(row);
        }
    }

    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(&raw).map_err(|e| format!("zlib write failed: {e}"))?;
    let compressed = encoder.finish().map_err(|e| format!("zlib finish failed: {e}"))?;

    let mut out = Vec::with_capacity(64 + compressed.len());
    out.extend_from_slice(&PNG_SIGNATURE);

    let mut ihdr = [0u8; 13];
    ihdr[0..4].copy_from_slice(&width.to_be_bytes());
    ihdr[4..8].copy_from_slice(&height.to_be_bytes());
    ihdr[8] = 8;
    ihdr[9] = 6;
    ihdr[10] = 0;
    ihdr[11] = 0;
    ihdr[12] = 0;
    write_chunk(&mut out, *b"IHDR", &ihdr);
    write_chunk(&mut out, *b"IDAT", &compressed);
    write_chunk(&mut out, *b"IEND", &[]);
    Ok(out)
}

fn write_chunk(out: &mut Vec<u8>, kind: [u8; 4], data: &[u8]) {
    let len = u32::try_from(data.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&kind);
    out.extend_from_slice(data);
    let mut crc = 0xffff_ffffu32;
    crc = crc32_update(crc, &kind);
    crc = crc32_update(crc, data);
    out.extend_from_slice(&(!crc).to_be_bytes());
}

fn crc32_update(mut crc: u32, bytes: &[u8]) -> u32 {
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use wgpu::TextureFormat;

    use super::{PNG_SIGNATURE, encode_rgba_png};

    #[test]
    fn mcp_png_encodes_rgba_and_bgra() {
        let rgba = [255u8, 0, 0, 255, 0, 255, 0, 255];
        let encoded_rgba = encode_rgba_png(&rgba, 2, 1, 8, TextureFormat::Rgba8Unorm).expect("rgba png");
        assert!(encoded_rgba.starts_with(&PNG_SIGNATURE));
        let encoded_bgra = encode_rgba_png(&rgba, 2, 1, 8, TextureFormat::Bgra8Unorm).expect("bgra png");
        assert!(encoded_bgra.starts_with(&PNG_SIGNATURE));
        assert_ne!(encoded_rgba, encoded_bgra);
    }
}
