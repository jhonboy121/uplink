//! Per-call tokio tasks, cancelled and awaited on drop so codecs and streams stop before the
//! surfaces and rings they use go away.

use tokio::runtime::Handle;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub struct Tasks {
    runtime: Handle,
    cancel: CancellationToken,
    handles: Vec<JoinHandle<()>>,
}

impl Tasks {
    pub fn new(runtime: Handle) -> Self {
        Self { runtime, cancel: CancellationToken::new(), handles: Vec::new() }
    }

    /// The token every spawned task selects on.
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    pub fn spawn(&mut self, task: impl Future<Output = ()> + Send + 'static) {
        self.handles.push(self.runtime.spawn(task));
    }

    /// False once any task has ended (an error ends its task).
    pub fn all_running(&self) -> bool {
        self.handles.iter().all(|handle| !handle.is_finished())
    }
}

impl Drop for Tasks {
    fn drop(&mut self) {
        self.cancel.cancel();
        for handle in self.handles.drain(..) {
            if let Err(e) = self.runtime.block_on(handle) {
                tracing::error!("call task: {e}");
            }
        }
    }
}
