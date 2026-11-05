# framefold

> **Status: research build.** This code is not production-ready. It is being hardened toward a production release, and interfaces, formats and results may change without notice.

framefold renders an HTML-style composition to video frames without starting a browser. It parses a bounded subset of HTML and CSS, lays the boxes out, runs the composition's timeline script, paints the frames with its own rasterizer, and hands RGBA frames to `transcode` for H.264/AAC MP4 encoding.

Licensed under the [Apache License, Version 2.0](LICENSE).

## What works

- **HTML:** a tolerant parser for well-formed markup authored by a tool. It is strict where leniency would hide a real problem. It is not an HTML5 parser.
- **CSS:** selectors, specificity, source order, inheritance and the shorthand expansions real stylesheets use. Not a full CSS engine: no media queries, pseudo-classes, `calc()`, custom properties or `!important`. Only the colour form of `background` is used.
- **Layout:** block flow, flex (row and column, with `gap`, `align-items`, `justify-content` and `flex-grow`), absolute positioning against the nearest positioned ancestor, and inline text runs measured with the composition's own fonts.
- **Text and fonts:** glyph outlines and advances come from `ruler`. WOFF2 `@font-face` files are decoded to TrueType by `woff2`, so a composition's own fonts are used.
- **Painting:** anti-aliased scanline rasterizer (nonzero winding), affine transforms, rectangular clipping, rounded rectangles, source-over alpha, and rectangle borders.
- **Timeline:** animation timelines are extracted by running the composition's script under a recording stub of GSAP. This needs the default `js` feature, which uses `rquickjs`.
- **Frames:** a frame is a pure function of its index, so frames render in parallel with no shared state. A glyph cache avoids re-rasterizing repeated glyphs.
- **Output:** frames are encoded to MP4 by `transcode::mp4::write_h264_mp4_sync` (H.264, with AAC audio where the pipeline provides it).

## What does not

- Floats, tables, CSS grid, multi-line inline wrapping with mixed styles, writing modes and margin collapsing. These are stated in `src/layout.rs`.
- Images and gradients are not drawn. `<img>` is parsed as a void element and nothing else.
- General path stroking. Only rectangle borders are stroked.
- Pixel-perfect agreement with a browser. No comparative speed or memory result is published here.

## Build and test

```sh
cargo build --locked --all-targets
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```

Some tests read a local composition directory and skip when it is absent. Some tests need `ffmpeg` on `PATH` and skip when it is absent.

## Use

```sh
cargo run --bin framefold -- probe <composition-dir>
cargo run --bin framefold -- render <composition-dir> <out.mp4> --fps 30 --seconds 5
```

The library exposes `Canvas` for drawing RGBA frames. See `src/lib.rs` and `src/bin/framefold.rs` for the current boundaries.

## Dependencies

- [`ruler`](https://github.com/webfold-ai/ruler): text geometry, TrueType outlines.
- [`transcode`](https://github.com/webfold-ai/transcode): H.264/AAC encoding and MP4 muxing.
- `brotli`: WOFF2 requires Brotli. It is isolated behind `woff2::to_ttf`.
- `rquickjs` (optional, default feature `js`): runs the composition's timeline script.

## Status

Early. The API and the composition subset may change. See [CHANGELOG.md](CHANGELOG.md).

## Contributing and security

See [CONTRIBUTING.md](CONTRIBUTING.md), [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md) and [SECURITY.md](SECURITY.md).
