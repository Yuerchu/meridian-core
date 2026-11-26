//! Reusable cancellable asynchronous job poller for long-running media
//! generation APIs (Google Veo, Volcengine Seedance, Kling, MiniMax, Vidu,
//! Alibaba Bailian Wan).

use std::future::Future;
use std::time::{Duration, Instant};

use meridian_provider_sdk::media_gen::{
    MediaGenError, MediaJobHandle, MediaJobStatus, VideoGenProvider, VideoGenResponse,
};

/// Configuration for [`AsyncMediaJobMixin::poll_until_done`].
#[derive(Debug, Clone, Copy)]
pub struct AsyncMediaJobMixin {
    /// Initial delay before the first poll.
    pub initial_delay: Duration,
    /// Upper bound on backoff interval between polls.
    pub max_interval: Duration,
    /// Total wall-clock timeout before aborting the job.
    pub timeout: Duration,
    /// Consecutive transient poll errors tolerated before failing the job.
    pub max_consecutive_poll_errors: u32,
}

impl Default for AsyncMediaJobMixin {
    fn default() -> Self {
        Self {
            initial_delay: Duration::from_secs(3),
            max_interval: Duration::from_secs(10),
            timeout: Duration::from_secs(900),
            max_consecutive_poll_errors: 5,
        }
    }
}

impl AsyncMediaJobMixin {
    /// Poll `provider.poll_video_job(handle)` until terminal completion,
    /// timeout, or `cancel` resolves.
    ///
    /// Invokes `on_progress(progress_pct)` whenever the upstream reports a
    /// status tick (`Queued` / `Running`). On cancellation, attempts a
    /// best-effort [`VideoGenProvider::cancel_video_job`] call before returning
    /// [`MediaGenError::Cancelled`].
    pub async fn poll_until_done<F, C>(
        &self,
        provider: &dyn VideoGenProvider,
        handle: &MediaJobHandle,
        mut on_progress: F,
        cancel: C,
    ) -> Result<VideoGenResponse, MediaGenError>
    where
        F: FnMut(Option<u8>),
        C: Future<Output = ()> + Send,
    {
        let started = Instant::now();
        let mut interval = self.initial_delay;
        let mut consecutive_errors = 0u32;
        tokio::pin!(cancel);

        loop {
            if started.elapsed() >= self.timeout {
                let _ = provider.cancel_video_job(handle).await;
                return Err(MediaGenError::Retryable(format!(
                    "media job `{}` timed out after {}s",
                    handle.job_id,
                    self.timeout.as_secs()
                )));
            }

            let sleep = tokio::time::sleep(interval);
            tokio::select! {
                _ = &mut cancel => {
                    let _ = provider.cancel_video_job(handle).await;
                    return Err(MediaGenError::Cancelled);
                }
                _ = sleep => {}
            }

            let poll_result = tokio::select! {
                _ = &mut cancel => {
                    let _ = provider.cancel_video_job(handle).await;
                    return Err(MediaGenError::Cancelled);
                }
                result = provider.poll_video_job(handle) => result,
            };

            match poll_result {
                Ok(MediaJobStatus::Queued) => {
                    consecutive_errors = 0;
                    on_progress(Some(0));
                }
                Ok(MediaJobStatus::Running { progress_pct }) => {
                    consecutive_errors = 0;
                    on_progress(progress_pct);
                }
                Ok(MediaJobStatus::Succeeded { media, usage }) => {
                    on_progress(Some(100));
                    return Ok(VideoGenResponse { videos: media, usage });
                }
                Ok(MediaJobStatus::Failed { message, retryable }) => {
                    return if retryable {
                        Err(MediaGenError::Retryable(message))
                    } else {
                        Err(MediaGenError::Terminal(message))
                    };
                }
                Ok(MediaJobStatus::Cancelled) => {
                    return Err(MediaGenError::Cancelled);
                }
                Err(err) if err.allows_failover() => {
                    consecutive_errors += 1;
                    if consecutive_errors > self.max_consecutive_poll_errors {
                        return Err(err);
                    }
                }
                Err(err) => return Err(err),
            }

            interval = (interval * 3 / 2).min(self.max_interval);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    use async_trait::async_trait;
    use meridian_provider_sdk::media_gen::{GeneratedMedia, VideoGenRequest};

    use super::*;

    struct ScriptedVideoProvider {
        polls: AtomicU32,
        succeed_on: u32,
        cancelled: Arc<AtomicBool>,
    }

    #[async_trait]
    impl VideoGenProvider for ScriptedVideoProvider {
        fn provider_id(&self) -> &'static str {
            "scripted_video"
        }

        async fn submit_video_job(&self, request: &VideoGenRequest) -> Result<MediaJobHandle, MediaGenError> {
            Ok(MediaJobHandle {
                job_id: "job-1".into(),
                model: request.model.clone(),
                metadata: serde_json::Value::Null,
            })
        }

        async fn poll_video_job(&self, _handle: &MediaJobHandle) -> Result<MediaJobStatus, MediaGenError> {
            let n = self.polls.fetch_add(1, Ordering::SeqCst) + 1;
            if n < self.succeed_on {
                Ok(MediaJobStatus::Running {
                    progress_pct: Some((n * 30).min(90) as u8),
                })
            } else {
                Ok(MediaJobStatus::Succeeded {
                    media: vec![GeneratedMedia {
                        mime_type: "video/mp4".into(),
                        bytes: vec![0, 1, 2, 3],
                        width: Some(1280),
                        height: Some(720),
                        duration_seconds: Some(5.0),
                        revised_prompt: None,
                    }],
                    usage: None,
                })
            }
        }

        async fn cancel_video_job(&self, _handle: &MediaJobHandle) -> Result<(), MediaGenError> {
            self.cancelled.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test]
    async fn polls_until_succeeded_and_reports_progress() {
        let provider = ScriptedVideoProvider {
            polls: AtomicU32::new(0),
            succeed_on: 3,
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        let handle = MediaJobHandle {
            job_id: "job-1".into(),
            model: "veo-3.1".into(),
            metadata: serde_json::Value::Null,
        };
        let mixin = AsyncMediaJobMixin {
            initial_delay: Duration::from_millis(5),
            max_interval: Duration::from_millis(10),
            timeout: Duration::from_secs(5),
            max_consecutive_poll_errors: 2,
        };
        let mut ticks = Vec::new();
        let response = mixin
            .poll_until_done(&provider, &handle, |p| ticks.push(p), std::future::pending())
            .await
            .unwrap();
        assert_eq!(response.videos.len(), 1);
        assert_eq!(ticks, vec![Some(30), Some(60), Some(100)]);
    }

    #[tokio::test]
    async fn cancellation_invokes_upstream_cancel_and_returns_cancelled() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let provider = ScriptedVideoProvider {
            polls: AtomicU32::new(0),
            succeed_on: 99,
            cancelled: cancelled.clone(),
        };
        let handle = MediaJobHandle {
            job_id: "job-cancel".into(),
            model: "veo-3.1".into(),
            metadata: serde_json::Value::Null,
        };
        let mixin = AsyncMediaJobMixin {
            initial_delay: Duration::from_secs(10),
            max_interval: Duration::from_secs(10),
            timeout: Duration::from_secs(60),
            max_consecutive_poll_errors: 1,
        };
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let _ = tx.send(());
        });
        let err = mixin
            .poll_until_done(&provider, &handle, |_| {}, async move {
                let _ = rx.await;
            })
            .await
            .unwrap_err();
        assert_eq!(err, MediaGenError::Cancelled);
        assert!(cancelled.load(Ordering::SeqCst));
    }
}
