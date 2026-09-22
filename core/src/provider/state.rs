use serde::{Deserialize, Serialize};

const STORAGE_VERSION: u32 = 1;
pub const GOOGLE_OPENAI_CHAT_PROTOCOL: &str = "openai_chat_completions";
pub const GOOGLE_GENERATE_CONTENT_PROTOCOL: &str = "google_generate_content";
/// The Responses API as reached through ChatGPT's Codex backend.
///
/// Its own protocol rather than plain `responses`, because what makes this state
/// meaningful is `store: false`: the upstream keeps nothing, so the reasoning
/// has to travel with the next request. A provider that stores its own responses
/// produces none of this.
pub const CODEX_RESPONSES_PROTOCOL: &str = "codex_responses";
/// The Responses API reached as an ordinary API, where the upstream stores the
/// response itself.
///
/// Nothing replayable comes back from it — that is what
/// [`CODEX_RESPONSES_PROTOCOL`] is for — but an assistant message still carries
/// a phase, and that has to travel with the row.
pub const RESPONSES_PROTOCOL: &str = "responses";

/// Provider-owned continuation state attached to one assistant message.
///
/// It is a domain object: wire DTOs convert into it, and the database codec
/// below converts it into a versioned storage DTO. It is never an IPC DTO.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderState {
    pub version: u32,
    pub producer: ProviderStateProducer,
    pub payload: ProviderStatePayload,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderStateProducer {
    pub vendor: String,
    pub protocol: String,
    pub model: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderStatePayload {
    GoogleThoughtSignatures {
        signatures: Vec<GoogleThoughtSignature>,
    },
    /// The signature alone, from before whole blocks were kept. Still read for
    /// rows written then; nothing writes it any more.
    AnthropicThinkingSignature {
        signature: String,
    },
    /// Every content block of one Messages-API assistant turn that is neither
    /// `text` nor a client `tool_use`, verbatim and in order: `thinking` with
    /// its signature, `redacted_thinking`, `server_tool_use` and the result
    /// blocks a server tool produced.
    ///
    /// Kept raw for the same reason as [`CodexReasoningItem`]: the API wants
    /// these back exactly as it sent them, and a block it adds next year must
    /// survive the round trip without anyone here having heard of it.
    AnthropicContentBlocks {
        blocks: Vec<AnthropicContentBlock>,
    },
    /// Reasoning items alone, from before an assistant message's phase had
    /// anywhere to live. Still read for rows written then; nothing writes it
    /// any more — see [`ProviderStatePayload::ResponsesTurn`].
    CodexReasoning {
        items: Vec<CodexReasoningItem>,
    },
    /// What one Responses-API assistant turn has to hand back on the next
    /// request: the reasoning items a `store: false` turn produced, and the
    /// phase its message carried.
    ///
    /// The two live in one payload because a single turn produces both and
    /// [`ProviderStateAccumulator`] holds exactly one — two payload kinds in
    /// one response is the "mixed incompatible" error, which is right for two
    /// *vendors* and wrong for two facts about the same reply.
    ResponsesTurn {
        /// Whole items, opaque and encrypted. See [`CodexReasoningItem`].
        /// Empty on the ordinary Responses API, which stores its own.
        reasoning: Vec<CodexReasoningItem>,
        /// `None` means the upstream did not say, which is not the same as
        /// either label — see [`ResponseMessagePhase`].
        phase: Option<ResponseMessagePhase>,
    },
}

/// Whether an assistant message was interim narration or the answer itself.
///
/// OpenAI documents this on both the input and the output shape, and asks for
/// it *back*: "for models like `gpt-5.3-codex` and beyond, when sending
/// follow-up requests, preserve and resend phase on all assistant messages —
/// dropping it can degrade performance". So it is a fact the upstream states
/// and we return, never one derived here: a row could be labelled from whether
/// it ended in a tool call and would be wrong exactly where the model cared.
///
/// Absent is its own answer and the common one. Codex says the same thing about
/// its own copy — "providers do not emit this consistently, so callers must
/// treat `None` as phase unknown" — and omitting the field is what a model that
/// has never heard of it expects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseMessagePhase {
    /// Mid-turn text: more tool calls or more output may follow.
    Commentary,
    /// The turn's terminal answer.
    FinalAnswer,
}

impl ResponseMessagePhase {
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::Commentary => "commentary",
            Self::FinalAnswer => "final_answer",
        }
    }

    /// `None` for a label this app has not heard of. Storing it unread and
    /// sending it back would be handing the upstream a string we cannot say is
    /// still one of its own.
    pub fn from_wire(value: &str) -> Option<Self> {
        match value {
            "commentary" => Some(Self::Commentary),
            "final_answer" => Some(Self::FinalAnswer),
            _ => None,
        }
    }
}

/// One reasoning item, exactly as the upstream sent it.
///
/// Stored as raw JSON rather than as fields we picked out. The payload is
/// encrypted and meant only to be handed back, so there is nothing to gain by
/// understanding it — and a schema we invented would silently drop whatever the
/// upstream adds next, which is the one thing that must not happen to a blob
/// whose whole purpose is to survive a round trip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexReasoningItem {
    /// Where this item sat in the response's `output` array.
    ///
    /// Kept because order is not free to choose. Reasoning and the calls it led
    /// to have to go back in the order they came out; a turn with two reasoning
    /// items and two calls, replayed with the reasoning bunched at the front, is
    /// a different conversation from the one that happened. Recording the index
    /// costs nothing now and cannot be reconstructed later.
    pub position: usize,
    pub item_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoogleThoughtSignature {
    pub location: GoogleSignatureLocation,
    pub signature: String,
}

/// One Anthropic content block, exactly as the upstream sent it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnthropicContentBlock {
    /// Where this block sat in the response's `content` array. Order is not
    /// free to choose: thinking has to precede everything, and a server tool's
    /// result has to follow the call that produced it.
    pub position: usize,
    /// The block as a JSON object string.
    pub block_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoogleSignatureLocation {
    Message,
    ContentPart { index: usize },
    ToolCall { index: usize, call_id: Option<String> },
}

/// The only representation allowed to cross the persistence boundary.
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum StoredProviderStateV1 {
    GoogleThoughtSignatures {
        version: u32,
        producer: StoredProviderStateProducer,
        payload: StoredGoogleThoughtSignaturesPayload,
    },
    AnthropicThinkingSignature {
        version: u32,
        producer: StoredProviderStateProducer,
        payload: StoredAnthropicThinkingSignaturePayload,
    },
    AnthropicContentBlocks {
        version: u32,
        producer: StoredProviderStateProducer,
        payload: StoredAnthropicContentBlocksPayload,
    },
    CodexReasoning {
        version: u32,
        producer: StoredProviderStateProducer,
        payload: StoredCodexReasoningPayload,
    },
    ResponsesTurn {
        version: u32,
        producer: StoredProviderStateProducer,
        payload: StoredResponsesTurnPayload,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredResponsesTurnPayload {
    reasoning: Vec<StoredCodexReasoningItem>,
    /// Omitted rather than written as null, so a row whose upstream said
    /// nothing is the same shape as a row from before this existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    phase: Option<StoredResponseMessagePhase>,
}

#[derive(Serialize, Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum StoredResponseMessagePhase {
    Commentary,
    FinalAnswer,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredAnthropicContentBlocksPayload {
    blocks: Vec<StoredAnthropicContentBlock>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredAnthropicContentBlock {
    position: usize,
    block_json: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredProviderStateProducer {
    vendor: String,
    protocol: String,
    model: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredGoogleThoughtSignaturesPayload {
    signatures: Vec<StoredGoogleThoughtSignature>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredAnthropicThinkingSignaturePayload {
    signature: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredCodexReasoningPayload {
    items: Vec<StoredCodexReasoningItem>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredCodexReasoningItem {
    position: usize,
    item_json: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredGoogleThoughtSignature {
    location: StoredGoogleSignatureLocation,
    signature: String,
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum StoredGoogleSignatureLocation {
    Message(StoredGoogleMessageLocation),
    ContentPart(StoredGoogleContentPartLocation),
    ToolCall(StoredGoogleToolCallLocation),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredGoogleMessageLocation {
    #[serde(rename = "type")]
    kind: StoredGoogleMessageLocationKind,
}

#[derive(Serialize, Deserialize)]
enum StoredGoogleMessageLocationKind {
    #[serde(rename = "message")]
    Message,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredGoogleContentPartLocation {
    #[serde(rename = "type")]
    kind: StoredGoogleContentPartLocationKind,
    index: usize,
}

#[derive(Serialize, Deserialize)]
enum StoredGoogleContentPartLocationKind {
    #[serde(rename = "content_part")]
    ContentPart,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredGoogleToolCallLocation {
    #[serde(rename = "type")]
    kind: StoredGoogleToolCallLocationKind,
    index: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    call_id: Option<String>,
}

#[derive(Serialize, Deserialize)]
enum StoredGoogleToolCallLocationKind {
    #[serde(rename = "tool_call")]
    ToolCall,
}

/// Typed provider-state changes emitted by streaming wire adapters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderStateUpdate {
    /// A whole block, announced once its `content_block_stop` has arrived —
    /// deltas are folded together by the adapter, which is the only place that
    /// knows which delta type extends which field.
    AnthropicContentBlock {
        model: String,
        position: usize,
        block_json: String,
    },
    /// A whole reasoning item, not a delta — the Responses API sends it complete
    /// on `response.output_item.done`, so there is nothing to accumulate.
    CodexReasoningItem {
        model: String,
        position: usize,
        item_json: String,
    },
    /// The phase off a completed `message` output item.
    ///
    /// Carries its protocol because the same adapter speaks both: the Codex
    /// shape stores reasoning beside this, the ordinary API stores only this.
    ResponsesMessagePhase {
        protocol: String,
        model: String,
        phase: ResponseMessagePhase,
    },
    GoogleThoughtSignatureDelta {
        protocol: String,
        model: String,
        location: GoogleSignatureLocation,
        delta: String,
    },
}

impl ProviderState {
    pub fn from_storage_json(raw: &str) -> Result<Self, String> {
        let stored: StoredProviderStateV1 =
            serde_json::from_str(raw).map_err(|e| format!("invalid provider-state JSON: {e}"))?;
        let state = Self::from(stored);
        state.validate()?;
        Ok(state)
    }

    pub fn to_storage_json(&self) -> Result<String, String> {
        self.validate()?;
        serde_json::to_string(&StoredProviderStateV1::from(self))
            .map_err(|e| format!("could not encode provider state: {e}"))
    }

    pub fn anthropic_signature_for(&self, model: &str) -> Option<&str> {
        if self.producer.vendor != "anthropic" || self.producer.protocol != "messages" || self.producer.model != model {
            return None;
        }
        match &self.payload {
            ProviderStatePayload::AnthropicThinkingSignature { signature } => Some(signature),
            _ => None,
        }
    }

    /// The blocks to replay, in the order they were produced. Model-matched
    /// like the signature: a thinking block signed by one model is rejected
    /// by another, and a server tool's result belongs to the turn that ran it.
    pub fn anthropic_blocks_for(&self, model: &str) -> Option<&[AnthropicContentBlock]> {
        if self.producer.vendor != "anthropic" || self.producer.protocol != "messages" || self.producer.model != model {
            return None;
        }
        match &self.payload {
            ProviderStatePayload::AnthropicContentBlocks { blocks } => Some(blocks),
            _ => None,
        }
    }

    pub fn google_signatures_for(&self, protocol: &str, model: &str) -> Option<&[GoogleThoughtSignature]> {
        if self.producer.vendor != "google" || self.producer.protocol != protocol || self.producer.model != model {
            return None;
        }
        match &self.payload {
            ProviderStatePayload::GoogleThoughtSignatures { signatures } => Some(signatures),
            _ => None,
        }
    }

    /// The reasoning to replay, in the order it was produced.
    ///
    /// Model-matched like the two above: reasoning from one model is not
    /// something another can be asked to continue from, and the upstream would
    /// reject it. Switching models mid-conversation therefore starts the
    /// reasoning chain over rather than sending something that cannot be used.
    pub fn codex_reasoning_for(&self, model: &str) -> Option<&[CodexReasoningItem]> {
        if self.producer.vendor != "openai"
            || self.producer.protocol != CODEX_RESPONSES_PROTOCOL
            || self.producer.model != model
        {
            return None;
        }
        match &self.payload {
            ProviderStatePayload::CodexReasoning { items } => Some(items),
            ProviderStatePayload::ResponsesTurn { reasoning, .. } => Some(reasoning),
            _ => None,
        }
    }

    /// The phase to send back with this turn's assistant message.
    ///
    /// **Not model-matched, which is the one place this parts company with the
    /// reasoning above.** That is an encrypted blob bound to the model that
    /// produced it, and handing another one back is a 400. A phase is a plain
    /// label about what an assistant message *was* — the next model is being
    /// shown that message either way, and withholding the label from it is
    /// precisely the degradation the field exists to prevent. The vendor and
    /// protocol checks are what keep it off a reply that came from somewhere
    /// else entirely.
    pub fn responses_phase(&self) -> Option<ResponseMessagePhase> {
        if self.producer.vendor != "openai"
            || !matches!(
                self.producer.protocol.as_str(),
                RESPONSES_PROTOCOL | CODEX_RESPONSES_PROTOCOL
            )
        {
            return None;
        }
        match &self.payload {
            ProviderStatePayload::ResponsesTurn { phase, .. } => *phase,
            _ => None,
        }
    }

    fn validate(&self) -> Result<(), String> {
        if self.version != STORAGE_VERSION {
            return Err(format!("unsupported provider-state version {}", self.version));
        }
        if self.producer.vendor.is_empty() || self.producer.protocol.is_empty() || self.producer.model.is_empty() {
            return Err("provider-state producer is incomplete".into());
        }
        match &self.payload {
            ProviderStatePayload::GoogleThoughtSignatures { signatures } => {
                if self.producer.vendor != "google"
                    || !matches!(
                        self.producer.protocol.as_str(),
                        GOOGLE_OPENAI_CHAT_PROTOCOL | GOOGLE_GENERATE_CONTENT_PROTOCOL
                    )
                {
                    return Err("Google thought signatures have the wrong producer".into());
                }
                if signatures.is_empty() || signatures.iter().any(|s| s.signature.is_empty()) {
                    return Err("Google thought signatures are empty".into());
                }
            }
            ProviderStatePayload::AnthropicThinkingSignature { signature } => {
                if self.producer.vendor != "anthropic" || self.producer.protocol != "messages" {
                    return Err("Anthropic thinking signature has the wrong producer".into());
                }
                if signature.is_empty() {
                    return Err("Anthropic thinking signature is empty".into());
                }
            }
            ProviderStatePayload::AnthropicContentBlocks { blocks } => {
                if self.producer.vendor != "anthropic" || self.producer.protocol != "messages" {
                    return Err("Anthropic content blocks have the wrong producer".into());
                }
                if blocks.is_empty() || blocks.iter().any(|block| block.block_json.is_empty()) {
                    return Err("Anthropic content blocks are empty".into());
                }
            }
            ProviderStatePayload::CodexReasoning { items } => {
                if self.producer.vendor != "openai" || self.producer.protocol != CODEX_RESPONSES_PROTOCOL {
                    return Err("Codex reasoning has the wrong producer".into());
                }
                if items.is_empty() || items.iter().any(|item| item.item_json.is_empty()) {
                    return Err("Codex reasoning is empty".into());
                }
            }
            ProviderStatePayload::ResponsesTurn { reasoning, phase } => {
                if self.producer.vendor != "openai"
                    || !matches!(
                        self.producer.protocol.as_str(),
                        RESPONSES_PROTOCOL | CODEX_RESPONSES_PROTOCOL
                    )
                {
                    return Err("a Responses turn has the wrong producer".into());
                }
                if reasoning.iter().any(|item| item.item_json.is_empty()) {
                    return Err("Codex reasoning is empty".into());
                }
                // Reasoning only survives `store: false`, which is the Codex
                // shape; under the ordinary API the upstream keeps its own and
                // an item here would be one nobody could have produced.
                if !reasoning.is_empty() && self.producer.protocol != CODEX_RESPONSES_PROTOCOL {
                    return Err("Responses reasoning has the wrong protocol".into());
                }
                if reasoning.is_empty() && phase.is_none() {
                    return Err("a Responses turn carries nothing".into());
                }
            }
        }
        Ok(())
    }
}

impl From<&ProviderState> for StoredProviderStateV1 {
    fn from(state: &ProviderState) -> Self {
        let producer = || StoredProviderStateProducer {
            vendor: state.producer.vendor.clone(),
            protocol: state.producer.protocol.clone(),
            model: state.producer.model.clone(),
        };
        match &state.payload {
            ProviderStatePayload::GoogleThoughtSignatures { signatures } => {
                StoredProviderStateV1::GoogleThoughtSignatures {
                    version: state.version,
                    producer: producer(),
                    payload: StoredGoogleThoughtSignaturesPayload {
                        signatures: signatures
                            .iter()
                            .map(|item| StoredGoogleThoughtSignature {
                                location: StoredGoogleSignatureLocation::from(&item.location),
                                signature: item.signature.clone(),
                            })
                            .collect(),
                    },
                }
            }
            ProviderStatePayload::AnthropicThinkingSignature { signature } => {
                StoredProviderStateV1::AnthropicThinkingSignature {
                    version: state.version,
                    producer: producer(),
                    payload: StoredAnthropicThinkingSignaturePayload {
                        signature: signature.clone(),
                    },
                }
            }
            ProviderStatePayload::AnthropicContentBlocks { blocks } => StoredProviderStateV1::AnthropicContentBlocks {
                version: state.version,
                producer: producer(),
                payload: StoredAnthropicContentBlocksPayload {
                    blocks: blocks
                        .iter()
                        .map(|block| StoredAnthropicContentBlock {
                            position: block.position,
                            block_json: block.block_json.clone(),
                        })
                        .collect(),
                },
            },
            ProviderStatePayload::CodexReasoning { items } => StoredProviderStateV1::CodexReasoning {
                version: state.version,
                producer: producer(),
                payload: StoredCodexReasoningPayload {
                    items: items
                        .iter()
                        .map(|item| StoredCodexReasoningItem {
                            position: item.position,
                            item_json: item.item_json.clone(),
                        })
                        .collect(),
                },
            },
            ProviderStatePayload::ResponsesTurn { reasoning, phase } => StoredProviderStateV1::ResponsesTurn {
                version: state.version,
                producer: producer(),
                payload: StoredResponsesTurnPayload {
                    reasoning: reasoning
                        .iter()
                        .map(|item| StoredCodexReasoningItem {
                            position: item.position,
                            item_json: item.item_json.clone(),
                        })
                        .collect(),
                    phase: phase.map(StoredResponseMessagePhase::from),
                },
            },
        }
    }
}

impl From<ResponseMessagePhase> for StoredResponseMessagePhase {
    fn from(phase: ResponseMessagePhase) -> Self {
        match phase {
            ResponseMessagePhase::Commentary => Self::Commentary,
            ResponseMessagePhase::FinalAnswer => Self::FinalAnswer,
        }
    }
}

impl From<StoredResponseMessagePhase> for ResponseMessagePhase {
    fn from(phase: StoredResponseMessagePhase) -> Self {
        match phase {
            StoredResponseMessagePhase::Commentary => Self::Commentary,
            StoredResponseMessagePhase::FinalAnswer => Self::FinalAnswer,
        }
    }
}

impl From<StoredProviderStateV1> for ProviderState {
    fn from(stored: StoredProviderStateV1) -> Self {
        let (version, producer, payload) = match stored {
            StoredProviderStateV1::GoogleThoughtSignatures {
                version,
                producer,
                payload,
            } => (
                version,
                producer,
                ProviderStatePayload::GoogleThoughtSignatures {
                    signatures: payload
                        .signatures
                        .into_iter()
                        .map(|item| GoogleThoughtSignature {
                            location: GoogleSignatureLocation::from(item.location),
                            signature: item.signature,
                        })
                        .collect(),
                },
            ),
            StoredProviderStateV1::AnthropicThinkingSignature {
                version,
                producer,
                payload,
            } => (
                version,
                producer,
                ProviderStatePayload::AnthropicThinkingSignature {
                    signature: payload.signature,
                },
            ),
            StoredProviderStateV1::AnthropicContentBlocks {
                version,
                producer,
                payload,
            } => (
                version,
                producer,
                ProviderStatePayload::AnthropicContentBlocks {
                    blocks: payload
                        .blocks
                        .into_iter()
                        .map(|block| AnthropicContentBlock {
                            position: block.position,
                            block_json: block.block_json,
                        })
                        .collect(),
                },
            ),
            StoredProviderStateV1::CodexReasoning {
                version,
                producer,
                payload,
            } => (
                version,
                producer,
                ProviderStatePayload::CodexReasoning {
                    items: payload
                        .items
                        .into_iter()
                        .map(|item| CodexReasoningItem {
                            position: item.position,
                            item_json: item.item_json,
                        })
                        .collect(),
                },
            ),
            StoredProviderStateV1::ResponsesTurn {
                version,
                producer,
                payload,
            } => (
                version,
                producer,
                ProviderStatePayload::ResponsesTurn {
                    reasoning: payload
                        .reasoning
                        .into_iter()
                        .map(|item| CodexReasoningItem {
                            position: item.position,
                            item_json: item.item_json,
                        })
                        .collect(),
                    phase: payload.phase.map(ResponseMessagePhase::from),
                },
            ),
        };
        Self {
            version,
            producer: ProviderStateProducer {
                vendor: producer.vendor,
                protocol: producer.protocol,
                model: producer.model,
            },
            payload,
        }
    }
}

impl From<&GoogleSignatureLocation> for StoredGoogleSignatureLocation {
    fn from(location: &GoogleSignatureLocation) -> Self {
        match location {
            GoogleSignatureLocation::Message => Self::Message(StoredGoogleMessageLocation {
                kind: StoredGoogleMessageLocationKind::Message,
            }),
            GoogleSignatureLocation::ContentPart { index } => Self::ContentPart(StoredGoogleContentPartLocation {
                kind: StoredGoogleContentPartLocationKind::ContentPart,
                index: *index,
            }),
            GoogleSignatureLocation::ToolCall { index, call_id } => Self::ToolCall(StoredGoogleToolCallLocation {
                kind: StoredGoogleToolCallLocationKind::ToolCall,
                index: *index,
                call_id: call_id.clone(),
            }),
        }
    }
}

impl From<StoredGoogleSignatureLocation> for GoogleSignatureLocation {
    fn from(location: StoredGoogleSignatureLocation) -> Self {
        match location {
            StoredGoogleSignatureLocation::Message(_) => Self::Message,
            StoredGoogleSignatureLocation::ContentPart(location) => Self::ContentPart { index: location.index },
            StoredGoogleSignatureLocation::ToolCall(location) => Self::ToolCall {
                index: location.index,
                call_id: location.call_id,
            },
        }
    }
}

#[derive(Default)]
pub struct ProviderStateAccumulator {
    state: Option<ProviderState>,
}

impl ProviderStateAccumulator {
    pub fn apply(&mut self, update: ProviderStateUpdate) -> Result<(), String> {
        match update {
            ProviderStateUpdate::AnthropicContentBlock {
                model,
                position,
                block_json,
            } => {
                if block_json.is_empty() {
                    return Ok(());
                }
                let arriving = AnthropicContentBlock { position, block_json };
                match self.state.as_mut() {
                    None => {
                        self.state = Some(ProviderState {
                            version: STORAGE_VERSION,
                            producer: ProviderStateProducer {
                                vendor: "anthropic".into(),
                                protocol: "messages".into(),
                                model,
                            },
                            payload: ProviderStatePayload::AnthropicContentBlocks { blocks: vec![arriving] },
                        });
                    }
                    Some(ProviderState {
                        producer,
                        payload: ProviderStatePayload::AnthropicContentBlocks { blocks },
                        ..
                    }) if producer.model == model => {
                        // One announcement per block, but a repeat at the same
                        // index would otherwise be replayed twice.
                        match blocks.iter_mut().find(|block| block.position == arriving.position) {
                            Some(existing) => *existing = arriving,
                            None => blocks.push(arriving),
                        }
                        blocks.sort_by_key(|block| block.position);
                    }
                    Some(_) => return Err("a response mixed incompatible provider state".into()),
                }
            }
            ProviderStateUpdate::CodexReasoningItem {
                model,
                position,
                item_json,
            } => {
                if item_json.is_empty() {
                    return Ok(());
                }
                let arriving = CodexReasoningItem { position, item_json };
                match self.state.as_mut() {
                    None => {
                        self.state = Some(ProviderState {
                            version: STORAGE_VERSION,
                            producer: ProviderStateProducer {
                                vendor: "openai".into(),
                                protocol: CODEX_RESPONSES_PROTOCOL.into(),
                                model,
                            },
                            payload: ProviderStatePayload::ResponsesTurn {
                                reasoning: vec![arriving],
                                phase: None,
                            },
                        });
                    }
                    Some(ProviderState {
                        producer,
                        payload: ProviderStatePayload::ResponsesTurn { reasoning, .. },
                        ..
                    }) if producer.model == model && producer.protocol == CODEX_RESPONSES_PROTOCOL => {
                        // The upstream announces an item once, but a repeat
                        // would otherwise be replayed twice at the same index.
                        match reasoning.iter_mut().find(|item| item.position == arriving.position) {
                            Some(existing) => *existing = arriving,
                            None => reasoning.push(arriving),
                        }
                    }
                    Some(_) => return Err("a response mixed incompatible provider state".into()),
                }
            }
            ProviderStateUpdate::ResponsesMessagePhase { protocol, model, phase } => {
                match self.state.as_mut() {
                    None => {
                        self.state = Some(ProviderState {
                            version: STORAGE_VERSION,
                            producer: ProviderStateProducer {
                                vendor: "openai".into(),
                                protocol,
                                model,
                            },
                            payload: ProviderStatePayload::ResponsesTurn {
                                reasoning: Vec::new(),
                                phase: Some(phase),
                            },
                        });
                    }
                    Some(ProviderState {
                        producer,
                        payload: ProviderStatePayload::ResponsesTurn { phase: existing, .. },
                        ..
                    }) if producer.model == model && producer.protocol == protocol => {
                        // **The last message item's phase wins, and that is a
                        // limitation rather than a choice.** One round is one
                        // response and one row, whose `content` is every
                        // message item of that response joined — so only one
                        // label can travel with it. A response that narrates
                        // and then answers gets `final_answer`, which is what
                        // the merged text ends as; the narration inside it is
                        // labelled with the answer's phase and nothing here can
                        // say otherwise without splitting the row.
                        *existing = Some(phase);
                    }
                    Some(_) => return Err("a response mixed incompatible provider state".into()),
                }
            }
            ProviderStateUpdate::GoogleThoughtSignatureDelta {
                protocol,
                model,
                location,
                delta,
            } => {
                if delta.is_empty() {
                    return Ok(());
                }
                match self.state.as_mut() {
                    None => {
                        self.state = Some(ProviderState {
                            version: STORAGE_VERSION,
                            producer: ProviderStateProducer {
                                vendor: "google".into(),
                                protocol,
                                model,
                            },
                            payload: ProviderStatePayload::GoogleThoughtSignatures {
                                signatures: vec![GoogleThoughtSignature {
                                    location,
                                    signature: delta,
                                }],
                            },
                        });
                    }
                    Some(ProviderState {
                        producer,
                        payload: ProviderStatePayload::GoogleThoughtSignatures { signatures },
                        ..
                    }) if producer.protocol == protocol && producer.model == model => {
                        if let Some(existing) = signatures.iter_mut().find(|s| same_location(&s.location, &location)) {
                            merge_location(&mut existing.location, location);
                            existing.signature.push_str(&delta);
                        } else {
                            signatures.push(GoogleThoughtSignature {
                                location,
                                signature: delta,
                            });
                        }
                    }
                    Some(_) => return Err("a response mixed incompatible provider state".into()),
                }
            }
        }
        Ok(())
    }

    pub fn finish(self) -> Option<ProviderState> {
        self.state
    }
}

fn same_location(a: &GoogleSignatureLocation, b: &GoogleSignatureLocation) -> bool {
    match (a, b) {
        (GoogleSignatureLocation::Message, GoogleSignatureLocation::Message) => true,
        (GoogleSignatureLocation::ContentPart { index: a }, GoogleSignatureLocation::ContentPart { index: b }) => {
            a == b
        }
        (GoogleSignatureLocation::ToolCall { index: a, .. }, GoogleSignatureLocation::ToolCall { index: b, .. }) => {
            a == b
        }
        _ => false,
    }
}

fn merge_location(existing: &mut GoogleSignatureLocation, incoming: GoogleSignatureLocation) {
    if let (
        GoogleSignatureLocation::ToolCall { call_id: current, .. },
        GoogleSignatureLocation::ToolCall { call_id: incoming, .. },
    ) = (existing, incoming)
        && current.is_none()
    {
        *current = incoming;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_round_trip_is_versioned_and_typed() {
        let mut acc = ProviderStateAccumulator::default();
        acc.apply(ProviderStateUpdate::GoogleThoughtSignatureDelta {
            protocol: GOOGLE_OPENAI_CHAT_PROTOCOL.into(),
            model: "gemini-3.7-flash".into(),
            location: GoogleSignatureLocation::ToolCall {
                index: 0,
                call_id: Some("call-1".into()),
            },
            delta: "sig".into(),
        })
        .unwrap();
        let state = acc.finish().unwrap();
        let raw = state.to_storage_json().unwrap();
        assert_eq!(ProviderState::from_storage_json(&raw).unwrap(), state);
        assert!(raw.contains("google_thought_signatures"));
    }

    #[test]
    fn native_content_part_state_round_trips_with_its_protocol() {
        let mut acc = ProviderStateAccumulator::default();
        acc.apply(ProviderStateUpdate::GoogleThoughtSignatureDelta {
            protocol: GOOGLE_GENERATE_CONTENT_PROTOCOL.into(),
            model: "gemini-3.7-flash".into(),
            location: GoogleSignatureLocation::ContentPart { index: 2 },
            delta: "native-sig".into(),
        })
        .unwrap();
        let state = acc.finish().unwrap();
        let raw = state.to_storage_json().unwrap();
        let restored = ProviderState::from_storage_json(&raw).unwrap();
        assert_eq!(restored, state);
        assert!(raw.contains("google_generate_content"));
        assert!(raw.contains("content_part"));
    }

    #[test]
    fn unknown_storage_version_is_rejected() {
        let raw = r#"{"version":2,"producer":{"vendor":"anthropic","protocol":"messages","model":"m"},"kind":"anthropic_thinking_signature","payload":{"signature":"sig"}}"#;
        assert!(ProviderState::from_storage_json(raw).unwrap_err().contains("version"));
    }

    fn valid_stored_google_state() -> serde_json::Value {
        serde_json::json!({
            "version": 1,
            "producer": {
                "vendor": "google",
                "protocol": GOOGLE_OPENAI_CHAT_PROTOCOL,
                "model": "gemini-3.7-flash"
            },
            "kind": "google_thought_signatures",
            "payload": {
                "signatures": [{
                    "location": { "type": "message" },
                    "signature": "sig"
                }]
            }
        })
    }

    fn rejects_stored_value(value: serde_json::Value) {
        let raw = serde_json::to_string(&value).unwrap();
        assert!(ProviderState::from_storage_json(&raw).is_err(), "{raw}");
    }

    #[test]
    fn unknown_top_level_provider_state_fields_are_rejected() {
        let mut value = valid_stored_google_state();
        value["future"] = serde_json::json!(true);
        rejects_stored_value(value);
    }

    #[test]
    fn unknown_nested_provider_state_fields_are_rejected() {
        let mut producer = valid_stored_google_state();
        producer["producer"]["future"] = serde_json::json!(true);
        rejects_stored_value(producer);

        let mut payload = valid_stored_google_state();
        payload["payload"]["future"] = serde_json::json!(true);
        rejects_stored_value(payload);

        let mut signature = valid_stored_google_state();
        signature["payload"]["signatures"][0]["future"] = serde_json::json!(true);
        rejects_stored_value(signature);

        let mut location = valid_stored_google_state();
        location["payload"]["signatures"][0]["location"]["future"] = serde_json::json!(true);
        rejects_stored_value(location);
    }

    #[test]
    fn unknown_codex_item_fields_are_rejected() {
        let mut acc = ProviderStateAccumulator::default();
        acc.apply(codex_item(0, "rs_strict")).unwrap();
        let mut value: serde_json::Value =
            serde_json::from_str(&acc.finish().unwrap().to_storage_json().unwrap()).unwrap();
        value["payload"]["reasoning"][0]["future"] = serde_json::json!(true);
        rejects_stored_value(value);
    }

    fn phase_update(protocol: &str, phase: ResponseMessagePhase) -> ProviderStateUpdate {
        ProviderStateUpdate::ResponsesMessagePhase {
            protocol: protocol.into(),
            model: "gpt-5.6".into(),
            phase,
        }
    }

    /// The reasoning and the phase are two facts about one reply, so they share
    /// a payload — holding them as two would be the "mixed incompatible" error,
    /// which is meant for two vendors.
    #[test]
    fn a_turns_reasoning_and_phase_live_in_one_payload() {
        let mut acc = ProviderStateAccumulator::default();
        acc.apply(codex_item(0, "rs_1")).unwrap();
        acc.apply(phase_update(
            CODEX_RESPONSES_PROTOCOL,
            ResponseMessagePhase::FinalAnswer,
        ))
        .unwrap();
        let state = acc.finish().unwrap();

        assert_eq!(state.codex_reasoning_for("gpt-5.6").unwrap().len(), 1);
        assert_eq!(state.responses_phase(), Some(ResponseMessagePhase::FinalAnswer));

        let raw = state.to_storage_json().unwrap();
        assert!(raw.contains("responses_turn"));
        assert!(raw.contains("final_answer"));
        assert_eq!(ProviderState::from_storage_json(&raw).unwrap(), state);
    }

    /// The ordinary Responses API stores its own reasoning, so a phase is all
    /// there is to keep — and it is kept under its own protocol.
    #[test]
    fn a_phase_alone_is_state_worth_storing() {
        let mut acc = ProviderStateAccumulator::default();
        acc.apply(phase_update(RESPONSES_PROTOCOL, ResponseMessagePhase::Commentary))
            .unwrap();
        let state = acc.finish().unwrap();

        assert_eq!(state.responses_phase(), Some(ResponseMessagePhase::Commentary));
        // Not the Codex shape, so nothing replayable came back with it.
        assert!(state.codex_reasoning_for("gpt-5.6").is_none());
        assert_eq!(
            ProviderState::from_storage_json(&state.to_storage_json().unwrap()).unwrap(),
            state
        );
    }

    /// One round is one row whose content is every message item joined, so only
    /// one label can travel with it; the one the response ended on is the one
    /// the merged text ends as.
    #[test]
    fn the_last_message_items_phase_is_the_one_kept() {
        let mut acc = ProviderStateAccumulator::default();
        acc.apply(phase_update(RESPONSES_PROTOCOL, ResponseMessagePhase::Commentary))
            .unwrap();
        acc.apply(phase_update(RESPONSES_PROTOCOL, ResponseMessagePhase::FinalAnswer))
            .unwrap();
        assert_eq!(
            acc.finish().unwrap().responses_phase(),
            Some(ResponseMessagePhase::FinalAnswer)
        );
    }

    /// Unlike the reasoning beside it, the phase is a plain label rather than a
    /// blob bound to the model that made it: the next model is shown that
    /// message either way, and withholding the label is the degradation the
    /// field exists to prevent.
    #[test]
    fn a_phase_survives_a_model_switch_while_the_reasoning_does_not() {
        let mut acc = ProviderStateAccumulator::default();
        acc.apply(codex_item(0, "rs_1")).unwrap();
        acc.apply(phase_update(
            CODEX_RESPONSES_PROTOCOL,
            ResponseMessagePhase::FinalAnswer,
        ))
        .unwrap();
        let state = acc.finish().unwrap();

        assert!(state.codex_reasoning_for("gpt-5.4").is_none());
        assert_eq!(state.responses_phase(), Some(ResponseMessagePhase::FinalAnswer));
    }

    /// Another vendor's reply has no phase to give, whatever it stored.
    #[test]
    fn another_vendors_state_offers_no_phase() {
        let mut acc = ProviderStateAccumulator::default();
        acc.apply(anthropic_block(
            0,
            r#"{"type":"thinking","thinking":"hm","signature":"sig"}"#,
        ))
        .unwrap();
        assert!(acc.finish().unwrap().responses_phase().is_none());
    }

    /// Rows written before the phase had anywhere to live still read.
    #[test]
    fn legacy_codex_reasoning_rows_still_read() {
        let raw = r#"{"version":1,"producer":{"vendor":"openai","protocol":"codex_responses","model":"gpt-5.6"},"kind":"codex_reasoning","payload":{"items":[{"position":0,"item_json":"{\"type\":\"reasoning\"}"}]}}"#;
        let state = ProviderState::from_storage_json(raw).unwrap();
        assert_eq!(state.codex_reasoning_for("gpt-5.6").unwrap().len(), 1);
        assert!(state.responses_phase().is_none());
    }

    /// Reasoning only survives `store: false`, so an item stored under the
    /// ordinary API is one nobody could have produced.
    #[test]
    fn reasoning_under_the_plain_protocol_is_refused() {
        let state = ProviderState {
            version: STORAGE_VERSION,
            producer: ProviderStateProducer {
                vendor: "openai".into(),
                protocol: RESPONSES_PROTOCOL.into(),
                model: "gpt-5.6".into(),
            },
            payload: ProviderStatePayload::ResponsesTurn {
                reasoning: vec![CodexReasoningItem {
                    position: 0,
                    item_json: "{}".into(),
                }],
                phase: None,
            },
        };
        assert!(state.to_storage_json().is_err());
    }

    /// A turn that captured neither is not a row.
    #[test]
    fn an_empty_responses_turn_is_refused() {
        let state = ProviderState {
            version: STORAGE_VERSION,
            producer: ProviderStateProducer {
                vendor: "openai".into(),
                protocol: RESPONSES_PROTOCOL.into(),
                model: "gpt-5.6".into(),
            },
            payload: ProviderStatePayload::ResponsesTurn {
                reasoning: Vec::new(),
                phase: None,
            },
        };
        assert!(state.to_storage_json().is_err());
    }

    #[test]
    fn state_is_only_replayed_to_its_producing_model() {
        let mut acc = ProviderStateAccumulator::default();
        acc.apply(ProviderStateUpdate::GoogleThoughtSignatureDelta {
            protocol: GOOGLE_OPENAI_CHAT_PROTOCOL.into(),
            model: "gemini-3.7-flash".into(),
            location: GoogleSignatureLocation::Message,
            delta: "sig".into(),
        })
        .unwrap();
        let state = acc.finish().unwrap();
        assert!(
            state
                .google_signatures_for(GOOGLE_OPENAI_CHAT_PROTOCOL, "gemini-3.7-flash")
                .is_some()
        );
        assert!(
            state
                .google_signatures_for(GOOGLE_GENERATE_CONTENT_PROTOCOL, "gemini-3.7-flash")
                .is_none()
        );
        assert!(
            state
                .google_signatures_for(GOOGLE_OPENAI_CHAT_PROTOCOL, "gemini-3.6-flash")
                .is_none()
        );
        assert!(state.anthropic_signature_for("gemini-3.7-flash").is_none());
    }

    fn codex_item(position: usize, id: &str) -> ProviderStateUpdate {
        ProviderStateUpdate::CodexReasoningItem {
            model: "gpt-5.6".into(),
            position,
            item_json: serde_json::json!({
                "type": "reasoning",
                "id": id,
                "encrypted_content": "opaque-blob",
            })
            .to_string(),
        }
    }

    /// The blob has to come back byte-for-byte: it is encrypted, and the only
    /// thing that can be done with it is hand it back.
    #[test]
    fn codex_reasoning_survives_storage_verbatim() {
        let mut acc = ProviderStateAccumulator::default();
        acc.apply(codex_item(0, "rs_1")).unwrap();
        acc.apply(codex_item(2, "rs_2")).unwrap();
        let state = acc.finish().unwrap();

        let json = state.to_storage_json().unwrap();
        let back = ProviderState::from_storage_json(&json).unwrap();
        assert_eq!(back, state);

        let items = back.codex_reasoning_for("gpt-5.6").unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].position, 0);
        assert_eq!(items[1].position, 2);
        assert!(items[0].item_json.contains("opaque-blob"));
    }

    /// Position is kept because replay order is not free to choose: reasoning
    /// and the calls it led to have to go back in the order they came out.
    #[test]
    fn out_of_order_arrival_keeps_each_items_own_position() {
        let mut acc = ProviderStateAccumulator::default();
        acc.apply(codex_item(3, "rs_late")).unwrap();
        acc.apply(codex_item(1, "rs_early")).unwrap();
        let state = acc.finish().unwrap();

        let items = state.codex_reasoning_for("gpt-5.6").unwrap();
        assert_eq!(items.iter().map(|i| i.position).collect::<Vec<_>>(), vec![3, 1]);
    }

    /// A repeat at the same index revises rather than duplicating — replaying an
    /// item twice would put two copies of the same reasoning into the next
    /// request.
    #[test]
    fn a_repeated_position_revises_rather_than_appends() {
        let mut acc = ProviderStateAccumulator::default();
        acc.apply(codex_item(0, "first")).unwrap();
        acc.apply(codex_item(0, "revised")).unwrap();
        let state = acc.finish().unwrap();

        let items = state.codex_reasoning_for("gpt-5.6").unwrap();
        assert_eq!(items.len(), 1);
        assert!(items[0].item_json.contains("revised"));
    }

    /// Reasoning from one model cannot be continued by another — the upstream
    /// would reject it, so the chain starts over instead.
    #[test]
    fn codex_reasoning_is_not_offered_to_another_model_or_protocol() {
        let mut acc = ProviderStateAccumulator::default();
        acc.apply(codex_item(0, "rs_1")).unwrap();
        let mut state = acc.finish().unwrap();

        assert!(state.codex_reasoning_for("gpt-5.6").is_some());
        assert!(state.codex_reasoning_for("gpt-5.4").is_none());

        // The same vendor over the ordinary Responses API stores its own
        // reasoning, so this state does not belong to it either.
        state.producer.protocol = "responses".into();
        assert!(state.codex_reasoning_for("gpt-5.6").is_none());
    }

    /// Two providers' state in one reply is a bug worth failing on, not merging.
    #[test]
    fn codex_reasoning_will_not_mix_with_another_vendors_state() {
        let mut acc = ProviderStateAccumulator::default();
        acc.apply(codex_item(0, "rs_1")).unwrap();
        let err = acc
            .apply(ProviderStateUpdate::AnthropicContentBlock {
                model: "claude".into(),
                position: 0,
                block_json: r#"{"type":"thinking","thinking":"","signature":"sig"}"#.into(),
            })
            .unwrap_err();
        assert!(err.contains("mixed incompatible"));
    }

    fn anthropic_block(position: usize, block_json: &str) -> ProviderStateUpdate {
        ProviderStateUpdate::AnthropicContentBlock {
            model: "claude-opus-5".into(),
            position,
            block_json: block_json.into(),
        }
    }

    /// Blocks go back in the order they came out, whatever order the stream
    /// announced them in, and survive the database verbatim.
    #[test]
    fn anthropic_blocks_round_trip_in_position_order() {
        let mut acc = ProviderStateAccumulator::default();
        acc.apply(anthropic_block(
            2,
            r#"{"type":"web_search_tool_result","tool_use_id":"srvtoolu_1","content":[]}"#,
        ))
        .unwrap();
        acc.apply(anthropic_block(
            0,
            r#"{"type":"thinking","thinking":"hm","signature":"sig"}"#,
        ))
        .unwrap();
        acc.apply(anthropic_block(
            1,
            r#"{"type":"server_tool_use","id":"srvtoolu_1","name":"web_search","input":{"query":"x"}}"#,
        ))
        .unwrap();
        let state = acc.finish().unwrap();

        let blocks = state.anthropic_blocks_for("claude-opus-5").unwrap();
        assert_eq!(blocks.iter().map(|b| b.position).collect::<Vec<_>>(), [0, 1, 2]);
        assert!(
            state.anthropic_blocks_for("claude-sonnet-5").is_none(),
            "signed for one model"
        );
        assert!(
            state.anthropic_signature_for("claude-opus-5").is_none(),
            "not the legacy shape"
        );

        let raw = state.to_storage_json().unwrap();
        assert!(raw.contains("anthropic_content_blocks"));
        assert_eq!(ProviderState::from_storage_json(&raw).unwrap(), state);
    }

    /// The signature-only rows written before blocks were kept still read.
    #[test]
    fn legacy_anthropic_signature_rows_still_read() {
        let raw = r#"{"version":1,"producer":{"vendor":"anthropic","protocol":"messages","model":"m"},"kind":"anthropic_thinking_signature","payload":{"signature":"sig"}}"#;
        let state = ProviderState::from_storage_json(raw).unwrap();
        assert_eq!(state.anthropic_signature_for("m"), Some("sig"));
        assert!(state.anthropic_blocks_for("m").is_none());
    }

    /// Storage refuses a producer that does not match the payload, so a
    /// hand-edited row cannot make one vendor's state look like another's.
    #[test]
    fn stored_codex_reasoning_must_name_its_own_producer() {
        let mut acc = ProviderStateAccumulator::default();
        acc.apply(codex_item(0, "rs_1")).unwrap();
        let mut state = acc.finish().unwrap();

        state.producer.vendor = "anthropic".into();
        assert!(state.to_storage_json().is_err());
    }

    /// An empty item says nothing and is not worth a row.
    #[test]
    fn an_empty_codex_item_is_ignored() {
        let mut acc = ProviderStateAccumulator::default();
        acc.apply(ProviderStateUpdate::CodexReasoningItem {
            model: "gpt-5.6".into(),
            position: 0,
            item_json: String::new(),
        })
        .unwrap();
        assert!(acc.finish().is_none());
    }
}
