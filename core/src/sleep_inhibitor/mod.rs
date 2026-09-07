// Ported from codex-rs/utils/sleep-inhibitor/src/lib.rs (Apache-2.0, OpenAI)
// NOTICE: This file contains code derived from the OpenAI Codex project.
// Changes: renamed platform modules; added refcounted AppSleepInhibitor and
// RAII TurnSleepGuard so concurrent chat turns share one OS power assertion;
// visibility reduced to pub(crate).

//! Cross-platform helper for preventing idle sleep while a turn is running.
//!
//! Platform-specific behavior:
//! - macOS: Uses native IOKit power assertions instead of spawning `caffeinate`.
//! - Linux: Spawns `systemd-inhibit` or `gnome-session-inhibit` while active.
//! - Windows: Uses `PowerCreateRequest` + `PowerSetRequest` with
//!   `PowerRequestSystemRequired`.
//! - Other platforms (Android, iOS, ...): No-op backend.

use std::sync::{Arc, Mutex};

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
mod dummy;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
use dummy as imp;
#[cfg(target_os = "linux")]
use linux as imp;
#[cfg(target_os = "macos")]
use macos as imp;
#[cfg(target_os = "windows")]
use windows as imp;

/// Keeps the machine awake while a turn is in progress when enabled.
#[derive(Debug)]
pub(crate) struct SleepInhibitor {
    enabled: bool,
    turn_running: bool,
    platform: imp::SleepInhibitor,
}

impl SleepInhibitor {
    pub(crate) fn new(enabled: bool) -> Self {
        Self {
            enabled,
            turn_running: false,
            platform: imp::SleepInhibitor::new(),
        }
    }

    /// Update the active turn state; turns sleep prevention on/off as needed.
    pub(crate) fn set_turn_running(&mut self, turn_running: bool) {
        self.turn_running = turn_running;
        if !self.enabled {
            self.release();
            return;
        }

        if turn_running {
            self.acquire();
        } else {
            self.release();
        }
    }

    fn acquire(&mut self) {
        self.platform.acquire();
    }

    fn release(&mut self) {
        self.platform.release();
    }

    /// Return the latest turn-running state requested by the caller.
    #[allow(dead_code)]
    pub(crate) fn is_turn_running(&self) -> bool {
        self.turn_running
    }
}

/// Refcounted wrapper shared as Tauri managed state: Meridian runs turns in
/// several conversations at once — the `TurnCoordinator` only makes them
/// exclusive per conversation, not across the app — so the OS assertion is held
/// until the last turn's guard drops.
///
/// Send/Sync note: Tauri's `manage()` enforces `Send + Sync` at compile time.
/// This holds today because windows-sys 0.52 defines `HANDLE = isize`;
/// upgrading to windows-sys >= 0.59 (pointer `HANDLE`) breaks `Send` loudly.
#[derive(Clone)]
pub struct AppSleepInhibitor(Arc<Mutex<SleepState>>);

struct SleepState {
    active_turns: usize,
    inhibitor: SleepInhibitor,
}

impl AppSleepInhibitor {
    pub(crate) fn new() -> Self {
        Self(Arc::new(Mutex::new(SleepState {
            active_turns: 0,
            inhibitor: SleepInhibitor::new(true),
        })))
    }

    /// Register a running turn; the machine stays awake until every returned
    /// guard has been dropped.
    pub fn begin_turn(&self) -> TurnSleepGuard {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        state.active_turns += 1;
        if state.active_turns == 1 {
            state.inhibitor.set_turn_running(true);
        }
        TurnSleepGuard(Arc::clone(&self.0))
    }
}

pub struct TurnSleepGuard(Arc<Mutex<SleepState>>);

impl Drop for TurnSleepGuard {
    fn drop(&mut self) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        state.active_turns = state.active_turns.saturating_sub(1);
        if state.active_turns == 0 {
            state.inhibitor.set_turn_running(false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::AppSleepInhibitor;
    use super::SleepInhibitor;

    #[test]
    fn sleep_inhibitor_toggles_without_panicking() {
        let mut inhibitor = SleepInhibitor::new(/*enabled*/ true);
        inhibitor.set_turn_running(/*turn_running*/ true);
        assert!(inhibitor.is_turn_running());
        inhibitor.set_turn_running(/*turn_running*/ false);
        assert!(!inhibitor.is_turn_running());
    }

    #[test]
    fn sleep_inhibitor_disabled_does_not_panic() {
        let mut inhibitor = SleepInhibitor::new(/*enabled*/ false);
        inhibitor.set_turn_running(/*turn_running*/ true);
        assert!(inhibitor.is_turn_running());
        inhibitor.set_turn_running(/*turn_running*/ false);
        assert!(!inhibitor.is_turn_running());
    }

    #[test]
    fn sleep_inhibitor_multiple_true_calls_are_idempotent() {
        let mut inhibitor = SleepInhibitor::new(/*enabled*/ true);
        inhibitor.set_turn_running(/*turn_running*/ true);
        inhibitor.set_turn_running(/*turn_running*/ true);
        inhibitor.set_turn_running(/*turn_running*/ true);
        inhibitor.set_turn_running(/*turn_running*/ false);
    }

    #[test]
    fn sleep_inhibitor_can_toggle_multiple_times() {
        let mut inhibitor = SleepInhibitor::new(/*enabled*/ true);
        inhibitor.set_turn_running(/*turn_running*/ true);
        inhibitor.set_turn_running(/*turn_running*/ false);
        inhibitor.set_turn_running(/*turn_running*/ true);
        inhibitor.set_turn_running(/*turn_running*/ false);
    }

    #[test]
    fn refcount_holds_until_last_guard_drops() {
        let app = AppSleepInhibitor::new();
        let g1 = app.begin_turn();
        let g2 = app.begin_turn();
        {
            let state = app.0.lock().unwrap();
            assert_eq!(state.active_turns, 2);
            assert!(state.inhibitor.is_turn_running());
        }
        drop(g1);
        {
            let state = app.0.lock().unwrap();
            assert_eq!(state.active_turns, 1);
            assert!(state.inhibitor.is_turn_running());
        }
        drop(g2);
        {
            let state = app.0.lock().unwrap();
            assert_eq!(state.active_turns, 0);
            assert!(!state.inhibitor.is_turn_running());
        }
    }
}
