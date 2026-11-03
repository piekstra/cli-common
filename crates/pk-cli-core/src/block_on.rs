//! A minimal executor: poll a future to completion on the current thread.
//!
//! Native only in practice (it parks the thread while the future is
//! pending). Two callers: a CLI's commands driving a provider client written
//! against [`crate::transport::Transport`] with a blocking transport, whose
//! futures are ready on their first poll; and `pk-cli-mcp`'s stdio server,
//! which runs each tool call through it on a blocking thread outside any
//! Tokio context, so provider code that drives its own runtime
//! (`reqwest::blocking`) works unchanged.

use std::future::Future;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::thread::Thread;

struct Unpark(Thread);

impl Wake for Unpark {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

/// Run `fut` to completion, parking the thread while it is pending.
pub fn block_on<F: Future>(fut: F) -> F::Output {
    let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut fut = std::pin::pin!(fut);
    loop {
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(out) => return out,
            // A spurious wake-up only costs one more poll.
            Poll::Pending => std::thread::park(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_a_ready_future() {
        assert_eq!(block_on(async { 1 + 1 }), 2);
    }

    #[test]
    fn waits_for_a_wake_from_another_thread() {
        let (tx, rx) = std::sync::mpsc::channel::<u8>();
        let fut = std::future::poll_fn(move |cx| match rx.try_recv() {
            Ok(v) => Poll::Ready(v),
            Err(_) => {
                let waker = cx.waker().clone();
                let tx = tx.clone();
                std::thread::spawn(move || {
                    let _ = tx.send(7);
                    waker.wake();
                });
                Poll::Pending
            }
        });
        assert_eq!(block_on(fut), 7);
    }
}
