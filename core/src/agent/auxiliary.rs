//! The model this app uses for its own background requests: summarising a
//! conversation for compaction, and naming it.
//!
//! Neither of those is the conversation. A summary is read back as background
//! and a title is a label in a list; the assistant that answers stays whatever
//! the conversation uses. So there is no reason to pay that model's price for
//! them — and on a long conversation the summariser's prompt is the whole
//! history, uncached (its system prompt and rendering are its own, so it can
//! never read the chat turns' cache), which makes it the largest single request
//! this app sends.
//!
//! **Unset means "the conversation's own model"**, which is what both did before
//! this existed. That is not a default standing in for a missing value: it is
//! the absence of a choice, and the callers keep their old path for it.
//!
//! **Set but unusable is an error, never a quiet return to the conversation's
//! model.** Somebody who chose a cheap model is told when it is not being used,
//! instead of finding out from the bill.

use std::sync::Arc;

use crate::agent::{
    ResolvedProvider, TurnParams, TurnParamsResolveRequest, resolve_turn_params, resolve_with_overrides,
};
use crate::db::sea::cap::Db;
use crate::db::sea::ops as sea_ops;
use crate::provider::codex_metadata::{CodexRequestKind, CodexThreadSource};
use crate::secrets::SecretsManager;

/// `provider_id:model_id`, the same encoding as `autoreview.model` and the
/// sub-agent defaults.
pub const AUXILIARY_MODEL_PREFERENCE: &str = "auxiliary.model";

/// Where a background request goes, resolved the way a turn's would be.
pub struct AuxiliaryModel {
    pub resolved: ResolvedProvider,
    pub turn: TurnParams,
}

/// The stored selection, read strictly: absent is no choice, anything present
/// has to be a canonical pair.
pub(crate) fn parse_selection(raw: Option<String>) -> Result<Option<(String, String)>, String> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let invalid = || format!("preference `{AUXILIARY_MODEL_PREFERENCE}` must be a canonical provider:model pair");
    let (provider_id, model_id) = raw.split_once(':').ok_or_else(invalid)?;
    if provider_id.is_empty() || model_id.is_empty() || provider_id.trim() != provider_id || model_id.trim() != model_id
    {
        return Err(invalid());
    }
    Ok(Some((provider_id.to_string(), model_id.to_string())))
}

/// The configured auxiliary model, or `None` when nobody chose one.
///
/// No assistant takes part: its temperature and output ceiling were set for
/// the model that answers, not for this one.
pub async fn configured(
    secrets: &Arc<SecretsManager>,
    db: &Db,
    kind: CodexRequestKind,
) -> Result<Option<AuxiliaryModel>, String> {
    let raw = db
        .read(async |tx| sea_ops::preference::get_preference(tx, AUXILIARY_MODEL_PREFERENCE).await)
        .await
        .map_err(|error| format!("failed to read preference {AUXILIARY_MODEL_PREFERENCE}: {error}"))?;
    let Some((provider_id, model_id)) = parse_selection(raw)? else {
        return Ok(None);
    };
    let unusable = |error: String| format!("the auxiliary model `{provider_id}:{model_id}` cannot be used: {error}");
    let resolved = resolve_with_overrides(secrets, db, None, Some(model_id.clone()), Some(&provider_id))
        .await
        .map_err(unusable)?;
    let turn = resolve_turn_params(
        db,
        TurnParamsResolveRequest {
            assistant: None,
            provider_id: Some(&resolved.provider_id),
            provider_type: &resolved.provider_type,
            api_format: &resolved.api_format,
            transport_profile: &resolved.transport_profile,
            codex_request_shape: resolved.codex_request_shape,
            codex_request_kind: kind,
            codex_thread_source: CodexThreadSource::User,
            model: &resolved.model,
            thinking_level: None,
            // Background work; it does not take the priority tier.
            fast: false,
        },
    )
    .await
    .map_err(unusable)?;
    Ok(Some(AuxiliaryModel { resolved, turn }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_selection_is_no_choice() {
        assert_eq!(parse_selection(None), Ok(None));
    }

    #[test]
    fn a_stored_pair_is_read_back() {
        assert_eq!(
            parse_selection(Some("p1:claude-haiku-5-5".into())),
            Ok(Some(("p1".into(), "claude-haiku-5-5".into())))
        );
    }

    /// Present but malformed is a broken setting, not "unset": reading it as
    /// unset would send every summary to the expensive model without a word.
    #[test]
    fn a_malformed_selection_is_an_error() {
        for bad in ["", "p1", "p1:", ":m", " p1:m", "p1:m "] {
            assert!(parse_selection(Some(bad.into())).is_err(), "{bad:?}");
        }
    }
}
