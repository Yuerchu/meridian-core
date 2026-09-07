//! The Codex CLI's ChatGPT login, read and kept fresh.
//!
//! Ported from `codex-rs/login` (Apache-2.0, OpenAI). Its own module rather than
//! part of the provider adapter, for two reasons: it has two consumers — the
//! request path and the settings panel showing which account is connected — and
//! it owns a refresh lock, which has to outlive any one adapter. A lock built
//! per request excludes nothing.
//!
//! **The credential is shared with another program.** The Codex CLI is running
//! its own sessions against the same file, and both of us can refresh. That is
//! the fact the whole design here is arranged around; see [`Manager::refresh`].

pub mod refresh;
pub mod storage;
pub mod token_data;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock, RwLock};

use chrono::{Duration, Utc};

use crate::keyring::{DefaultKeyringStore, KeyringStore};
use refresh::{RefreshFailure, RefreshRequest, RefreshResponse};
use storage::{AuthBackend, AuthDotJson};
use token_data::TokenData;

/// Refresh this long before a token actually expires, so a request is never sent
/// with one about to lapse mid-flight.
const REFRESH_WINDOW: Duration = Duration::minutes(5);

/// How stale a token may get when it carries no expiry to check.
const MAX_AGE_WITHOUT_EXPIRY: Duration = Duration::days(8);

/// Which credential store a manager speaks for.
///
/// **Deliberately free of anything read out of the tokens.** Keying on the
/// account would be circular — the registry needs an identity to hand back a
/// manager, the manager is what loads the credential, and the account is only
/// known once it has. An app-side login that has not happened yet has no account
/// at all, and a refresh that changes accounts would silently orphan the manager
/// its own callers are holding.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum StoreId {
    /// The Codex CLI's own login, shared with it.
    CodexCli { home: PathBuf },
    /// A login this app made and owns. Reserved for the in-app OAuth flow.
    ///
    /// `slot` is `"default"` today — one account per provider. It exists now so
    /// that supporting several later does not mean migrating stored secrets to a
    /// new key shape.
    #[allow(dead_code)]
    MeridianOwned { provider_id: String, slot: String },
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("{0}")]
    NotLoggedIn(String),
    #[error("{0}")]
    Refused(String),
    #[error("{0}")]
    Unavailable(String),
}

/// What a request needs to reach the ChatGPT backend.
#[derive(Debug, Clone)]
pub struct Bearer {
    pub access_token: String,
    pub account_id: String,
    /// Adds one request header rather than changing the endpoint.
    pub is_fedramp: bool,
}

/// What the settings panel shows. Carries no token material.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AuthStatus {
    pub logged_in: bool,
    pub email: Option<String>,
    pub plan: Option<String>,
    /// `file` or `keyring`, so a user can tell where their login actually lives.
    pub storage: Option<String>,
    /// The resolved `CODEX_HOME`.
    ///
    /// Shown because a GUI process need not inherit a terminal's environment,
    /// and "logged in at the terminal but not here" is otherwise unexplainable.
    pub codex_home: Option<String>,
    /// Present when something is wrong, phrased as what to do about it.
    pub problem: Option<String>,
}

/// One credential store, with the lock that serialises refreshes against it.
pub struct Manager {
    id: StoreId,
    keyring: Arc<dyn KeyringStore>,
    /// Injected so tests can drive the exchange without a network — the reason
    /// the transport is a trait at all.
    transport: Arc<dyn crate::client::HttpTransport>,
    /// Held across the whole read-decide-write of a refresh. Per store rather
    /// than per process: two different logins have nothing to serialise against
    /// each other.
    refresh_lock: tokio::sync::Mutex<()>,
    /// Last known good, to save a disk read per request. Never trusted while
    /// deciding whether to refresh — see [`Manager::refresh`].
    cached: RwLock<Option<(AuthDotJson, AuthBackend)>>,
}

impl std::fmt::Debug for Manager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // No credential material, by construction.
        f.debug_struct("Manager").field("id", &self.id).finish_non_exhaustive()
    }
}

/// The managers in this process, one per store.
///
/// A registry rather than a single manager: a specific `CODEX_HOME` welded into
/// a global would ignore the variable changing after startup, prevent two
/// providers pointing at different homes, and give the tests nothing to vary.
#[derive(Default)]
pub struct Registry {
    managers: std::sync::Mutex<HashMap<StoreId, Arc<Manager>>>,
}

static REGISTRY: OnceLock<Registry> = OnceLock::new();

pub fn registry() -> &'static Registry {
    REGISTRY.get_or_init(Registry::default)
}

impl Registry {
    pub fn get(&self, id: StoreId) -> Arc<Manager> {
        let mut map = self.managers.lock().expect("registry lock");
        Arc::clone(map.entry(id.clone()).or_insert_with(|| {
            Arc::new(Manager::new(
                id,
                Arc::new(DefaultKeyringStore),
                Arc::new(crate::client::ReqwestTransport::shared()),
            ))
        }))
    }
}

impl Manager {
    pub fn new(id: StoreId, keyring: Arc<dyn KeyringStore>, transport: Arc<dyn crate::client::HttpTransport>) -> Self {
        Self {
            id,
            keyring,
            transport,
            refresh_lock: tokio::sync::Mutex::new(()),
            cached: RwLock::new(None),
        }
    }

    fn home(&self) -> Result<PathBuf, AuthError> {
        match &self.id {
            StoreId::CodexCli { home } => Ok(home.clone()),
            StoreId::MeridianOwned { .. } => Err(AuthError::Unavailable(
                "in-app ChatGPT login is not implemented yet".into(),
            )),
        }
    }

    /// Read the credential from its store, ignoring anything cached.
    fn load_fresh(&self) -> Result<(AuthDotJson, AuthBackend), AuthError> {
        let home = self.home()?;
        match storage::load(&home, self.keyring.as_ref()) {
            Ok(Some(found)) => Ok(found),
            Ok(None) => Err(AuthError::NotLoggedIn(format!(
                "Codex is not logged in. Run `codex login` in a terminal, then refresh. \
                 (looked in {})",
                home.display()
            ))),
            Err(e) => Err(AuthError::Unavailable(e.to_string())),
        }
    }

    /// The token to put on a request, refreshing first if it is due.
    pub async fn bearer(&self) -> Result<Bearer, AuthError> {
        let current = match self.cached_copy() {
            Some(found) => found,
            None => {
                let found = self.load_fresh()?;
                self.store(found.clone());
                found
            }
        };

        let auth = if due_for_refresh(&current.0, Utc::now()) {
            self.refresh(None).await?
        } else {
            current.0
        };

        bearer_from(&auth)
    }

    /// Refresh after the backend rejected a token, if nobody has beaten us to it.
    pub async fn refresh_after_rejection(&self, rejected: &str) -> Result<(), AuthError> {
        self.refresh(Some(rejected)).await.map(|_| ())
    }

    /// Exchange the refresh token, unless it turns out not to be needed.
    ///
    /// **The reload under the lock is the whole point, and it is not an
    /// optimisation.** The Codex CLI shares this credential and refreshes it on
    /// its own schedule; so does this app, from other turns. Refresh tokens
    /// rotate, and presenting a spent one is what a backend reads as a stolen
    /// session — it then invalidates the chain, signing the user out of the CLI
    /// as well. So the cache is discarded and the store re-read *after* the lock
    /// is held, and the exchange only happens if the credential still looks like
    /// the one that needed replacing.
    async fn refresh(&self, rejected: Option<&str>) -> Result<AuthDotJson, AuthError> {
        let _guard = self.refresh_lock.lock().await;

        let (current, backend) = self.load_fresh()?;
        let tokens = current.tokens.clone().ok_or_else(|| {
            AuthError::NotLoggedIn(
                "This Codex install is signed in with an API key rather than a ChatGPT account. \
                 Either run `codex login`, or configure an ordinary OpenAI provider instead."
                    .into(),
            )
        })?;

        // Somebody already did the work while we waited for the lock.
        let already_done = match rejected {
            Some(rejected) => tokens.access_token != rejected,
            None => !due_for_refresh(&current, Utc::now()),
        };
        if already_done {
            self.store((current.clone(), backend));
            return Ok(current);
        }

        let response = self.exchange(&tokens.refresh_token).await?;
        let updated = merge(current, &response)?;

        // Persist before believing it. A rotated refresh token that reached the
        // backend but not the disk is one this process would keep using while
        // the next launch presents the spent one — the exact reuse this guards
        // against, delayed until restart. Failing here also means failing the
        // request in flight: succeeding now would hide a session that is already
        // inconsistent until it surfaces as an unexplainable logout.
        let home = self.home()?;
        storage::save(&home, backend, &updated, self.keyring.as_ref()).map_err(|e| {
            self.forget();
            AuthError::Unavailable(format!(
                "The Codex login was renewed but could not be saved ({e}). \
                 The previous credentials are untouched; you may need to run `codex login` again."
            ))
        })?;

        self.store((updated.clone(), backend));
        Ok(updated)
    }

    async fn exchange(&self, refresh_token: &str) -> Result<RefreshResponse, AuthError> {
        let mut request = crate::client::Request::new(http::Method::POST, refresh::token_url());
        request.headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );
        request.body = Some(crate::client::RequestBody::Json(
            serde_json::to_value(RefreshRequest::new(refresh_token.to_string()))
                .map_err(|e| AuthError::Unavailable(e.to_string()))?,
        ));

        let response = match self.transport.execute(request).await {
            Ok(response) => response,
            Err(crate::client::TransportError::Http { status, body, .. }) => {
                let failure = refresh::classify(status.as_u16(), body.as_deref().unwrap_or_default());
                // Never log the body: a refusal can echo parts of the credential.
                tracing::warn!(
                    status = status.as_u16(),
                    permanent = failure.is_permanent(),
                    "Codex token refresh failed"
                );
                return Err(if failure.is_permanent() {
                    AuthError::Refused(failure.message())
                } else {
                    AuthError::Unavailable(failure.message())
                });
            }
            Err(e) => {
                return Err(AuthError::Unavailable(
                    RefreshFailure::Transient(e.to_string()).message(),
                ));
            }
        };

        serde_json::from_slice::<RefreshResponse>(&response.body)
            .map_err(|e| AuthError::Unavailable(format!("The login service sent an unreadable reply: {e}")))
    }

    /// Who is signed in, for the settings panel. Never refreshes: opening a
    /// settings page should not spend a refresh token.
    pub fn status(&self) -> AuthStatus {
        let codex_home = self.home().ok().map(|p| p.display().to_string());
        match self.load_fresh() {
            Ok((auth, backend)) => {
                let info = auth.tokens.as_ref().map(|t| &t.id_token);
                AuthStatus {
                    logged_in: auth.is_chatgpt_login(),
                    email: info.and_then(|i| i.email.clone()),
                    plan: info.and_then(|i| i.chatgpt_plan_type.clone()),
                    storage: Some(
                        match backend {
                            AuthBackend::File => "file",
                            AuthBackend::Keyring => "keyring",
                        }
                        .into(),
                    ),
                    codex_home,
                    problem: (!auth.is_chatgpt_login()).then(|| {
                        "This Codex install is signed in with an API key rather than a ChatGPT \
                         account. Run `codex login`, or configure an ordinary OpenAI provider."
                            .to_string()
                    }),
                }
            }
            Err(e) => AuthStatus {
                logged_in: false,
                email: None,
                plan: None,
                storage: None,
                codex_home,
                problem: Some(e.to_string()),
            },
        }
    }

    fn cached_copy(&self) -> Option<(AuthDotJson, AuthBackend)> {
        self.cached.read().ok()?.clone()
    }

    fn store(&self, value: (AuthDotJson, AuthBackend)) {
        if let Ok(mut slot) = self.cached.write() {
            *slot = Some(value);
        }
    }

    fn forget(&self) {
        if let Ok(mut slot) = self.cached.write() {
            *slot = None;
        }
    }
}

/// Whether the credential should be exchanged before being used again.
///
/// An expiry in the token is the answer when it has one. When it does not, age
/// is the only signal left — a token that has sat unrefreshed for over a week is
/// likelier to be rejected than not.
fn due_for_refresh(auth: &AuthDotJson, now: chrono::DateTime<Utc>) -> bool {
    let Some(tokens) = auth.tokens.as_ref() else {
        return false;
    };
    match token_data::expires_at(&tokens.access_token) {
        Ok(Some(expiry)) => expiry <= now + REFRESH_WINDOW,
        // No expiry to read, or an access token we cannot parse at all: fall
        // back to age. Refusing to refresh something unreadable would strand the
        // session; refreshing it is at worst a wasted exchange.
        _ => auth
            .last_refresh
            .is_none_or(|last| last <= now - MAX_AGE_WITHOUT_EXPIRY),
    }
}

/// Apply a refresh response, keeping anything it did not rotate.
fn merge(mut auth: AuthDotJson, response: &RefreshResponse) -> Result<AuthDotJson, AuthError> {
    let tokens = auth
        .tokens
        .as_mut()
        .ok_or_else(|| AuthError::NotLoggedIn("no ChatGPT session to renew".into()))?;

    if let Some(id_token) = response.id_token.as_deref() {
        tokens.id_token = token_data::parse_id_token(id_token)
            .map_err(|e| AuthError::Unavailable(format!("The renewed login is unreadable: {e}")))?;
    }
    if let Some(access) = response.access_token.as_deref() {
        access.clone_into(&mut tokens.access_token);
    }
    // Absent means "keep using the one you have". Overwriting it with an empty
    // string would end the session at the next refresh.
    if let Some(refresh) = response.refresh_token.as_deref() {
        refresh.clone_into(&mut tokens.refresh_token);
    }
    auth.last_refresh = Some(Utc::now());
    Ok(auth)
}

fn bearer_from(auth: &AuthDotJson) -> Result<Bearer, AuthError> {
    let tokens: &TokenData = auth.tokens.as_ref().ok_or_else(|| {
        AuthError::NotLoggedIn(
            "This Codex install is signed in with an API key rather than a ChatGPT account. \
             Either run `codex login`, or configure an ordinary OpenAI provider instead."
                .into(),
        )
    })?;
    // No account id means no workspace to bill against. Sending the request
    // without the header gets a 401 that explains nothing, so refuse here where
    // there is something useful to say.
    let account_id = tokens
        .account_id()
        .ok_or_else(|| AuthError::NotLoggedIn("This Codex login names no account. Run `codex login` again.".into()))?;
    Ok(Bearer {
        access_token: tokens.access_token.clone(),
        account_id: account_id.to_string(),
        is_fedramp: tokens.id_token.chatgpt_account_is_fedramp,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{Request, TransportError};
    use crate::keyring::test_support::MockKeyringStore;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Counts exchanges and answers with whatever the test queued.
    ///
    /// The count is what most of these assert on: "did we go to the authority at
    /// all" is the question, and a refresh token spent needlessly is the bug.
    #[derive(Debug, Default)]
    struct FakeAuthority {
        calls: AtomicUsize,
        replies: StdMutex<Vec<Result<serde_json::Value, (u16, String)>>>,
    }

    impl FakeAuthority {
        fn with(replies: Vec<Result<serde_json::Value, (u16, String)>>) -> Arc<Self> {
            Arc::new(Self {
                calls: AtomicUsize::new(0),
                replies: StdMutex::new(replies),
            })
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl crate::client::HttpTransport for FakeAuthority {
        async fn execute(&self, _req: Request) -> Result<crate::client::Response, TransportError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let reply = {
                let mut queued = self.replies.lock().unwrap();
                if queued.is_empty() {
                    None
                } else {
                    Some(queued.remove(0))
                }
            };
            match reply {
                Some(Ok(body)) => Ok(crate::client::Response {
                    status: http::StatusCode::OK,
                    headers: Default::default(),
                    body: body.to_string().into(),
                }),
                Some(Err((status, body))) => Err(TransportError::Http {
                    status: http::StatusCode::from_u16(status).unwrap(),
                    url: Some(refresh::token_url()),
                    headers: Default::default(),
                    body: Some(body),
                }),
                None => panic!("the authority was called more often than the test expected"),
            }
        }

        async fn stream(&self, _req: Request) -> Result<crate::client::StreamResponse, TransportError> {
            unimplemented!("the refresh exchange is not streamed")
        }
    }

    fn jwt(payload: serde_json::Value) -> String {
        use base64::Engine;
        let e = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        format!(
            "{}.{}.sig",
            e.encode(br#"{"alg":"none"}"#),
            e.encode(payload.to_string())
        )
    }

    /// An access token that expires at `offset` from now.
    fn access_token(offset: Duration) -> String {
        jwt(serde_json::json!({ "exp": (Utc::now() + offset).timestamp() }))
    }

    fn write_login(home: &std::path::Path, access: &str, refresh_token: &str) {
        std::fs::create_dir_all(home).unwrap();
        let auth = serde_json::json!({
            "auth_mode": "chatgpt",
            "tokens": {
                "id_token": jwt(serde_json::json!({
                    "https://api.openai.com/auth": {
                        "chatgpt_account_id": "acct-1",
                        "chatgpt_plan_type": "pro"
                    }
                })),
                "access_token": access,
                "refresh_token": refresh_token,
                "account_id": "acct-1"
            },
            "last_refresh": Utc::now().to_rfc3339(),
        });
        std::fs::write(storage::auth_file(home), serde_json::to_string_pretty(&auth).unwrap()).unwrap();
    }

    fn manager(home: &std::path::Path, authority: Arc<FakeAuthority>) -> Manager {
        Manager::new(
            StoreId::CodexCli {
                home: home.to_path_buf(),
            },
            Arc::new(MockKeyringStore::new()),
            authority,
        )
    }

    /// A live token is used as-is. Refreshing when there is no need spends a
    /// rotation for nothing and widens the window in which this app and the CLI
    /// disagree about which token is current.
    #[tokio::test]
    async fn a_valid_token_is_used_without_contacting_anyone() {
        let dir = tempfile::tempdir().unwrap();
        write_login(dir.path(), &access_token(Duration::hours(2)), "refresh-1");
        let authority = FakeAuthority::with(vec![]);
        let manager = manager(dir.path(), Arc::clone(&authority));

        assert_eq!(manager.bearer().await.unwrap().account_id, "acct-1");
        assert_eq!(authority.calls(), 0);
    }

    /// Exchanged before it lapses, not after — a token that expires mid-request
    /// is a failure the user sees.
    #[tokio::test]
    async fn a_token_about_to_expire_is_exchanged_first() {
        let dir = tempfile::tempdir().unwrap();
        write_login(dir.path(), &access_token(Duration::minutes(1)), "refresh-1");
        let fresh = access_token(Duration::hours(2));
        let authority = FakeAuthority::with(vec![Ok(serde_json::json!({
            "access_token": fresh,
            "refresh_token": "refresh-2"
        }))]);
        let manager = manager(dir.path(), Arc::clone(&authority));

        assert_eq!(manager.bearer().await.unwrap().access_token, fresh);
        assert_eq!(authority.calls(), 1);

        // And it reached the disk, so the CLI sees the same session.
        let (saved, _) = storage::load(dir.path(), &MockKeyringStore::new()).unwrap().unwrap();
        let tokens = saved.tokens.unwrap();
        assert_eq!(tokens.access_token, fresh);
        assert_eq!(tokens.refresh_token, "refresh-2");
    }

    /// **The guarded reload.** Somebody else — the Codex CLI, or another turn —
    /// refreshed while we waited for the lock. Presenting the token we set out
    /// with would be presenting a spent one, which is what a backend reads as a
    /// stolen session; it then invalidates the chain and signs the user out of
    /// the CLI as well.
    #[tokio::test]
    async fn a_refresh_someone_else_already_did_is_not_repeated() {
        let dir = tempfile::tempdir().unwrap();
        write_login(dir.path(), &access_token(Duration::minutes(1)), "refresh-1");
        let authority = FakeAuthority::with(vec![]);
        let manager = manager(dir.path(), Arc::clone(&authority));

        // Prime the cache with a stale view, then let the store move on
        // underneath it — what a concurrent CLI refresh looks like from here.
        let rejected = access_token(Duration::minutes(1));
        let (mut stale, _) = storage::load(dir.path(), &MockKeyringStore::new()).unwrap().unwrap();
        stale.tokens.as_mut().unwrap().access_token = rejected.clone();
        manager.store((stale, AuthBackend::File));

        let renewed = access_token(Duration::hours(3));
        write_login(dir.path(), &renewed, "refresh-2");

        manager.refresh_after_rejection(&rejected).await.unwrap();
        assert_eq!(authority.calls(), 0, "the refresh token must not be spent again");
        assert_eq!(manager.bearer().await.unwrap().access_token, renewed);
    }

    /// Several turns hitting a 401 at once must produce one exchange, not one
    /// each — every extra is a rotation attempt with an already-spent token.
    #[tokio::test]
    async fn concurrent_rejections_produce_one_exchange() {
        let dir = tempfile::tempdir().unwrap();
        let rejected = access_token(Duration::minutes(1));
        write_login(dir.path(), &rejected, "refresh-1");
        let authority = FakeAuthority::with(vec![Ok(serde_json::json!({
            "access_token": access_token(Duration::hours(2)),
            "refresh_token": "refresh-2"
        }))]);
        let manager = Arc::new(manager(dir.path(), Arc::clone(&authority)));

        let mut tasks = Vec::new();
        for _ in 0..8 {
            let manager = Arc::clone(&manager);
            let rejected = rejected.clone();
            tasks.push(tokio::spawn(
                async move { manager.refresh_after_rejection(&rejected).await },
            ));
        }
        for task in tasks {
            task.await.unwrap().unwrap();
        }
        assert_eq!(authority.calls(), 1);
    }

    /// A spent refresh token ends the session. Retrying cannot help, and the
    /// message has to say what will.
    #[tokio::test]
    async fn a_permanent_refusal_asks_for_a_new_login() {
        let dir = tempfile::tempdir().unwrap();
        let rejected = access_token(Duration::minutes(1));
        write_login(dir.path(), &rejected, "refresh-1");
        let authority = FakeAuthority::with(vec![Err((400, r#"{"error":{"code":"refresh_token_reused"}}"#.into()))]);
        let manager = manager(dir.path(), Arc::clone(&authority));

        let err = manager.refresh_after_rejection(&rejected).await.unwrap_err();
        assert!(matches!(err, AuthError::Refused(_)));
        assert!(err.to_string().contains("codex login"));
    }

    /// A server having a bad day is not a reason to sign anybody out.
    #[tokio::test]
    async fn a_transient_failure_is_reported_as_such() {
        let dir = tempfile::tempdir().unwrap();
        let rejected = access_token(Duration::minutes(1));
        write_login(dir.path(), &rejected, "refresh-1");
        let authority = FakeAuthority::with(vec![Err((503, "upstream down".into()))]);
        let manager = manager(dir.path(), Arc::clone(&authority));

        let err = manager.refresh_after_rejection(&rejected).await.unwrap_err();
        assert!(matches!(err, AuthError::Unavailable(_)));
        assert!(!err.to_string().contains("codex login"));
    }

    /// The rotation reached the backend but not the disk. Carrying on would
    /// leave this process holding a token the next launch cannot reproduce,
    /// while the one on disk has already been spent — so the request fails now,
    /// where it can be explained, and the file is left as it was.
    #[tokio::test]
    async fn a_rotation_that_cannot_be_saved_fails_the_request() {
        let dir = tempfile::tempdir().unwrap();
        let rejected = access_token(Duration::minutes(1));
        write_login(dir.path(), &rejected, "refresh-1");
        let before = std::fs::read_to_string(storage::auth_file(dir.path())).unwrap();

        let authority = FakeAuthority::with(vec![Ok(serde_json::json!({
            "access_token": access_token(Duration::hours(2)),
            "refresh_token": "refresh-2"
        }))]);
        // A home that cannot be written to, because the path is a file.
        let manager = Manager::new(
            StoreId::CodexCli {
                home: storage::auth_file(dir.path()),
            },
            Arc::new(MockKeyringStore::new()),
            authority,
        );
        let (auth, backend) = storage::load(dir.path(), &MockKeyringStore::new()).unwrap().unwrap();
        manager.store((auth, backend));

        assert!(manager.refresh_after_rejection(&rejected).await.is_err());
        assert_eq!(
            std::fs::read_to_string(storage::auth_file(dir.path())).unwrap(),
            before,
            "the original credentials must be untouched"
        );
    }

    /// An API-key install is not a ChatGPT session, and saying so beats a 401
    /// from an endpoint that does not take keys.
    #[tokio::test]
    async fn an_api_key_install_is_told_apart() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(
            storage::auth_file(dir.path()),
            r#"{"auth_mode":"apikey","OPENAI_API_KEY":"sk-test"}"#,
        )
        .unwrap();
        let manager = manager(dir.path(), FakeAuthority::with(vec![]));

        let err = manager.bearer().await.unwrap_err();
        assert!(err.to_string().contains("API key"), "{err}");
        assert!(!manager.status().logged_in);
        assert!(manager.status().problem.is_some());
    }

    /// The resolved home is on the status card because a GUI process need not
    /// inherit a terminal's environment — it is the only way to explain "logged
    /// in at the terminal, not here".
    #[tokio::test]
    async fn the_status_names_where_it_looked() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(dir.path(), FakeAuthority::with(vec![]));
        let status = manager.status();
        assert!(!status.logged_in);
        assert!(status.codex_home.is_some());
        assert!(status.problem.unwrap().contains("codex login"));

        write_login(dir.path(), &access_token(Duration::hours(1)), "refresh-1");
        let status = manager.status();
        assert!(status.logged_in);
        assert_eq!(status.plan.as_deref(), Some("pro"));
        assert_eq!(status.storage.as_deref(), Some("file"));
    }

    /// Opening a settings page must not spend a refresh token.
    #[tokio::test]
    async fn asking_for_status_never_refreshes() {
        let dir = tempfile::tempdir().unwrap();
        write_login(dir.path(), &access_token(Duration::minutes(1)), "refresh-1");
        let authority = FakeAuthority::with(vec![]);
        let manager = manager(dir.path(), Arc::clone(&authority));

        assert!(manager.status().logged_in);
        assert_eq!(authority.calls(), 0);
    }

    /// Two homes are two logins and must not share a manager — nor a refresh
    /// lock, which would serialise unrelated sessions against each other.
    #[test]
    fn the_registry_keeps_stores_apart() {
        let a = StoreId::CodexCli {
            home: PathBuf::from("/one"),
        };
        let b = StoreId::CodexCli {
            home: PathBuf::from("/two"),
        };
        let registry = Registry::default();
        assert!(Arc::ptr_eq(&registry.get(a.clone()), &registry.get(a.clone())));
        assert!(!Arc::ptr_eq(&registry.get(a), &registry.get(b)));
    }

    /// A response that rotates only the access token keeps the refresh token —
    /// blanking it would end the session at the next renewal.
    #[test]
    fn a_partial_response_keeps_what_it_did_not_send() {
        let auth = AuthDotJson {
            tokens: Some(TokenData {
                access_token: "old-access".into(),
                refresh_token: "keep-me".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let merged = merge(
            auth,
            &RefreshResponse {
                access_token: Some("new-access".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let tokens = merged.tokens.unwrap();
        assert_eq!(tokens.access_token, "new-access");
        assert_eq!(tokens.refresh_token, "keep-me");
    }
}
