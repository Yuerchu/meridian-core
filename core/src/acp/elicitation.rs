//! The agent asking a question, drawn as this app's own `ask_user` form.
//!
//! `elicitation/create` is how an ACP agent asks for typed input. Claude Code
//! reaches it by three routes — the built-in `AskUserQuestion` tool, an MCP
//! server's own elicitation, and the consent prompt shown when a model refuses
//! and a fallback is available — and all three arrive here as a JSON Schema to
//! render plus a `content` object to fill in.
//!
//! **Declaring the capability is not a nicety, it is the whole feature.** The
//! adapter builds `disallowedTools` from it at `session/new`
//! (`disallowedTools = elicitationSupport.form ? [] : ["AskUserQuestion"]`), so
//! a client that says nothing does not merely fail to draw a form: the tool is
//! withdrawn from the model, in subagents too, and what the user sees is Claude
//! reporting that asking questions is disabled in this session. That was the
//! bug this module exists to fix, and it is why
//! [`ClientCapabilities`](super::protocol::ClientCapabilities) has one field set
//! to true.
//!
//! **The form is translated into `ask_user`'s shape rather than given a UI of
//! its own.** The transcript already has a form with per-question options, a
//! free-text box, skip, and an answer path (`respond_to_ask`) that retires the
//! attention queue — and the shapes line up almost exactly, down to the "Other"
//! box, which upstream models as a companion field beside each select rather
//! than one field for the whole form. So the work here is a mapping in each
//! direction and no new component.
//!
//! Three things about the return trip are easy to get wrong, and they share a
//! shape: each produces a reply that is well formed, and wrong, with nothing
//! downstream to report it.
//!
//! **`const` is the answer, `title` is the drawing.** They are the same string
//! for `AskUserQuestion`, whose options are their own labels, and different for
//! the refusal-fallback prompt, whose `const`s are the CLI's wire values
//! (`retry_fallback` / `cancelled`). Sending back what the button said would
//! there be answering something other than what was picked.
//!
//! **An accept is all or nothing, so a required question is enforced on the
//! card.** Upstream validates the whole `content` against the schema it sent,
//! and the adapter's own comment is that a malformed accept "yields empty
//! content" — not the offending field dropped, every field dropped. So a form
//! answered everywhere except in the one place the agent insisted on does not
//! lose that answer, it loses all of them.
//!
//! That is why `required` reaches the card (`questions_json`), which withholds
//! the skip button and the submit button until it is answered. The check here
//! is a backstop and cannot be the mechanism: by the time `content_from` sees
//! the answers, the user has been told the form was sent and the queue entry
//! has been retired. A required field this app cannot *draw* is refused at
//! `parse`, before anybody fills in the rest of it for nothing.
//!
//! **Which makes "answered" a schema question, not a "did they type anything"
//! one.** A free-text box beside an enumerated question is a *different
//! property* — the adapter's companion field — so filling it in leaves the
//! required one missing all the same, and the card holds a required enumerated
//! question to a selection. Where there is no companion the box is withheld
//! outright (`accepts_text`), since the value has to be one of the `const`s and
//! typed text there could only be an accept the asker rejects.
//!
//! And it makes **one string carry two actions**, which has to be undone on
//! the way back. `formatAnswer` joins a selection to the note beside it with
//! `NOTE_SEPARATOR`; read as free text the pair goes only to the companion, so
//! a required question answered *from its own list* arrives missing and the
//! whole form is discarded over an answer that was given. Both places are
//! filled instead — the `const` in the property, the whole string in the
//! companion, since upstream reads a companion answer as replacing the
//! selection and the note alone would report the annotation as the answer.
//!
//! **That separator is a candidate boundary, not a landmark**, because an
//! option's label is model-written text this app never constrained and may
//! contain one. Cut at the first occurrence, such a label picked on its own is
//! severed at a boundary that was never there: the prefix names no option and
//! the required field arrives missing, or — worse — names a *different* option
//! and submits a choice nobody made. So `read_answer` tries every occurrence
//! along with the whole string and takes the answer only if one reading
//! resolves, which is the rule the multi-select below is read under too.
//!
//! **A joined multi-select is reversed only where it has one reading.** The
//! card joins the labels with `", "`, and `"A, B"` against options `"A, B"`,
//! `"A"` and `"B"` means either the first picked alone or the other two picked
//! together — a string that resolves confidently to a selection nobody made.
//! Naively splitting fails safely on its own, being all-or-nothing; this is the
//! case that does not. **The ambiguity is a property of the answer, not of the
//! field**: refusing the whole field because *some* label contains the
//! separator also costs `"C"`, which has one reading and nothing to be unsure
//! about.
//!
//! **A form with no `toolCallId` cannot be drawn, so it is declined rather than
//! parked.** The card an answer would be typed into is the tool call's own, and
//! MCP-originated elicitations carry no call id. Registering the question
//! anyway lights the attention dot for a form that exists nowhere, and the agent
//! waits for an answer no one can give. `decline` is the protocol's word for
//! "the user passed on this", which is what has effectively happened.

use tokio::sync::oneshot;

use crate::agent::engine::{self, ApprovalDecision};
use crate::db::models::turn::TurnPhase;
use crate::events::ChatStreamEvent;
use crate::services::Services;
use crate::state::PendingApproval;

use super::approvals::TurnContext;
use super::protocol::{self, CreateElicitationParams, ElicitationSchema, EnumOption, PropertySchema};

/// The name the card is drawn under.
///
/// Deliberately this app's own tool name and not `AskUserQuestion`: three
/// places key off it — `kind: 'ask'` in the attention queue, `pendingAsks`
/// rather than `pendingApprovals`, and the toast offering a way in instead of
/// two buttons — and all three are right for a form and wrong for a permission.
/// The transcript card keeps the agent's name for the call, which is what it
/// actually ran.
const ASK_TOOL: &str = "ask_user";

/// Sentinels [`formatAnswer`](../../../../src/components/chat/tool-call-block.tsx)
/// writes for a question that was passed over. They are answers to nothing and
/// must not be sent as though the user had typed them.
const SKIPPED: [&str; 2] = ["(skipped)", "(no answer)"];

/// What the same function puts between a selection and the note beside it.
///
/// Read here for the same reason the sentinels above are: the card answers a
/// question with one string, and this app has to know how it built it. Two
/// actions went into that string and the schema has a separate property for
/// each, so it is taken apart again — see `content_from`.
const NOTE_SEPARATOR: &str = "\n\nNotes: ";

/// One question, with everything needed to read its answer back.
struct Field {
    /// The schema property this question is, which is also the id the form
    /// sends its answer back under.
    key: String,
    question: String,
    /// `(title, const)` per choice, in the order they were offered. Empty for a
    /// free-text field.
    options: Vec<(String, serde_json::Value)>,
    descriptions: Vec<Option<String>>,
    multi: bool,
    /// The companion free-text property, when the schema declared one. A typed
    /// answer goes here and upstream reads it as replacing the selection.
    custom_key: Option<String>,
    /// Whether the field takes a bare string, which decides whether a typed
    /// answer can be sent at all when there is no companion.
    text_takes_strings: bool,
    /// Whether the agent refuses an answer without this one, which the card is
    /// told so it can stop somebody submitting a form that would be thrown
    /// away.
    required: bool,
}

/// A parsed form: what to draw, and how to read the answers.
pub struct Form {
    fields: Vec<Field>,
    /// Properties the agent will not take an answer without.
    ///
    /// Read rather than ignored, because upstream validates the whole `content`
    /// against the schema it sent (`CreateElicitationResponse.isAccept`, which
    /// is a zod `safeParse`) and a payload that fails is not partially kept —
    /// the adapter's own comment says a malformed accept "yields empty
    /// content". So an accept missing a required field does not lose that
    /// field, it loses *every* answer on the form, silently.
    required: Vec<String>,
}

impl Form {
    /// Read the schema, or nothing when it describes no question this app can
    /// draw.
    ///
    /// `message` is folded into the single-question case because that is where
    /// the adapter puts the question text when there is only one — the field's
    /// own `description` is left empty precisely so the text appears once.
    fn parse(schema: &ElicitationSchema, message: Option<&str>) -> Option<Self> {
        // Companion boxes first: they are questions in the schema and not
        // questions to a reader, so they have to be known before the pass that
        // would otherwise draw each one as an empty free-text prompt.
        let mut companions: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
        for (key, prop) in &schema.properties.0 {
            if let Some(owner) = prop
                .meta
                .as_ref()
                .and_then(|m| m.custom_answer.as_ref())
                .and_then(|c| c.question_id.as_deref())
            {
                companions.insert(owner, key.as_str());
            }
        }
        let is_companion: std::collections::HashSet<&str> = companions.values().copied().collect();

        let real: Vec<&(String, PropertySchema)> = schema
            .properties
            .0
            .iter()
            .filter(|(key, _)| !is_companion.contains(key.as_str()))
            .collect();
        let single = real.len() == 1;

        let mut fields = Vec::new();
        for (key, prop) in real {
            let options = choices_of(prop);
            let multi = prop.ty.as_deref() == Some("array");
            // A field with neither choices nor a string type is a number, a
            // boolean or something this form cannot express. Guessing at it
            // would send the agent a string where it asked for a count.
            let text_takes_strings = prop.ty.as_deref() == Some("string");
            if options.is_empty() && !text_takes_strings {
                tracing::debug!(
                    key,
                    ty = prop.ty.as_deref(),
                    "skipping an elicitation field this form cannot draw"
                );
                continue;
            }
            let question = prop
                .description
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .or_else(|| {
                    single
                        .then_some(message)
                        .flatten()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                })
                .or_else(|| prop.title.as_deref().map(str::trim).filter(|s| !s.is_empty()))
                .unwrap_or(key.as_str())
                .to_string();

            fields.push(Field {
                required: schema.required.iter().any(|r| r == key),
                key: key.clone(),
                question,
                descriptions: options.iter().map(|o| o.description.clone()).collect(),
                options: options.iter().map(|o| (label_of(o), o.value.clone())).collect(),
                multi,
                custom_key: companions.get(key.as_str()).map(|s| s.to_string()),
                text_takes_strings,
            });
        }

        if fields.is_empty() {
            return None;
        }
        // A required field this form cannot draw makes the whole form
        // unanswerable: there is no reply that would satisfy the schema, so
        // every answer typed into it would be thrown away on arrival. Declining
        // now costs the agent the same and costs the user nothing.
        if let Some(missing) = schema
            .required
            .iter()
            .find(|key| !fields.iter().any(|field| field.key == **key))
        {
            tracing::warn!(key = %missing, "an elicitation requires a field this form cannot draw");
            return None;
        }

        Some(Self {
            fields,
            required: schema.required.clone(),
        })
    }

    /// The form as `ask_user` arguments, which is what the card renders from.
    fn questions_json(&self) -> serde_json::Value {
        let questions: Vec<serde_json::Value> = self
            .fields
            .iter()
            .map(|f| {
                let mut q = serde_json::json!({ "id": f.key, "question": f.question });
                if !f.options.is_empty() {
                    let options: Vec<serde_json::Value> = f
                        .options
                        .iter()
                        .zip(&f.descriptions)
                        .map(|((label, _), description)| match description {
                            Some(d) => serde_json::json!({ "label": label, "description": d }),
                            None => serde_json::json!({ "label": label }),
                        })
                        .collect();
                    q["options"] = serde_json::Value::Array(options);
                }
                if f.multi {
                    q["multi_select"] = serde_json::Value::Bool(true);
                }
                // The card withholds "skip" and the submit button for these.
                // Without it somebody fills a form in, is told it was sent, and
                // the whole of it is declined on the way out — see the header.
                if f.required {
                    q["required"] = serde_json::Value::Bool(true);
                }
                // And it withholds the free-text box where nothing could carry
                // what was typed. Sent anyway that text does not merely go
                // nowhere: for a question already answered from the list it
                // *replaces* the selection in what the card submits, turning a
                // valid answer into an unplaceable string. Absent means yes,
                // which is what this app's own `ask_user` questions are.
                if !f.options.is_empty() && f.custom_key.is_none() {
                    q["accepts_text"] = serde_json::Value::Bool(false);
                }
                // And it withholds the free-text box where nothing could carry
                // what was typed. Sent anyway that text does not merely go
                // nowhere: for a question already answered from the list it
                // *replaces* the selection in what the card submits, turning a
                // valid answer into an unplaceable string. Absent means yes,
                // which is what this app's own `ask_user` questions are.
                q
            })
            .collect();
        serde_json::json!({ "questions": questions })
    }

    /// Fold the form's answers back into an elicitation `content` object.
    ///
    /// Returns nothing when there is no accept worth sending — every question
    /// passed over, or a required one that could not be answered. An empty
    /// `content` and a decline mean the same thing to the agent, and the second
    /// says it in the protocol's own vocabulary.
    fn content_from(&self, answers: &serde_json::Value) -> Option<serde_json::Value> {
        let mut content = serde_json::Map::new();
        for field in &self.fields {
            let Some(raw) = answers.get(&field.key).and_then(|v| v.as_str()).map(str::trim) else {
                continue;
            };
            if raw.is_empty() || SKIPPED.contains(&raw) {
                continue;
            }
            // The card sends one string per question and the schema has two
            // places to put it, so a selection annotated with a note is taken
            // apart again rather than treated as free text. Both halves are
            // wanted: without the `const` in the property itself, a *required*
            // enumerated question answered from its own list still arrives
            // missing, and the whole form is discarded over an answer that was
            // given.
            match field.read_answer(raw) {
                Some(answer) => {
                    content.insert(field.key.clone(), answer.selection);
                    // And the note beside it, whole rather than on its own:
                    // upstream reads a companion answer as *the* answer, and
                    // the whole string is what this app's own `ask_user` would
                    // have recorded for the same two actions.
                    if answer.annotated {
                        match &field.custom_key {
                            Some(custom) => {
                                content.insert(custom.clone(), serde_json::Value::String(raw.to_string()));
                            }
                            // Unreachable from the card, which withholds the
                            // box in exactly this case (`accepts_text`). The
                            // selection is still sent; the note is what is lost.
                            None => tracing::warn!(
                                key = %field.key,
                                "a note beside a selection had nowhere to go and was not sent"
                            ),
                        }
                    }
                }
                // Typed instead of picked, which is what the companion box is
                // for: an answer the options did not cover.
                None => match &field.custom_key {
                    Some(custom) => {
                        content.insert(custom.clone(), serde_json::Value::String(raw.to_string()));
                    }
                    // A field with no options takes whatever was typed, because
                    // the string *is* the value it asked for. **One with
                    // options does not**, even when its type is `string`: the
                    // value has to be one of the `const`s, so putting free text
                    // there is an accept the schema rejects — and a rejected
                    // accept is not that field dropped, it is the whole form
                    // dropped. `accepts_text` is the card's half of this.
                    None if field.text_takes_strings && field.options.is_empty() => {
                        content.insert(field.key.clone(), serde_json::Value::String(raw.to_string()));
                    }
                    // Answered, with nowhere the schema would accept it: an
                    // enumerated field this string does not name a choice in,
                    // and no companion box to put it in verbatim. Warned rather
                    // than logged at debug, because somebody typed this and it
                    // is not being sent.
                    None => {
                        tracing::warn!(
                            key = %field.key,
                            "an answer had nowhere to go in this elicitation form and was not sent"
                        );
                    }
                },
            }
        }

        // Upstream validates the whole payload, so an accept missing a required
        // field is not a partial answer — it is every answer on the form
        // discarded, with nothing said. Decline instead: the agent hears "the
        // user passed", which is true of the question it insisted on.
        if let Some(missing) = self.required.iter().find(|key| !content.contains_key(key.as_str())) {
            tracing::warn!(key = %missing, "an ACP elicitation went unanswered in a field it requires");
            return None;
        }

        (!content.is_empty()).then_some(serde_json::Value::Object(content))
    }
}

/// What a question's one string turned out to say.
struct Answer {
    /// The `const` value(s) picked from the list.
    selection: serde_json::Value,
    /// Whether a note was typed beside the selection, in which case the whole
    /// string also belongs in the companion box.
    annotated: bool,
}

impl Field {
    /// Read a card's answer as a selection, and a note beside it if there was
    /// one — where the string says so in exactly one way.
    ///
    /// **The separator is not a landmark, it is a candidate.** `NOTE_SEPARATOR`
    /// can occur inside an option's own label, which is model-written text this
    /// app never constrained: a label carrying one, picked with nothing typed
    /// beside it, is cut at a boundary that was never a boundary. The prefix
    /// then matches no option — so a *required* question answered from its own
    /// list arrives missing and the whole form is declined — or worse, matches
    /// a different option and submits a choice the user did not make.
    ///
    /// So every occurrence is tried, along with the string as a whole, and the
    /// answer is taken only if exactly one of those readings resolves. This is
    /// the same rule the joined multi-select is read under, for the same
    /// reason: what separates is decided by the options, not by the punctuation.
    fn read_answer(&self, raw: &str) -> Option<Answer> {
        let mut found: Option<Answer> = None;
        let mut consider = |selection: Option<serde_json::Value>, annotated: bool| -> bool {
            let Some(selection) = selection else {
                return true;
            };
            if found.is_some() {
                // A second reading. Neither may be taken, and no later one can
                // change that.
                found = None;
                return false;
            }
            found = Some(Answer { selection, annotated });
            true
        };

        // The whole string, picked with nothing typed beside it.
        if !consider(self.selection_for(raw), false) {
            return None;
        }
        // Or a selection, a separator, and a note.
        for (at, _) in raw.match_indices(NOTE_SEPARATOR) {
            if !consider(self.selection_for(&raw[..at]), true) {
                return None;
            }
        }
        found
    }

    /// The `const` value(s) this answer names, when it names them exactly and
    /// in exactly one way.
    ///
    /// Exact matching only. An answer that merely contains an option's label is
    /// not that selection, and treating it as one would drop the rest silently.
    fn selection_for(&self, raw: &str) -> Option<serde_json::Value> {
        if self.options.is_empty() {
            return None;
        }
        if !self.multi {
            return self
                .options
                .iter()
                .find(|(label, _)| label == raw)
                .map(|(_, value)| value.clone());
        }
        // A multi-select arrives joined with `", "`, and the join is reversible
        // only where it has one reading. `"A, B"` against options `"A, B"`,
        // `"A"` and `"B"` has two — the one option picked alone, or the other
        // two picked together — and nothing in the string says which, so it is
        // left for the companion box rather than answered confidently and
        // wrongly.
        //
        // **The ambiguity is a property of the answer, not of the field.** An
        // earlier version refused to split any field where *some* label
        // contained the separator, which also cost `"C"` — one plain label,
        // one reading, nothing to be unsure about — and, with options `"A, B"`
        // and `"C"`, cost the perfectly unambiguous `"A, B, C"` as well. Only
        // the string in hand can say whether it is ambiguous.
        let mut readings = Vec::new();
        self.read_joined(raw, &mut Vec::new(), &mut readings);
        match readings.len() {
            1 => Some(serde_json::Value::Array(readings.remove(0))),
            _ => None,
        }
    }

    /// Every way `rest` can be read as this field's labels joined with `", "`.
    ///
    /// Depth-first over the labels rather than a split, because a separator
    /// inside a label means the two are not the same question. Stops as soon as
    /// a second reading exists: past that the count no longer matters, and the
    /// bound is what keeps a pathological option list from being exponential.
    fn read_joined(&self, rest: &str, picked: &mut Vec<serde_json::Value>, out: &mut Vec<Vec<serde_json::Value>>) {
        if out.len() > 1 {
            return;
        }
        for (label, value) in &self.options {
            let Some(tail) = rest.strip_prefix(label.as_str()) else {
                continue;
            };
            picked.push(value.clone());
            if tail.is_empty() {
                out.push(picked.clone());
            } else if let Some(tail) = tail.strip_prefix(", ") {
                self.read_joined(tail, picked, out);
            }
            picked.pop();
            if out.len() > 1 {
                return;
            }
        }
    }
}

/// What a choice is called on screen. `const` stands in for a missing `title`
/// because a button has to say something, and for `AskUserQuestion` the two are
/// the same string anyway.
fn label_of(option: &EnumOption) -> String {
    option
        .title
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| match &option.value {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        })
}

/// A field's choices, from whichever of the two places the schema puts them.
fn choices_of(prop: &PropertySchema) -> Vec<&EnumOption> {
    if !prop.one_of.is_empty() {
        return prop.one_of.iter().collect();
    }
    prop.items
        .as_ref()
        .map(|i| i.any_of.iter().collect())
        .unwrap_or_default()
}

/// Everything the form needs out of the request, or the reason there is nothing
/// to draw.
///
/// Consumes the request rather than borrowing from it, so that nothing about
/// the wire shape is still alive across the wait that follows — which is a
/// person's, and minutes long.
fn prepare(params: CreateElicitationParams) -> Result<(Form, String), &'static str> {
    if params.mode.as_deref() != Some("form") {
        return Err("this client advertised no such elicitation mode");
    }
    // The card an answer is typed into is the tool call's own. Without an id
    // there is no card, and registering the question would light the sidebar
    // for a form that exists nowhere.
    let call_id = params.tool_call_id.ok_or("there is no tool call to draw it on")?;
    let schema = params.requested_schema.ok_or("it described no form")?;
    let form = Form::parse(&schema, params.message.as_deref()).ok_or("it described no question this app can draw")?;
    Ok((form, call_id))
}

/// Put an `elicitation/create` in front of the user and wait.
///
/// Runtime uncertainty returns a valid reply, for the same reason
/// [`approvals::ask`](super::approvals::ask) does. A broken first-party approval
/// preference is returned as an error rather than silently changing the TTL.
///
/// **The uncertain cases resolve to `decline`, not `cancel`, and that is the
/// opposite of the permission path.** A permission that cannot be asked about
/// must not be granted, so that one cancels. A *question* that cannot be asked
/// is one the agent should carry on without: `cancel` aborts the tool call and
/// takes the turn with it, over a form nobody could have filled in. `cancel` is
/// kept for the one case that means it — the turn ended while the form was on
/// screen, so there is nothing left to answer into.
pub async fn ask(
    services: &Services,
    conversation_id: &str,
    turn: &TurnContext,
    params: CreateElicitationParams,
) -> Result<serde_json::Value, String> {
    let (form, call_id) = match prepare(params) {
        Ok(ready) => ready,
        Err(why) => {
            tracing::warn!(why, "could not draw an ACP elicitation");
            return Ok(protocol::elicitation_declined());
        }
    };

    let approval_id = uuid::Uuid::new_v4().to_string();
    let (tx, rx) = oneshot::channel();
    let arguments = form.questions_json().to_string();

    // Worked out once, here, rather than by the waiter — see the note in
    // `acp::approvals::ask`.
    let ttl = crate::approval::ttl(services)?;

    // Registered before the event goes out, so an answer cannot arrive before
    // there is somewhere to put it.
    services.approvals.lock().insert(
        approval_id.clone(),
        PendingApproval {
            conversation_id: conversation_id.to_string(),
            turn_id: turn.turn_id.clone(),
            assistant_message_id: turn.assistant_message_id.clone(),
            provider_call_id: call_id.clone(),
            origin_call_id: None,
            tool_name: ASK_TOOL.to_string(),
            arguments: arguments.clone(),
            retry_reason: None,
            bubble: None,
            expires_at: ttl.map(|ttl| std::time::Instant::now() + ttl),
            sender: tx,
        },
    );

    let event = ChatStreamEvent::ToolApprovalReq {
        approval_id: approval_id.clone(),
        call_id,
        tool_name: ASK_TOOL.to_string(),
        arguments,
        message_id: turn.assistant_message_id.clone(),
        conversation_id: conversation_id.to_string(),
        delegation: None,
        retry: None,
    };
    if let Err(e) = services.events.emit_chat(event) {
        services.approvals.claim(&approval_id);
        tracing::warn!(error = %e, "could not draw an ACP elicitation form");
        return Ok(protocol::elicitation_declined());
    }

    let pool = services.db.clone();
    let decision = engine::in_phase(
        &pool,
        &turn.turn_id,
        TurnPhase::AwaitingApproval,
        Some(ASK_TOOL),
        crate::approval::wait(services, &approval_id, rx, &turn.cancel, ttl),
    )
    .await;

    Ok(match decision {
        Some(ApprovalDecision::Response(answers)) => match serde_json::from_str(&answers) {
            Ok(answers) => match form.content_from(&answers) {
                Some(content) => protocol::elicitation_accepted(content),
                None => protocol::elicitation_declined(),
            },
            Err(e) => {
                tracing::warn!(error = %e, "could not read the answers to an ACP elicitation");
                protocol::elicitation_declined()
            }
        },
        // Neither button belongs to this card — it draws a form, not an
        // approval — but the register they come from is shared, and a decision
        // that arrives is one somebody made. Both read as "no answer given",
        // which is what `decline` says.
        Some(ApprovalDecision::Approved) | Some(ApprovalDecision::Denied(_)) => protocol::elicitation_declined(),
        // The turn ended under the form. Nothing is owed and nothing can be
        // answered into it.
        None => protocol::elicitation_cancelled(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape `askUserQuestionsToCreateRequest` produces, as it produces it:
    /// a select followed by its own companion box, per question.
    fn ask_user_schema(multi: bool) -> ElicitationSchema {
        let select = if multi {
            serde_json::json!({
                "type": "array",
                "title": "Scope",
                "items": { "anyOf": [
                    { "const": "Just the parser", "title": "Just the parser", "description": "Smallest change" },
                    { "const": "Parser and the tests", "title": "Parser and the tests" },
                ]},
            })
        } else {
            serde_json::json!({
                "type": "string",
                "title": "Scope",
                "oneOf": [
                    { "const": "Just the parser", "title": "Just the parser", "description": "Smallest change" },
                    { "const": "Parser and the tests", "title": "Parser and the tests" },
                ],
            })
        };
        serde_json::from_value(serde_json::json!({
            "type": "object",
            "properties": {
                "question_0": select,
                "question_0_custom": {
                    "type": "string",
                    "title": "Other",
                    "description": "Type your own answer instead of choosing an option above (optional).",
                    "_meta": { "_askUserQuestionCustomAnswer": { "questionId": "question_0", "isCustomAnswer": true } },
                },
            },
        }))
        .unwrap()
    }

    /// The companion box is part of the question above it, not a question. Drawn
    /// as one it becomes an empty prompt reading "Type your own answer instead
    /// of choosing an option above" with no options anywhere near it.
    #[test]
    fn the_other_box_is_not_a_question_of_its_own() {
        let form = Form::parse(&ask_user_schema(false), Some("Which files should change?")).unwrap();
        assert_eq!(form.fields.len(), 1);
        assert_eq!(form.fields[0].custom_key.as_deref(), Some("question_0_custom"));

        let drawn = form.questions_json();
        let questions = drawn["questions"].as_array().unwrap();
        assert_eq!(questions.len(), 1);
        // With one question the text is in `message` — the field's own
        // description is left empty so it is not said twice.
        assert_eq!(questions[0]["question"], "Which files should change?");
        assert_eq!(questions[0]["options"].as_array().unwrap().len(), 2);
    }

    /// A clean pick answers the select. Nothing goes in the companion box,
    /// which upstream reads as *replacing* the selection.
    #[test]
    fn a_chosen_option_answers_the_field_it_belongs_to() {
        let form = Form::parse(&ask_user_schema(false), Some("Which?")).unwrap();
        let answers = serde_json::json!({ "question_0": "Just the parser" });
        let content = form.content_from(&answers).unwrap();
        assert_eq!(content["question_0"], "Just the parser");
        assert!(content.get("question_0_custom").is_none());
    }

    /// A note beside a selection arrives as one string standing for two
    /// actions, and the schema has a property for each — so both are filled.
    ///
    /// The whole string goes in the companion rather than just the note,
    /// because upstream reads a companion answer as replacing the selection:
    /// the note alone would report the annotation as the answer and lose what
    /// was picked. And the `const` goes in the property itself, because without
    /// it a *required* question answered from its own list still arrives
    /// missing — see the test below.
    #[test]
    fn a_selection_with_a_note_fills_both_places_the_schema_has() {
        let form = Form::parse(&ask_user_schema(false), Some("Which?")).unwrap();
        let answers = serde_json::json!({ "question_0": "Just the parser\n\nNotes: keep the old entry point" });
        let content = form.content_from(&answers).unwrap();
        assert_eq!(content["question_0"], "Just the parser");
        assert_eq!(
            content["question_0_custom"],
            "Just the parser\n\nNotes: keep the old entry point"
        );
    }

    /// A required enumerated question whose option labels contain the note
    /// separator — which they may, being model-written text this app never
    /// constrained.
    fn separator_in_the_labels(required: bool) -> ElicitationSchema {
        serde_json::from_value(serde_json::json!({
            "type": "object",
            "required": if required { vec!["question_0"] } else { Vec::new() },
            "properties": {
                "question_0": {
                    "type": "string",
                    "oneOf": [
                        { "const": "keep", "title": "Keep it\n\nNotes: see the RFC" },
                        { "const": "drop", "title": "Drop it" },
                    ],
                },
                "question_0_custom": {
                    "type": "string",
                    "title": "Other",
                    "_meta": { "_askUserQuestionCustomAnswer": { "questionId": "question_0" } },
                },
            },
        }))
        .unwrap()
    }

    /// Picking a label that contains the separator, with nothing typed beside
    /// it, is still that selection.
    ///
    /// Cut at the first occurrence instead, the prefix names no option — so the
    /// required field arrives missing and the whole form is declined, after the
    /// card has reported success. The separator is a candidate boundary, not a
    /// landmark.
    #[test]
    fn a_label_containing_the_note_separator_is_still_picked() {
        let form = Form::parse(&separator_in_the_labels(true), None).unwrap();
        let reply = match form.content_from(&serde_json::json!({
            "question_0": "Keep it\n\nNotes: see the RFC"
        })) {
            Some(content) => protocol::elicitation_accepted(content),
            None => protocol::elicitation_declined(),
        };
        assert_eq!(reply["action"], "accept", "the form was declined: {reply}");
        assert_eq!(reply["content"]["question_0"], "keep");
        assert!(reply["content"].get("question_0_custom").is_none());
    }

    /// And annotating that same option still fills both places: the reading
    /// that resolves is the one cutting at the *second* occurrence, which is
    /// the only one whose prefix names an option.
    #[test]
    fn a_label_containing_the_separator_can_still_be_annotated() {
        let form = Form::parse(&separator_in_the_labels(true), None).unwrap();
        let raw = "Keep it\n\nNotes: see the RFC\n\nNotes: and the migration";
        let content = form.content_from(&serde_json::json!({ "question_0": raw })).unwrap();
        assert_eq!(content["question_0"], "keep");
        assert_eq!(content["question_0_custom"], raw);
    }

    /// Where two readings resolve, neither is taken. Here `"Drop it"` is an
    /// option *and* the prefix of a longer label that is also one, so the string
    /// says both "picked the long one" and "picked the short one and wrote a
    /// note" — and the second reading is the one that would submit a choice
    /// nobody made.
    #[test]
    fn an_answer_that_reads_two_ways_is_not_resolved_into_either() {
        let schema: ElicitationSchema = serde_json::from_str(
            r#"{"type":"object","properties":{
                "question_0":{"type":"string","oneOf":[
                    {"const":"long","title":"Drop it\n\nNotes: it is unused"},
                    {"const":"short","title":"Drop it"}
                ]},
                "question_0_custom":{"type":"string","title":"Other",
                    "_meta":{"_askUserQuestionCustomAnswer":{"questionId":"question_0"}}}
            }}"#,
        )
        .unwrap();
        let form = Form::parse(&schema, None).unwrap();
        let raw = "Drop it\n\nNotes: it is unused";
        let content = form.content_from(&serde_json::json!({ "question_0": raw })).unwrap();
        assert!(
            content.get("question_0").is_none(),
            "an ambiguous answer was resolved into a selection: {content:?}"
        );
        assert_eq!(content["question_0_custom"], raw);
    }

    /// The whole return trip for the shape that broke it: a required question,
    /// answered from its list, with a note typed beside it.
    ///
    /// The card sends those two actions as one string. Read as free text it
    /// goes only to the companion — which is a *different property* — leaving
    /// the required one missing, so the accept is refused and every answer on
    /// the form is discarded after the card has already reported success. The
    /// reply here is what the agent has to receive for that not to happen.
    #[test]
    fn a_required_question_answered_with_a_note_beside_it_is_still_accepted() {
        let mut schema = ask_user_schema(false);
        schema.required = vec!["question_0".into()];
        let form = Form::parse(&schema, Some("Which?")).unwrap();

        let reply = match form.content_from(&serde_json::json!({
            "question_0": "Just the parser\n\nNotes: keep the old entry point"
        })) {
            Some(content) => protocol::elicitation_accepted(content),
            None => protocol::elicitation_declined(),
        };
        assert_eq!(reply["action"], "accept", "the form was declined: {reply}");
        assert_eq!(reply["content"]["question_0"], "Just the parser");
        assert_eq!(
            reply["content"]["question_0_custom"],
            "Just the parser\n\nNotes: keep the old entry point"
        );
    }

    /// A multi-select answers with an array; the card joined it with `", "` and
    /// this is the other half of that.
    #[test]
    fn a_multi_select_comes_back_as_a_list() {
        let form = Form::parse(&ask_user_schema(true), Some("Which?")).unwrap();
        assert!(form.fields[0].multi);
        let answers = serde_json::json!({ "question_0": "Just the parser, Parser and the tests" });
        let content = form.content_from(&answers).unwrap();
        assert_eq!(
            content["question_0"],
            serde_json::json!(["Just the parser", "Parser and the tests"])
        );
    }

    /// `const` is what goes back, even when the button said something else.
    /// This is the refusal-fallback prompt's shape, where the two differ and a
    /// title sent in a `const`'s place is a well-formed wrong answer.
    #[test]
    fn the_value_sent_back_is_the_const_and_not_the_label() {
        let schema: ElicitationSchema = serde_json::from_value(serde_json::json!({
            "type": "object",
            "properties": {
                "choice": {
                    "type": "string",
                    "oneOf": [
                        { "const": "retry_fallback", "title": "Retry with claude-opus-5" },
                        { "const": "cancelled", "title": "Keep the refusal" },
                    ],
                },
            },
        }))
        .unwrap();
        let form = Form::parse(&schema, Some("Retry?")).unwrap();
        let drawn = form.questions_json();
        assert_eq!(drawn["questions"][0]["options"][0]["label"], "Retry with claude-opus-5");

        let content = form
            .content_from(&serde_json::json!({ "choice": "Retry with claude-opus-5" }))
            .unwrap();
        assert_eq!(content["choice"], "retry_fallback");
    }

    /// Skipping every question is a decline, not an accept carrying nothing.
    /// The sentinels are what the card writes for a question passed over, and
    /// sending them verbatim would have the model read "(skipped)" as an answer.
    #[test]
    fn a_form_nobody_filled_in_answers_nothing() {
        let form = Form::parse(&ask_user_schema(false), Some("Which?")).unwrap();
        assert!(
            form.content_from(&serde_json::json!({ "question_0": "(skipped)" }))
                .is_none()
        );
        assert!(
            form.content_from(&serde_json::json!({ "question_0": "(no answer)" }))
                .is_none()
        );
        assert!(form.content_from(&serde_json::json!({ "question_0": "   " })).is_none());
        assert!(form.content_from(&serde_json::json!({})).is_none());
    }

    /// Several questions keep the order the schema listed them in — which is
    /// why `properties` is an `IndexMap`. Sorted, `question_10` lands between
    /// `question_1` and `question_2`.
    #[test]
    fn the_questions_keep_the_order_they_arrived_in() {
        // Parsed from text rather than built through `serde_json::json!`, whose
        // own map would sort the fixture before the code under test ever sees
        // it — and the test would pass for the wrong reason.
        let schema: ElicitationSchema = serde_json::from_str(
            r#"{"type":"object","properties":{
                "question_0":{"type":"string","description":"Q0"},
                "question_10":{"type":"string","description":"Q10"},
                "question_1":{"type":"string","description":"Q1"},
                "question_2":{"type":"string","description":"Q2"}
            }}"#,
        )
        .unwrap();
        let form = Form::parse(&schema, None).unwrap();
        let keys: Vec<&str> = form.fields.iter().map(|f| f.key.as_str()).collect();
        assert_eq!(keys, ["question_0", "question_10", "question_1", "question_2"]);
    }

    /// With more than one question the text is on each field, and `message` is
    /// upstream's own filler ("Please answer the following questions.") — using
    /// it would replace every question with that sentence.
    #[test]
    fn each_question_of_several_carries_its_own_text() {
        let schema: ElicitationSchema = serde_json::from_str(
            r#"{"type":"object","properties":{
                "question_0":{"type":"string","title":"Scope","description":"How far does this go?"},
                "question_1":{"type":"string","title":"Naming","description":"Rename the columns?"}
            }}"#,
        )
        .unwrap();
        let form = Form::parse(&schema, Some("Please answer the following questions.")).unwrap();
        let drawn = form.questions_json();
        assert_eq!(drawn["questions"][0]["question"], "How far does this go?");
        assert_eq!(drawn["questions"][1]["question"], "Rename the columns?");
    }

    /// A field that is neither a string nor a set of choices — a number, a
    /// boolean — has no control on this form. It is left out rather than drawn
    /// as a text box that would send the agent a string where it asked for a
    /// count.
    #[test]
    fn a_field_this_form_cannot_draw_is_left_out() {
        let schema: ElicitationSchema = serde_json::from_str(
            r#"{"type":"object","properties":{
                "count":{"type":"integer","description":"How many?"},
                "why":{"type":"string","description":"Why?"}
            }}"#,
        )
        .unwrap();
        let form = Form::parse(&schema, None).unwrap();
        let keys: Vec<&str> = form.fields.iter().map(|f| f.key.as_str()).collect();
        assert_eq!(keys, ["why"]);

        // And a schema with nothing drawable in it at all is not a form.
        let none: ElicitationSchema =
            serde_json::from_str(r#"{"type":"object","properties":{"count":{"type":"integer"}}}"#).unwrap();
        assert!(Form::parse(&none, None).is_none());
    }

    /// Free text with no options behind it answers its own field: there is no
    /// companion box on an MCP server's schema, and nothing to match against.
    #[test]
    fn a_plain_text_field_answers_itself() {
        let schema: ElicitationSchema = serde_json::from_str(
            r#"{"type":"object","properties":{"branch":{"type":"string","description":"Which branch?"}}}"#,
        )
        .unwrap();
        let form = Form::parse(&schema, None).unwrap();
        let content = form
            .content_from(&serde_json::json!({ "branch": "release/2.1" }))
            .unwrap();
        assert_eq!(content["branch"], "release/2.1");
    }

    /// A required question left blank is a decline, not an accept carrying the
    /// rest. Upstream validates the whole payload — a malformed accept yields
    /// *empty* content there — so sending one loses every other answer on the
    /// form as well, and says nothing about it.
    #[test]
    fn an_accept_is_never_sent_missing_a_required_answer() {
        let schema: ElicitationSchema = serde_json::from_str(
            r#"{"type":"object","required":["branch"],"properties":{
                "branch":{"type":"string","description":"Which branch?"},
                "note":{"type":"string","description":"Anything else?"}
            }}"#,
        )
        .unwrap();
        let form = Form::parse(&schema, None).unwrap();

        // The optional half answered and the required one skipped: nothing an
        // accept could legally carry.
        assert!(
            form.content_from(&serde_json::json!({ "branch": "(skipped)", "note": "be careful" }))
                .is_none()
        );
        // Answered, and it goes through with the optional field beside it.
        let content = form
            .content_from(&serde_json::json!({ "branch": "main", "note": "be careful" }))
            .unwrap();
        assert_eq!(content["branch"], "main");
        assert_eq!(content["note"], "be careful");
    }

    /// A required field this form has no control for can never be answered, so
    /// the form is refused before anybody fills the rest of it in.
    #[test]
    fn a_form_requiring_something_undrawable_is_not_drawn_at_all() {
        let schema: ElicitationSchema = serde_json::from_str(
            r#"{"type":"object","required":["count"],"properties":{
                "count":{"type":"integer","description":"How many?"},
                "why":{"type":"string","description":"Why?"}
            }}"#,
        )
        .unwrap();
        assert!(Form::parse(&schema, None).is_none());
    }

    /// A multi-select field whose options make some joins ambiguous.
    fn overlapping_labels(consts: &[&str]) -> ElicitationSchema {
        let options: Vec<serde_json::Value> = consts
            .iter()
            .map(|c| serde_json::json!({ "const": c, "title": c }))
            .collect();
        serde_json::from_value(serde_json::json!({
            "type": "object",
            "properties": {
                "question_0": { "type": "array", "items": { "anyOf": options } },
                "question_0_custom": {
                    "type": "string",
                    "title": "Other",
                    "_meta": { "_askUserQuestionCustomAnswer": { "questionId": "question_0" } },
                },
            },
        }))
        .unwrap()
    }

    /// The join is reversed only where it has one reading. `"A, B"` against
    /// options `"A, B"`, `"A"` and `"B"` has two — the one picked alone, or the
    /// other two picked together — and nothing in the string says which, so it
    /// goes to the companion box instead of coming back as a selection nobody
    /// made.
    ///
    /// **The case that matters is the one that resolves.** A string that cannot
    /// be taken apart at all already falls through on its own; what does not is
    /// this one, where a label *is* the join of two others.
    #[test]
    fn an_ambiguous_join_is_not_read_as_a_selection() {
        let form = Form::parse(&overlapping_labels(&["A, B", "A", "B"]), None).unwrap();
        let picked = form.content_from(&serde_json::json!({ "question_0": "A, B" })).unwrap();
        assert!(
            picked.get("question_0").is_none(),
            "an ambiguous join was resolved into a selection: {picked:?}"
        );
        assert_eq!(picked["question_0_custom"], "A, B");
    }

    /// **Ambiguity belongs to the answer, not to the field.** An earlier version
    /// gave up on any field where some label contained the separator, which cost
    /// every unambiguous answer on that field too: `"C"` is one plain label with
    /// one reading, and `"A, B, C"` has exactly one as well — there is no second
    /// way to read either.
    #[test]
    fn an_unambiguous_answer_survives_a_field_that_has_ambiguous_ones() {
        let form = Form::parse(&overlapping_labels(&["A, B", "C"]), None).unwrap();

        let one = form.content_from(&serde_json::json!({ "question_0": "C" })).unwrap();
        assert_eq!(one["question_0"], serde_json::json!(["C"]));

        let both = form
            .content_from(&serde_json::json!({ "question_0": "A, B, C" }))
            .unwrap();
        assert_eq!(both["question_0"], serde_json::json!(["A, B", "C"]));

        // And the label containing the separator, picked by itself.
        let alone = form.content_from(&serde_json::json!({ "question_0": "A, B" })).unwrap();
        assert_eq!(alone["question_0"], serde_json::json!(["A, B"]));
    }

    /// The ordinary multi-select, which is what the last two are measured
    /// against.
    #[test]
    fn a_multi_select_with_ordinary_labels_is_still_resolved() {
        let form = Form::parse(&ask_user_schema(true), Some("Which?")).unwrap();
        let content = form
            .content_from(&serde_json::json!({ "question_0": "Parser and the tests" }))
            .unwrap();
        assert_eq!(content["question_0"], serde_json::json!(["Parser and the tests"]));
    }

    /// **Free text never goes into an enumerated field**, whatever its type
    /// says. A `string` property with `oneOf` takes one of its `const`s, so a
    /// typed answer put there is an accept the asker rejects — and a rejected
    /// accept is the whole form discarded, not that one field. Without a
    /// companion box there is nowhere for it to go, and it is dropped and
    /// logged rather than sent.
    #[test]
    fn a_typed_answer_is_never_put_into_an_enumerated_field() {
        let schema: ElicitationSchema = serde_json::from_str(
            r#"{"type":"object","properties":{
                "choice":{"type":"string","description":"Which?","oneOf":[
                    {"const":"a","title":"A"},{"const":"b","title":"B"}
                ]}
            }}"#,
        )
        .unwrap();
        let form = Form::parse(&schema, None).unwrap();

        assert!(
            form.content_from(&serde_json::json!({ "choice": "something else" }))
                .is_none()
        );
        // The card is told, so this is a state it should never be asked to
        // resolve: with options and no companion, no box is drawn.
        let drawn = form.questions_json();
        assert_eq!(drawn["questions"][0]["accepts_text"], false);

        // Picking from the list still works, and answers with the `const`.
        let content = form.content_from(&serde_json::json!({ "choice": "A" })).unwrap();
        assert_eq!(content["choice"], "a");
    }

    /// A question with a companion box does take typed text — that is what the
    /// box is — so the card keeps it. The two cases share one condition and
    /// this is the other half of it.
    #[test]
    fn a_question_with_a_companion_box_still_takes_typed_text() {
        let drawn = Form::parse(&ask_user_schema(false), Some("Which?"))
            .unwrap()
            .questions_json();
        assert!(drawn["questions"][0].get("accepts_text").is_none());
    }

    /// A required question is marked on the card, which is the only place it can
    /// be enforced before the answers are gone. Declining on the way out is the
    /// backstop, not the mechanism: by then the user has been told the form was
    /// sent.
    #[test]
    fn the_card_is_told_which_questions_are_required() {
        let schema: ElicitationSchema = serde_json::from_str(
            r#"{"type":"object","required":["branch"],"properties":{
                "branch":{"type":"string","description":"Which branch?"},
                "note":{"type":"string","description":"Anything else?"}
            }}"#,
        )
        .unwrap();
        let drawn = Form::parse(&schema, None).unwrap().questions_json();
        assert_eq!(drawn["questions"][0]["required"], true);
        // Absent rather than false, so the card's optional case needs no key.
        assert!(drawn["questions"][1].get("required").is_none());
    }
}
