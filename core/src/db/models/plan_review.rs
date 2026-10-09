//! Persistence rows for the durable plan document and its reviews.
//!
//! These rows deliberately stay below IPC.  JSON columns are storage details;
//! command handlers must decode them into their exact request/response types
//! instead of exposing the strings.

// The stored enums and the native runtime config live with the entities,
// which name them in their columns; re-exported here while the Diesel rows
// and ops still use them.
pub use crate::db::entity::plan_comment::{PlanCommentAnchorKind, PlanCommentState};
pub use crate::db::entity::plan_document::PlanDocumentState;
pub use crate::db::entity::plan_materialization::PlanMaterializationState;
pub use crate::db::entity::plan_review_delivery::{PlanDeliveryState, PlanDeliveryTarget};
pub use crate::db::entity::plan_review_draft::PlanReviewDraftMode;
pub use crate::db::entity::plan_review_session::{
    NativePlanReviewRuntimeConfig, PlanReviewProviderKind, PlanReviewState,
};
pub use crate::db::entity::plan_revision::PlanRevisionAuthorKind;
