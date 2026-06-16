/// Progress hook for push/pull. The CLI provides an indicatif-aware
/// impl; non-CLI callers can use [`NullObserver`].
pub trait Observer: Send + Sync {
    /// A remote's transfer is starting.
    fn remote_start(&self, name: &str);

    /// A status line from a backend (rsync stdout/stderr, an s3 "put"
    /// notice, etc.). Lines are not newline-terminated.
    fn line(&self, text: &str);

    /// A remote's transfer has finished. `result` is `Ok` on success
    /// or `Err(message)` with a human-readable error.
    fn remote_finish(&self, name: &str, result: Result<(), &str>);
}

pub struct NullObserver;

impl Observer for NullObserver {
    fn remote_start(&self, _name: &str) {}
    fn line(&self, _text: &str) {}
    fn remote_finish(&self, _name: &str, _result: Result<(), &str>) {}
}
