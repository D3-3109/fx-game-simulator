use crate::{
    app::{App, Event},
    storage::{self, Error},
    trading, web,
};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{
        DefaultBodyLimit, Query, State,
        rejection::BytesRejection,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::get,
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::Duration};

type ApiResult = Result<Value, Failure>;
struct Failure(StatusCode, String);
impl From<String> for Failure {
    fn from(s: String) -> Self {
        Self(StatusCode::BAD_REQUEST, s)
    }
}
impl From<Error> for Failure {
    fn from(e: Error) -> Self {
        match e {
            Error::Invalid(s) => s.into(),
            _ => Self(
                StatusCode::SERVICE_UNAVAILABLE,
                "保存或读取失败，账户未改动；可重试".into(),
            ),
        }
    }
}
impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"ok":false,"err":self.1}))).into_response()
    }
}
fn response(result: ApiResult) -> Response {
    match result {
        Ok(value) => Json(value).into_response(),
        Err(e) => e.into_response(),
    }
}
fn bad(s: &str) -> Failure {
    Failure(StatusCode::BAD_REQUEST, s.into())
}
fn upstream(s: String) -> Failure {
    Failure(StatusCode::BAD_GATEWAY, s)
}
fn bearer(headers: &HeaderMap) -> &str {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .unwrap_or("")
}
fn text<'a>(body: &'a Value, key: &str) -> Result<&'a str, Failure> {
    body[key]
        .as_str()
        .ok_or_else(|| bad(&format!("{key} 无效")))
}
fn number(body: &Value, key: &str) -> Result<f64, Failure> {
    body[key]
        .as_f64()
        .or_else(|| body[key].as_str().and_then(|s| s.parse().ok()))
        .filter(|x| x.is_finite())
        .ok_or_else(|| bad(&format!("{key} 无效")))
}
fn integer(body: &Value, key: &str) -> Result<u64, Failure> {
    let n = number(body, key)?;
    if n < 0.0 || n.fract() != 0.0 || n > trading::MAX_LEVERAGE as f64 {
        return Err(bad(&format!("{key} 无效")));
    }
    Ok(n as u64)
}
fn params(uri: &Uri) -> Result<BTreeMap<String, String>, Failure> {
    Query::<BTreeMap<String, String>>::try_from_uri(uri)
        .map(|q| q.0)
        .map_err(|_| bad("查询参数无效"))
}
async fn user(app: &App, headers: &HeaderMap) -> Result<String, Failure> {
    app.store
        .authenticate(bearer(headers))
        .await?
        .ok_or_else(|| Failure(StatusCode::UNAUTHORIZED, "auth".into()))
}
fn admin(app: &App, headers: &HeaderMap) -> Result<(), Failure> {
    if app.admin_authorized(bearer(headers), storage::now_ms()) {
        Ok(())
    } else {
        Err(Failure(StatusCode::UNAUTHORIZED, "管理员登录已失效".into()))
    }
}

pub fn router(app: Arc<App>, static_dir: PathBuf) -> Router {
    Router::new()
        .route("/ws", get(ws_upgrade))
        .route("/api/{*path}", get(api_get).post(api_post))
        .layer(DefaultBodyLimit::max(65536))
        .with_state(app)
        .merge(web::static_router(static_dir))
}
async fn api_get(State(app): State<Arc<App>>, uri: Uri, headers: HeaderMap) -> Response {
    response(get_inner(&app, &uri, &headers).await)
}
async fn get_inner(app: &App, uri: &Uri, headers: &HeaderMap) -> ApiResult {
    let path = uri.path();
    if path.starts_with("/api/admin/") {
        admin(app, headers)?;
        return match path {
            "/api/admin/users" => {
                let totals = app.store.deposit_totals().await?;
                Ok(
                    json!({"ok":true,"rows":app.store.users().await?.into_iter().map(|u|json!({"name":u.name,"balance":u.account.balance,"deposited":totals.get(&u.name).copied().unwrap_or(0.0),"positions":u.account.positions.len()})).collect::<Vec<_>>()}),
                )
            }
            "/api/admin/deposits" => Ok(json!({"ok":true,"rows":app.store.events().await?})),
            "/api/admin/symbols" => {
                let mut v = app.store.settings().await?;
                v["ok"] = json!(true);
                Ok(v)
            }
            _ => Err(Failure(StatusCode::NOT_FOUND, "unknown".into())),
        };
    }
    match path {
        "/api/symbols" => {
            let mut v = app.store.settings().await?;
            v["ok"] = json!(true);
            Ok(v)
        }
        "/api/leaderboard" => app.leaderboard().await.map_err(upstream),
        "/api/klines" => {
            let q = params(uri)?;
            let symbol = q
                .get("symbol")
                .map(|s| s.to_uppercase())
                .unwrap_or("USDJPY".into());
            let interval = q.get("interval").map(String::as_str).unwrap_or("15m");
            let limit = q
                .get("limit")
                .map(|s| s.parse::<usize>())
                .transpose()
                .map_err(|_| bad("K线数量无效"))?
                .unwrap_or(500)
                .min(1000);
            app.market
                .klines(&symbol, interval, limit)
                .await
                .map_err(upstream)
        }
        "/api/me" => Ok(json!({"ok":true,"name":user(app,headers).await?})),
        "/api/state" => app
            .state(&user(app, headers).await?)
            .await
            .map_err(upstream),
        "/api/history" => {
            Ok(json!({"ok":true,"rows":app.store.history(&user(app,headers).await?).await?}))
        }
        "/api/chat" => {
            user(app, headers).await?;
            Ok(json!({"ok":true,"rows":app.store.chat().await?}))
        }
        _ => Err(Failure(StatusCode::NOT_FOUND, "unknown".into())),
    }
}
async fn api_post(
    State(app): State<Arc<App>>,
    uri: Uri,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let bytes = match body {
        Ok(b) => b,
        Err(_) => return Failure(StatusCode::PAYLOAD_TOO_LARGE, "请求过大".into()).into_response(),
    };
    let body = match serde_json::from_slice::<Value>(&bytes) {
        Ok(v) if v.is_object() => v,
        _ => return bad("请求JSON无效").into_response(),
    };
    response(post_inner(&app, uri.path(), &headers, &body).await)
}
async fn post_inner(app: &App, path: &str, headers: &HeaderMap, body: &Value) -> ApiResult {
    let now = storage::now_ms();
    if path == "/api/admin/login" {
        let token = app
            .admin_login(body["psk"].as_str().unwrap_or(""), now)
            .await?;
        return Ok(json!({"ok":true,"token":token}));
    }
    if path.starts_with("/api/admin/") {
        admin(app, headers)?;
        return match path {
            "/api/admin/logout" => {
                app.admin_logout(bearer(headers));
                Ok(json!({"ok":true}))
            }
            "/api/admin/deposit" => Ok(
                json!({"ok":true,"row":app.store.deposit(text(body,"name")?,number(body,"amount")?,text(body,"request_id")?,now).await?}),
            ),
            "/api/admin/delete" => {
                let name = text(body, "name")?;
                let row = app
                    .store
                    .delete(name, text(body, "confirmation")?, now)
                    .await?;
                app.deleted(name);
                Ok(json!({"ok":true,"row":row}))
            }
            "/api/admin/symbols" => {
                let symbol = text(body, "symbol")?.to_uppercase();
                let enabled = body["enabled"]
                    .as_bool()
                    .ok_or_else(|| bad("币种对或启用状态无效"))?;
                let mut value = app.store.set_symbol(&symbol, enabled, now).await?;
                let mut packet = value.clone();
                packet["type"] = json!("symbols");
                app.broadcast(packet);
                value["ok"] = json!(true);
                Ok(value)
            }
            _ => Err(Failure(StatusCode::NOT_FOUND, "unknown".into())),
        };
    }
    if path == "/api/auth" {
        let name = text(body, "name")?.trim();
        let pwd = text(body, "pwd")?;
        let token = if body["action"] == "register" {
            app.store.register(name, pwd).await?
        } else {
            app.store.login(name, pwd).await?
        };
        return Ok(json!({"ok":true,"token":token,"name":name}));
    }
    let name = user(app, headers).await?;
    match path {
        "/api/chat" => {
            let row = app.store.post_chat(&name, text(body, "text")?, now).await?;
            app.broadcast(json!({"type":"chat","row":row}));
            return Ok(json!({"ok":true,"row":row}));
        }
        "/api/trade" => {
            let side = match text(body, "side")? {
                "buy" | "long" => "long",
                "sell" | "short" => "short",
                _ => return Err(bad("交易方向无效")),
            };
            let lots = number(body, "lots")?;
            let u = app.store.user(&name).await?;
            app.store.require_tradable(&u.symbol).await?;
            let prices = app
                .market
                .trading_prices(
                    std::iter::once(u.symbol.as_str())
                        .chain(u.account.positions.iter().map(|p| p.symbol.as_str())),
                )
                .await?;
            app.store
                .transaction(&name, |a, symbol, id| {
                    let mut trades = vec![a.open(side, lots, symbol, &prices, id, now)?];
                    trades.extend(a.check_stopout(&prices, now)?);
                    Ok(trades)
                })
                .await?;
        }
        "/api/close" => {
            let id = if body["id"].is_null() {
                None
            } else {
                let id = integer(body, "id")?;
                if id == 0 { None } else { Some(id) }
            };
            let u = app.store.user(&name).await?;
            let prices = app
                .market
                .trading_prices(
                    u.account
                        .positions
                        .iter()
                        .filter(|p| id.is_none_or(|id| id == p.id))
                        .map(|p| p.symbol.as_str()),
                )
                .await?;
            app.store
                .transaction(&name, |a, _, _| a.close(id, &prices, now))
                .await?;
        }
        "/api/leverage" => {
            let leverage = integer(body, "leverage")?;
            if leverage == 0 {
                return Err(bad("杠杆必须为有效正整数"));
            }
            app.store
                .transaction(&name, |a, _, _| {
                    if !a.positions.is_empty() {
                        return Err("先平仓".into());
                    }
                    a.leverage = leverage;
                    Ok(vec![])
                })
                .await?;
        }
        "/api/symbol" => {
            app.store
                .select_symbol(&name, &text(body, "symbol")?.to_uppercase())
                .await?
        }
        _ => return Err(Failure(StatusCode::NOT_FOUND, "unknown".into())),
    }
    Ok(json!({"ok":true}))
}

async fn ws_upgrade(State(app): State<Arc<App>>, uri: Uri, upgrade: WebSocketUpgrade) -> Response {
    let q = match params(&uri) {
        Ok(q) => q,
        Err(e) => return e.into_response(),
    };
    let token = q.get("token").cloned().unwrap_or_default();
    match app.store.authenticate(&token).await {
        Ok(Some(name)) => upgrade
            .max_message_size(65536)
            .max_frame_size(65536)
            .on_upgrade(move |socket| serve_ws(socket, app, name, token))
            .into_response(),
        _ => Failure(StatusCode::UNAUTHORIZED, "auth".into()).into_response(),
    }
}
async fn send(socket: &mut WebSocket, value: Value) -> bool {
    tokio::time::timeout(
        Duration::from_secs(5),
        socket.send(Message::Text(value.to_string().into())),
    )
    .await
    .is_ok_and(|r| r.is_ok())
}
async fn serve_ws(mut socket: WebSocket, app: Arc<App>, name: String, token: String) {
    let mut events = app.events.subscribe();
    // Recheck after upgrade to close the deletion/handshake race.
    if app
        .store
        .authenticate(&token)
        .await
        .ok()
        .flatten()
        .as_deref()
        != Some(&name)
    {
        return;
    }
    let _presence = app.connected(&name);
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    loop {
        if app.shutting_down.load(std::sync::atomic::Ordering::SeqCst) {
            break;
        }
        tokio::select! {
            incoming=socket.recv()=>match incoming {
                Some(Ok(Message::Ping(data)))=> {if !tokio::time::timeout(Duration::from_secs(5),socket.send(Message::Pong(data))).await.is_ok_and(|r|r.is_ok()) {break;}},
                Some(Ok(Message::Close(_)))|None|Some(Err(_))=>break,
                _=>{},
            },
            event=events.recv()=>match event {
                Ok(Event::Message(value))=>if !send(&mut socket,value).await {break;},
                Ok(Event::Deleted(owner))=>if owner==name {break;},
                Ok(Event::Shutdown)=>break,
                Err(_)=>break,
            },
            _=interval.tick()=> {
                if app.store.authenticate(&token).await.ok().flatten().as_deref()!=Some(&name) {break;}
                if let Ok(state)=app.state(&name).await && !send(&mut socket,state).await {break;}
            }
        }
    }
    let _ = tokio::time::timeout(Duration::from_secs(2), socket.send(Message::Close(None))).await;
}
