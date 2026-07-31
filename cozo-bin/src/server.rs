/*
 * Copyright 2023, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::path::{Path as FsPath, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{header, HeaderName, HeaderValue, Method, Request, Response, StatusCode};
use axum::response::sse::{Event, KeepAlive};
use axum::response::{Html, Sse};
use axum::routing::{get, post, put};
use axum::{Extension, Json, Router};
use clap::Args;
use futures::future::BoxFuture;
use futures::stream::Stream;
use itertools::Itertools;
use log::{error, info, warn};
use miette::miette;
// use miette::miette;
use rand::Rng;
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use tokio::task::spawn_blocking;
use tower_http::auth::{AsyncAuthorizeRequest, AsyncRequireAuthorizationLayer};
use tower_http::compression::CompressionLayer;
use tower_http::cors::{Any, CorsLayer};

use cozo::{
    format_error_as_json, DataValue, DbInstance, MultiTransaction, NamedRows, ScriptMutability,
    ScriptRunOptions, SimpleFixedRule,
};

#[derive(Args, Debug)]
pub(crate) struct ServerArgs {
    /// Database engine, can be `mem`, `sqlite`, `rocksdb` and others.
    #[clap(short, long, default_value_t = String::from("mem"))]
    engine: String,

    /// Path to the directory to store the database
    #[clap(short, long, default_value_t = String::from("cozo.db"))]
    path: String,

    /// Directory containing files exposed by the HTTP backup APIs
    #[clap(long, default_value_t = String::from("backups"))]
    backup_dir: String,

    /// Restore from the specified backup before starting the server
    #[clap(long)]
    restore: Option<String>,

    /// Extra config in JSON format
    #[clap(short, long, default_value_t = String::from("{}"))]
    config: String,

    /// Address to bind the service to
    #[clap(short, long, default_value_t = String::from("127.0.0.1"))]
    bind: String,

    /// Disable HTTP authentication. This is intentionally explicit and is
    /// accepted only for a loopback bind address.
    #[clap(long, default_value_t = false)]
    insecure_no_auth: bool,

    /// Port to use
    #[clap(short = 'P', long, default_value_t = 9070)]
    port: u16,

    /// When set, the content of the named table will be used as a token table
    #[clap(long)]
    token_table: Option<String>,

    /// Default per-query wall-clock budget in seconds (mnestic fork, query
    /// budget). Unset or non-positive = no default. Bounds every query; a
    /// per-request `timeout` or an in-script `:timeout` can only tighten it.
    #[clap(long)]
    default_query_timeout: Option<f64>,
}

#[derive(Clone)]
struct DbState {
    db: DbInstance,
    backup_dir: Arc<PathBuf>,
    rule_senders: Arc<Mutex<BTreeMap<u32, crossbeam::channel::Sender<miette::Result<NamedRows>>>>>,
    rule_counter: Arc<AtomicU32>,
    tx_counter: Arc<AtomicU32>,
    txs: Arc<Mutex<BTreeMap<u32, StoredTransaction>>>,
}

struct StoredTransaction {
    tx: Arc<MultiTransaction>,
    owner: Principal,
    write: bool,
    expires_at: Instant,
    in_flight: usize,
}

const TRANSACTION_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const TRANSACTION_REAPER_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, Eq, PartialEq)]
enum Principal {
    Local,
    Admin,
    Bearer([u8; 32]),
}

#[derive(Clone)]
struct AuthContext {
    mutability: ScriptMutability,
    principal: Principal,
}

struct TransactionLease {
    txs: Arc<Mutex<BTreeMap<u32, StoredTransaction>>>,
    id: u32,
}

impl Drop for TransactionLease {
    fn drop(&mut self) {
        if let Some(stored) = self.txs.lock().unwrap().get_mut(&self.id) {
            stored.in_flight = stored.in_flight.saturating_sub(1);
            if stored.in_flight == 0 {
                stored.expires_at = Instant::now() + TRANSACTION_IDLE_TIMEOUT;
            }
        }
    }
}

fn bearer_principal(token: &str) -> Principal {
    Principal::Bearer(Sha256::digest(token.as_bytes()).into())
}

fn is_idle_expired(expires_at: Instant, in_flight: usize, now: Instant) -> bool {
    in_flight == 0 && expires_at <= now
}

#[derive(Clone)]
struct MyAuth {
    skip_auth: bool,
    allowed_origin: String,
    auth_guard: String,
    token_table: Option<Arc<(String, DbInstance)>>,
}

fn has_forbidden_origin(request: &Request<Body>, allowed_origin: &str) -> bool {
    request
        .headers()
        .get(header::ORIGIN)
        .is_some_and(|origin| origin.as_bytes() != allowed_origin.as_bytes())
}

impl AsyncAuthorizeRequest<Body> for MyAuth {
    type RequestBody = Body;
    type ResponseBody = Body;
    type Future = BoxFuture<'static, Result<Request<Body>, Response<Self::ResponseBody>>>;

    fn authorize(&mut self, mut request: Request<Body>) -> Self::Future {
        let skip_auth = self.skip_auth;
        let allowed_origin = self.allowed_origin.clone();
        let auth_guard = self.auth_guard.clone();
        let token_table = self.token_table.clone();
        Box::pin(async move {
            if skip_auth {
                // Loopback is not an authentication boundary for browsers. Reject
                // requests originating on another site before granting the
                // unauthenticated local-server exception.
                if has_forbidden_origin(&request, &allowed_origin) {
                    return Err(Response::builder()
                        .status(StatusCode::FORBIDDEN)
                        .body(Body::empty())
                        .unwrap());
                }
                request.extensions_mut().insert(ScriptMutability::Mutable);
                request.extensions_mut().insert(AuthContext {
                    mutability: ScriptMutability::Mutable,
                    principal: Principal::Local,
                });
                return Ok(request);
            }

            // Token-table bearer auth, evaluated INDEPENDENTLY of any query string.
            // Previously this lived only in the `query() == None` branch, so a
            // valid `Authorization: Bearer ...` was ignored for every endpoint
            // that needs query params (e.g. `/transact?write=true`,
            // `/rules/name?arity=2`). Returns None when there's no token table or
            // no usable bearer header, so callers can fall through to deny.
            let bearer_grant = || -> Option<AuthContext> {
                let tt = token_table.as_ref()?;
                let (name, db) = tt.as_ref();
                let auth_str = request.headers().get("Authorization")?.to_str().ok()?;
                let token = auth_str.strip_prefix("Bearer ")?;
                match db.run_script(
                    &format!("?[mutable] := *{name} {{ token: $token, mutable }}"),
                    BTreeMap::from([(String::from("token"), DataValue::from(token))]),
                    ScriptMutability::Immutable,
                ) {
                    Ok(rows) => rows.rows.first().map(|val| AuthContext {
                        mutability: if val[0].get_bool() == Some(true) {
                            ScriptMutability::Mutable
                        } else {
                            ScriptMutability::Immutable
                        },
                        principal: bearer_principal(token),
                    }),
                    Err(err) => {
                        eprintln!("Error: {}", err);
                        None
                    }
                }
            };

            let mutability = match request.headers().get("x-cozo-auth") {
                None => {
                    // Try the `?auth=<secret>` query-string credential first, then
                    // fall back to bearer-token auth regardless of the query string.
                    let query_grant = request.uri().query().and_then(|q_str| {
                        for pair in q_str.split('&') {
                            if let Some((k, v)) = pair.split_once('=') {
                                if k == "auth" {
                                    return (v == auth_guard.as_str()).then_some(AuthContext {
                                        mutability: ScriptMutability::Mutable,
                                        principal: Principal::Admin,
                                    });
                                }
                            }
                        }
                        None
                    });
                    query_grant.or_else(bearer_grant)
                }
                Some(data) => match data.to_str() {
                    Ok(s) => {
                        if s == auth_guard.as_str() {
                            Some(AuthContext {
                                mutability: ScriptMutability::Mutable,
                                principal: Principal::Admin,
                            })
                        } else {
                            None
                        }
                    }
                    Err(_) => None,
                },
            };
            if let Some(auth) = mutability {
                request.extensions_mut().insert(auth.mutability);
                request.extensions_mut().insert(auth);
                Ok(request)
            } else {
                let unauthorized_response = Response::builder()
                    .status(StatusCode::UNAUTHORIZED)
                    .body(Body::empty())
                    .unwrap();

                Err(unauthorized_response)
            }
        })
    }
}

fn require_mutable(
    mutability: ScriptMutability,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    match mutability {
        ScriptMutability::Mutable => Ok(()),
        ScriptMutability::Immutable => Err((
            StatusCode::FORBIDDEN,
            json!({"ok": false, "message": "this operation requires a mutable token"}).into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mutable_operations_require_mutable_authorization() {
        assert!(require_mutable(ScriptMutability::Mutable).is_ok());

        let (status, _) = require_mutable(ScriptMutability::Immutable).unwrap_err();
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[test]
    fn bearer_principals_are_stable_without_retaining_tokens() {
        assert_eq!(bearer_principal("secret"), bearer_principal("secret"));
        assert_ne!(bearer_principal("secret"), bearer_principal("other"));
        assert!(matches!(bearer_principal("secret"), Principal::Bearer(_)));
    }

    #[test]
    fn active_transactions_are_not_idle_expired() {
        let now = Instant::now();
        let expired = now.checked_sub(Duration::from_secs(1)).unwrap();
        assert!(is_idle_expired(expired, 0, now));
        assert!(!is_idle_expired(expired, 1, now));
    }

    #[test]
    fn loopback_origin_guard_allows_non_browser_and_same_origin_requests() {
        let no_origin = Request::new(Body::empty());
        assert!(!has_forbidden_origin(&no_origin, "http://127.0.0.1:9070"));

        let same_origin = Request::builder()
            .header(header::ORIGIN, "http://127.0.0.1:9070")
            .body(Body::empty())
            .unwrap();
        assert!(!has_forbidden_origin(&same_origin, "http://127.0.0.1:9070"));
    }

    #[test]
    fn loopback_origin_guard_rejects_cross_origin_requests() {
        let request = Request::builder()
            .header(header::ORIGIN, "https://attacker.example")
            .body(Body::empty())
            .unwrap();
        assert!(has_forbidden_origin(&request, "http://127.0.0.1:9070"));
    }
}

pub(crate) async fn server_main(args: ServerArgs) {
    let db = DbInstance::new(&args.engine, &args.path, &args.config).unwrap();
    std::fs::create_dir_all(&args.backup_dir).expect("Cannot create HTTP backup directory");
    let backup_dir =
        std::fs::canonicalize(&args.backup_dir).expect("Cannot resolve HTTP backup directory");
    if let Some(secs) = args.default_query_timeout {
        db.set_default_query_timeout(Some(secs));
    }
    if let Some(p) = &args.restore {
        if let Err(err) = db.restore_backup(p) {
            error!("{}", err);
            error!("Restore from backup failed, terminate");
            panic!()
        }
    }

    let bind_is_loopback = IpAddr::from_str(&args.bind)
        .map(|address| address.is_loopback())
        .unwrap_or(false);
    if args.insecure_no_auth && !bind_is_loopback {
        panic!("--insecure-no-auth is permitted only with a loopback bind address");
    }
    let skip_auth = args.insecure_no_auth;
    let server_origin = if Ipv6Addr::from_str(&args.bind).is_ok() {
        format!("http://[{}]:{}", args.bind, args.port)
    } else {
        format!("http://{}:{}", args.bind, args.port)
    };

    let conf_path = if skip_auth {
        "".to_string()
    } else {
        format!("{}.{}.cozo_auth", args.path, args.engine)
    };
    let auth_guard = if skip_auth {
        "".to_string()
    } else {
        match tokio::fs::read_to_string(&conf_path).await {
            Ok(s) => s.trim().to_string(),
            Err(_) => {
                let s = rand::thread_rng()
                    .sample_iter(&rand::distributions::Alphanumeric)
                    .take(64)
                    .map(char::from)
                    .collect();
                tokio::fs::write(&conf_path, &s).await.unwrap();
                s
            }
        }
    };

    let auth_obj = MyAuth {
        skip_auth,
        allowed_origin: server_origin.clone(),
        auth_guard,
        token_table: args.token_table.map(|t| Arc::new((t, db.clone()))),
    };

    let state = DbState {
        db,
        backup_dir: Arc::new(backup_dir),
        rule_senders: Default::default(),
        rule_counter: Default::default(),
        tx_counter: Default::default(),
        txs: Default::default(),
    };
    let transaction_state = state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(TRANSACTION_REAPER_INTERVAL);
        loop {
            interval.tick().await;
            let now = Instant::now();
            let expired = {
                let mut txs = transaction_state.txs.lock().unwrap();
                let expired_ids = txs
                    .iter()
                    .filter_map(|(&id, entry)| {
                        is_idle_expired(entry.expires_at, entry.in_flight, now).then_some(id)
                    })
                    .collect_vec();
                expired_ids
                    .into_iter()
                    .filter_map(|id| txs.remove(&id))
                    .collect_vec()
            };
            for entry in expired {
                // Aborting waits for the database worker, so never do it on the
                // async runtime thread. Removal first also prevents new work
                // from extending an already-expired transaction.
                tokio::task::spawn_blocking(move || {
                    if let Err(err) = entry.tx.abort() {
                        warn!("Failed to abort expired HTTP transaction: {err}");
                    }
                });
            }
        }
    });
    let cors = CorsLayer::new()
        .allow_methods([Method::GET, Method::POST, Method::PUT, Method::DELETE])
        .allow_headers([
            header::CONTENT_TYPE,
            header::AUTHORIZATION,
            HeaderName::from_static("x-cozo-auth"),
        ]);
    let cors = if skip_auth {
        cors.allow_origin(server_origin.parse::<HeaderValue>().unwrap())
    } else {
        cors.allow_origin(Any)
    };

    let app = Router::new()
        .route("/text-query", post(text_query))
        .route("/export/:relations", get(export_relations))
        .route("/import", put(import_relations))
        .route("/backup", post(backup))
        .route("/import-from-backup", post(import_from_backup))
        .route("/changes/:relation", get(observe_changes))
        .route("/rules/:name", get(register_rule))
        .route(
            "/rule-result/:id",
            post(post_rule_result).delete(post_rule_err),
        ) // +keep alive
        .route("/transact", post(start_transact))
        .route("/transact/:id", post(transact_query).put(finish_query))
        .with_state(state)
        .layer(AsyncRequireAuthorizationLayer::new(auth_obj))
        .fallback(not_found)
        .route("/", get(root))
        .layer(cors)
        .layer(CompressionLayer::new())
        .layer(DefaultBodyLimit::disable());

    let addr = if Ipv6Addr::from_str(&args.bind).is_ok() {
        SocketAddr::from_str(&format!("[{}]:{}", args.bind, args.port)).unwrap()
    } else {
        SocketAddr::from_str(&format!("{}:{}", args.bind, args.port)).unwrap()
    };

    if !bind_is_loopback {
        warn!("{}", include_str!("./security.txt"));
    }
    if skip_auth {
        warn!("HTTP authentication is disabled by --insecure-no-auth");
    } else {
        info!("The auth token is in the file: {conf_path}");
    }

    info!(
        "Starting Cozo ({}-backed) API at http://{}",
        args.engine, addr
    );

    let listener = TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app.into_make_service())
        .await
        .unwrap();
}

#[derive(serde_derive::Deserialize)]
struct StartTransactPayload {
    write: bool,
}

fn acquire_transaction(
    st: &DbState,
    id: u32,
    auth: &AuthContext,
) -> Result<(Arc<MultiTransaction>, TransactionLease), (StatusCode, Json<serde_json::Value>)> {
    let mut txs = st.txs.lock().unwrap();
    let stored = txs
        .get_mut(&id)
        .ok_or_else(|| (StatusCode::NOT_FOUND, json!({"ok": false}).into()))?;
    if stored.owner != auth.principal {
        return Err((StatusCode::FORBIDDEN, json!({"ok": false}).into()));
    }
    if stored.write && matches!(auth.mutability, ScriptMutability::Immutable) {
        return Err((
            StatusCode::FORBIDDEN,
            json!({"ok": false, "message": "write transaction requires a mutable token"}).into(),
        ));
    }
    if stored.in_flight != 0 {
        return Err((
            StatusCode::CONFLICT,
            json!({"ok": false, "message": "transaction already has an operation in progress"})
                .into(),
        ));
    }
    stored.in_flight += 1;
    let tx = stored.tx.clone();
    drop(txs);
    Ok((
        tx,
        TransactionLease {
            txs: st.txs.clone(),
            id,
        },
    ))
}

async fn start_transact(
    Extension(auth): Extension<AuthContext>,
    State(st): State<DbState>,
    Query(payload): Query<StartTransactPayload>,
) -> (StatusCode, Json<serde_json::Value>) {
    if payload.write && matches!(auth.mutability, ScriptMutability::Immutable) {
        return (StatusCode::FORBIDDEN, json!({"ok": false}).into());
    }
    let tx = st.db.multi_transaction(payload.write);
    let id = st.tx_counter.fetch_add(1, Ordering::SeqCst);
    st.txs.lock().unwrap().insert(
        id,
        StoredTransaction {
            tx: Arc::new(tx),
            owner: auth.principal,
            write: payload.write,
            expires_at: Instant::now() + TRANSACTION_IDLE_TIMEOUT,
            in_flight: 0,
        },
    );
    (StatusCode::OK, json!({"ok": true, "id": id}).into())
}

async fn transact_query(
    Extension(auth): Extension<AuthContext>,
    State(st): State<DbState>,
    Path(id): Path<u32>,
    Json(payload): Json<QueryPayload>,
) -> (StatusCode, Json<serde_json::Value>) {
    let (tx, lease) = match acquire_transaction(&st, id, &auth) {
        Ok(acquired) => acquired,
        Err(response) => return response,
    };
    let src = payload.script.clone();
    let result = spawn_blocking(move || {
        let _lease = lease;
        let params = payload
            .params
            .into_iter()
            .map(|(k, v)| (k, DataValue::from(v)))
            .collect();
        let query = payload.script;
        tx.run_script(&query, params)
    })
    .await;
    match result {
        Ok(Ok(res)) => (StatusCode::OK, res.into_json().into()),
        Ok(Err(err)) => (
            StatusCode::BAD_REQUEST,
            format_error_as_json(err, Some(&src)).into(),
        ),
        Err(err) => internal_error(err),
    }
}

#[derive(serde_derive::Deserialize)]
struct FinishTransactPayload {
    abort: bool,
}

async fn finish_query(
    Extension(auth): Extension<AuthContext>,
    State(st): State<DbState>,
    Path(id): Path<u32>,
    Json(payload): Json<FinishTransactPayload>,
) -> (StatusCode, Json<serde_json::Value>) {
    let tx = match st.txs.lock().unwrap().entry(id) {
        std::collections::btree_map::Entry::Vacant(_) => {
            return (StatusCode::NOT_FOUND, json!({"ok": false}).into())
        }
        std::collections::btree_map::Entry::Occupied(entry)
            if entry.get().owner != auth.principal =>
        {
            return (StatusCode::FORBIDDEN, json!({"ok": false}).into())
        }
        std::collections::btree_map::Entry::Occupied(entry)
            if entry.get().write
                && matches!(auth.mutability, ScriptMutability::Immutable)
                && !payload.abort =>
        {
            return (
                StatusCode::FORBIDDEN,
                json!({"ok": false, "message": "write transaction requires a mutable token"})
                    .into(),
            )
        }
        std::collections::btree_map::Entry::Occupied(entry) if entry.get().in_flight != 0 => {
            return (
                StatusCode::CONFLICT,
                json!({"ok": false, "message": "transaction has an operation in progress"}).into(),
            )
        }
        std::collections::btree_map::Entry::Occupied(entry) => entry.remove().tx,
    };
    let res = if payload.abort {
        tx.abort()
    } else {
        tx.commit()
    };
    match res {
        Ok(_) => (StatusCode::OK, json!({"ok": true}).into()),
        Err(err) => (
            StatusCode::BAD_REQUEST,
            json!({"ok": false, "message": err.to_string()}).into(),
        ),
    }
}

#[derive(serde_derive::Deserialize)]
struct QueryPayload {
    script: String,
    params: BTreeMap<String, serde_json::Value>,
    immutable: Option<bool>,
    /// Per-call wall-clock budget in seconds (mnestic fork, query budget).
    /// Optional; combined via `min` with any in-script `:timeout` and the
    /// server's `--default-query-timeout`. Ignored on the `/transact/:id` path.
    timeout: Option<f64>,
}

async fn text_query(
    Extension(mutability): Extension<ScriptMutability>,
    State(st): State<DbState>,
    Json(payload): Json<QueryPayload>,
) -> (StatusCode, Json<serde_json::Value>) {
    let params = payload
        .params
        .into_iter()
        .map(|(k, v)| (k, DataValue::from(v)))
        .collect();
    let immutable = match mutability {
        ScriptMutability::Mutable => payload.immutable.unwrap_or(false),
        ScriptMutability::Immutable => true,
    };
    let options = ScriptRunOptions {
        timeout: payload.timeout,
    };
    let result = spawn_blocking(move || {
        st.db.run_script_fold_err_with_options(
            &payload.script,
            params,
            if immutable {
                ScriptMutability::Immutable
            } else {
                ScriptMutability::Mutable
            },
            options,
        )
    })
    .await;
    match result {
        Ok(res) => wrap_json(res),
        Err(err) => internal_error(err),
    }
}

async fn export_relations(
    State(st): State<DbState>,
    Path(relations): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let relations = relations
        .split(',')
        .filter_map(|t| {
            if t.is_empty() {
                None
            } else {
                Some(t.to_string())
            }
        })
        .collect_vec();
    let result = spawn_blocking(move || st.db.export_relations(relations.iter())).await;
    match result {
        Ok(Ok(s)) => {
            let s: serde_json::Map<_, _> = s.into_iter().map(|(k, v)| (k, v.into_json())).collect();
            let ret = json!({"ok": true, "data": s});
            (StatusCode::OK, ret.into())
        }
        Ok(Err(err)) => {
            let ret = json!({"ok": false, "message": err.to_string()});
            (StatusCode::BAD_REQUEST, ret.into())
        }
        Err(err) => internal_error(err),
    }
}

async fn import_relations(
    Extension(mutability): Extension<ScriptMutability>,
    State(st): State<DbState>,
    Json(payload): Json<serde_json::Value>,
) -> (StatusCode, Json<serde_json::Value>) {
    if let Err(response) = require_mutable(mutability) {
        return response;
    }

    let payload = match payload.as_object() {
        None => {
            return (
                StatusCode::BAD_REQUEST,
                json!({"ok": false, "message": "payload must be a JSON object"}).into(),
            );
        }
        Some(pl) => {
            let mut ret = BTreeMap::new();
            for (k, v) in pl {
                let nr = match NamedRows::from_json(v) {
                    Ok(p) => p,
                    Err(err) => {
                        return (
                            StatusCode::BAD_REQUEST,
                            json!({"ok": false, "message": err.to_string()}).into(),
                        );
                    }
                };
                ret.insert(k.to_string(), nr);
            }
            ret
        }
    };

    let result = spawn_blocking(move || st.db.import_relations(payload)).await;
    match result {
        Ok(Ok(_)) => (StatusCode::OK, json!({"ok": true}).into()),
        Ok(Err(err)) => {
            let ret = json!({"ok": false, "message": err.to_string()});
            (StatusCode::BAD_REQUEST, ret.into())
        }
        Err(err) => internal_error(err),
    }
}

#[derive(serde_derive::Deserialize)]
struct BackupPayload {
    path: String,
}

fn resolve_backup_path(backup_dir: &FsPath, requested: &str) -> miette::Result<PathBuf> {
    let requested = FsPath::new(requested);
    if requested.as_os_str().is_empty() || requested.is_absolute() {
        return Err(miette!(
            "backup path must be relative to the HTTP backup directory"
        ));
    }

    let candidate = backup_dir.join(requested);
    let parent = candidate
        .parent()
        .ok_or_else(|| miette!("backup path has no parent directory"))?;
    let resolved_parent =
        std::fs::canonicalize(parent).map_err(|_| miette!("backup path parent does not exist"))?;
    if !resolved_parent.starts_with(backup_dir) {
        return Err(miette!("backup path escapes the HTTP backup directory"));
    }

    // Canonicalize an existing file as well, so a symlink cannot point outside
    // the configured directory. New files inherit the checked parent.
    match std::fs::symlink_metadata(&candidate) {
        Ok(_) => {
            let resolved = std::fs::canonicalize(&candidate)
                .map_err(|_| miette!("cannot resolve backup path"))?;
            if !resolved.starts_with(backup_dir) {
                return Err(miette!("backup path escapes the HTTP backup directory"));
            }
            Ok(resolved)
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            Ok(resolved_parent.join(candidate.file_name().unwrap()))
        }
        Err(_) => Err(miette!("cannot inspect backup path")),
    }
}

#[cfg(test)]
mod backup_path_tests {
    use super::resolve_backup_path;
    use std::fs;

    #[test]
    fn confines_http_backup_paths_to_configured_directory() {
        let root = std::env::temp_dir().join(format!("cozo-backup-path-{}", std::process::id()));
        let backups = root.join("backups");
        fs::create_dir_all(backups.join("nested")).unwrap();
        let backups = fs::canonicalize(backups).unwrap();

        assert_eq!(
            resolve_backup_path(&backups, "nested/safe.db").unwrap(),
            backups.join("nested/safe.db")
        );
        assert!(resolve_backup_path(&backups, "/tmp/outside.db").is_err());
        assert!(resolve_backup_path(&backups, "../outside.db").is_err());

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(root.join("missing-outside.db"), backups.join("link.db"))
                .unwrap();
            assert!(resolve_backup_path(&backups, "link.db").is_err());
        }

        fs::remove_dir_all(root).unwrap();
    }
}

async fn backup(
    Extension(mutability): Extension<ScriptMutability>,
    State(st): State<DbState>,
    Json(payload): Json<BackupPayload>,
) -> (StatusCode, Json<serde_json::Value>) {
    if let Err(response) = require_mutable(mutability) {
        return response;
    }

    let path = match resolve_backup_path(&st.backup_dir, &payload.path) {
        Ok(path) => path,
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                json!({"ok": false, "message": err.to_string()}).into(),
            );
        }
    };
    let result = spawn_blocking(move || st.db.backup_db(path)).await;

    match result {
        Ok(Ok(())) => {
            let ret = json!({"ok": true});
            (StatusCode::OK, ret.into())
        }
        Ok(Err(err)) => {
            let ret = json!({"ok": false, "message": err.to_string()});
            (StatusCode::BAD_REQUEST, ret.into())
        }
        Err(err) => internal_error(err),
    }
}

#[derive(serde_derive::Deserialize)]
struct BackupImportPayload {
    path: String,
    relations: Vec<String>,
}

async fn import_from_backup(
    Extension(mutability): Extension<ScriptMutability>,
    State(st): State<DbState>,
    Json(payload): Json<BackupImportPayload>,
) -> (StatusCode, Json<serde_json::Value>) {
    if let Err(response) = require_mutable(mutability) {
        return response;
    }

    let path = match resolve_backup_path(&st.backup_dir, &payload.path) {
        Ok(path) => path,
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                json!({"ok": false, "message": err.to_string()}).into(),
            );
        }
    };
    let result = spawn_blocking(move || st.db.import_from_backup(path, &payload.relations)).await;

    match result {
        Ok(Ok(())) => {
            let ret = json!({"ok": true});
            (StatusCode::OK, ret.into())
        }
        Ok(Err(err)) => {
            let ret = json!({"ok": false, "message": err.to_string()});
            (StatusCode::BAD_REQUEST, ret.into())
        }
        Err(err) => internal_error(err),
    }
}

#[derive(serde_derive::Deserialize)]
struct RuleRegisterOptions {
    arity: usize,
}

async fn post_rule_result(
    State(st): State<DbState>,
    Path(id): Path<u32>,
    Json(res): Json<serde_json::Value>,
) -> (StatusCode, Json<serde_json::Value>) {
    let res = match NamedRows::from_json(&res) {
        Ok(res) => res,
        Err(err) => {
            if let Some(ch) = st.rule_senders.lock().unwrap().remove(&id) {
                let _ = ch.send(Err(miette!("downstream posted malformed result")));
            }
            return (
                StatusCode::BAD_REQUEST,
                json!({"ok": false, "message": err.to_string()}).into(),
            );
        }
    };
    if let Some(ch) = st.rule_senders.lock().unwrap().remove(&id) {
        match ch.send(Ok(res)) {
            Ok(_) => (StatusCode::OK, json!({"ok": true}).into()),
            Err(err) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"ok": false, "message": err.to_string()}).into(),
            ),
        }
    } else {
        (StatusCode::NOT_FOUND, json!({"ok": false}).into())
    }
}

async fn post_rule_err(
    State(st): State<DbState>,
    Path(id): Path<u32>,
) -> (StatusCode, Json<serde_json::Value>) {
    if let Some(ch) = st.rule_senders.lock().unwrap().remove(&id) {
        match ch.send(Err(miette!("downstream cancelled computation"))) {
            Ok(_) => (StatusCode::OK, json!({"ok": true}).into()),
            Err(err) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"ok": false, "message": err.to_string()}).into(),
            ),
        }
    } else {
        (StatusCode::NOT_FOUND, json!({"ok": false}).into())
    }
}

async fn register_rule(
    State(st): State<DbState>,
    Path(name): Path<String>,
    Query(rule_opts): Query<RuleRegisterOptions>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let (rule, task_receiver) = SimpleFixedRule::rule_with_channel(rule_opts.arity);
    let (down_sender, mut down_receiver) = tokio::sync::mpsc::channel(1);
    let mut errored = None;

    if let Err(err) = st.db.register_fixed_rule(name.clone(), rule) {
        errored = Some(err);
    } else {
        let rule_senders = st.rule_senders.clone();
        let rule_counter = st.rule_counter.clone();

        rayon::spawn(move || {
            for (inputs, options, sender) in task_receiver {
                let id = rule_counter.fetch_add(1, Ordering::AcqRel);
                let inputs: serde_json::Value =
                    inputs.into_iter().map(|ip| ip.into_json()).collect();
                let options: serde_json::Value = options
                    .into_iter()
                    .map(|(k, v)| (k, serde_json::Value::from(v)))
                    .collect();
                if down_sender.blocking_send((id, inputs, options)).is_err() {
                    let _ = sender.send(Err(miette!("cannot send request to downstream")));
                } else {
                    rule_senders.lock().unwrap().insert(id, sender);
                }
            }
        });
    }

    struct Guard {
        name: String,
        db: DbInstance,
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            info!("dropping rules SSE {}", self.name);
            let _ = self.db.unregister_fixed_rule(&self.name);
        }
    }

    let stream = async_stream::stream! {
        if let Some(err) = errored {
            let item = json!({"type": "register-error", "error": err.to_string()});
            yield Ok(Event::default().json_data(item).unwrap());
        } else {
            info!("starting rule SSE {}", name);
            let _guard = Guard {db: st.db, name};
            while let Some((id, inputs, options)) = down_receiver.recv().await {
                let item = json!({"type": "request", "id": id, "inputs": inputs, "options": options});
                yield Ok(Event::default().json_data(item).unwrap());
            }
        }
    };
    Sse::new(stream).keep_alive(KeepAlive::default())
}

async fn observe_changes(
    State(st): State<DbState>,
    Path(relation): Path<String>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let (id, recv) = st.db.register_callback(&relation, None);
    let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
    struct Guard {
        id: u32,
        db: DbInstance,
        relation: String,
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            info!("dropping changes SSE {}: {}", self.relation, self.id);
            self.db.unregister_callback(self.id);
        }
    }

    spawn_blocking(move || {
        for data in recv {
            sender.blocking_send(data).unwrap();
        }
    });
    let stream = async_stream::stream! {
        info!("starting changes SSE {}: {}", relation, id);
        let _guard = Guard {id, db: st.db, relation};
        while let Some((op, new, old)) = receiver.recv().await {
            let item = json!({"op": op.to_string(), "new_rows": new.into_json(), "old_rows": old.into_json()});
            yield Ok(Event::default().json_data(item).unwrap());
        }
    };
    Sse::new(stream).keep_alive(KeepAlive::default())
}

async fn root() -> Html<&'static str> {
    Html(include_str!("./index.html"))
}

fn internal_error<E>(err: E) -> (StatusCode, Json<serde_json::Value>)
where
    E: std::error::Error,
{
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        json!({"ok": false, "message": err.to_string()}).into(),
    )
}

fn wrap_json(json: serde_json::Value) -> (StatusCode, Json<serde_json::Value>) {
    let code = if let Some(serde_json::Value::Bool(true)) = json.get("ok") {
        StatusCode::OK
    } else {
        StatusCode::BAD_REQUEST
    };
    (code, json.into())
}

pub async fn not_found(uri: axum::http::Uri) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::NOT_FOUND,
        json!({"ok": false, "message": format!("No route {}", uri)}).into(),
    )
}
