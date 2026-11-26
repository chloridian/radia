import argparse
import hashlib
import random
import time
from pathlib import Path

from PIL import Image, ImageFilter, features


def pixels(width, height, channels):
    row = bytes((x * 31 + c * 63) % 256 for x in range(width) for c in range(channels))
    return b"".join(bytes((v + y * 17) % 256 for v in row) for y in range(height))


def detailed(size):
    rng = random.Random(1)
    gradient = Image.linear_gradient("L").resize((size, size))
    bands = []
    for angle in (0, 90, 180):
        noise = Image.frombytes("L", (size, size), rng.randbytes(size * size)).filter(ImageFilter.GaussianBlur(1.5))
        bands.append(Image.blend(gradient.rotate(angle), noise, 0.6))
    bands.append(gradient.rotate(270).point(lambda v: 255 if v > 96 else v * 2))
    return Image.merge("RGBA", bands)


def generate_detailed(path, size):
    detailed(size).save(path, format="JPEG2000", no_jp2=True, irreversible=True, mct=1, num_resolutions=6,
                        progression="RPCL", quality_mode="rates", quality_layers=[128, 64, 32])
    start = time.perf_counter()
    decoded = Image.open(path)
    decoded.load()
    elapsed = time.perf_counter() - start
    print(path.name, "OpenJPEG", features.version("jpg_2000"), "seconds", round(elapsed, 6),
          "encoded", path.stat().st_size, "sha256", hashlib.sha256(decoded.tobytes()).hexdigest())


def generate(path, mode, size, *, lossy=False, raw=True):
    source = pixels(size, size, len(mode))
    image = Image.frombytes(mode, (size, size), source)
    image.save(path, format="JPEG2000", no_jp2=path.suffix == ".j2c",
               irreversible=lossy, mct=1, num_resolutions=min(6, size.bit_length()),
               quality_mode="rates", quality_layers=[16, 8, 1] if lossy else [1])
    start = time.perf_counter()
    decoded = Image.open(path)
    decoded.load()
    elapsed = time.perf_counter() - start
    actual = decoded.tobytes()
    if not lossy:
        assert actual == source
    if raw:
        path.with_suffix(".raw").write_bytes(source)
    print(path.name, "OpenJPEG", features.version("jpg_2000"), "seconds", round(elapsed, 6),
          "encoded", path.stat().st_size, "output", len(actual), "sha256", hashlib.sha256(actual).hexdigest())


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--large", action="store_true")
    args = parser.parse_args()
    root = Path(__file__).parent
    for mode, name in [("L", "gray"), ("LA", "gray-alpha"), ("RGB", "rgb"), ("RGBA", "rgba")]:
        generate(root / f"{name}.j2c", mode, 8)
    generate(root / "rgba.jp2", "RGBA", 8)
    generate(root / "rgba-lossy.j2c", "RGBA", 8, lossy=True)
    Image.new("L", (4096, 4096), 73).save(root / "gray-4096.j2c", format="JPEG2000",
                                       no_jp2=True, irreversible=False, num_resolutions=6)
    generate_detailed(root / "rgba-2048-lossy.j2c", 2048)
    if args.large:
        target = root.resolve().parents[3] / "target"
        assert target.is_dir(), "run Cargo once before generating evaluation inputs"
        for size in [1024, 2048, 4096]:
            generate(target / f"rgba-{size}.j2c", "RGBA", size, raw=False)
