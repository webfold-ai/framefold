//! Extracting a composition's animation timeline by *running* its script.
//!
//! # Why this executes JavaScript
//!
//! The obvious approach is to pattern-match `tl.fromTo(...)` calls out of the
//! source. It does not survive contact with a real composition. glossa builds
//! half its timeline from a function:
//!
//! ```js
//! function wordCascade(containerSel, t0) {
//!   const words = document.querySelectorAll(containerSel + " .w");
//!   words.forEach((el, i) => {
//!     const offset = i === 0 ? 70 : i === words.length - 1 ? 36 : 46;
//!     tl.set(el, { opacity: 1, y: offset }, t);
//!     tl.to(el, { y: 0, duration: dur, ease: "power4.out" }, t);
//!     t = t + dur - F;
//!   });
//! }
//! ```
//!
//! Fourteen tweens, on elements with no id, at times computed by an
//! accumulator. A matcher would have to become a JavaScript interpreter to read
//! that — and would still break on the next composition written slightly
//! differently. So the script is executed, and what it does is recorded.
//!
//! **GSAP itself is never loaded.** The script only ever *tells* the timeline
//! what to do, so a stub that records `set`/`to`/`fromTo` captures everything
//! without implementing a tweening engine. Interpolation happens natively, in
//! Rust, from the recording.
//!
//! What the shim provides: `document.querySelectorAll`/`querySelector`, a
//! `gsap` object with `timeline()`, and `window.__timelines`. Elements are
//! handles carrying a document-order index, so a tween can address an element
//! that has no id — which the word cascade requires.

use crate::css;
use crate::html::Element;
use crate::paint::{Animator, Overrides};
use std::collections::HashMap;

/// One animatable property, as GSAP names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Prop {
    Opacity,
    X,
    Y,
    Scale,
    /// `clip-path: inset(...)`, tracked as the right-hand inset fraction,
    /// which is the form compositions animate for a wipe reveal.
    ClipInsetRight,
    /// `filter: blur(Npx)`.
    Blur,
}

impl Prop {
    fn from_key(k: &str) -> Option<Prop> {
        match k {
            "opacity" => Some(Prop::Opacity),
            "x" => Some(Prop::X),
            "y" => Some(Prop::Y),
            "scale" => Some(Prop::Scale),
            "clipPath" => Some(Prop::ClipInsetRight),
            "filter" => Some(Prop::Blur),
            _ => None,
        }
    }

    /// The value this property has when nothing has animated it.
    fn resting(self) -> f32 {
        match self {
            Prop::Opacity | Prop::Scale => 1.0,
            Prop::X | Prop::Y | Prop::ClipInsetRight | Prop::Blur => 0.0,
        }
    }
}

/// GSAP's easing functions, as far as compositions use them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Ease {
    None,
    Power(u8, Dir),
    /// `back.out(overshoot)` — overshoots the target then settles.
    Back(f32, Dir),
    /// `steps(n)` — quantised, used for typewriter reveals.
    Steps(u32),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Dir {
    In,
    Out,
    InOut,
}

impl Ease {
    /// Parse a GSAP ease string. An unrecognised ease falls back to the
    /// library's own default rather than to linear, since linear is visibly
    /// wrong for entrance motion.
    pub fn parse(s: &str) -> Ease {
        let s = s.trim();
        if s == "none" || s == "linear" {
            return Ease::None;
        }
        if let Some(rest) = s.strip_prefix("steps(") {
            if let Some(n) = rest.strip_suffix(')').and_then(|n| n.trim().parse().ok()) {
                return Ease::Steps(n);
            }
        }
        let (name, dir) = match s.split_once('.') {
            Some((n, d)) => (
                n,
                match d.split('(').next().unwrap_or("out") {
                    "in" => Dir::In,
                    "inOut" => Dir::InOut,
                    _ => Dir::Out,
                },
            ),
            None => (s, Dir::Out),
        };
        if name == "back" {
            let overshoot = s
                .split_once('(')
                .and_then(|(_, r)| r.strip_suffix(')'))
                .and_then(|n| n.trim().parse().ok())
                .unwrap_or(1.70158);
            return Ease::Back(overshoot, dir);
        }
        let power = match name {
            "power1" | "quad" => 2,
            "power2" | "cubic" => 3,
            "power3" | "quart" => 4,
            "power4" | "quint" | "strong" => 5,
            // GSAP's default ease is power1.out, not linear.
            _ => 2,
        };
        Ease::Power(power, dir)
    }

    /// Map linear progress to eased progress.
    pub fn apply(self, t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        match self {
            Ease::None => t,
            Ease::Steps(n) if n > 0 => {
                // GSAP's steps() lands on the final value at t=1.
                (t * n as f32).floor() / n as f32
            }
            Ease::Steps(_) => t,
            Ease::Power(p, dir) => {
                let p = p as i32;
                match dir {
                    Dir::In => t.powi(p),
                    Dir::Out => 1.0 - (1.0 - t).powi(p),
                    Dir::InOut => {
                        if t < 0.5 {
                            (2.0 * t).powi(p) / 2.0
                        } else {
                            1.0 - (2.0 - 2.0 * t).powi(p) / 2.0
                        }
                    }
                }
            }
            Ease::Back(s, dir) => match dir {
                Dir::In => t * t * ((s + 1.0) * t - s),
                Dir::Out => {
                    let t = t - 1.0;
                    t * t * ((s + 1.0) * t + s) + 1.0
                }
                Dir::InOut => {
                    let s = s * 1.525;
                    if t < 0.5 {
                        let t = 2.0 * t;
                        0.5 * (t * t * ((s + 1.0) * t - s))
                    } else {
                        let t = 2.0 * t - 2.0;
                        0.5 * (t * t * ((s + 1.0) * t + s) + 2.0)
                    }
                }
            },
        }
    }
}

/// One recorded tween on one element and property.
#[derive(Clone, Debug)]
pub struct Tween {
    pub node: usize,
    pub prop: Prop,
    pub start: f32,
    pub duration: f32,
    /// `None` means "from wherever the value already is" — a `.to()` tween,
    /// whose start value depends on everything scheduled before it.
    pub from: Option<f32>,
    pub to: f32,
    pub ease: Ease,
    /// Whether this tween's start value applies *before* the tween runs.
    ///
    /// Only `fromTo` does: GSAP defaults `immediateRender` to true for it, so
    /// the from-state is established as soon as the timeline is built. That is
    /// how a composition declares its at-rest appearance.
    ///
    /// `set` and `to` do not. A `set` is a zero-duration tween *at its time* —
    /// before then the element keeps whatever CSS gave it. Treating a `set` as
    /// pinning makes every element it touches jump to its final value at t=0:
    /// on a real composition that put the entire hook on screen in frame 1,
    /// where the browser shows an empty background.
    pub pins: bool,
}

impl Tween {
    fn end(&self) -> f32 {
        self.start + self.duration
    }
}

/// A composition's whole timeline: every tween, resolved against the document.
#[derive(Clone, Debug, Default)]
pub struct Timeline {
    pub tweens: Vec<Tween>,
    pub duration: f32,
}

impl Timeline {
    /// Value of one property on one element at time `t`.
    ///
    /// Tweens are applied in schedule order, each starting from the value left
    /// by the ones before it. That sequencing is what makes a bare `.to()`
    /// meaningful: GSAP records the value at the moment the tween begins, so a
    /// `.to()` after a `.set()` starts from what the `.set()` left behind.
    pub fn value_at(&self, node: usize, prop: Prop, t: f32) -> f32 {
        self.value_at_opt(node, prop, t)
            .unwrap_or_else(|| prop.resting())
    }

    /// As [`Self::value_at`], but `None` when no tween has taken effect yet.
    ///
    /// The distinction decides what a frame looks like before its animation
    /// starts. A property no tween has touched must fall through to whatever
    /// the stylesheet said — and the stylesheet is where a composition declares
    /// its at-rest state, typically `opacity: 0` on everything that fades in.
    /// Returning a generic default instead (1.0 for opacity) and overriding
    /// with it puts the entire first scene on screen in frame one, where the
    /// browser shows an empty background.
    ///
    /// A tween "has taken effect" at `t` when it has started, or when it is a
    /// `fromTo`, whose from-state applies as soon as the timeline is built.
    pub fn value_at_opt(&self, node: usize, prop: Prop, t: f32) -> Option<f32> {
        let mut current = prop.resting();
        let mut touched = false;
        let mut relevant: Vec<&Tween> = self
            .tweens
            .iter()
            .filter(|tw| tw.node == node && tw.prop == prop)
            .collect();
        relevant.sort_by(|a, b| {
            a.start
                .partial_cmp(&b.start)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        for tw in relevant {
            let from = tw.from.unwrap_or(current);
            if t >= tw.end() {
                current = tw.to;
                touched = true;
            } else if t >= tw.start {
                touched = true;
                let raw = if tw.duration <= 0.0 {
                    1.0
                } else {
                    (t - tw.start) / tw.duration
                };
                current = from + (tw.to - from) * tw.ease.apply(raw);
            } else {
                // Not started. Only a `fromTo` reaches back before its own
                // start time; see Tween::pins.
                if tw.pins {
                    if let Some(f) = tw.from {
                        current = f;
                        touched = true;
                    }
                }
                break;
            }
        }
        touched.then_some(current)
    }

    /// Overrides for one element at time `t`.
    pub fn overrides_at(&self, node: usize, t: f32) -> Overrides {
        let mut ov = Overrides::default();
        if let Some(o) = self.value_at_opt(node, Prop::Opacity, t) {
            ov.opacity = Some(o.clamp(0.0, 1.0));
        }
        let x = self.value_at_opt(node, Prop::X, t);
        let y = self.value_at_opt(node, Prop::Y, t);
        if x.is_some() || y.is_some() {
            ov.translate = (x.unwrap_or(0.0), y.unwrap_or(0.0));
        }
        if let Some(s) = self.value_at_opt(node, Prop::Scale, t) {
            ov.scale = Some(s);
        }
        if let Some(b) = self.value_at_opt(node, Prop::Blur, t) {
            if b > 0.5 {
                ov.blur = Some(b);
            }
        }
        if let Some(r) = self.value_at_opt(node, Prop::ClipInsetRight, t) {
            ov.inset_clip = Some((0.0, r.clamp(0.0, 1.0), 0.0, 0.0));
        }
        ov
    }

    /// True if any tween touches blur.
    pub fn uses_blur(&self) -> bool {
        self.tweens.iter().any(|t| t.prop == Prop::Blur)
    }
}

/// A timeline bound to a specific frame time, usable as a paint [`Animator`].
pub struct FrameAnimator<'a> {
    pub timeline: &'a Timeline,
    pub time: f32,
}

impl Animator for FrameAnimator<'_> {
    fn for_id(&self, _id: Option<&str>) -> Overrides {
        // Ids are not how a timeline addresses elements here; see for_node.
        Overrides::default()
    }

    fn for_node(&self, node: usize, _id: Option<&str>) -> Overrides {
        self.timeline.overrides_at(node, self.time)
    }
}

/// Parse `inset(0 100% 0 0)` into its right-hand fraction.
fn inset_right(v: &str) -> Option<f32> {
    let (_, r, _, _) = crate::paint::parse_inset(v)?;
    Some(r)
}

/// Parse `blur(12px)` into pixels.
fn blur_px(v: &str) -> Option<f32> {
    let inner = v.trim().strip_prefix("blur(")?.strip_suffix(')')?;
    inner.trim().trim_end_matches("px").trim().parse().ok()
}

/// Convert one recorded vars value to a number for its property.
fn numeric(prop: Prop, raw: &str) -> Option<f32> {
    match prop {
        Prop::ClipInsetRight => inset_right(raw),
        Prop::Blur => blur_px(raw),
        _ => raw.trim().trim_end_matches("px").trim().parse::<f32>().ok(),
    }
}

/// One flat record from the shim: a single element/property pair.
#[derive(Debug)]
struct RawCall {
    kind: String,
    node: usize,
    key: String,
    from: Option<String>,
    to: String,
    position: f32,
    duration: f32,
    ease: String,
}

/// Parse one separator-delimited record. The shim emits exactly nine fields.
fn decode_call(line: &str) -> Result<RawCall, String> {
    let f: Vec<&str> = line.split('\u{1}').collect();
    if f.len() != 9 {
        return Err(format!(
            "timeline record has {} fields, expected 9",
            f.len()
        ));
    }
    Ok(RawCall {
        kind: f[0].to_string(),
        node: f[1].parse().map_err(|_| "bad node index".to_string())?,
        key: f[2].to_string(),
        from: (f[3] == "1").then(|| f[4].to_string()),
        to: f[5].to_string(),
        position: f[6].parse().unwrap_or(0.0),
        duration: f[7].parse().unwrap_or(0.0),
        ease: f[8].to_string(),
    })
}

/// Run a composition's inline scripts and record the timeline they build.
///
/// `root` must be **the same element the layout was built from**. Node indices
/// are document-order positions within that subtree, so extracting against
/// `<html>` while laying out from the composition root silently shifts every
/// index by the number of elements in `<head>` — the timeline then animates
/// the wrong boxes, plausibly and without error.
pub fn extract(root: &Element, script: &str) -> Result<Timeline, String> {
    let calls = run_script(root, script)?;
    let mut tl = Timeline::default();

    for c in calls {
        let Some(prop) = Prop::from_key(&c.key) else {
            continue;
        };
        let Some(to) = numeric(prop, &c.to) else {
            continue;
        };
        let from = c.from.as_deref().and_then(|v| numeric(prop, v));
        // `set` pins a value with no interpolation. `fromTo` pins its start
        // immediately — GSAP's immediateRender defaults to true for fromTo,
        // which is how a composition establishes its at-rest state. A bare
        // `to` has immediateRender false and inherits whatever came before.
        let is_set = c.kind == "set";
        let from = if is_set { Some(to) } else { from };
        tl.tweens.push(Tween {
            node: c.node,
            prop,
            start: c.position,
            duration: if is_set { 0.0 } else { c.duration },
            from,
            to,
            ease: Ease::parse(&c.ease),
            pins: c.kind == "fromTo",
        });
    }

    tl.duration = tl.tweens.iter().map(|t| t.end()).fold(0.0, f32::max);
    Ok(tl)
}

#[cfg(feature = "js")]
fn run_script(root: &Element, script: &str) -> Result<Vec<RawCall>, String> {
    use rquickjs::{Context, Function, Runtime};

    // Resolve selectors against the real document, in Rust — the shim only
    // needs to hand JavaScript opaque handles. The callback must outlive this
    // scope as far as the engine is concerned, so it owns its own copy of the
    // tree rather than borrowing one.
    let doc = root.clone();
    let lookup = move |sel: String| -> Vec<usize> { css::query_indices_list(&doc, &sel) };

    let rt = Runtime::new().map_err(|e| format!("js runtime: {e}"))?;
    let ctx = Context::full(&rt).map_err(|e| format!("js context: {e}"))?;

    let recorded: std::rc::Rc<std::cell::RefCell<Vec<String>>> =
        std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));

    let out = recorded.clone();
    ctx.with(|c| -> Result<(), String> {
        let globals = c.globals();

        let q = Function::new(c.clone(), move |sel: String| lookup(sel))
            .map_err(|e| format!("js fn: {e}"))?;
        globals
            .set("__query", q)
            .map_err(|e| format!("js set: {e}"))?;

        let rec = Function::new(c.clone(), move |json: String| {
            out.borrow_mut().push(json);
        })
        .map_err(|e| format!("js fn: {e}"))?;
        globals
            .set("__record", rec)
            .map_err(|e| format!("js set: {e}"))?;

        // The shim. Elements are `{__node: i}` handles; a timeline method
        // records its arguments and returns `this` so calls can chain.
        let shim = r#"
            globalThis.window = globalThis;
            function __wrap(indices) {
                var list = indices.map(function (i) { return { __node: i }; });
                list.forEach = Array.prototype.forEach.bind(list);
                return list;
            }
            globalThis.document = {
                querySelectorAll: function (sel) { return __wrap(__query(String(sel))); },
                querySelector: function (sel) { var l = __wrap(__query(String(sel))); return l[0] || null; },
                getElementById: function (id) { var l = __wrap(__query('#' + id)); return l[0] || null; },
            };
            function __nodes(target) {
                if (target == null) return [];
                if (typeof target === 'string') return __query(target);
                if (Array.isArray(target)) {
                    var out = [];
                    target.forEach(function (t) { out = out.concat(__nodes(t)); });
                    return out;
                }
                if (typeof target === 'object' && '__node' in target) return [target.__node];
                return [];
            }
            function __pairs(vars) {
                var out = [];
                if (!vars) return out;
                Object.keys(vars).forEach(function (k) {
                    var v = vars[k];
                    if (typeof v === 'number' || typeof v === 'string') {
                        out.push([k, String(v)]);
                    }
                });
                return out;
            }
            function __mk() {
                var tl = {};
                // One flat record per (element, property). Emitting nested JSON
                // and re-parsing it in Rust was a needless second format to get
                // wrong — an empty `from` array is indistinguishable from a
                // missing one without real bracket matching, and reading past it
                // turns a bare .to() into a .fromTo(), pinning its end value from
                // t=0. A field-separated line has no nesting to mis-read.
                var SEP = '\u0001';
                function push(kind, target, a, b, pos) {
                    var from = b ? a : null;
                    var to = b ? b : a;
                    var nodes = __nodes(target);
                    var dur = (to && typeof to.duration === 'number') ? to.duration : 0;
                    var ease = (to && typeof to.ease === 'string') ? to.ease : '';
                    var position = typeof pos === 'number' ? pos : 0;
                    __pairs(to).forEach(function (kv) {
                        var key = kv[0];
                        if (key === 'duration' || key === 'ease' || key === 'delay') return;
                        var fromVal = '';
                        var hasFrom = '0';
                        if (from && Object.prototype.hasOwnProperty.call(from, key)) {
                            fromVal = String(from[key]);
                            hasFrom = '1';
                        }
                        nodes.forEach(function (n) {
                            __record([kind, n, key, hasFrom, fromVal, String(kv[1]),
                                      position, dur, ease].join(SEP));
                        });
                    });
                    return tl;
                }
                tl.set = function (t, v, p) { return push('set', t, v, null, p); };
                tl.to = function (t, v, p) { return push('to', t, v, null, p); };
                tl.from = function (t, v, p) { return push('from', t, v, null, p); };
                tl.fromTo = function (t, a, b, p) { return push('fromTo', t, a, b, p); };
                tl.add = function () { return tl; };
                tl.call = function () { return tl; };
                tl.addLabel = function () { return tl; };
                tl.timeScale = function () { return tl; };
                tl.pause = function () { return tl; };
                tl.play = function () { return tl; };
                tl.seek = function () { return tl; };
                tl.duration = function () { return 0; };
                return tl;
            }
            globalThis.gsap = {
                timeline: __mk,
                to: function (t, v) { return __mk().to(t, v, 0); },
                set: function (t, v) { return __mk().set(t, v, 0); },
                fromTo: function (t, a, b) { return __mk().fromTo(t, a, b, 0); },
                registerPlugin: function () {},
                utils: { toArray: function (t) { return __wrap(__nodes(t)); } },
            };
            globalThis.__timelines = {};
        "#;
        c.eval::<(), _>(shim)
            .map_err(|e| format!("shim failed: {e}"))?;
        c.eval::<(), _>(script)
            .map_err(|e| format!("composition script failed: {e}"))?;
        Ok(())
    })?;

    // Decode what the shim recorded.
    let mut calls = Vec::new();
    for json in recorded.borrow().iter() {
        calls.push(decode_call(json)?);
    }
    Ok(calls)
}

#[cfg(not(feature = "js"))]
fn run_script(_root: &Element, _script: &str) -> Result<Vec<RawCall>, String> {
    Err("timeline extraction needs the `js` feature".into())
}

/// Node-indexed overrides for a frame, ready for paint.
pub struct NodeOverrides {
    pub map: HashMap<usize, Overrides>,
}

impl NodeOverrides {
    pub fn at(timeline: &Timeline, t: f32) -> NodeOverrides {
        let mut map: HashMap<usize, Overrides> = HashMap::new();
        let mut nodes: Vec<usize> = timeline.tweens.iter().map(|tw| tw.node).collect();
        nodes.sort_unstable();
        nodes.dedup();
        for n in nodes {
            map.insert(n, timeline.overrides_at(n, t));
        }
        NodeOverrides { map }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eases_land_on_their_endpoints() {
        for e in [
            Ease::None,
            Ease::Power(3, Dir::Out),
            Ease::Power(4, Dir::InOut),
            Ease::Back(1.3, Dir::Out),
            Ease::Steps(18),
        ] {
            assert!((e.apply(0.0) - 0.0).abs() < 1e-6, "{e:?} at 0");
            assert!((e.apply(1.0) - 1.0).abs() < 1e-6, "{e:?} at 1");
        }
    }

    #[test]
    fn power_out_decelerates() {
        let e = Ease::Power(3, Dir::Out);
        // An out-ease covers more than half its distance by the halfway point.
        assert!(e.apply(0.5) > 0.5);
        assert!(e.apply(0.25) > 0.25);
    }

    #[test]
    fn back_out_overshoots_then_settles() {
        let e = Ease::Back(1.3, Dir::Out);
        let peak = (60..=90)
            .map(|i| e.apply(i as f32 / 100.0))
            .fold(0.0f32, f32::max);
        assert!(
            peak > 1.0,
            "back.out must overshoot past 1, peaked at {peak}"
        );
        assert!((e.apply(1.0) - 1.0).abs() < 1e-6, "and still land on 1");
    }

    #[test]
    fn steps_quantises() {
        let e = Ease::Steps(4);
        assert_eq!(e.apply(0.1), 0.0);
        assert_eq!(e.apply(0.3), 0.25);
        assert_eq!(e.apply(0.6), 0.5);
        assert_eq!(e.apply(1.0), 1.0);
    }

    #[test]
    fn ease_strings_parse_as_gsap_writes_them() {
        assert_eq!(Ease::parse("none"), Ease::None);
        assert_eq!(Ease::parse("power3.out"), Ease::Power(4, Dir::Out));
        assert_eq!(Ease::parse("power1.in"), Ease::Power(2, Dir::In));
        assert_eq!(Ease::parse("power2.inOut"), Ease::Power(3, Dir::InOut));
        assert_eq!(Ease::parse("steps(18)"), Ease::Steps(18));
        assert_eq!(Ease::parse("back.out(1.3)"), Ease::Back(1.3, Dir::Out));
    }

    fn tl_of(tweens: Vec<Tween>) -> Timeline {
        let duration = tweens.iter().map(|t| t.end()).fold(0.0, f32::max);
        Timeline { tweens, duration }
    }

    #[test]
    fn a_from_to_pins_its_start_before_it_runs() {
        let tl = tl_of(vec![Tween {
            node: 1,
            prop: Prop::Opacity,
            start: 5.0,
            duration: 1.0,
            from: Some(0.0),
            to: 1.0,
            ease: Ease::None,
            pins: true,
        }]);
        // Before the tween, the element sits at the `from` value — that is how
        // a composition's initial hidden state is established.
        assert_eq!(tl.value_at(1, Prop::Opacity, 0.0), 0.0);
        assert_eq!(tl.value_at(1, Prop::Opacity, 5.5), 0.5);
        assert_eq!(tl.value_at(1, Prop::Opacity, 9.0), 1.0);
    }

    #[test]
    fn a_bare_to_continues_from_what_came_before() {
        let tl = tl_of(vec![
            Tween {
                node: 1,
                prop: Prop::Y,
                start: 0.0,
                duration: 0.0,
                from: Some(70.0),
                to: 70.0,
                ease: Ease::None,
                pins: true,
            },
            Tween {
                node: 1,
                prop: Prop::Y,
                start: 0.0,
                duration: 1.0,
                from: None,
                to: 0.0,
                ease: Ease::None,
                pins: true,
            },
        ]);
        // The `set` leaves y at 70; the `to` must start there, not at 0.
        assert_eq!(tl.value_at(1, Prop::Y, 0.5), 35.0);
        assert_eq!(tl.value_at(1, Prop::Y, 1.0), 0.0);
    }

    #[test]
    fn untouched_properties_rest_at_their_defaults() {
        let tl = tl_of(vec![]);
        assert_eq!(tl.value_at(0, Prop::Opacity, 3.0), 1.0);
        assert_eq!(tl.value_at(0, Prop::Scale, 3.0), 1.0);
        assert_eq!(tl.value_at(0, Prop::Y, 3.0), 0.0);
        // And an untouched element gets no overrides at all.
        assert!(tl.overrides_at(0, 3.0).is_identity());
    }

    #[test]
    fn inset_and_blur_values_parse() {
        assert_eq!(inset_right("inset(0 100% 0 0)"), Some(1.0));
        assert_eq!(inset_right("inset(0 0% 0 0)"), Some(0.0));
        assert_eq!(blur_px("blur(15px)"), Some(15.0));
        assert_eq!(blur_px("blur(0px)"), Some(0.0));
    }
}

#[cfg(all(test, feature = "js"))]
mod real_tests {
    use super::*;

    const COMP: &str = "/home/ctown/projects/glossa/brag-output/composition";

    fn extract_real() -> Option<(Timeline, crate::html::Element)> {
        let src = std::fs::read_to_string(format!("{COMP}/index.html")).ok()?;
        let root = crate::html::parse(&src).ok()?;
        let script = root.collect_inline_scripts();
        let tl = extract(&root, &script).expect("timeline extraction");
        Some((tl, root))
    }

    #[test]
    fn extracts_the_whole_timeline_including_generated_tweens() {
        let Some((tl, root)) = extract_real() else {
            eprintln!("skip: composition not present");
            return;
        };
        // The composition declares 20s; the recorded tweens must reach it.
        let comp = root.find_with_attr("data-composition-id").unwrap();
        let declared: f32 = comp.attr("data-duration").unwrap().parse().unwrap();
        assert!(
            tl.duration > declared * 0.8 && tl.duration <= declared + 1.0,
            "timeline spans {:.2}s against a declared {declared}s",
            tl.duration
        );

        // The word cascade is generated in a loop over elements with no id.
        // A static matcher could not produce these, so their presence is the
        // proof that the script really ran.
        let word_nodes: Vec<usize> = css::query_indices(&root, "#s1-line1 .w");
        assert_eq!(word_nodes.len(), 8, "line 1 should have 8 words");
        for n in &word_nodes {
            let has = tl.tweens.iter().any(|t| t.node == *n && t.prop == Prop::Y);
            assert!(has, "word node {n} got no y tween from the cascade");
        }
    }

    #[test]
    fn the_typewriter_wipes_are_recorded_as_step_eased_clip() {
        let Some((tl, root)) = extract_real() else {
            return;
        };
        for id in ["s2-wrap1", "s2-wrap2", "s2-wrap3"] {
            let n = css::query_indices(&root, &format!("#{id}"));
            assert_eq!(n.len(), 1, "#{id} not found");
            let tw: Vec<&Tween> = tl
                .tweens
                .iter()
                .filter(|t| t.node == n[0] && t.prop == Prop::ClipInsetRight)
                .collect();
            assert_eq!(tw.len(), 1, "#{id} should have one clip tween");
            assert_eq!(tw[0].from, Some(1.0), "wipes start fully clipped");
            assert_eq!(tw[0].to, 0.0, "and end fully revealed");
            assert!(
                matches!(tw[0].ease, Ease::Steps(_)),
                "typewriter reveals use steps(), got {:?}",
                tw[0].ease
            );
        }
    }

    #[test]
    fn the_command_is_hidden_at_rest_and_revealed_later() {
        let Some((tl, root)) = extract_real() else {
            return;
        };
        let n = css::query_indices(&root, "#s2-wrap1")[0];
        // At t=0 the wipe has not run: fully clipped from the right.
        assert_eq!(tl.value_at(n, Prop::ClipInsetRight, 0.0), 1.0);
        // By the end of its tween it is fully open.
        assert_eq!(tl.value_at(n, Prop::ClipInsetRight, 6.0), 0.0);
    }

    #[test]
    fn scenes_cross_fade_over_the_run() {
        let Some((tl, root)) = extract_real() else {
            return;
        };
        let s3 = css::query_indices(&root, "#s3")[0];
        // #s3 fades in around 9.75s and out around 16.8s.
        assert!(tl.value_at(s3, Prop::Opacity, 5.0) < 0.5, "hidden early");
        assert!(tl.value_at(s3, Prop::Opacity, 12.0) > 0.9, "visible mid");
        assert!(
            tl.value_at(s3, Prop::Opacity, 19.0) < 0.1,
            "gone by the outro"
        );
    }

    #[test]
    fn blur_is_detected_and_reported_not_silently_dropped() {
        let Some((tl, _)) = extract_real() else {
            return;
        };
        // Three transitions animate filter: blur(). The rasterizer cannot draw
        // it yet, so the timeline must at least say so.
        assert!(tl.uses_blur(), "blur tweens should be recorded");
    }
}

/// A render plan: which frames are actually distinct, and what each frame shows.
///
/// # The collapse
///
/// Rendering every frame from scratch runs a loop whose result is frequently
/// identical to the previous iteration. The timeline can tell which iterations
/// those are *without producing them*: if no element's overrides differ between
/// two times, the frames are identical by construction, because a frame is a
/// pure function of the overrides.
///
/// On glossa that is 324 of 600 frames — 54% — which never need to be drawn or
/// encoded. Detecting the same thing by comparing rendered pixels would require
/// rendering them first, which is the work being avoided.
#[derive(Clone, Debug)]
pub struct Plan {
    /// Times to actually render, one per distinct frame.
    pub distinct_times: Vec<f32>,
    /// `shows[i]` indexes `distinct_times` for output frame `i`.
    pub shows: Vec<usize>,
}

impl Plan {
    pub fn frames(&self) -> usize {
        self.shows.len()
    }
    pub fn distinct(&self) -> usize {
        self.distinct_times.len()
    }
    /// Fraction of frames that repeat their predecessor.
    pub fn repeat_fraction(&self) -> f32 {
        if self.shows.is_empty() {
            return 0.0;
        }
        let repeats = self.shows.windows(2).filter(|w| w[0] == w[1]).count();
        repeats as f32 / self.shows.len() as f32
    }
}

impl Timeline {
    /// Build a render plan for `count` frames at `fps`.
    ///
    /// Frames are grouped by a signature of every animated node's overrides,
    /// quantised to what a renderer can actually express: a translation below a
    /// quarter-pixel or an opacity below 1/255 produces the same pixels, so
    /// treating those as different would render identical frames twice.
    ///
    /// Only *consecutive* repeats collapse. A frame identical to one from ten
    /// seconds ago is still drawn, because the encoder can only skip against
    /// the immediately preceding reference.
    pub fn plan(&self, count: usize, fps: f32) -> Plan {
        let mut nodes: Vec<usize> = self.tweens.iter().map(|t| t.node).collect();
        nodes.sort_unstable();
        nodes.dedup();

        let signature = |t: f32| -> Vec<i32> {
            let mut sig = Vec::with_capacity(nodes.len() * 6);
            for &n in &nodes {
                let o = self.overrides_at(n, t);
                let q = |v: f32, step: f32| (v / step).round() as i32;
                sig.push(q(o.opacity.unwrap_or(1.0), 1.0 / 255.0));
                sig.push(q(o.translate.0, 0.25));
                sig.push(q(o.translate.1, 0.25));
                sig.push(q(o.scale.unwrap_or(1.0), 0.0005));
                sig.push(q(o.inset_clip.map(|c| c.1).unwrap_or(0.0), 0.0005));
                sig.push(q(o.blur.unwrap_or(0.0), 0.25));
            }
            sig
        };

        let mut distinct_times = Vec::new();
        let mut shows = Vec::with_capacity(count);
        let mut prev: Option<Vec<i32>> = None;
        for i in 0..count {
            let t = i as f32 / fps;
            let sig = signature(t);
            if prev.as_ref() == Some(&sig) {
                shows.push(distinct_times.len() - 1);
            } else {
                distinct_times.push(t);
                shows.push(distinct_times.len() - 1);
                prev = Some(sig);
            }
        }
        Plan {
            distinct_times,
            shows,
        }
    }
}

#[cfg(test)]
mod plan_tests {
    use super::*;

    fn tl(tweens: Vec<Tween>) -> Timeline {
        let duration = tweens
            .iter()
            .map(|t| t.start + t.duration)
            .fold(0.0, f32::max);
        Timeline { tweens, duration }
    }

    #[test]
    fn a_static_timeline_collapses_to_one_frame() {
        let p = tl(vec![]).plan(300, 30.0);
        assert_eq!(p.frames(), 300);
        assert_eq!(p.distinct(), 1, "nothing animates, so one frame suffices");
        assert!(p.repeat_fraction() > 0.99);
    }

    #[test]
    fn a_hold_after_a_tween_collapses_only_the_hold() {
        // One second of motion, then two seconds holding still.
        let p = tl(vec![Tween {
            node: 1,
            prop: Prop::Opacity,
            start: 0.0,
            duration: 1.0,
            from: Some(0.0),
            to: 1.0,
            ease: Ease::None,
            pins: true,
        }])
        .plan(90, 30.0);
        assert_eq!(p.frames(), 90);
        // The moving second yields distinct frames; the still two do not.
        assert!(
            p.distinct() > 20 && p.distinct() < 40,
            "got {}",
            p.distinct()
        );
    }

    #[test]
    fn every_output_frame_maps_to_a_rendered_one() {
        let p = tl(vec![Tween {
            node: 3,
            prop: Prop::Y,
            start: 0.5,
            duration: 0.4,
            from: Some(20.0),
            to: 0.0,
            ease: Ease::Power(4, Dir::Out),
            pins: true,
        }])
        .plan(60, 30.0);
        assert_eq!(p.shows.len(), 60);
        for &s in &p.shows {
            assert!(s < p.distinct(), "plan points past the rendered set");
        }
        // Indices are non-decreasing: a frame never points backwards.
        assert!(p.shows.windows(2).all(|w| w[1] >= w[0]));
    }

    #[test]
    fn sub_perceptible_motion_does_not_force_a_redraw() {
        // A tween so slow that consecutive frames differ by far less than a
        // pixel: those frames are the same image and must not be drawn twice.
        let p = tl(vec![Tween {
            node: 1,
            prop: Prop::Y,
            start: 0.0,
            duration: 100.0,
            from: Some(0.0),
            to: 1.0,
            ease: Ease::None,
            pins: true,
        }])
        .plan(120, 30.0);
        assert!(
            p.distinct() < 10,
            "a sub-pixel drift should collapse, got {} distinct",
            p.distinct()
        );
    }
}
