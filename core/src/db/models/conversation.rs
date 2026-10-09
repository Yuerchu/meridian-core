impl crate::db::entity::conversation::Model {
    /// The assistant this conversation actually runs on.
    ///
    /// An ordinary conversation runs on whatever the assistant says; its model
    /// lives in front-end state and never reaches here. A delegated run is
    /// different — the model it was started with is on the row, and the whole
    /// transcript was written by that model. Continuing the conversation,
    /// compacting it and sizing its context indicator all have to agree with it,
    /// or a run on a 64K model reports how full a 200K window is and compaction
    /// waits for a threshold no request will ever reach.
    ///
    /// `context_limit` is cleared along with the model, and that is the part
    /// that is easy to miss: `resolve_turn_params` prefers a non-zero value on
    /// the assistant to anything the model says, so leaving the parent's number
    /// here would make the swap look done while changing nothing that matters.
    pub fn pin_model(
        &self,
        assistant: Option<crate::db::entity::assistant::Model>,
    ) -> Option<crate::db::entity::assistant::Model> {
        let (provider, model) = match (&self.agent_provider_id, &self.agent_model_id) {
            (Some(p), Some(m)) if !p.trim().is_empty() && !m.trim().is_empty() => (p, m),
            _ => return assistant,
        };
        assistant.map(|mut a| {
            a.provider_id = Some(provider.clone());
            a.model_id = Some(model.clone());
            a.context_limit = 0;
            a
        })
    }
}

/// One delegated run, as the card on the parent's turn needs it.
///
/// The turn is carried whole rather than reduced to a status string because the
/// coordinator has the last word on whether a `running` row is still running,
/// and that judgement belongs to the command layer — the same split
/// `TurnInfoResponse` makes.
#[derive(Debug, Clone)]
pub struct SubAgentRun {
    pub conversation_id: String,
    /// The assistant row and the provider call id of the delegating call. Both,
    /// always: call ids repeat within a conversation.
    pub spawned_by_message_id: Option<String>,
    pub spawned_by_call_id: Option<String>,
    pub spawned_turn_id: Option<String>,
    pub agent_kind: Option<String>,
    pub title: Option<String>,
    /// AssistantRow iterations in the delegated run — how many times the model was
    /// asked, not how many tools it called.
    pub steps: i64,
    /// The row named by `spawned_turn_id`, if it is still there.
    pub turn: Option<crate::db::entity::turn::Model>,
}
