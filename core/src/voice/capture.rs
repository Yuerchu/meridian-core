//! Microphone capture via cpal.
//!
//! `cpal::Stream` is `!Send`, so it must never cross an await point or sit in
//! tauri state. The stream lives and dies on a dedicated OS thread; commands
//! hold only the control sender and the result receiver.
//!
//! Capture runs in two stages. Opening an input device costs anywhere from tens
//! to hundreds of milliseconds, and a user who clicks and immediately starts
//! talking loses however long that takes — "你是谁啊" came back as "谁啊". So the
//! device is opened early (on hover), discarding samples until the recording
//! actually starts, at which point collection begins with no device latency at
//! all.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Instant;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use tokio::sync::oneshot;

pub struct Captured {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
}

enum CaptureCmd {
    /// Begin keeping samples. Until this arrives the device runs and its audio
    /// is thrown away.
    Collect,
    /// Stop and hand back what was collected.
    Stop,
    /// Stop and discard.
    Cancel,
}

pub struct RecordingSession {
    ctrl: mpsc::Sender<CaptureCmd>,
    done: Option<oneshot::Receiver<Result<Captured, String>>>,
    collecting: Arc<AtomicBool>,
    /// Set when collection begins, not when the device opened — the duration
    /// check and the on-screen timer must both describe real audio.
    pub started_at: Instant,
}

impl RecordingSession {
    /// Open the input device without recording yet. Returns once the stream is
    /// confirmed running, so a permission or device error surfaces here rather
    /// than at release time.
    pub fn open() -> Result<Self, String> {
        let (ctrl_tx, ctrl_rx) = mpsc::channel::<CaptureCmd>();
        let (done_tx, done_rx) = oneshot::channel();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
        let collecting = Arc::new(AtomicBool::new(false));
        let collecting_thread = collecting.clone();

        std::thread::Builder::new()
            .name("voice-capture".into())
            .spawn(move || capture_thread(ctrl_rx, done_tx, ready_tx, collecting_thread))
            .map_err(|e| format!("Failed to spawn capture thread: {e}"))?;

        ready_rx
            .recv()
            .map_err(|_| "Capture thread died before starting".to_string())??;

        Ok(RecordingSession {
            ctrl: ctrl_tx,
            done: Some(done_rx),
            collecting,
            started_at: Instant::now(),
        })
    }

    /// Start keeping audio. The device is already running, so this is immediate.
    pub fn begin_collecting(&mut self) -> Result<(), String> {
        self.ctrl
            .send(CaptureCmd::Collect)
            .map_err(|_| "Capture thread is gone".to_string())?;
        self.started_at = Instant::now();
        Ok(())
    }

    /// Whether this session is still idling on hover rather than recording.
    pub fn is_idle(&self) -> bool {
        !self.collecting.load(Ordering::Relaxed)
    }

    /// Stop and retrieve the recording.
    pub async fn stop(mut self) -> Result<Captured, String> {
        self.ctrl
            .send(CaptureCmd::Stop)
            .map_err(|_| "Capture thread is gone".to_string())?;
        let done = self.done.take().expect("stop consumes the session");
        done.await
            .map_err(|_| "Capture thread dropped without a result".to_string())?
    }

    /// Stop and discard. Best-effort: if the thread is already gone there is
    /// nothing to clean up.
    pub fn cancel(self) {
        let _ = self.ctrl.send(CaptureCmd::Cancel);
    }
}

fn capture_thread(
    ctrl: mpsc::Receiver<CaptureCmd>,
    done: oneshot::Sender<Result<Captured, String>>,
    ready: mpsc::Sender<Result<(), String>>,
    collecting: Arc<AtomicBool>,
) {
    let collecting_cb = collecting.clone();
    let stream_setup = (|| {
        let host = cpal::default_host();
        let device = host.default_input_device().ok_or("No microphone found".to_string())?;
        let config = device
            .default_input_config()
            .map_err(|e| format!("Cannot read microphone config: {e}"))?;

        let sample_rate = config.sample_rate().0;
        let channels = config.channels() as usize;
        let (samples_tx, samples_rx) = mpsc::channel::<Vec<f32>>();

        let stream = device
            .build_input_stream(
                &config.into(),
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    // Pre-roll: the device is warm but the user has not started
                    // recording, so this audio is deliberately dropped.
                    if !collecting_cb.load(Ordering::Relaxed) {
                        return;
                    }
                    // Mono for the recognizer: take the first channel only.
                    let mono: Vec<f32> = data.iter().step_by(channels).copied().collect();
                    let _ = samples_tx.send(mono);
                },
                |err| tracing::warn!("voice capture error: {err}"),
                None,
            )
            .map_err(|e| format!("Cannot open microphone: {e}"))?;
        stream.play().map_err(|e| format!("Cannot start recording: {e}"))?;

        Ok::<_, String>((stream, samples_rx, sample_rate))
    })();

    let (stream, samples_rx, sample_rate) = match stream_setup {
        Ok(parts) => {
            let _ = ready.send(Ok(()));
            parts
        }
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };

    // Idle here while the device warms; the cpal callback discards audio until
    // `Collect` flips the flag.
    let outcome = loop {
        match ctrl.recv() {
            Ok(CaptureCmd::Collect) => collecting.store(true, Ordering::Relaxed),
            Ok(CaptureCmd::Stop) => break CaptureCmd::Stop,
            // A dead channel means the session was dropped without a decision:
            // treat it as a cancel so the device is released.
            Ok(CaptureCmd::Cancel) | Err(_) => break CaptureCmd::Cancel,
        }
    };

    collecting.store(false, Ordering::Relaxed);
    drop(stream); // closes the device and hangs up samples_tx

    if matches!(outcome, CaptureCmd::Stop) {
        let mut samples = Vec::new();
        while let Ok(chunk) = samples_rx.try_recv() {
            samples.extend_from_slice(&chunk);
        }
        let _ = done.send(Ok(Captured { samples, sample_rate }));
    }
}
