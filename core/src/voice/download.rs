//! Model download: stream the archive to a `.part` file with progress events,
//! then hand off to `model::unpack_and_install` for verification and the
//! atomic swap. Cancellation and failure both clean up after themselves.

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
/// Hard cap far above the real 127MB archive; a runaway body means a broken
/// mirror, not a bigger model.
const MAX_ARCHIVE_BYTES: u64 = 1024 * 1024 * 1024;

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

fn part_path(app_data_dir: &Path) -> PathBuf {
    models_dir(app_data_dir).join("download.part")
}

/// Download and install the model. Emits `voice-model-download` progress and a
/// final `voice-model-download-done`; the caller only spawns and forgets.
pub async fn run(events: EventBus, app_data_dir: PathBuf, url: Option<String>, cancel: CancellationToken) {
    let url = url
        .filter(|u| !u.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_MODEL_URL.to_string());
    let result = tokio::select! {
        r = fetch_and_install(&events, &app_data_dir, &url) => r,
        _ = cancel.cancelled() => Err("cancelled".to_string()),
    };

    let _ = std::fs::remove_file(part_path(&app_data_dir));
    let event = match result {
        Ok(()) => VoiceModelDownloadDoneEvent::Completed {},
        Err(error) if error == "cancelled" => VoiceModelDownloadDoneEvent::Cancelled {},
        Err(error) => VoiceModelDownloadDoneEvent::Failed { error },
    };
    let _ = events.emit_voice_model_download_done(&event);
}

async fn fetch_and_install(events: &EventBus, app_data_dir: &Path, url: &str) -> Result<(), String> {
    let resp = http_client()?
        .get(url)
        .send()
        .await
        .map_err(|e| format!("Request failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    let total = resp.content_length();

    let part = part_path(app_data_dir);
    if let Some(parent) = part.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("Cannot create directory: {e}"))?;
    }
    let mut file = std::fs::File::create(&part).map_err(|e| format!("Cannot write file: {e}"))?;

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

    let data_dir = app_data_dir.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let file = std::fs::File::open(part_path(&data_dir)).map_err(|e| format!("Cannot reopen download: {e}"))?;
        let decoder = bzip2::read::BzDecoder::new(file);
        let mut archive = tar::Archive::new(decoder);
        super::model::unpack_and_install(&mut archive, &data_dir)
    })
    .await
    .map_err(|e| e.to_string())?
}
