// Ported from codex-rs/utils/sleep-inhibitor/src/dummy.rs (Apache-2.0, OpenAI)
// NOTICE: This file contains code derived from the OpenAI Codex project.
// Changes: none.

#[derive(Debug, Default)]
pub(crate) struct SleepInhibitor;

impl SleepInhibitor {
    pub(crate) fn new() -> Self {
        Self
    }

    pub(crate) fn acquire(&mut self) {}

    pub(crate) fn release(&mut self) {}
}
