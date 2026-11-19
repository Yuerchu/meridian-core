//! Revocation fence shared by all media work, including detached/queued jobs.
use std::future::Future;

use tokio_util::{sync::CancellationToken, task::TaskTracker};

#[derive(Default)]
pub(in crate::onebot) struct MediaGeneration {
    cancelled: CancellationToken,
    tasks: TaskTracker,
    gate: std::sync::Mutex<()>,
}

impl MediaGeneration {
    pub(in crate::onebot) async fn run<T>(&self, fetch: impl Future<Output = Result<T, String>>) -> Result<T, String> {
        let _active = {
            let _gate = self.gate.lock().unwrap();
            if self.cancelled.is_cancelled() {
                return Err("OneBot media generation has stopped".into());
            }
            self.tasks.token()
        };
        // Drop the network future before the active token, so stop() returning
        // means DNS/connect/body futures have really relinquished their work.
        {
            let fetch = fetch;
            tokio::pin!(fetch);
            tokio::select! {
                biased;
                _ = self.cancelled.cancelled() => Err("OneBot media generation has stopped".into()),
                result = &mut fetch => result,
            }
        }
    }

    pub(in crate::onebot) async fn stop(&self) {
        {
            let _gate = self.gate.lock().unwrap();
            self.cancelled.cancel();
            self.tasks.close();
        }
        self.tasks.wait().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn queued_work_cannot_begin_a_fetch_after_revocation() {
        let generation = Arc::new(MediaGeneration::default());
        let queued_generation = generation.clone();
        let (release, queued) = tokio::sync::oneshot::channel();
        let worker = tokio::spawn(async move {
            queued.await.unwrap();
            queued_generation
                .run(async {
                    panic!("revoked queued job must never poll its connector");
                    #[allow(unreachable_code)]
                    Ok::<_, String>(())
                })
                .await
        });
        generation.stop().await;
        release.send(()).unwrap();
        assert!(worker.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn stop_drops_stalled_network_future_before_returning() {
        struct Dropped(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let generation = Arc::new(MediaGeneration::default());
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let guard = Dropped(dropped.clone());
        let worker_generation = generation.clone();
        let (started, ready) = tokio::sync::oneshot::channel();
        let worker = tokio::spawn(async move {
            worker_generation
                .run(async move {
                    let _guard = guard;
                    started.send(()).unwrap();
                    std::future::pending::<Result<(), String>>().await
                })
                .await
        });
        ready.await.unwrap();
        generation.stop().await;
        assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
        assert!(worker.await.unwrap().is_err());
    }
}
