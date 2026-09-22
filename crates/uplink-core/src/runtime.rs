//! Tokio runtime whose threads log through a given `Dispatch` (no global subscriber).

use tokio::runtime::{Builder, Runtime};
use tracing::Dispatch;

pub fn build(dispatch: Dispatch) -> std::io::Result<Runtime> {
    Builder::new_multi_thread()
        .enable_all()
        // Worker and blocking threads get the dispatch as their thread default for their whole
        // life; the guard is leaked on purpose since the thread ends with the runtime.
        .on_thread_start(move || std::mem::forget(tracing::dispatcher::set_default(&dispatch)))
        .build()
}

#[cfg(test)]
mod tests {
    use tracing::span::{Attributes, Id, Record};
    use tracing::{Event, Metadata, Subscriber};

    use super::*;

    const SPAN_ID: u64 = 1;

    /// Identifies our dispatch; records nothing.
    struct Marker;

    impl Subscriber for Marker {
        fn enabled(&self, _: &Metadata<'_>) -> bool {
            false
        }
        fn new_span(&self, _: &Attributes<'_>) -> Id {
            Id::from_u64(SPAN_ID)
        }
        fn record(&self, _: &Id, _: &Record<'_>) {}
        fn record_follows_from(&self, _: &Id, _: &Id) {}
        fn event(&self, _: &Event<'_>) {}
        fn enter(&self, _: &Id) {}
        fn exit(&self, _: &Id) {}
    }

    fn uses_marker() -> bool {
        tracing::dispatcher::get_default(|dispatch| dispatch.is::<Marker>())
    }

    #[test]
    fn worker_and_blocking_threads_use_the_dispatch() -> anyhow::Result<()> {
        let runtime = build(Dispatch::new(Marker))?;
        assert!(!uses_marker(), "the caller's thread must stay untouched");
        assert!(runtime.block_on(runtime.spawn(async { uses_marker() }))?);
        assert!(runtime.block_on(runtime.spawn_blocking(uses_marker))?);
        Ok(())
    }
}
