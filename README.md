# framefold

Render supported HTML/CSS compositions into video frames without a browser.

framefold is a bounded renderer, not a general browser and not a pixel-perfect replacement for one. It implements a small, documented subset of HTML, CSS and layout, and it fails or draws nothing for anything outside that subset. See [Supported subset](#supported-subset) and [What does not work](#what-does-not-work) before pointing it at an arbitrary page.

> **Status: research build.** This code is not production-ready. It is being hardened toward a production release, and interfaces, formats and results may change without notice.

framefold parses a composition's HTML and CSS, lays the boxes out, runs the composition's timeline script, paints each frame with its own rasterizer, and hands RGBA frames to [`transcode`](https://github.com/webfold-ai/transcode) for H.264 MP4 encoding.

Licensed under the [Apache License, Version 2.0](LICENSE).

## "Without a browser" is not "without JavaScript"

No browser, WebView or headless Chrome is started. There is no DOM and no CSS engine other than the subset implemented in this repository.

framefold does embed a JavaScript engine, though. The default feature set (`default = ["js"]`) includes [`rquickjs`](https://crates.io/crates/rquickjs), a binding to QuickJS. It is used for one purpose only: to run the composition's timeline script and record the animation it describes (`src/timeline.rs`). GSAP itself is never loaded; the script is run against a small recording stub of it.

What executes:

- Every inline `<script>` in the document that has no `src` attribute, concatenated in document order and run once, synchronously, in a single QuickJS context before any frame is drawn. The `type` attribute is not inspected, so a `<script type="application/json">` body is also evaluated as JavaScript and will fail if it is not valid JavaScript.
- `<script src="...">` is never fetched or run. External stylesheets are not loaded either; only inline `<style>` elements are read.

What the script can use:

- The standard ECMAScript built-ins of a full QuickJS context (functions, loops, `Array`, `Math`, `JSON`, `Date`, and so on).
- `window` (an alias of the global object) and `window.__timelines` (a plain object; nothing reads it back).
- `document.querySelectorAll(sel)`, `document.querySelector(sel)` and `document.getElementById(id)`. They return opaque element handles (an array with `forEach` for the list form), not DOM nodes. `sel` accepts the same selector subset as the stylesheet parser: tag, `#id`, `.class`, compounds of those, descendant combinators and comma lists.
- `gsap.timeline()`, `gsap.to`, `gsap.set`, `gsap.fromTo`, `gsap.registerPlugin` (no-op) and `gsap.utils.toArray`. Timeline objects record `set`, `to`, `from` and `fromTo` and chain. `add`, `call`, `addLabel`, `timeScale`, `pause`, `play` and `seek` are accepted and do nothing; `duration()` returns 0.

What is not supported:

- There is no `console`, `setTimeout`/`setInterval`, `requestAnimationFrame`, `fetch` or `addEventListener`. There are no element properties or methods (`style`, `classList`, `textContent`, `createElement`, and so on). Calling any of them throws.
- Any uncaught exception aborts the whole run. In the current binary that is a panic with `timeline: "composition script failed: ..."`.
- Promise callbacks and other deferred work do not run after the script finishes, so tweens created in them are not recorded.
- Only these properties are recorded: `opacity`, `x`, `y`, `scale`, `clipPath` (an `inset(...)` value; only the right-hand inset is animated) and `filter` (`blur(Npx)`). Other keys (for example `rotation`) are ignored.
- The tween position argument must be a number of seconds. Relative or label positions such as `"+=0.5"` are read as `0`. `delay`, `repeat`, `yoyo` and stagger are ignored.
- `tl.from(...)` is recorded but interpreted like `tl.to(...)`, not as a from-tween.
- Eases understood: `none`/`linear`, `power1..4` (and `quad`, `cubic`, `quart`, `quint`, `strong`) with `.in`/`.out`/`.inOut`, `back`, and `steps(n)`. Anything else falls back to a quadratic ease-out.

Without the `js` feature (`--no-default-features`) the library still builds, but timeline extraction returns an error, so the `framefold` binary cannot render a composition.

## Composition directory

A composition is a directory. The binary reads exactly one entry point:

```
<composition-dir>/
  index.html              required
  <font files>            optional, referenced by @font-face url(...) in index.html,
                          resolved relative to <composition-dir>
```

Rules, from `src/bin/framefold.rs`, `src/html.rs`, `src/css.rs` and `src/fonts.rs`:

- `index.html` must be well-formed enough for the parser (a mismatched closing tag is an error).
- The composition root is the first element carrying a `data-composition-id` attribute. Its attributes set the output: `data-width` (default 1920), `data-height` (default 1080), `data-duration` in seconds (default 10) and `data-fps` (default 30). Only that element's subtree is laid out and painted. Styles and scripts are collected from the whole document.
- Fonts come from `@font-face` rules in an inline `<style>`. Only the first `url(...)` of each rule is used. A file starting with the `wOF2` magic is decoded as WOFF2; any other file is parsed as TrueType. A face that cannot be read or parsed is an error, not a silent substitution.
- Generic or undeclared families fall back to the font at `FRAMEFOLD_FALLBACK_FONT` (default `/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf`) if that file exists. If a family resolves to no font at all, that element draws no text and the render does not fail.
- No sample composition is committed to this repository. Tests that need a real one read a local directory and skip when it is absent.

### A minimal composition

`index.html`:

```html
<!DOCTYPE html>
<html>
<head>
<style>
  .stage { width: 100%; height: 100%; background: #101014;
           display: flex; align-items: center; justify-content: center; }
  .title { color: #ffffff; font-size: 96px; opacity: 0; }
</style>
</head>
<body>
<div data-composition-id="main" data-width="1280" data-height="720"
     data-duration="2" data-fps="30" class="stage">
  <div id="title" class="title">Hello framefold</div>
</div>
<script>
  const tl = gsap.timeline({ paused: true });
  tl.fromTo("#title", { opacity: 0, y: 40 }, { opacity: 1, y: 0, duration: 1, ease: "power3.out" }, 0.25);
  window.__timelines = window.__timelines || {};
  window.__timelines["main"] = tl;
</script>
</body>
</html>
```

This composition declares no `@font-face`, so its text uses the fallback font described above. To use your own, add an `@font-face` with `font-family`, `font-weight` and `src: url("fonts/MyFont.woff2")` and set `font-family` on the element.

## Use

```sh
cargo run --release --bin framefold -- probe  <composition-dir>
cargo run --release --bin framefold -- render <composition-dir> <out.mp4> [--fps N] [--seconds N]
```

`probe` prints one JSON line to stdout and renders nothing. For the composition above it printed:

```json
{"width":1280,"height":720,"fps":30,"duration":2,"frames":60,"frames_drawn":26,"tweens":2,"uses_blur":false}
```

`frames` is what the composition declares; `frames_drawn` is how many distinct frames the renderer actually has to paint, because frames whose animated state is identical are not drawn again.

`render` writes progress to stderr (`progress <percent> <done>/<total>`) and writes the MP4. `--fps` overrides `data-fps`; `--seconds` overrides `data-duration`.

Worker threads default to one fewer than the available cores. Set `FRAMEFOLD_THREADS=N` to choose a number, or `FRAMEFOLD_MEMORY_MB=N` to pick the most workers that fit a memory budget (a 1080p worker needs roughly 23 MB). The usage text also lists `--threads` and `--memory-mb` flags, but the current binary reads only the environment variables.

The library exposes `Canvas` for drawing RGBA frames. See `src/lib.rs` and `src/bin/framefold.rs` for the current boundaries.

## Timeline behaviour

- Layout runs once. Geometry does not change over time; a frame is produced by painting the same box tree with per-element overrides (opacity, translation, scale, inset clip, blur) taken from the recorded timeline. A frame is therefore a pure function of its time, and frames render in parallel with no shared state.
- Output length is `data-duration` (or `--seconds`) times the frame rate, rounded to whole frames. It is not taken from the length of the timeline.
- A `fromTo` establishes its start state from time 0, which is how a composition declares its at-rest appearance. `set` and `to` take effect only from their own start time; before that the element keeps what the stylesheet gave it. A bare `to` starts from the value left by earlier tweens on the same element and property.
- Before encoding, the timeline is asked which frames can differ. Repeats are neither painted nor encoded as pixels; they become skip P-frames.
- An element is addressed by its document-order position inside the composition root, so elements without an `id` can be animated.

## Encoder requirements and expected output

- Encoding runs in-process through `transcode`. No browser, no `ffmpeg` and no other external encoder is needed to render. `ffmpeg`/`ffprobe` are used only by one test, which skips when they are absent.
- The `render` command writes an MP4 containing one H.264 video track (Baseline profile, level 3.0, fixed quantiser `transcode::h264::DEFAULT_QP`). Each distinct frame is an IDR frame; repeated frames are skip P-frames, and the sync-sample table lists only the IDR frames. The frame rate is rounded to an integer.
- The `render` command writes no audio track. (`transcode` can encode AAC, but the binary does not use it.)
- Size is `data-width` x `data-height`; dimensions that are not multiples of 16 are cropped in the stream header.
- Rendering the minimal composition above produced a 1280x720, 30 fps, 60-frame, 2.0 second H.264 MP4 when checked with `ffprobe`. Timings and file size depend on the machine and are not published here.

## Supported subset

Derived from `src/html.rs`, `src/css.rs`, `src/layout.rs` and `src/paint.rs`.

- **HTML:** a tolerant parser for well-formed markup authored by a tool. It is strict where leniency would hide a real problem. It is not an HTML5 parser. `<img>` is parsed as a void element and nothing is drawn for it.
- **Selectors:** tag, `#id`, `.class`, compounds of those, descendant combinators, comma lists, and `*`. A selector using `>`, `+`, `~`, pseudo-classes or attribute selectors is dropped.
- **Cascade:** specificity, source order, inheritance of the usual text properties, an inline `style` attribute, and these shorthands: `margin`, `padding`, `inset`, `gap`, `border` and `border-<side>`, `flex`, and the colour form of `background`. `!important` is stripped, not honoured. `@media`, `@keyframes` and other at-rules are skipped; `@font-face` is read.
- **Units:** `px`, `rem` (fixed at 16px), `em`, `%`, and a bare `0`. A unitless non-zero number is not a length.
- **Colours:** `#hex`, `rgb()`, `rgba()`, and the names `black`, `white`, `transparent`, `red`, `green`, `blue`, `gray`, `grey`. No `calc()` and no custom properties.
- **Layout:** `display` of `block`, `flex`, `inline`, `none` (and `inline-block`, `inline-flex`, `flow-root`, `list-item` mapped onto those); `flex-direction` (`row`, `column`), `gap`, `align-items`, `justify-content` (`flex-start`, `flex-end`, `center`, `space-between`, `space-around`, `space-evenly`), `flex-grow`, `flex-basis`; `position: absolute` and `fixed` with `top`/`right`/`bottom`/`left`; `width`, `height`, `box-sizing`, `margin`, `padding`; text with `font-family`, `font-size`, `font-weight`, `line-height`, `letter-spacing`, `white-space` (`nowrap`, `pre`), `text-transform` and `text-align`.
- **Painting:** anti-aliased scanline rasterizer (nonzero winding), affine transforms, rectangular clipping (`overflow: hidden`, `clip-path: inset(...)` in percentages), `background-color`, `border-radius` on backgrounds, per-side rectangle borders, `opacity`, `transform-origin`, text from real TrueType outlines, and an animated blur.
- **Text and fonts:** glyph outlines and advances come from `ruler`. WOFF2 files are decoded to TrueType by `woff2`, so a composition's own fonts are used.

## What does not work

- Floats, tables, CSS grid, multi-line inline wrapping with mixed styles, writing modes and margin collapsing (stated in `src/layout.rs`).
- Properties that no layout or paint code reads, including `min-*`/`max-*` sizes, `flex-wrap`, `flex-shrink`, `align-self`, `z-index`, a static `transform`, `box-shadow`, `text-decoration`, `visibility`, `border-style` and `font` shorthand. They are accepted by the parser and have no effect.
- Images and gradients are not drawn.
- General path stroking. Only rectangle borders are stroked.
- Media queries, pseudo-classes and pseudo-elements, `calc()`, custom properties.
- Pixel-perfect agreement with a browser. No comparative speed or memory result is published here.

## Dependencies

| Dependency | Source | Pin | Required or feature-gated | Purpose |
| --- | --- | --- | --- | --- |
| [`ruler`](https://github.com/webfold-ai/ruler) | git | commit `6994c74203641fecf16b0d7b147a950f0e1fae06` | Required | Text geometry, TrueType outlines and font metrics. |
| [`transcode`](https://github.com/webfold-ai/transcode) | git | commit `f855e4217105863354f58c1d1c840685fff6e44b` | Required | H.264 encoding and MP4 muxing. |
| [`brotli`](https://crates.io/crates/brotli) | crates.io | `8` (`default-features = false`, `std`) | Required | WOFF2 uses Brotli. Isolated behind `woff2::to_ttf`. |
| [`rquickjs`](https://crates.io/crates/rquickjs) | crates.io | `0.10` (`bindgen`) | Optional: feature `js`, on by default | Runs the composition's timeline script. Without it, timeline extraction returns an error. |

`Cargo.toml` still addresses `ruler` and `transcode` through their former `github.com/Ptyktos/...` URLs. The pinned commits were checked to exist in the `webfold-ai` repositories linked above. The Rust toolchain must be 1.89 or newer. The `bindgen` feature of `rquickjs` needs a C compiler and libclang at build time.

## Build and test

```sh
cargo build --all-targets
cargo test
cargo clippy --all-targets -- -D warnings
```

Some tests read a local composition directory and skip when it is absent. Some tests need `ffmpeg` on `PATH` and skip when it is absent. CI runs the same commands with `--locked`; see Status below.

## Status

Early. The API and the composition subset may change. See [CHANGELOG.md](CHANGELOG.md).

Known issues at the time of writing, found by building the pinned revisions:

- The committed `Cargo.lock` still records `ruler` from a former GitLab URL, so `cargo build --locked` fails with "cannot update the lock file". Regenerating the lock file resolves it.
- With the lock file regenerated, `cargo build --bins` builds the `framefold` binary against the pinned `transcode` commit, which provides `encode_idr_sample`, `assemble_planned`, `nal_unit_pub` and `write_h264_mp4_sync`.
- `render::budget_tests::a_budget_buys_proportionally_many_workers` failed on a 4-core machine; the worker count is clamped to the core count, which is the likely cause. The other 132 tests in the run passed (tests that skip count as passed).
- The binary panics on a missing `index.html`, a missing `data-composition-id`, or a script error, rather than returning a diagnostic.

## Contributing and security

See [CONTRIBUTING.md](CONTRIBUTING.md), [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md) and [SECURITY.md](SECURITY.md). Report vulnerabilities through [GitHub private vulnerability reporting](https://github.com/webfold-ai/framefold/security/advisories/new).
