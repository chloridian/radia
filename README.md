Radia is a new viewer for Second Life, written in Rust. It aims to keep familiar content and everyday activities working while rethinking how residents use the viewer. Its interface and interactions are open to broad redesign wherever a clearer, easier, more enjoyable experience is possible. Right now, Radia is focused on a close visual match to the official viewer, with simpler, safer, faster software underneath.

Once familiar content and everyday activities work well in Radia, it can explore new visual ideas for existing Second Life content without asking creators to redo their work, a longer-term direction is described in [the project vision](docs/vision.md).

## Try Radia

Radia is still in development, and prebuilt downloads aren't available yet. You can try the current version by building it from source. Install [stable Rust through rustup](https://rustup.rs/). On Windows, also install Visual Studio C++ Build Tools and the Windows SDK.

```sh
git clone https://github.com/chloridian/radia.git
cd Radia
cargo run --release
```

When prebuilt downloads are available, they'll be listed on [GitHub Releases](https://github.com/chloridian/radia/releases). For platform setup, see the [development guide](docs/tooling.md). For more on how Radia is built, see [architecture](docs/architecture.md) and [compatibility references](docs/reference.md).
