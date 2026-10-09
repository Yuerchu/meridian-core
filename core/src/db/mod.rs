pub mod entity;
pub mod models;
pub mod sea;
pub mod sql;
pub mod types;

/// How long a connection waits for a lock before giving up.
///
/// Long enough to sit through any write this app makes — they are single-row
/// inserts and updates — while still failing rather than hanging if something
/// holds the write lock indefinitely. SQLite defaults this to zero: a
/// connection that meets a held lock fails on the spot with "database is
/// locked" instead of waiting.
pub(crate) const BUSY_TIMEOUT_MS: u32 = 5_000;

/// How long the pool waits for a free connection.
///
/// Long enough to ride out a burst, short enough that an exhausted pool reads
/// as an error rather than as the app having frozen. Every caller either
/// reports the failure or falls back within a request, so failing fast is
/// strictly better than waiting.
pub(crate) const POOL_ACQUIRE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
