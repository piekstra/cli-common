//! What a module imports from its host: the five `pk_host` functions.
//!
//! On `wasm32` these are the real imports. Everywhere else they are a stand-in
//! that records what was asked of the host, so the runtime, a CLI's
//! [`HostTransport`](super::HostTransport) code and its Worker toolset can be
//! tested natively (`wasm::testing`, feature `test-util`).

/// How serious a [`log`] line is. The host maps it onto `console.error`,
/// `console.warn` and `console.log`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Error = 0,
    Warn = 1,
    Info = 2,
}

#[cfg(target_arch = "wasm32")]
mod imports {
    #[link(wasm_import_module = "pk_host")]
    extern "C" {
        pub fn now_ms() -> f64;
        pub fn random(ptr: *mut u8, len: usize);
        pub fn fetch(op: u32, request: *const u8, len: usize);
        pub fn sign_es256(
            op: u32,
            key: *const u8,
            key_len: usize,
            data: *const u8,
            data_len: usize,
        );
        pub fn log(level: u32, message: *const u8, len: usize);
    }
}

/// Milliseconds since the Unix epoch, from the host's clock (`Date.now()`).
/// `std::time::SystemTime::now()` panics on `wasm32-unknown-unknown`.
#[cfg(target_arch = "wasm32")]
pub fn now_ms() -> f64 {
    // SAFETY: no arguments; the host returns a number.
    unsafe { imports::now_ms() }
}

/// Fill `buf` from the host's CSPRNG (`crypto.getRandomValues`).
#[cfg(target_arch = "wasm32")]
pub fn fill_random(buf: &mut [u8]) {
    // SAFETY: the pointer and length describe `buf`, which the host writes
    // in place before returning.
    unsafe { imports::random(buf.as_mut_ptr(), buf.len()) }
}

/// A line for the host's log (the Worker's logs).
#[cfg(target_arch = "wasm32")]
pub fn log(level: Level, message: &str) {
    // SAFETY: the host copies the bytes before returning.
    unsafe { imports::log(level as u32, message.as_ptr(), message.len()) }
}

#[cfg(target_arch = "wasm32")]
pub(crate) fn start_fetch(op: u32, request: &[u8]) {
    // SAFETY: the host copies the request before returning and answers later
    // through `pk_complete`; it never calls back into the module from here.
    unsafe { imports::fetch(op, request.as_ptr(), request.len()) }
}

#[cfg(target_arch = "wasm32")]
pub(crate) fn start_sign_es256(op: u32, key: &[u8], data: &[u8]) {
    // SAFETY: as for `start_fetch`.
    unsafe { imports::sign_es256(op, key.as_ptr(), key.len(), data.as_ptr(), data.len()) }
}

#[cfg(not(target_arch = "wasm32"))]
pub use native::*;

/// The native stand-in for the host. Requests are queued for a test to
/// answer; the clock and the random source are real, so code that reaches
/// them natively still gets a sensible answer.
#[cfg(not(target_arch = "wasm32"))]
mod native {
    use std::cell::RefCell;
    use std::hash::{BuildHasher, Hasher};

    use super::Level;

    /// A request the module made of the host, waiting for an answer.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Started {
        /// `fetch(op, request)`: the request JSON, as the module wrote it.
        Fetch { op: u32, request: Vec<u8> },
        /// `sign_es256(op, key, data)`.
        SignEs256 {
            op: u32,
            key: Vec<u8>,
            data: Vec<u8>,
        },
    }

    thread_local! {
        static STARTED: RefCell<Vec<Started>> = const { RefCell::new(Vec::new()) };
        static LOGS: RefCell<Vec<(Level, String)>> = const { RefCell::new(Vec::new()) };
        static RNG: (std::collections::hash_map::RandomState, std::cell::Cell<u64>) =
            (std::collections::hash_map::RandomState::new(), std::cell::Cell::new(0));
    }

    pub fn now_ms() -> f64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as f64)
            .unwrap_or(0.0)
    }

    /// SipHash over a counter, keyed from the OS by `RandomState`: different
    /// in every process and unpredictable, but the host's CSPRNG is the real
    /// source. Native code that needs secret randomness uses its own.
    pub fn fill_random(buf: &mut [u8]) {
        RNG.with(|(keys, counter)| {
            for chunk in buf.chunks_mut(8) {
                let mut h = keys.build_hasher();
                h.write_u64(counter.get());
                counter.set(counter.get().wrapping_add(1));
                let word = h.finish().to_le_bytes();
                chunk.copy_from_slice(&word[..chunk.len()]);
            }
        });
    }

    pub fn log(level: Level, message: &str) {
        LOGS.with_borrow_mut(|l| l.push((level, message.to_string())));
    }

    /// The requests started since the last call, oldest first.
    #[cfg(any(test, feature = "test-util"))]
    pub fn take_started() -> Vec<Started> {
        STARTED.with_borrow_mut(std::mem::take)
    }

    /// The lines logged since the last call.
    #[cfg(any(test, feature = "test-util"))]
    pub fn take_logs() -> Vec<(Level, String)> {
        LOGS.with_borrow_mut(std::mem::take)
    }

    pub(crate) fn start_fetch(op: u32, request: &[u8]) {
        STARTED.with_borrow_mut(|s| {
            s.push(Started::Fetch {
                op,
                request: request.to_vec(),
            })
        });
    }

    pub(crate) fn start_sign_es256(op: u32, key: &[u8], data: &[u8]) {
        STARTED.with_borrow_mut(|s| {
            s.push(Started::SignEs256 {
                op,
                key: key.to_vec(),
                data: data.to_vec(),
            })
        });
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    #[test]
    fn the_native_random_source_does_not_repeat() {
        let mut a = [0u8; 21];
        let mut b = [0u8; 21];
        fill_random(&mut a);
        fill_random(&mut b);
        assert_ne!(a, b);
        assert_ne!(a, [0u8; 21]);
    }

    #[test]
    fn the_native_clock_is_the_system_clock() {
        // 2020-01-01 in ms: a clock that answers 0 would fail every JWT.
        assert!(now_ms() > 1_577_836_800_000.0);
    }

    #[test]
    fn logs_are_kept_for_a_test_to_read() {
        log(Level::Warn, "careful");
        assert_eq!(take_logs(), vec![(Level::Warn, "careful".to_string())]);
        assert!(take_logs().is_empty());
    }
}
