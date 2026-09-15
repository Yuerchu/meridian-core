# meridiand

Meridian's watcher, running where nobody is looking: it polls what an upstream
says is left on your account and what this install has been spending, and posts
an alert to a webhook when either looks wrong.

It is the same core the desktop app runs on, minus the window. It listens on
nothing and serves nothing — configuration is a file, applied on every start.

## Why not just leave the app open

Closing the desktop window already hides it to the tray, so "no window" is
something you can have without this binary. What you cannot have is **no
display server**: the Linux desktop build needs `libwebkit2gtk` and
`libappindicator`, which a container, a NAS or a VPS does not have. That is the
whole reason this exists. If your monitoring can live on a machine with a
desktop session that never sleeps, the tray is a complete answer and you do not
need `meridiand`.

## What it can actually watch

**Balances — DeepSeek only.** Almost no vendor publishes one: Anthropic and xAI
publish nothing, and OpenAI withdrew the endpoint that used to. This is a fact
about the vendors rather than a gap here, and a provider whose upstream
publishes no balance is logged as unwatchable at startup rather than silently
polled for nothing.

**Spending — only what this install spent.** The usage-surge watcher reads
`audit_messages`, which holds the turns *this* process ran. If your keys are
spent somewhere else — another deployment, another product — the ledger here is
empty and the watcher correctly stays silent. Attributing spend per key across
environments needs an upstream usage API, which this does not have.

## Running it

```bash
meridiand --config /etc/meridiand.toml --data-dir /var/lib/meridiand
```

| Flag | |
|---|---|
| `--config <PATH>` | The configuration file. Required. |
| `--data-dir <PATH>` | Database, secrets file and logs. Required; also `MERIDIAN_DATA_DIR`. |
| `--check` | Parse and validate, print a summary, exit. Touches no database. |

Both paths are required rather than defaulted: a daemon that guessed either
would write somewhere nobody meant.

Run `--check` before restarting the real one. It answers without needing the
data directory to exist, so it works in CI.

### The passphrase

Secrets (provider API keys, webhook signing secrets) live in an encrypted file
under `--data-dir`. The desktop keeps that file's passphrase in the OS keychain;
a server has none to use, so you supply it:

| Variable | |
|---|---|
| `MERIDIAN_SECRETS_PASSPHRASE` | The passphrase itself. |
| `MERIDIAN_SECRETS_PASSPHRASE_FILE` | A path to read it from — how a container mounts a secret without it showing in `docker inspect`. |

Set **one**, not both; setting both is refused rather than ranked, because a
deployment that believes the wrong one is in force finds out via an unreadable
secrets file. Minimum 16 characters. Surrounding whitespace is stripped, so a
mounted file's trailing newline does not become part of it.

Set neither and it uses the machine's keychain, which is correct on a laptop —
`meridiand` and the desktop app then share one secrets file.

> **Keep it.** Lose the passphrase and every stored key becomes unreadable; the
> encrypted file is preserved but nothing can open it.

### The data directory

Give it one of its own. The configuration file is a *desired state*, so
applying it to a directory a desktop install configured would switch off every
provider and endpoint the file does not name. `meridiand` refuses to start in a
directory that already holds providers and is not marked as its own, and says
so.

## Configuration

Declarative TOML, applied on every start. Stable ids make applying it twice the
same as applying it once, so a recreated container converges instead of
accumulating duplicates.

**No credential is in this file.** Keys and signing secrets are named by the
*environment variable* that holds them, so the file can be committed and
reviewed. A pasted key is refused outright rather than warned about — a warning
in a log nobody reads is how a secret reaches a git history.

```toml
[notify]
enabled = true
balance_threshold = "5"          # decimal string, never a number

[[provider]]
id = "deepseek-prod"             # stable; also names the key's keyring entry
name = "DeepSeek prod"
type = "deepseek"
base_url = "https://api.deepseek.com"
api_key_env = "DEEPSEEK_PROD_KEY"

[[webhook]]
id = "ops"
name = "运维群"
url = "https://oapi.dingtalk.com/robot/send?access_token=..."
format = "dingtalk"
events = ["balance_low", "balance_unavailable"]
secret_env = "OPS_DINGTALK_SECRET"
```

Unknown keys are refused. A misspelled key is you believing you configured
something, and the symptom of accepting it is an alert that never fires.

### `[notify]`

An absent key means the built-in default; they are not restated in your file.

| Key | Default | |
|---|---|---|
| `enabled` | `false` | The master switch. |
| `balance_threshold` | *absent* | Absent switches balance alerting off. `"0"` keeps checking and reports only when an upstream says the account is unusable — which is a real setting, because a postpaid account can be refused while showing a healthy figure. |
| `balance_interval_minutes` | `360` | Also the repeat interval: a standing alert is re-sent no more often than it is checked. |
| `usage_enabled` | `false` | |
| `usage_check_interval_minutes` | `15` | |
| `usage_window_hours` | `1` | 1–168. |
| `usage_baseline_days` | `7` | 1–90, and must cover at least one whole window. |
| `usage_multiplier` | `"3"` | ≥ 1. Below 1 the rule would read "alert when this window cost *less* than usual". |
| `usage_min_cost` | `"1"` | The floor. A window under it is never a surge however large the ratio. |
| `usage_cooldown_minutes` | `360` | |

Money is a **decimal string**, here as everywhere in Meridian. A TOML float has
already been through a binary double by the time it is parsed, and is refused.

### `[[provider]]` and `[[webhook]]`

| Provider | |
|---|---|
| `id` | Letters, digits, `-`, `_`. Changing it orphans the stored key. |
| `name`, `type`, `base_url` | |
| `api_key_env` | The variable's **name**. |
| `enabled` | Default `true`. |

| Webhook | |
|---|---|
| `id`, `name`, `url` | `http` and `https` only. |
| `format` | `generic`, `dingtalk`, `feishu`, `wecom`, `slack`. |
| `events` | At least one. An endpoint subscribed to nothing is refused. |
| `secret_env` | Optional; absent means unsigned. |
| `enabled` | Default `true`. |

Events: `balance_low`, `balance_unavailable`, `usage_surge`, `test`.

At most 32 endpoints — a single alert should not become a hundred outbound
requests.

### What the file stops naming

A provider or endpoint removed from the file is **disabled, not deleted**.
Removing it has to stop it firing, or the file is lying about what is
configured; deleting instead would mean a mistyped id quietly destroys an
endpoint's delivery history, while a disabled row can be switched back on.

### Missing variables

If any named variable is unset or empty, **nothing is written at all**. A
half-applied configuration leaves a provider with no key, which reports as an
upstream failure and reads like an outage. An empty variable counts as missing:
that is the usual shape of a secret that failed to inject.

## The alert payload

Only `format = "generic"` is Meridian's own contract. The other four are the
vendors' own — including four different signing schemes, and the habit of three
of them to report a rejected message with HTTP 200 and an error code in the
body.

`generic` posts JSON with these headers:

| Header | |
|---|---|
| `X-Meridian-Event` | `balance_low` \| `balance_unavailable` \| `usage_surge` \| `test` |
| `X-Meridian-Delivery` | A UUID per attempt. |
| `X-Meridian-Timestamp` | Milliseconds. |
| `X-Meridian-Signature` | `sha256=<hex>`, present only when a secret is configured. |

The signature is `HMAC-SHA256(secret, "<timestamp>.<raw body>")`. The timestamp
is *inside* the signed material, not merely beside it, so a captured request
does not replay for ever. **Verify against the raw bytes you received**, not
against a re-serialization of the parsed JSON.

With no secret there is no signature header at all, rather than one computed
over an empty key — which would verify against an empty key and read as signed.

```json
{
  "spec_version": "1",
  "delivery_id": "03d201b2-…",
  "event": "balance_low",
  "raised_at": 1789494252816,
  "alert_key": "balance:deepseek-prod",
  "title": "DeepSeek prod 余额偏低",
  "summary": "…",
  "balance": {
    "provider_id": "deepseek-prod",
    "provider_name": "DeepSeek prod",
    "is_available": true,
    "threshold": "5",
    "accounts": [
      { "currency": "CNY", "total_balance": "3.2",
        "granted_balance": "0", "topped_up_balance": "3.2" }
    ]
  },
  "usage": null,
  "test": null
}
```

`balance`, `usage` and `test` are all present, with all but one `null`. Switch
on `event` and read the matching object; absent-versus-null is a distinction
you should not have to make.

A `usage_surge` carries the window and baseline figures, `baseline_windows`,
`is_lower_bound`, and the top three providers and conversations by cost.

Every amount is a decimal **string**. Every key is `snake_case`, at every depth.

### Delivery

10s timeout. A 429 or 5xx is retried twice with backoff; a 4xx is not, because
the same request gets the same answer. Each endpoint's last attempt, last
success, last error and consecutive-failure count are recorded — and nothing
disables an endpoint automatically, because a notification channel that
switches itself off silently is the failure this is meant to prevent.

## When alerts repeat

An alert is raised and reported in **two separate writes**, in that order, with
a network round trip between them. Delivery accepted by no endpoint is not
recorded as reported, so the next check offers it again — a webhook host that
was down does not cost you the alert.

A standing condition is reported once per cooldown. It is reported again
immediately if it gets *worse* (a low balance becoming an unusable account),
because that is not a repeat. When the condition clears, the record is dropped,
so the next occurrence is a new alert rather than a suppressed one.

The first check runs 60 seconds after start — long enough that a restart loop
does not become a request loop, short enough that "I just set this up" gets an
answer while you are still watching.

## Deploying

The binary needs `sherpa-onnx` and `onnxruntime` shared libraries **beside it**
(its rpath is `$ORIGIN`). On Windows that is `sherpa-onnx-c-api.dll`,
`sherpa-onnx-cxx-api.dll`, `onnxruntime.dll` and
`onnxruntime_providers_shared.dll`; `cargo build --release` leaves them in the
target directory. Without them the process does not start, and the error names
a DLL rather than anything about Meridian.

That is ~21 MB of speech libraries this daemon never calls, against a 14 MB
binary. `meridian-core` links them unconditionally — voice reaches `Services`,
`state`, `bootstrap` and most of `onebot`, so gating it is a refactor rather
than a flag. It is a known cost, not a mystery.

`SIGTERM` and Ctrl-C both stop it, and stopping waits for a check in flight to
finish: a tick can be between "the alert was delivered" and "record that
somebody was told", and losing that second write means the next start reports
the same alert again.

> **Not verified yet.** The `SIGTERM` handler is `#[cfg(unix)]` and this was
> developed on Windows, where that branch is not compiled, let alone run.
> Check it on your target before relying on a clean stop.

## Troubleshooting

| What you see | |
|---|---|
| `error while loading shared libraries` / exit `0xC0000135` | The `.dll`/`.so` files are not beside the binary. |
| `needs $X, which is unset or empty` | A named variable did not reach the process. Nothing was written. |
| `already holds N provider(s) and is not marked as the daemon's` | `--data-dir` points at another install's directory. |
| `notify.enabled is true but no webhook is enabled` | Refused at parse: it would watch and have nowhere to report. |
| `is not an environment variable name` | A secret was pasted where its variable's name belongs. |
| Starts, logs `is watching`, says nothing | Expected if nothing crossed a threshold. Balance alerting needs `balance_threshold` set *and* a provider whose upstream publishes one — today, DeepSeek. Usage alerting needs a ledger this install produced. |
| `this upstream publishes no balance` | That provider can never produce a balance alert. |

Logs go to stdout and to `<data-dir>/logs/meridian.log` as JSONL.

## License

Apache-2.0, with the rest of `meridian-core`.
