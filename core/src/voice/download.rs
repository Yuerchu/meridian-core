//! Model download: stream the archive to a `.part` file with progress events,
//! then hand off to `model::import_archive` for verification and installation.
//! Cancellation and failure both clean up after themselves.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use crate::events::{EventBus, VoiceModelDownloadDoneEvent, VoiceModelDownloadEvent};

use super::{DEFAULT_MODEL_URL, models_dir};

/// Big file on a slow mirror: no request timeout, rely on connect timeout and
/// cancellation instead.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const PROGRESS_INTERVAL: Duration = Duration::from_millis(200);
use super::model::MAX_ARCHIVE_BYTES;

static HTTP: OnceLock<reqwest::Client> = OnceLock::new();

fn http_client() -> Result<&'static reqwest::Client, String> {
    match HTTP.get() {
        Some(c) => Ok(c),
        None => {
            let client = reqwest::Client::builder()
                .connect_timeout(CONNECT_TIMEOUT)
                .build()
                .map_err(|e| e.to_string())?;
            Ok(HTTP.get_or_init(|| client))
        }
    }
}

struct DownloadPart(PathBuf);

impl Drop for DownloadPart {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Download and install the model. Emits `voice-model-download` progress and a
/// final `voice-model-download-done`; the caller only spawns and forgets.
pub async fn run(events: EventBus, app_data_dir: PathBuf, url: Option<String>, cancel: CancellationToken) {
    let url = url
        .filter(|u| !u.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_MODEL_URL.to_string());
    let fetched = tokio::select! {
        r = fetch(&events, &app_data_dir, &url) => r,
        _ = cancel.cancelled() => Err("cancelled".to_string()),
    };
    // Once installation starts, wait for its result; dropping a blocking task does not cancel it.
    let result = match fetched {
        Ok(part) => tokio::task::spawn_blocking(move || super::model::import_archive(&app_data_dir, &part.0))
            .await
            .map_err(|e| e.to_string())
            .and_then(|r| r),
        Err(e) => Err(e),
    };
    let event = match result {
        Ok(()) => VoiceModelDownloadDoneEvent::Completed {},
        Err(error) if error == "cancelled" => VoiceModelDownloadDoneEvent::Cancelled {},
        Err(error) => VoiceModelDownloadDoneEvent::Failed { error },
    };
    let _ = events.emit_voice_model_download_done(&event);
}

async fn fetch(events: &EventBus, app_data_dir: &Path, url: &str) -> Result<DownloadPart, String> {
    let resp = http_client()?
        .get(url)
        .send()
        .await
        .map_err(|e| format!("Request failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    let total = resp.content_length();

    if total.is_some_and(|n| n > MAX_ARCHIVE_BYTES) {
        return Err("Download exceeded the size limit; check the mirror URL".into());
    }
    let parent = models_dir(app_data_dir);
    std::fs::create_dir_all(&parent).map_err(|e| format!("Cannot create directory: {e}"))?;
    let path = parent.join(format!("download-{}.part", uuid::Uuid::new_v4()));
    let file = std::fs::File::create_new(&path).map_err(|e| format!("Cannot write file: {e}"))?;
    let part = DownloadPart(path);
    // Move the handle after the guard so cancellation closes it before unlinking on Windows.
    let mut file = file;

    let mut resp = resp;
    let mut downloaded: u64 = 0;
    let mut last_emit = Instant::now() - PROGRESS_INTERVAL;
    while let Some(chunk) = resp.chunk().await.map_err(|e| format!("Download failed: {e}"))? {
        downloaded += chunk.len() as u64;
        if downloaded > MAX_ARCHIVE_BYTES {
            return Err("Download exceeded the size limit; check the mirror URL".into());
        }
        file.write_all(&chunk).map_err(|e| format!("Cannot write file: {e}"))?;
        if last_emit.elapsed() >= PROGRESS_INTERVAL {
            last_emit = Instant::now();
            let _ = events.emit_voice_model_download(&VoiceModelDownloadEvent::Progress { downloaded, total });
        }
    }
    file.flush().map_err(|e| format!("Cannot write file: {e}"))?;
    drop(file);

    // Final progress tick so the bar lands on 100% before the unpack pause.
    let _ = events.emit_voice_model_download(&VoiceModelDownloadEvent::Progress { downloaded, total });

    Ok(part)
}
