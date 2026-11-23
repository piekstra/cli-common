//! Transfers side by side: a few workers taking jobs from one list, all on
//! one [`Drive`](super::Drive), whose HTTP client, token source and path map
//! are safe to share between threads.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// Run `job` on every item, at most `workers` at a time, and give the answers
/// in the items' order. Once an answer `halts`, no job starts after it: the
/// jobs already running finish, and those never started answer `None`.
pub(crate) fn run<T: Send, R: Send>(
    items: Vec<T>,
    workers: usize,
    job: impl Fn(T) -> R + Sync,
    halts: impl Fn(&R) -> bool + Sync,
) -> Vec<Option<R>> {
    let n = items.len();
    let workers = workers.clamp(1, n.max(1));
    if workers == 1 {
        let mut halted = false;
        return items
            .into_iter()
            .map(|item| {
                if halted {
                    return None;
                }
                let r = job(item);
                halted = halts(&r);
                Some(r)
            })
            .collect();
    }
    let queue = Mutex::new(items.into_iter().enumerate());
    let answers: Mutex<Vec<Option<R>>> = Mutex::new((0..n).map(|_| None).collect());
    let halted = AtomicBool::new(false);
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| loop {
                if halted.load(Ordering::SeqCst) {
                    return;
                }
                let next = queue.lock().unwrap_or_else(|p| p.into_inner()).next();
                let Some((i, item)) = next else {
                    return;
                };
                let r = job(item);
                if halts(&r) {
                    halted.store(true, Ordering::SeqCst);
                }
                answers.lock().unwrap_or_else(|p| p.into_inner())[i] = Some(r);
            });
        }
    });
    answers.into_inner().unwrap_or_else(|p| p.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    #[test]
    fn answers_keep_the_items_order_and_never_exceed_the_workers() {
        let running = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let answers = run(
            (0..40u64).collect(),
            8,
            |i| {
                let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                // Later items finish sooner, so the order is not the
                // finishing order.
                std::thread::sleep(Duration::from_millis(40 - i));
                running.fetch_sub(1, Ordering::SeqCst);
                i * 2
            },
            |_| false,
        );
        let got: Vec<u64> = answers.into_iter().map(|a| a.unwrap()).collect();
        assert_eq!(got, (0..40u64).map(|i| i * 2).collect::<Vec<_>>());
        let peak = peak.load(Ordering::SeqCst);
        assert!((2..=8).contains(&peak), "peak {peak}");
    }

    #[test]
    fn a_halting_answer_starts_no_later_job() {
        for workers in [1, 4] {
            let started = AtomicUsize::new(0);
            let answers = run(
                (0..100).collect(),
                workers,
                |i: i32| {
                    started.fetch_add(1, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(2));
                    if i == 3 {
                        Err(i)
                    } else {
                        Ok(i)
                    }
                },
                |r| r.is_err(),
            );
            assert_eq!(answers[3], Some(Err(3)), "{workers}");
            assert!(answers[..3].iter().all(Option::is_some), "{workers}");
            // At most the jobs already running when item 3 failed ran after it.
            let ran = started.load(Ordering::SeqCst);
            assert!(ran <= 4 + workers, "{workers}: {ran} started");
            assert!(answers[50..].iter().all(Option::is_none), "{workers}");
        }
    }

    #[test]
    fn no_items_and_one_worker_are_fine() {
        assert!(run(Vec::<u8>::new(), 8, |i| i, |_| false).is_empty());
        assert_eq!(run(vec![1, 2], 0, |i| i + 1, |_| false), [Some(2), Some(3)]);
    }
}
