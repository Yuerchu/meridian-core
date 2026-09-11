/// Hosting another coding agent over ACP. Desktop only: every session is a
/// child process.
#[cfg(not(target_os = "android"))]
pub mod acp;
pub mod agent;
/// The JNI calls the core makes into the Android side. The `Java_*` entry
/// points Android calls back into stay in the shell, beside the activity that
/// declares them.
#[cfg(target_os = "android")]
pub mod android_bridge;
pub mod approval;
pub mod bootstrap;
pub mod client;
pub mod codex_auth;
/// Running a command in a container rather than on this machine. Desktop only:
/// Android has no `run_command` to place, and no Docker to place it in.
#[cfg(not(target_os = "android"))]
pub mod container;
pub mod db;
pub mod decimal;
pub mod emoji;
pub mod events;
pub mod files;
/// The endpoint another coding agent's hooks call into. Desktop only: it is a
/// listening socket, and Android has nothing to point at it.
#[cfg(not(target_os = "android"))]
pub mod hooks;
pub mod journal;
pub mod keyring;
pub mod listen_guard;
pub mod logging;
pub mod mcp;
#[cfg(not(target_os = "android"))]
pub mod onebot;
pub mod plan_files;
pub mod provider;
pub mod redaction;
#[cfg(not(target_os = "android"))]
pub mod sandbox;
pub mod secrets;
pub mod services;
pub mod sleep_inhibitor;
pub mod state;
pub mod template;
pub mod tools;
pub mod tts;
pub mod turn;
pub mod util;
pub mod voice;
pub mod voice_corpus;
pub mod workspace;
