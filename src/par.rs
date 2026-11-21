//! Opt-in segment parallelism.
//!
//! Strips and tiles are independent compressed units, so their
//! decompression (and compression) can run on several threads when the
//! caller grants a budget — `DecodeOptions::with_threads`,
//! `EncodeOptions::with_threads`, or the framework's
//! `ExecutionContext`. The default budget is one thread: nothing here
//! spawns unless asked to. Results are handed back in index order, so
//! the output is identical at every budget.

use std::sync::atomic::{AtomicUsize, Ordering};

use crate::error::Result;

/// Run `job(i)` for every `i in 0..n` and pass each result to `sink`
/// in ascending index order.
///
/// With `threads <= 1` (or a single job) everything runs inline on the
/// calling thread, one result alive at a time. Otherwise up to
/// `threads` scoped workers pull indices from a shared counter, the
/// call blocks until they are done, and the results — all of them held
/// at once — are then drained into `sink`. The first failing index (in
/// index order) is the error returned.
pub(crate) fn run_ordered<T, F, S>(threads: usize, n: usize, job: F, mut sink: S) -> Result<()>
where
    T: Send,
    F: Fn(usize) -> Result<T> + Sync,
    S: FnMut(usize, T) -> Result<()>,
{
    let workers = threads.min(n);
    if workers <= 1 {
        for i in 0..n {
            sink(i, job(i)?)?;
        }
        return Ok(());
    }
    let next = AtomicUsize::new(0);
    let parts: Vec<Vec<(usize, Result<T>)>> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                scope.spawn(|| {
                    let mut local = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        if i >= n {
                            break;
                        }
                        local.push((i, job(i)));
                    }
                    local
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| match h.join() {
                Ok(v) => v,
                Err(panic) => std::panic::resume_unwind(panic),
            })
            .collect()
    });
    let mut slots: Vec<Option<Result<T>>> = (0..n).map(|_| None).collect();
    for (i, r) in parts.into_iter().flatten() {
        slots[i] = Some(r);
    }
    for (i, slot) in slots.into_iter().enumerate() {
        let value = slot.unwrap_or_else(|| {
            Err(crate::error::Error::invalid(
                "TIFF: internal error — a segment was not processed",
            ))
        })?;
        sink(i, value)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;

    #[test]
    fn results_arrive_in_index_order_at_every_budget() {
        for threads in [0, 1, 2, 3, 8, 64] {
            let mut seen = Vec::new();
            run_ordered(
                threads,
                37,
                |i| Ok(i * i),
                |i, v| {
                    seen.push((i, v));
                    Ok(())
                },
            )
            .unwrap();
            let want: Vec<_> = (0..37).map(|i| (i, i * i)).collect();
            assert_eq!(seen, want, "threads {threads}");
        }
    }

    #[test]
    fn first_error_in_index_order_wins() {
        for threads in [1, 4] {
            let mut delivered = 0;
            let err = run_ordered(
                threads,
                20,
                |i| {
                    if i == 5 || i == 11 {
                        Err(Error::invalid(format!("job {i}")))
                    } else {
                        Ok(i)
                    }
                },
                |_, _| {
                    delivered += 1;
                    Ok(())
                },
            )
            .unwrap_err();
            assert!(err.to_string().contains("job 5"), "{err}");
            assert_eq!(delivered, 5);
        }
    }

    #[test]
    fn zero_jobs_is_a_no_op() {
        run_ordered(4, 0, |_| Ok(()), |_, _| Ok(())).unwrap();
    }
}
