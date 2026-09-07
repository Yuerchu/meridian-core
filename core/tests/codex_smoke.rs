//! Does the ChatGPT backend accept a request from *this* app?
//!
//! Ignored by default: it needs a real `codex login` and it spends real quota.
//!
//! ```
//! cargo test -p meridian-core --test codex_smoke -- --ignored --nocapture
//! ```
//!
//! It exists to settle decisions that cannot be settled from a fixture.
//!
//! **What it settled.** `originator: meridian` is accepted — the backend has no
//! opinion about an originator it has not seen, so the decision not to
//! impersonate the CLI stands. A custom `instructions` is accepted and echoed
//! back. And the model cannot be hardcoded: `gpt-5.4` answers *"not supported
//! when using Codex with a ChatGPT account"* while the model in the CLI's own
//! `config.toml` succeeds.
//!
//! **What it could not settle.** On the account this was run against — a `free`
//! plan on `gpt-5.6-terra` — the backend emits **no reasoning items at all**,
//! even when asked for them explicitly with `reasoning: {effort, summary}` and
//! `include: ["reasoning.encrypted_content"]`. The event stream carries only
//! message items. So the reasoning round-trip is exercised by unit tests and has
//! never been seen working against a live server. Both tests print the event
//! types they received for exactly this reason: on a plan that does produce
//! reasoning, `response.output_item.done` carrying a `reasoning` item will
//! appear in that list, and the capture can be confirmed then.
//!
//! If a refusal ever names the originator, that is a decision to revisit
//! deliberately rather than something to work around quietly.

use meridian_core::codex_auth::{Manager, StoreId, storage};
use meridian_core::keyring::DefaultKeyringStore;
use std::sync::Arc;

/// Matches what the adapter will send. Kept here rather than imported so this
/// file is honest about what it is testing even before the adapter exists.
const ORIGINATOR: &str = "meridian";
const BASE_URL: &str = "https://chatgpt.com/backend-api/codex";

/// Whatever the CLI is configured to use, because the set of models a ChatGPT
/// account may reach is narrower than the API's and moves.
///
/// Measured: `gpt-5.4` answers *"The 'gpt-5.4' model is not supported when using
/// Codex with a ChatGPT account"* while the model in `config.toml` succeeds. A
/// hardcoded name here would fail for reasons that have nothing to do with what
/// the test is checking.
fn configured_model(home: &std::path::Path) -> String {
    std::fs::read_to_string(home.join("config.toml"))
        .ok()
        .and_then(|text| {
            text.lines()
                .filter_map(|line| line.trim().strip_prefix("model")?.trim().strip_prefix('='))
                .map(|value| value.trim().trim_matches('"').to_string())
                .find(|value| !value.is_empty())
        })
        .unwrap_or_else(|| "gpt-5.6".to_string())
}

#[tokio::test]
#[ignore = "needs a real ChatGPT login and spends quota"]
async fn the_backend_accepts_a_request_from_this_app() {
    let home = storage::find_codex_home().expect("no home directory");
    if !storage::auth_file(&home).exists() {
        panic!("no login at {} — run `codex login` first", home.display());
    }

    let manager = Manager::new(
        StoreId::CodexCli { home: home.clone() },
        Arc::new(DefaultKeyringStore),
        Arc::new(meridian_core::client::ReqwestTransport::shared()),
    );

    let status = manager.status();
    println!("account: {:?} plan: {:?}", status.email, status.plan);
    assert!(status.logged_in, "{:?}", status.problem);

    let bearer = manager.bearer().await.expect("could not obtain a token");
    println!("account_id: {} fedramp: {}", bearer.account_id, bearer.is_fedramp);

    // The smallest thing that still exercises the whole path: auth headers,
    // originator, the Responses shape, and `store: false`.
    let body = serde_json::json!({
        "model": configured_model(&home),
        "instructions": "Reply with the single word: ok",
        "input": [{
            "type": "message",
            "role": "user",
            "content": [{ "type": "input_text", "text": "A farmer has 17 sheep. All but 9 run away. How many are left?" }]
        }],
        "stream": true,
        "store": false,
        "include": ["reasoning.encrypted_content"],
        "tool_choice": "auto",
        "parallel_tool_calls": false,
        "tools": [],
        "reasoning": { "effort": "medium", "summary": "auto" },
    });

    let client = reqwest::Client::new();
    let mut request = client
        .post(format!("{BASE_URL}/responses"))
        .header("authorization", format!("Bearer {}", bearer.access_token))
        .header("chatgpt-account-id", &bearer.account_id)
        .header("originator", ORIGINATOR)
        .header("user-agent", format!("{ORIGINATOR}/0.2.0"))
        .header("session_id", uuid::Uuid::new_v4().to_string())
        .header("content-type", "application/json")
        .json(&body);
    if bearer.is_fedramp {
        request = request.header("x-openai-fedramp", "true");
    }

    let response = request.send().await.expect("request failed to send");
    let status = response.status();
    let text = response.text().await.unwrap_or_default();

    println!("HTTP {status}");
    // The event types rather than the stream, which is long and would put model
    // output in a log. This list is the evidence for the module note above: a
    // plan that produces reasoning shows a `reasoning` item among the
    // `response.output_item.*` events, and this account shows none.
    let events: Vec<&str> = text.lines().filter_map(|line| line.strip_prefix("event: ")).collect();
    println!("events: {events:?}");
    let reasoning_items = text
        .lines()
        .filter(|line| line.contains("\"item\"") && line.contains("\"type\":\"reasoning\""))
        .count();
    println!("reasoning items in the stream: {reasoning_items}");

    assert!(
        status.is_success(),
        "the backend refused a request from originator={ORIGINATOR:?}.\n\
         If the refusal names the originator, that is the decision to revisit — \
         report it rather than switching to codex_cli_rs.\nHTTP {status}: {text}"
    );
}

/// The same round trip, through the adapter the app actually uses.
///
/// The test above proves the credential and the headers are accepted; this one
/// proves `CodexProvider` builds a request the backend takes and parses what
/// comes back. Separate because they fail for different reasons — one is a fact
/// about their server, the other is a fact about our code.
#[tokio::test]
#[ignore = "needs a real ChatGPT login and spends quota"]
async fn the_adapter_completes_a_turn() {
    use futures::StreamExt;
    use meridian_core::provider::{ChatMessage, ChatParams, ChatProvider, StreamEvent, codex::CodexProvider};

    let home = storage::find_codex_home().expect("no home directory");
    let manager =
        meridian_core::codex_auth::registry().get(meridian_core::codex_auth::StoreId::CodexCli { home: home.clone() });
    let provider = CodexProvider::new("https://chatgpt.com/backend-api/codex", manager);

    // Asked to think, and given something worth thinking about: the reasoning
    // round-trip is the part of this adapter that a simple question does not
    // exercise at all.
    let params = ChatParams {
        model: configured_model(&home),
        thinking_effort: Some("medium".into()),
        ..Default::default()
    };
    let mut stream = provider
        .stream_chat_with_tools(
            vec![ChatMessage::user(
                "A farmer has 17 sheep. All but 9 run away. How many are left? \
                 Answer with just the number.",
            )],
            vec![],
            params,
        )
        .await
        .expect("the adapter could not start a turn");

    let mut text = String::new();
    let mut reasoning_items = 0usize;
    let mut stop_reason = None;
    while let Some(event) = stream.next().await {
        match event.expect("the stream reported an error") {
            StreamEvent::Text { content } => text.push_str(&content),
            StreamEvent::ProviderStateUpdate { .. } => reasoning_items += 1,
            StreamEvent::Stop { reason, .. } => stop_reason = Some(reason),
            _ => {}
        }
    }

    println!("text: {text:?}");
    println!("stop reason: {stop_reason:?}");
    // Zero on the account this was written against — see the module note. It is
    // printed rather than asserted on, because a plan that emits no reasoning is
    // not a failure of this adapter, and asserting either way would make the
    // test lie about one plan or the other.
    println!("reasoning items captured: {reasoning_items}");

    assert!(!text.is_empty(), "the turn produced no text");
    assert!(stop_reason.is_some(), "the turn never reported a stop");
}
