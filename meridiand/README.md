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

**Balances — DeepSeek, Moonshot (Kimi) and SiliconFlow.** Most vendors publish
nothing: Anthropic and xAI never did, OpenAI withdrew the endpoint that used to,
and OpenRouter's account credits need a *management* key rather than the
inference key a provider row holds. This is a fact about the vendors rather than
a gap here, and a provider no balance can be read for is logged as unwatchable
at startup rather than silently polled for nothing. `--status` says which is
which per provider.

**Spending — only what this install spent.** The usage-surge watcher reads
`audit_messages`, which holds the turns *this* process ran. If your keys are
spent somewhere else — another deployment, another product — the ledger here is
empty and the watcher correctly stays silent. Attributing spend per key across
environments needs an upstream usage API, which this does not have.

## Running it

```bash
meridiand --config /etc/meridiand.toml
```

| Flag | |
|---|---|
| `--config <PATH>` | The configuration file. **Required.** |
| `--data-dir <PATH>` | Database, secrets file and logs. Defaults to the platform location below; also `MERIDIAN_DATA_DIR`. |
| `--check` | Parse and validate, print a summary and the data directory, exit. Touches no database. |
| `--test <WEBHOOK_ID>` | Apply the configuration, send one test alert to that endpoint, exit. Non-zero if the receiver refuses it. |
| `--status` | Print each endpoint's delivery health and exit. |

`--test` is how you find out whether a receiver's authentication and schema are
right without waiting for a real condition to occur:

```
$ meridiand --config m.toml --test alertpipe
meridiand: alertpipe accepted the test — HTTP 200 in 13ms, 1 attempt(s)
  it answered: {"ok":true}
```

`--status` reads what the database holds, in two halves: which providers a
balance can actually be read for, and what each endpoint's last delivery did.
`last_success_at` is kept across failures on purpose — "it worked at 09:00 and
has failed since" is the useful sentence, and clearing it would leave only "it
is failing":

```
$ meridiand --config m.toml --status
on   deepseek-prod  deepseek  https://api.deepseek.com
     balance read as `deepseek`
on   kimi-prod  openai  https://api.moonshot.cn/v1
     balance read as `moonshot`
on   relay  openai  https://codex-api.example/v1
     no balance: this upstream publishes none, or `vendor` is unset

on   alertpipe  [balance_low,balance_unavailable]  http://alertpipe.internal/webhooks/balance
     last attempt 2026-09-15T18:57:37.699Z   last success 2026-09-15T18:56:53.001Z
     1 consecutive failure(s): network error: error sending request
```

The provider half is worth reading first, because the failure it shows is
otherwise completely silent: the daemon runs, the endpoints are healthy, and no
alert was ever going to be raised.

`--config` is required because no location is conventional for a configuration
file, and picking one silently would be worse than saying so. A data directory
is the opposite — every daemon has a conventional home — so it defaults to
`<platform data dir>/cn.yuxiaoqiu.meridiand`:

| | |
|---|---|
| Windows | `%APPDATA%\cn.yuxiaoqiu.meridiand` |
| macOS | `~/Library/Application Support/cn.yuxiaoqiu.meridiand` |
| Linux | `$XDG_DATA_HOME/cn.yuxiaoqiu.meridiand`, or `~/.local/share/…` |

Deliberately **beside** the desktop app's `cn.yuxiaoqiu.meridian` rather than
inside it. Sharing that directory would meet the ownership guard below and
refuse to start — correct, and a baffling way to be greeted on a first run.

The resolved path is printed by `--check` and logged at every start, so a
default never becomes a mystery. A platform with no data directory at all — a
service with no `HOME` and no `XDG_DATA_HOME` — is an error naming the flag
rather than a guess.

Run `--check` before restarting the real one. It answers without the data
directory existing, so it works in CI.

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

### The ownership guard

The default above is already the daemon's own, so this normally never comes up.
It matters when you point `--data-dir` somewhere by hand.

The configuration file is a *desired state*, so applying it to a directory a
desktop install configured would switch off every provider and endpoint the
file does not name. `meridiand` refuses to start in a directory that already
holds providers and is not marked as its own, and names the flag to change.

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

The file names `DEEPSEEK_PROD_KEY`; the credential goes in that variable, in the
environment this process runs under:

```bash
export DEEPSEEK_PROD_KEY='sk-…'          # sh, bash, zsh
```
```powershell
$env:DEEPSEEK_PROD_KEY = 'sk-…'          # PowerShell
```

> PowerShell's `$DEEPSEEK_PROD_KEY = '…'` makes a *shell* variable, which the
> process never sees. The `$env:` prefix is what puts it in the environment, and
> the daemon will otherwise report the variable as unset — correctly.

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
| `name`, `base_url` | |
| `type` | The adapter family: `openai`, `anthropic`, `deepseek`, `xai`, `google`. It says how requests are *sent*; it does not decide whether a balance can be read. |
| `vendor` | Which upstream this actually is — see below. Optional. |
| `api_key_env` | The variable's **name**. |
| `enabled` | Default `true`. |

#### Which upstreams publish a balance

Three, and `vendor` is what names them: **`deepseek`**, **`moonshot`**
(Moonshot AI, whose platform is branded Kimi) and **`siliconflow`**. Everyone
else publishes nothing — Anthropic and xAI never did, OpenAI withdrew the
endpoint, and OpenRouter's account credits need a *management* key rather than
the inference key a provider row holds.

`type` cannot answer this. Moonshot and SiliconFlow are both reached over the
OpenAI dialect, so both are `type = "openai"` and only `vendor` tells them
apart. Left out, `vendor` is inferred from `base_url` when that is a vendor's
own address verbatim — a relay address identifies nobody and is never probed,
which is deliberate: the alternative is posting your API key to an account
endpoint whose operator never published one.

```toml
[[provider]]
id = "kimi-prod"
name = "Kimi prod"
type = "openai"                  # the dialect
vendor = "moonshot"              # whose account endpoint to ask
base_url = "https://api.moonshot.cn/v1"
api_key_env = "KIMI_PROD_KEY"
```

Moonshot and SiliconFlow each run a mainland site and an international one, on
separate accounts whose keys their own documentation says are not
interchangeable. Both are supported; set `base_url` to the one your key belongs
to (`api.moonshot.cn` / `api.moonshot.ai`, `api.siliconflow.cn` /
`api.siliconflow.com`) and name `vendor` explicitly, since only the mainland
addresses are recognised by inference. The currency in an alert is taken from
the host, because neither upstream says which one its figures are in.

A provider no balance can be read for is logged at startup rather than refused —
it is a legitimate row to have, and the commonest cause of an unexpected one is
a missing `vendor`.

| Webhook | |
|---|---|
| `id`, `name`, `url` | `http` and `https` only. |
| `format` | `generic`, `dingtalk`, `feishu`, `wecom`, `slack`, `custom`. |
| `events` | At least one. An endpoint subscribed to nothing is refused. |
| `secret_env` | Optional. **What the secret is depends on the format** — see below. |
| `body` | The JSON document to post. Required for `custom`, refused for every other format. |
| `enabled` | Default `true`. |

Events: `balance_low`, `balance_unavailable`, `usage_surge`, `test`.

At most 32 endpoints — a single alert should not become a hundred outbound
requests.

### What the secret is

There is one secret per endpoint and the format decides what it does with it.
That is why `custom` needs no second field: a bearer token *is* this endpoint's
secret.

| `format` | `secret_env` holds | and it becomes |
|---|---|---|
| `generic` | a signing key | `X-Meridian-Signature: sha256=…` |
| `dingtalk` | its signing secret | `?timestamp=&sign=` on the URL |
| `feishu` | its signing secret | `timestamp`/`sign` inside the body |
| `wecom`, `slack` | — | nothing; the key is already in the URL |
| `custom` | a bearer token | `Authorization: Bearer …` |

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

## Your own alert pipe: `format = "custom"`

The five other formats are either Meridian's contract or a vendor's published
product. Neither covers the common case of a company's own alert pipe, which
has a schema its operations team wrote down and authenticates with a bearer
token. `custom` is that: you give the JSON document, with placeholders where the
alert's values go.

```toml
[[webhook]]
id = "alertpipe"
name = "ops alert pipe"
url = "http://alertpipe.internal.example/webhooks/balance"
format = "custom"
events = ["balance_low", "balance_unavailable", "test"]
secret_env = "ALERTPIPE_TOKEN"        # becomes Authorization: Bearer …

[webhook.body]
title = "账户余额告警"                  # a constant
message = "{{summary}}"
service = "billing"
environment = "prod"
requestId = "{{delivery_id}}"
timestamp = "{{raised_at_iso}}"

[webhook.body.data]
account = "{{balance.provider_name}}"
balance = "{{balance.total:number}}"   # a JSON number, not a string
threshold = "{{balance.threshold:number}}"
currency = "{{balance.currency}}"
```

produces

```json
{
  "title": "账户余额告警",
  "message": "DeepSeek dev 余额偏低（阈值 100）\nCNY 12.5（充值 12.5 / 赠送 0）…",
  "service": "billing",
  "environment": "prod",
  "requestId": "df112bb0-a0ac-4f77-b1a7-77066677888f",
  "timestamp": "2026-09-15T18:56:52.983Z",
  "data": { "account": "DeepSeek dev", "balance": 12.5, "threshold": 100, "currency": "CNY" }
}
```

### How substitution works

**Structurally, not textually.** The template is parsed as JSON first and
placeholders are replaced at the *value* level, so a provider name or an
upstream error message containing a quote or a brace cannot change the
document's shape. String interpolation — the obvious implementation — would make
every one of those values an injection into your parser.

Three forms follow:

| in the template | becomes |
|---|---|
| `"{{name}}"` — the whole string | that value, with its own type |
| `"{{name:number}}"` | a JSON **number** |
| `"cost is {{name}}"` | text, always |

Anything that is not a string — a number, a bool, a nested table — is a constant
and passes through untouched.

`:number` is the one place Meridian emits money as a number rather than an exact
decimal string. Inside the app that rule is not negotiable; outbound, your
receiver's schema wins — the same concession the vendor formats already make.
You are choosing it, and choosing whatever precision your receiver's JSON parser
imposes.

### Placeholders

Every alert: `event`, `alert_key`, `title`, `summary`, `delivery_id`,
`raised_at_ms`, `raised_at_iso`.

Balance alerts: `balance.provider_id`, `balance.provider_name`,
`balance.is_available`, `balance.threshold`, `balance.currency`,
`balance.total`, `balance.granted`, `balance.topped_up`, `balance.accounts`.

Usage alerts: `usage.window_hours`, `usage.baseline_days`, `usage.multiplier`,
`usage.window_cost`, `usage.baseline_cost`, `usage.baseline_windows`,
`usage.is_lower_bound`, `usage.top_provider`, `usage.top_conversation`.

`:number` applies to `raised_at_ms`, the four `balance.*` amounts, and the
numeric `usage.*` ones.

Two things to know about the balance fields. **`balance.currency` and the
amounts beside it describe the account that triggered the alert** — the first
one under the floor — because a balance alert can carry several currencies and a
receiving schema usually has room for one; `balance.accounts` is the whole list
if you want it. And a placeholder that is valid but **does not apply to this
alert renders `null`** (empty, inside a larger string) rather than failing:
losing an alert over a field your receiver may not even read is the worse
outcome.

An unknown placeholder, or `:number` on something that is not a number, is
refused when the file is read — not at delivery. A typo caught at delivery time
is caught by nobody, because the delivery it breaks is the one nobody is
watching for.

### What it does not do

`custom` reports failure on the HTTP status alone. DingTalk, Feishu and WeCom
answer a rejected message with 200 and an error code in the body, and those are
handled; guessing at an error shape for an endpoint nobody here has seen would
turn a delivered alert into a duplicate on the next tick. Use `--test` to see
what your receiver actually answers.

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
| `already holds N provider(s) and is not marked as the daemon's` | `--data-dir` points at another install's directory. |
| `notify.enabled is true but no webhook is enabled` | Refused at parse: it would watch and have nowhere to report. |
| `takes the *name* of an environment variable … not its value` | The credential was pasted where its variable's name belongs. The value is not echoed back, in case it is the credential. |
| `needs $X, which is unset or empty` | The variable did not reach the process. In PowerShell, check you used `$env:X = '…'` and not `$X = '…'`. |
| `is `custom` but has no `body`` | A `custom` endpoint needs the document it posts; see above. |
| `a `body_template` only applies to `custom`` | The other formats send their own shape; drop the `body` or change the format. |
| `is not a placeholder this app substitutes` | A typo, refused at parse. The list is above. |
| Delivered but the receiver ignores it | Probably the wrong `format`. A vendor format sends *that vendor's* shape and signs the way that vendor signs — pointing one at your own pipe posts a document it will not recognise, unauthenticated. `--test` shows what comes back. |
| Starts, logs `is watching`, says nothing | Expected if nothing crossed a threshold. Balance alerting needs `balance_threshold` set *and* a provider a balance can be read for — run `--status`, which says so per provider. Usage alerting needs a ledger this install produced. |
| `--status` says `no balance` for an upstream that has one | Its `vendor` is unset and its address is not one the catalog recognises — a relay, or a vendor's other site. Name `vendor` and re-run. |
| `this upstream publishes no balance` | That provider can never produce a balance alert. |

Logs go to stdout and to `<data-dir>/logs/meridian.log` as JSONL.

## License

Apache-2.0, with the rest of `meridian-core`.
