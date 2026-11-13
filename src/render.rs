//! Rendering many frames at once.
//!
//! A frame is a pure function of its index — that is the determinism contract
//! the whole design rests on — so frames have no ordering dependency and can be
//! rendered on as many threads as there are cores. This is the payoff for
//! keeping `style_at(t)` free of hidden state: parallelism needs no locking, no
//! shared mutable canvas, and no coordination beyond handing each worker a
//! disjoint set of indices.
//!
//! Compare what a browser has to do for the same result: one operating-system
//! process per worker, each with its own renderer, ~256 MB apiece.

use std::sync::atomic::{AtomicUsize, Ordering};

/// How many threads to use by default — one per core, minus one left for the
/// rest of the machine, clamped to at least one.
pub fn default_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get().saturating_sub(1).max(1))
        .unwrap_or(1)
}

/// Render `count` frames concurrently, returning them in index order.
///
/// `render` is called once per index and must be pure: given the same index it
/// must produce the same bytes, regardless of which thread runs it or when.
///
/// Work is claimed from a shared counter rather than sliced into fixed ranges,
/// so a frame that happens to be expensive (a dense text scene, say) does not
/// leave one worker running long after the others have finished.
pub fn render_frames<F>(count: usize, threads: usize, render: F) -> Vec<Vec<u8>>
where
    F: Fn(usize) -> Vec<u8> + Sync,
{
    if count == 0 {
        return Vec::new();
    }
    let threads = threads.max(1).min(count);
    if threads == 1 {
        return (0..count).map(render).collect();
    }

    let next = AtomicUsize::new(0);
    let render = &render;
    let next = &next;

    // Each worker keeps its own results and they are merged once at the end.
    // Nothing is shared on the hot path except the claim counter, so a worker
    // never waits on another to hand back a frame.
    let collected: Vec<Vec<(usize, Vec<u8>)>> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(move || {
                    let mut mine = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        if i >= count {
                            break;
                        }
                        mine.push((i, render(i)));
                    }
                    mine
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("render worker panicked"))
            .collect()
    });

    let mut out: Vec<Vec<u8>> = (0..count).map(|_| Vec::new()).collect();
    for batch in collected {
        for (i, frame) in batch {
            out[i] = frame;
        }
    }
    out
}

/// Render `count` frames on [`default_threads`] threads.
pub fn render_frames_parallel<F>(count: usize, render: F) -> Vec<Vec<u8>>
where
    F: Fn(usize) -> Vec<u8> + Sync,
{
    render_frames(count, default_threads(), render)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_order() {
        let frames = render_frames(64, 8, |i| vec![i as u8, (i * 2) as u8]);
        assert_eq!(frames.len(), 64);
        for (i, f) in frames.iter().enumerate() {
            assert_eq!(f, &vec![i as u8, (i * 2) as u8], "frame {i} out of order");
        }
    }

    #[test]
    fn matches_serial_output_exactly() {
        let f = |i: usize| -> Vec<u8> {
            // Something with enough structure that a race would show up.
            (0..97).map(|k| ((i * 31 + k * 7) % 251) as u8).collect()
        };
        let serial: Vec<Vec<u8>> = (0..200).map(f).collect();
        let parallel = render_frames(200, 8, f);
        assert_eq!(serial, parallel, "parallel must equal serial byte for byte");
    }

    #[test]
    fn handles_edge_counts() {
        assert!(render_frames(0, 4, |_| vec![1]).is_empty());
        assert_eq!(render_frames(1, 8, |i| vec![i as u8]), vec![vec![0u8]]);
        // More threads than frames must not spawn idle workers that misindex.
        assert_eq!(render_frames(3, 64, |i| vec![i as u8]).len(), 3);
    }

    #[test]
    fn single_thread_path_is_equivalent() {
        let f = |i: usize| vec![(i % 256) as u8; 3];
        assert_eq!(render_frames(50, 1, f), render_frames(50, 8, f));
    }

    #[test]
    fn every_index_is_rendered_exactly_once() {
        let calls: Vec<AtomicUsize> = (0..128).map(|_| AtomicUsize::new(0)).collect();
        let frames = render_frames(128, 12, |i| {
            calls[i].fetch_add(1, Ordering::Relaxed);
            vec![i as u8]
        });
        assert_eq!(frames.len(), 128);
        for (i, c) in calls.iter().enumerate() {
            assert_eq!(
                c.load(Ordering::Relaxed),
                1,
                "index {i} rendered twice or never"
            );
        }
    }
}

/// Fold a frame range into one small result per frame, streaming.
///
/// # The shape, in apeiron's terms
///
/// `apeiron::merge` records that order-free accumulation needs commutativity
/// and associativity, and that both hold on every face of the two-slot algebra —
/// so "splitting a stream across threads and recombining is licensed
/// everywhere". Its table puts an ordered append (the graph face, `link`) as
/// associative but **not** commutative, and that is exactly a video: encoded
/// samples must keep their order, while associativity is what licenses
/// splitting the range across workers and recombining.
///
/// So this is a fold into `link`, and the consequence is the memory profile.
/// A frame is 8.3 MB at 1080p; its encoded sample is ~25 KB. Accumulating
/// frames and encoding afterwards costs `frames × 8.3 MB` — 5 GB for twenty
/// seconds. Folding instead means only the *result* accumulates, and the raw
/// pixels of a frame die the moment that frame is reduced.
///
/// Each worker keeps **one** canvas and clears it per frame rather than
/// allocating a new one, so the live set is `threads × frame` regardless of how
/// long the composition runs — O(1) in duration.
///
/// `reduce` is handed the frame index and a cleared canvas, and returns
/// whatever small value that frame becomes.
pub fn fold_frames<T, F>(count: usize, threads: usize, width: u32, height: u32, reduce: F) -> Vec<T>
where
    T: Send,
    F: Fn(usize, &mut crate::raster::Canvas) -> T + Sync,
{
    if count == 0 {
        return Vec::new();
    }
    let threads = threads.max(1).min(count);
    let next = AtomicUsize::new(0);
    let next = &next;
    let reduce = &reduce;

    let collected: Vec<Vec<(usize, T)>> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(move || {
                    // One canvas per worker, reused for every frame it claims.
                    let mut canvas = crate::raster::Canvas::new(width, height);
                    let mut mine = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        if i >= count {
                            break;
                        }
                        canvas.clear(crate::raster::Rgba::TRANSPARENT);
                        mine.push((i, reduce(i, &mut canvas)));
                    }
                    mine
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("fold worker panicked"))
            .collect()
    });

    // The recombine. Associativity is what makes this sound regardless of which
    // worker produced which frame; the index restores the order that `link`
    // does not supply on its own.
    let mut slots: Vec<Option<T>> = (0..count).map(|_| None).collect();
    for batch in collected {
        for (i, v) in batch {
            slots[i] = Some(v);
        }
    }
    slots
        .into_iter()
        .map(|s| s.expect("every index is claimed exactly once"))
        .collect()
}

/// Bytes held live by [`fold_frames`] for the frames themselves.
pub fn live_frame_bytes(threads: usize, width: u32, height: u32) -> usize {
    threads.max(1) * width as usize * height as usize * 4
}

/// Threads that fit a memory budget, measured rather than assumed.
///
/// Peak resident memory for a render is close to linear in the worker count:
/// each holds its own frame buffer, a scratch layer for blurred subtrees, and a
/// glyph cache. Measured on glossa at 1920x1080 — 2 threads 64 MB, 4 threads
/// 112 MB, 8 threads 198 MB — which is about 16 MB fixed plus 23 MB per worker.
///
/// The frame buffer alone is `width * height * 4`, so at 1080p no amount of
/// tuning takes a worker below ~8 MB; the trade between speed and footprint is
/// structural, not an implementation detail. This turns it into a dial: say
/// what you can spend and get the most workers that fit.
pub fn threads_for_budget(budget_mb: usize, width: u32, height: u32) -> usize {
    const FIXED_MB: usize = 16;
    let frame_mb = (width as usize * height as usize * 4) / (1024 * 1024);
    // A worker costs its frame buffer, a scratch layer of the same size, and
    // roughly a third of that again in cached glyph masks.
    let per_worker = (frame_mb * 2 + frame_mb / 3).max(1);
    budget_mb
        .saturating_sub(FIXED_MB)
        .checked_div(per_worker)
        .unwrap_or(1)
        .clamp(1, default_threads())
}

#[cfg(test)]
mod budget_tests {
    use super::*;

    #[test]
    fn a_budget_buys_proportionally_many_workers() {
        let small = threads_for_budget(64, 1920, 1080);
        let large = threads_for_budget(256, 1920, 1080);
        assert!(large > small, "more memory should buy more workers");
        assert!(small >= 1, "always at least one worker");
    }

    #[test]
    fn a_tiny_budget_still_renders() {
        assert_eq!(threads_for_budget(1, 1920, 1080), 1);
        assert_eq!(threads_for_budget(0, 1920, 1080), 1);
    }

    #[test]
    fn a_smaller_frame_buys_more_workers_for_the_same_budget() {
        let hd = threads_for_budget(128, 1920, 1080);
        let sd = threads_for_budget(128, 640, 360);
        assert!(
            sd >= hd,
            "smaller frames are cheaper per worker: {sd} vs {hd}"
        );
    }
}
