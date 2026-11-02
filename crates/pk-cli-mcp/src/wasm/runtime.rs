//! The module's side of the ABI: buffers, tasks, and the host operations a
//! task waits on.
//!
//! WebAssembly here is single-threaded and the host is JavaScript, so the
//! executor is the host's event loop. A `tools/call` becomes a task (a boxed
//! future); the host polls it, and each time it is pending the host waits for
//! one of the operations the task started (a `fetch`, a signature) to settle,
//! hands the answer back with `pk_complete`, and polls again. There are no
//! wakers to keep: the host knows which operations each poll started, so it
//! re-polls exactly when one of them settles.

use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

use serde_json::{json, Value};

use super::host;
use super::InWorker;
use crate::toolset::{call_tool_json, instructions, tools_list, Toolset};

/// `pk_complete` status for an operation the host could not do (a failed
/// `fetch`, a rejected key); the data is the error message.
pub const STATUS_FAILED: i32 = -1;
/// `pk_complete` status for an operation the host does not offer (no
/// WebCrypto ECDSA); the module falls back to its own code.
pub const STATUS_UNSUPPORTED: i32 = -2;

/// JSON-RPC "Internal error", for a call the module could not even start.
const INTERNAL_ERROR: i64 = -32603;

type Task = Pin<Box<dyn Future<Output = Value>>>;

enum Op {
    Pending,
    Done(i32, Vec<u8>),
    /// The task waiting on it was dropped; the answer is discarded.
    Abandoned,
}

#[derive(Default)]
struct Runtime {
    next_task: u32,
    tasks: HashMap<u32, Task>,
    next_op: u32,
    ops: HashMap<u32, Op>,
    /// The last result handed out; the host reads it through `pk_out_ptr`
    /// before its next call into the module.
    out: Vec<u8>,
}

thread_local! {
    static RT: RefCell<Runtime> = RefCell::new(Runtime::default());
}

/// Ids start at 1 and skip any still in use, so a wrapped counter can never
/// hand out a live id.
fn next_id<V>(counter: &mut u32, live: &HashMap<u32, V>) -> u32 {
    loop {
        *counter = counter.wrapping_add(1);
        if *counter != 0 && !live.contains_key(counter) {
            return *counter;
        }
    }
}

/// An operation the host performs asynchronously, as a future of its
/// `(status, data)` answer.
pub(crate) struct HostOp {
    op: u32,
    finished: bool,
}

impl HostOp {
    fn start(start: impl FnOnce(u32)) -> Self {
        let op = RT.with_borrow_mut(|rt| {
            let op = next_id(&mut rt.next_op, &rt.ops);
            rt.ops.insert(op, Op::Pending);
            op
        });
        // Outside the borrow: the host may read memory, but never calls back
        // into the module from an import.
        start(op);
        Self {
            op,
            finished: false,
        }
    }

    /// `fetch` the request JSON (`{"method","url","headers","body"}`).
    pub(crate) fn fetch(request: &[u8]) -> Self {
        Self::start(|op| host::start_fetch(op, request))
    }

    /// ECDSA P-256 / SHA-256 over `data` with the PKCS#8 DER `key`.
    pub(crate) fn sign_es256(key: &[u8], data: &[u8]) -> Self {
        Self::start(|op| host::start_sign_es256(op, key, data))
    }
}

impl Future for HostOp {
    type Output = (i32, Vec<u8>);

    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        let op = self.op;
        let done = RT.with_borrow_mut(|rt| match rt.ops.remove(&op) {
            Some(Op::Done(status, data)) => Some((status, data)),
            Some(other) => {
                rt.ops.insert(op, other);
                None
            }
            // Only reachable by polling a finished operation again.
            None => Some((
                STATUS_FAILED,
                b"the operation was already finished".to_vec(),
            )),
        });
        match done {
            Some(answer) => {
                self.finished = true;
                Poll::Ready(answer)
            }
            None => Poll::Pending,
        }
    }
}

impl Drop for HostOp {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let op = self.op;
        // `try_with`: a thread-local already torn down has nothing to update.
        let _ = RT.try_with(|rt| {
            if let Ok(mut rt) = rt.try_borrow_mut() {
                match rt.ops.get(&op) {
                    Some(Op::Pending) => {
                        rt.ops.insert(op, Op::Abandoned);
                    }
                    Some(Op::Done(..)) => {
                        rt.ops.remove(&op);
                    }
                    _ => {}
                }
            }
        });
    }
}

/// The host answers operation `op`. A late answer for an abandoned
/// operation, or one for an id the module never issued, is dropped.
pub(crate) fn complete(op: u32, status: i32, data: Vec<u8>) {
    RT.with_borrow_mut(|rt| match rt.ops.get(&op) {
        Some(Op::Pending) => {
            rt.ops.insert(op, Op::Done(status, data));
        }
        Some(Op::Abandoned) => {
            rt.ops.remove(&op);
        }
        _ => {}
    });
}

/// What the host needs before any call: identity, instructions and tools.
pub(crate) fn describe<T: Toolset>(toolset: &T) -> Value {
    let toolset = InWorker(toolset);
    json!({
        "abi": super::ABI_VERSION,
        "server": toolset.server().server_info_json(),
        "instructions": instructions(&toolset),
        "tools": tools_list(&toolset),
    })
}

/// Start a `tools/call`: `params` is the call's params object, `config` the
/// host's per-call configuration, which `factory` turns into the toolset.
pub(crate) fn spawn_call<T, F>(params: &[u8], config: &[u8], factory: F) -> u32
where
    T: Toolset + 'static,
    F: FnOnce(&Value) -> T,
{
    let task: Task = match (
        serde_json::from_slice::<Value>(params),
        serde_json::from_slice::<Value>(config),
    ) {
        (Ok(params), Ok(config)) => {
            let toolset = InWorker(factory(&config));
            Box::pin(async move {
                match call_tool_json(&toolset, &params).await {
                    Ok(result) => json!({ "result": result }),
                    Err(error) => json!({ "error": error }),
                }
            })
        }
        (Err(e), _) => internal_error(format!("the call's params are not JSON: {e}")),
        (_, Err(e)) => internal_error(format!("the host's config is not JSON: {e}")),
    };
    RT.with_borrow_mut(|rt| {
        let id = next_id(&mut rt.next_task, &rt.tasks);
        rt.tasks.insert(id, task);
        id
    })
}

fn internal_error(message: String) -> Task {
    Box::pin(async move { json!({ "error": { "code": INTERNAL_ERROR, "message": message } }) })
}

/// Poll a task once: its answer (`{"result": …}` or `{"error": …}`) when it
/// finished, which also forgets it, or `None` while it waits on the host.
pub(crate) fn poll(task: u32) -> Option<Value> {
    let Some(mut fut) = RT.with_borrow_mut(|rt| rt.tasks.remove(&task)) else {
        return Some(json!({
            "error": { "code": INTERNAL_ERROR, "message": format!("no task {task}") }
        }));
    };
    // Polled outside the runtime's borrow: the task starts host operations.
    let mut cx = Context::from_waker(Waker::noop());
    match fut.as_mut().poll(&mut cx) {
        Poll::Ready(answer) => Some(answer),
        Poll::Pending => {
            RT.with_borrow_mut(|rt| rt.tasks.insert(task, fut));
            None
        }
    }
}

/// Forget a task the host gave up on; its pending operations are abandoned.
pub(crate) fn drop_task(task: u32) {
    let fut = RT.with_borrow_mut(|rt| rt.tasks.remove(&task));
    drop(fut);
}

/// Keep `bytes` for the host to read through `pk_out_ptr`; their length.
pub(crate) fn hand_out(bytes: Vec<u8>) -> usize {
    RT.with_borrow_mut(|rt| {
        rt.out = bytes;
        rt.out.len()
    })
}

pub(crate) fn out_ptr() -> *const u8 {
    RT.with_borrow(|rt| rt.out.as_ptr())
}

#[cfg(test)]
pub(crate) fn live() -> (usize, usize) {
    RT.with_borrow(|rt| (rt.tasks.len(), rt.ops.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_skip_zero_and_live_ones_when_the_counter_wraps() {
        let mut live: HashMap<u32, ()> = HashMap::new();
        live.insert(1, ());
        let mut counter = u32::MAX;
        assert_eq!(next_id(&mut counter, &live), 2);
    }

    #[test]
    fn an_answer_reaches_the_operation_and_an_abandoned_one_is_discarded() {
        let mut first = Box::pin(HostOp::fetch(b"{}"));
        let started = host::take_started();
        let op = match &started[..] {
            [host::Started::Fetch { op, .. }] => *op,
            other => panic!("{other:?}"),
        };
        let mut cx = Context::from_waker(Waker::noop());
        assert!(first.as_mut().poll(&mut cx).is_pending());
        complete(op, 200, b"ok".to_vec());
        assert_eq!(
            first.as_mut().poll(&mut cx),
            Poll::Ready((200, b"ok".to_vec()))
        );
        drop(first);

        let second = HostOp::sign_es256(b"k", b"d");
        let op2 = match &host::take_started()[..] {
            [host::Started::SignEs256 { op, key, data }] => {
                assert_eq!((key.as_slice(), data.as_slice()), (&b"k"[..], &b"d"[..]));
                *op
            }
            other => panic!("{other:?}"),
        };
        drop(second);
        complete(op2, 0, vec![1]);
        // A stray answer for an id never issued changes nothing.
        complete(9_999, 0, vec![]);
        assert_eq!(live().1, 0, "no operation is left behind");
    }

    #[test]
    fn an_unknown_task_answers_an_error_rather_than_hanging() {
        let answer = poll(4_242).unwrap();
        assert_eq!(answer["error"]["code"], INTERNAL_ERROR);
    }
}
