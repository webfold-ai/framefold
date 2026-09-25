# Framefold

Rendering a short video from a web layout can mean opening a browser for every frame. That adds work and makes each render harder to run at scale. Framefold draws supported layouts directly into video frames and sends them to an encoder.

This is an early renderer. It supports its own bounded HTML and CSS path, timeline handling, text and shapes. It is not a general browser or a pixel-perfect replacement for every web page. No comparative speed or memory result is published here yet.

## Try it

```sh
cargo run --bin framefold -- probe <composition-dir>
cargo run --bin framefold -- render <composition-dir> <out.mp4> --fps 30 --seconds 5
```

The library exposes `Canvas` for drawing RGBA frames. The binary uses `ruler` for text geometry and `transcode` for video encoding. See `src/lib.rs` and `src/bin/framefold.rs` for the current boundaries.
