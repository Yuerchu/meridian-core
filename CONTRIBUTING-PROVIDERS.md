# Contributing Providers to Meridian (`meridian-core`)

`meridian-core` (`src-tauri/crates/`) is licensed under **Apache-2.0**. External
vendors and community contributors are welcome to submit pull requests to add or
update protocol adapters and vendor presets across all supported capability
domains (`chat`, `web_search`, `tts`, `embedding`, `image_gen`, `video_gen`).

## 1. Official Standard Protocol Policy (Mandatory)

Meridian prioritizes each model family's **official standard wire protocol**:

| Model Family | Canonical Official Protocols |
| :--- | :--- |
| Anthropic Claude (`anthropic-claude`) | `anthropic_messages` |
| Google Gemini (`google-gemini`) | `gemini_generate_content`, `gemini_interactions` |
| OpenAI GPT / o-series (`openai-gpt`) | `openai_responses`, `openai_chat` |
| DeepSeek (`deepseek`) | `openai_chat`, `openai_responses` |
| xAI Grok (`xai-grok`) | `openai_responses`, `openai_chat` |

Rules enforced in review and CI:

1. **Do not invent private chat protocols.** If your platform serves standard
   models, declare the model family's official protocol(s) in `manifest.json`.
   New wire protocols (`protocols/<proto>/`) are accepted only for official
   model-origin protocols or widely adopted cloud standards (e.g. AWS Bedrock
   Converse, Ollama Native).
2. **Do not hardcode degraded OpenAI-compat mappings for official presets.**
   If your gateway supports Anthropic Messages or Gemini GenerateContent
   natively alongside OpenAI Chat Completions, declare all supported protocols
   so each model can route over its official standard protocol.
3. **Honest degradation for user-configured relays.** When a user points a
   custom relay at Claude or Gemini over `openai_chat`, Meridian allows the
   connection at runtime but flags it as `ProtocolCompliance::Degraded` and
   surfaces exact lost capabilities (native PDF, prompt cache breakpoints,
   thinking signature continuity, server tools).
4. **Standard-protocol extensions only.** Implementations of
   `OpenAIChatExtension` (or analogous extension traits) are limited to
   standard-protocol extension points: vendor auth/routing headers, documented
   extension parameters, standard-compatible usage/signature extraction, and
   endpoint path variants.

## 2. Three Contribution Tiers

### Tier L1 — Declarative Vendor Preset (**0 lines of Rust, 0 lines of Frontend**)

Use this tier if your service speaks an existing built-in standard protocol
(`openai_chat`, `openai_responses`, `anthropic_messages`,
`gemini_generate_content`, `gemini_interactions`).

1. Create `providers/src/vendors/<vendor_id>/manifest.json` following the strict
   `VendorManifest` schema (`deny_unknown_fields`, explicit nullable keys, no
   silent defaults).
2. `providers/build.rs` auto-discovers every `src/vendors/*/manifest.json` at
   compile time.
3. Declare any non-secret extra configuration fields in `config_fields`
   (fields with `"secret": true` are routed exclusively through `SecretsManager`
   and never written to `config_json`).
4. Declare your `models.dev` provider mapping in `models_dev` so automatic
   pricing sync works out of the box.

### Tier L2 — Standard Protocol + Vendor Extension / Search / TTS (**0 lines of Frontend**)

Use this tier if you need:
- A standard-protocol extension (e.g. custom cache routing header, vendor
  thinking parameters, or typed response field extraction), OR
- A `web_search`, `tts`, `embedding`, `image_gen`, or `video_gen` provider, OR
- An account balance endpoint.

1. Add `providers/src/vendors/<vendor_id>/manifest.json`.
2. Add `providers/src/vendors/<vendor_id>/mod.rs` composing the existing
   protocol parent (`OpenAIChatProvider`, `OpenAIResponsesProvider`,
   `AnthropicMessagesProvider`, `GoogleGenerateContentProvider`,
   `GoogleInteractionsProvider`) and reusable mixins (`crate::mixins::*`).
3. Register the extension or capability provider in `registry.rs` /
   `web_search/mod.rs` / `tts/mod.rs`.
4. Add unit tests calling the `meridian-provider-sdk` `conformance` harness.

### Tier L3 — New Official Wire Protocol

1. Add `providers/src/protocols/<protocol_id>.rs` with bare protocol naming
   (see Section 3).
2. Register a `ProtocolDescriptor` in `providers/src/protocols/mod.rs`.
3. Provide full request serialization, SSE streaming, non-streaming response
   parsing, unknown-field drift warnings (`ExtraIgnore` + `warn_extra_fields`),
   and `SwitchImpact` classification.

## 3. Naming & Modeling Conventions

- **No invented suffixes:** never use `Engine`, `Core`, `Abstract`, `Dto`,
  `Hook`, or `Patch` (use `capability_overrides`, not `capability_patch`).
- **Protocol parents take bare names:** `OpenAIChatProvider`,
  `OpenAIResponsesProvider`, `AnthropicMessagesProvider`,
  `GoogleGenerateContentProvider`, `GoogleInteractionsProvider`.
- **Greatest-common-divisor shared wire structs use `*Base`:**
  `OpenAIChatRequestBase`, `OpenAIChatResponseBase`, etc.
- **Concrete wire payloads use `*Request` / `*Response`:**
  `DeepSeekThinkingRequest`, `GoogleExtraContentResponse`, `WebSearchRequest`,
  `WebSearchResponse`.
- **Non-GCD capabilities and cross-protocol components use `*Mixin`:**
  `OpenAISamplingParamsMixin`, `AnthropicSamplingParamsMixin`,
  `GoogleSamplingParamsMixin`, `VertexServiceAccountAuthMixin`,
  `SseStreamMixin`, `ToolImageCarryMixin`.
  - Sampling parameters (`temperature`, `top_p`) are protocol-specific
    capabilities, never merged across vendors and never forced onto a base GCD
    struct when variants forbid them.
- **Response deserialization anti-pitfall:** never put two `#[serde(flatten)]`
  fields on one struct. Keep a single `#[serde(default, flatten)] pub extra: ExtraIgnore`
  on `*ResponseBase` and let vendor extensions `remove()` their declared keys
  into typed `*Response` structs before `warn_ignored_fields()` runs.
- **Money & Numerics:** all balance and price figures use
  `meridian_provider_sdk::decimal::Decimal` (`NUMERIC(38,18)`), never `f32` or
  `f64`.

## 4. Verification Before Opening a PR

```bash
# 1. Run SDK & provider unit + conformance suites
cargo test --manifest-path src-tauri/crates/Cargo.toml -p meridian-provider-sdk --features conformance
cargo test --manifest-path src-tauri/crates/Cargo.toml -p meridian-providers

# 2. Verify zero clippy warnings
cargo clippy --manifest-path src-tauri/crates/Cargo.toml -p meridian-provider-sdk -p meridian-providers --all-targets

# 3. Run catalog & contract static gates (from meridian workspace root)
pnpm catalog:check
pnpm contracts:check
```
