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
            assert_eq!(c.load(Ordering::Relaxed), 1, "index {i} rendered twice or never");
        }
    }
}
