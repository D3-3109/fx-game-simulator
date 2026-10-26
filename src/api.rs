use crate::{
    app::{App, Event},
    limits::{SocketSlot, client_key},
    market::NstarDesk,
    storage::{self, Error},
    symbols, trading, web,
};
use axum::{
    Extension, Json, Router,
    body::Bytes,
    extract::{
        ConnectInfo, DefaultBodyLimit, Query, Request, State,
        rejection::BytesRejection,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, HeaderValue, StatusCode, Uri, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use futures_util::{SinkExt, StreamExt};
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
fn require_symbol(app: &App, symbol: &str) -> Result<(), Failure> {
    if app.store.is_nstar_only() && symbol != symbols::NSTAR {
        return Err(bad("仅支持 NSTAR"));
    }
    Ok(())
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
/// Optional non-negative price field; absent, null or empty means 0 (unset).
fn optional_price(body: &Value, key: &str) -> Result<f64, Failure> {
    match &body[key] {
        Value::Null => Ok(0.0),
        Value::String(s) if s.trim().is_empty() => Ok(0.0),
        _ => number(body, key).and_then(|v| {
            if v >= 0.0 {
                Ok(v)
            } else {
                Err(bad(&format!("{key} 无效")))
            }
        }),
    }
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
    if n < 0.0 || n.fract() != 0.0 || n > u64::MAX as f64 {
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
        .layer(middleware::from_fn_with_state(app.clone(), guard))
        .with_state(app)
        .merge(web::static_router(static_dir))
}
/// Hold the NSTAR book for orders touching it, so each fill is priced off the latest mid.
async fn nstar_desk<'a>(app: &'a App, symbols: &[&str]) -> Option<NstarDesk<'a>> {
    if symbols.contains(&symbols::NSTAR) {
        Some(app.market.nstar_desk().await)
    } else {
        None
    }
}
/// Apply committed NSTAR flow and publish the moved price and any whale alerts.
async fn commit(app: &App, desk: Option<NstarDesk<'_>>) {
    let Some(desk) = desk else { return };
    match desk.commit().await {
        Ok((packet, events)) => {
            app.broadcast(packet);
            events.into_iter().for_each(|e| app.broadcast(e));
        }
        Err(_) => eprintln!("[nstar] order settlement broadcast failed; tick will retry"),
    }
}
/// Rate-limit key of the caller, set by `guard` for every API and WebSocket request.
#[derive(Clone)]
struct Client(String);

fn too_many(retry: u64) -> Response {
    let mut r = Failure(
        StatusCode::TOO_MANY_REQUESTS,
        format!("请求过于频繁，请 {retry} 秒后重试"),
    )
    .into_response();
    r.headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from(retry));
    r
}
/// Per-client token buckets in front of every API route and WebSocket handshake.
async fn guard(State(app): State<Arc<App>>, mut req: Request, next: Next) -> Response {
    let peer = req
        .extensions()
        .get::<ConnectInfo<std::net::SocketAddr>>()
        .map(|c| c.0.ip());
    let client = client_key(app.client_ip(peer, req.headers()));
    let path = req.uri().path();
    let limits = app.limits;
    let now = std::time::Instant::now();
    let mut checks = vec![if path == "/ws" {
        ("ws", limits.ws)
    } else {
        ("api", limits.api)
    }];
    if path == "/api/auth" || path == "/api/admin/login" {
        checks.push(("auth", limits.auth));
    }
    for (kind, rate) in checks {
        if let Err(retry) = app.limiter.check(&client, kind, rate, now) {
            return too_many(retry);
        }
    }
    req.extensions_mut().insert(Client(client));
    next.run(req).await
}
async fn api_get(State(app): State<Arc<App>>, uri: Uri, headers: HeaderMap) -> Response {
    response(get_inner(&app, &uri, &headers).await)
}
async fn get_inner(app: &App, uri: &Uri, headers: &HeaderMap) -> ApiResult {
    let path = uri.path();
    if path.starts_with("/api/admin/") {
        admin(app, headers)?;
        return match path {
            "/api/admin/performance" => Ok(
                json!({"ok":true,"persistence":app.store.persistence_stats(),
                "pending_bars":app.market.pending_bar_count()}),
            ),
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
        "/api/leaderboard" => {
            let q = params(uri)?;
            let parse = |key: &str, default| -> Result<usize, Failure> {
                q.get(key)
                    .map(|s| s.parse::<usize>())
                    .transpose()
                    .map_err(|_| bad("分页参数无效"))
                    .map(|n| n.unwrap_or(default))
            };
            let offset = parse("offset", 0)?;
            let limit = parse("limit", 50)?.min(100);
            let name = app.store.authenticate(bearer(headers)).await?;
            app.leaderboard_page(name.as_deref(), offset, limit)
                .await
                .map_err(upstream)
        }
        "/api/market" => {
            let mut v = app.market.nstar_stats().await;
            v["ok"] = json!(true);
            Ok(v)
        }
        "/api/klines" => {
            let q = params(uri)?;
            let symbol = q
                .get("symbol")
                .map(|s| s.to_uppercase())
                .unwrap_or_else(|| {
                    if app.store.is_nstar_only() {
                        symbols::NSTAR
                    } else {
                        "USDJPY"
                    }
                    .into()
                });
            require_symbol(app, &symbol)?;
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
    Extension(Client(client)): Extension<Client>,
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
    response(post_inner(&app, &client, uri.path(), &headers, &body).await)
}
async fn post_inner(
    app: &App,
    client: &str,
    path: &str,
    headers: &HeaderMap,
    body: &Value,
) -> ApiResult {
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
                require_symbol(app, &symbol)?;
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
            storage::validate_credentials(name, pwd)?;
            app.store
                .register_from(name, pwd, client, &app.limits.register, now)
                .await?
        } else {
            app.store.login(name, pwd).await?
        };
        return Ok(json!({"ok":true,"token":token,"name":name}));
    }
    let name = user(app, headers).await?;
    if app.store.is_nstar_only()
        && matches!(path, "/api/trade" | "/api/order" | "/api/spot")
        && body.get("symbol").is_some()
    {
        require_symbol(app, &text(body, "symbol")?.to_uppercase())?;
    }
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
            let (tp, sl) = (optional_price(body, "tp")?, optional_price(body, "sl")?);
            let u = app.store.user(&name).await?;
            app.store.require_tradable(&u.symbol).await?;
            let symbols: Vec<&str> = std::iter::once(u.symbol.as_str())
                .chain(u.account.symbols())
                .collect();
            let mut prices = app
                .market
                .trading_prices(symbols.iter().copied().filter(|s| *s != symbols::NSTAR))
                .await?;
            let desk = nstar_desk(app, &symbols).await;
            if let Some(desk) = &desk {
                desk.price(&mut prices)?;
            }
            let result = app
                .store
                .transaction(&name, |a, symbol, id| {
                    let trade = a.open(side, lots, symbol, &prices, id, now)?;
                    let mut after = prices.clone();
                    trading::apply_fill(&mut after, &trade)?;
                    if tp > 0.0 || sl > 0.0 {
                        // Rejecting a bad TP/SL rolls the whole order back.
                        a.set_triggers(id, tp, sl, &after)?;
                    }
                    let mut trades = vec![trade];
                    trades.extend(a.check_stopout(&after, now)?);
                    Ok(trades)
                })
                .await;
            commit(app, desk).await;
            result?;
        }
        "/api/order" => {
            let spot = body["market"].as_str() == Some("spot");
            let side = match (text(body, "side")?, spot) {
                ("buy", true) => "buy",
                ("sell", true) => "sell",
                ("buy" | "long", false) => "long",
                ("sell" | "short", false) => "short",
                _ => return Err(bad("交易方向或市场无效")),
            };
            let kind = text(body, "kind")?.to_owned();
            let lots = if spot {
                number(body, "units")? / trading::contract(symbols::NSTAR)
            } else {
                number(body, "lots")?
            };
            let price = number(body, "price")?;
            let limit = optional_price(body, "limit")?;
            let (tp, sl) = (optional_price(body, "tp")?, optional_price(body, "sl")?);
            let u = app.store.user(&name).await?;
            app.store.require_tradable(&u.symbol).await?;
            let symbols: Vec<&str> = std::iter::once(u.symbol.as_str())
                .chain(u.account.symbols())
                .collect();
            let mut prices = app
                .market
                .trading_prices(symbols.iter().copied().filter(|s| *s != symbols::NSTAR))
                .await?;
            let desk = nstar_desk(app, &symbols).await;
            if let Some(desk) = &desk {
                desk.price(&mut prices)?;
            }
            let result = app
                .store
                .transaction(&name, |a, symbol, id| {
                    a.place_order(
                        &kind, side, lots, symbol, price, limit, tp, sl, &prices, now,
                    )?;
                    // A limit already through the market fills right away, as far as it can.
                    let mut trades = a.check_orders(&prices, id, now)?;
                    let mut after = prices.clone();
                    for t in &trades {
                        trading::apply_fill(&mut after, t)?;
                    }
                    trades.extend(a.check_stopout(&after, now)?);
                    Ok(trades)
                })
                .await;
            commit(app, desk).await;
            result?;
        }
        "/api/cancel" => {
            let id = integer(body, "id")?;
            app.store
                .transaction(&name, |a, _, _| {
                    Ok(vec![a.cancel_order(id, "cancel", now)?])
                })
                .await?;
        }
        "/api/spot" => {
            let side = text(body, "side")?;
            let units = number(body, "units")?;
            let units = match side {
                "buy" => units,
                "sell" => -units,
                _ => return Err(bad("交易方向无效")),
            };
            let symbol = symbols::NSTAR;
            app.store.require_tradable(symbol).await?;
            let u = app.store.user(&name).await?;
            let held: Vec<&str> = u.account.symbols().collect();
            let mut prices = app
                .market
                .trading_prices(held.iter().copied().filter(|s| *s != symbols::NSTAR))
                .await?;
            let desk = app.market.nstar_desk().await;
            desk.price(&mut prices)?;
            let result = app
                .store
                .transaction(&name, |a, _, _| {
                    let trade = a.spot(symbol, units, &prices, now)?;
                    let mut after = prices.clone();
                    trading::apply_fill(&mut after, &trade)?;
                    let mut trades = vec![trade];
                    trades.extend(a.check_stopout(&after, now)?);
                    Ok(trades)
                })
                .await;
            commit(app, Some(desk)).await;
            result?;
        }
        "/api/tpsl" => {
            let id = integer(body, "id")?;
            let tp = body["tp"].as_f64().unwrap_or(0.0);
            let sl = body["sl"].as_f64().unwrap_or(0.0);
            let u = app.store.user(&name).await?;
            let prices = app.market.prices(u.account.symbols()).await?;
            app.store
                .transaction(&name, |a, _, _| {
                    a.set_triggers(id, tp, sl, &prices)?;
                    Ok(vec![])
                })
                .await?;
        }
        "/api/reset" => {
            app.store
                .transaction(&name, |a, _, _| Ok(vec![a.reset(now)]))
                .await?;
        }
        "/api/borrow" => {
            let amount = number(body, "amount")?;
            app.store
                .transaction(&name, |a, _, _| {
                    a.borrow(amount, now)?;
                    Ok(vec![a.loan_trade("borrow", amount, now)])
                })
                .await?;
        }
        "/api/repay" => {
            let amount = number(body, "amount")?;
            app.store
                .transaction(&name, |a, _, _| {
                    a.repay(amount, now)?;
                    Ok(vec![a.loan_trade("repay", amount, now)])
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
            let symbols: Vec<&str> = u
                .account
                .positions
                .iter()
                .filter(|p| id.is_none_or(|id| id == p.id))
                .map(|p| p.symbol.as_str())
                .collect();
            let mut prices = app
                .market
                .trading_prices(symbols.iter().copied().filter(|s| *s != symbols::NSTAR))
                .await?;
            let desk = nstar_desk(app, &symbols).await;
            if let Some(desk) = &desk {
                desk.price(&mut prices)?;
            }
            let result = app
                .store
                .transaction(&name, |a, _, _| a.close(id, &prices, now))
                .await;
            commit(app, desk).await;
            result?;
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
            let symbol = text(body, "symbol")?.to_uppercase();
            require_symbol(app, &symbol)?;
            app.store.select_symbol(&name, &symbol).await?
        }
        _ => return Err(Failure(StatusCode::NOT_FOUND, "unknown".into())),
    }
    // Hand back the post-trade account so the page needs no follow-up GET /api/state.
    let mut out = json!({"ok":true});
    if let Ok(state) = app.state(&name).await {
        out["state"] = state;
    }
    Ok(out)
}

async fn ws_upgrade(
    State(app): State<Arc<App>>,
    Extension(Client(client)): Extension<Client>,
    uri: Uri,
    upgrade: WebSocketUpgrade,
) -> Response {
    let q = match params(&uri) {
        Ok(q) => q,
        Err(e) => return e.into_response(),
    };
    let Some(slot) = app.limiter.socket(&client, app.limits.ws_per_client) else {
        return too_many(30);
    };
    let token = q.get("token").cloned().unwrap_or_default();
    match app.store.authenticate(&token).await {
        Ok(Some(name)) => upgrade
            .max_message_size(65536)
            .max_frame_size(65536)
            .on_upgrade(move |socket| serve_ws(socket, app, name, token, slot))
            .into_response(),
        _ => Failure(StatusCode::UNAUTHORIZED, "auth".into()).into_response(),
    }
}
// Display slots replace old values; only controls have a bounded FIFO queue.
#[derive(Clone, Default)]
struct Display {
    epoch: u64,
    state: Option<(u64, Arc<str>)>,
    quote: Option<(u64, Arc<str>)>,
    leaderboard: Option<(u64, Arc<str>)>,
}
#[derive(Default)]
struct View {
    hidden: bool,
    no_leaderboard: bool,
    interval: Option<String>, // None retains all intervals for legacy clients.
    low_power: bool,
}
async fn ws_writer(
    mut sink: futures_util::stream::SplitSink<WebSocket, Message>,
    mut controls: tokio::sync::mpsc::Receiver<Message>,
    mut display: tokio::sync::watch::Receiver<Display>,
) {
    let mut sent = [0; 3];
    let mut epoch = 0;
    loop {
        let latest = display.borrow_and_update().clone();
        if epoch != latest.epoch {
            epoch = latest.epoch;
            sent = [0; 3];
        }
        let next = [latest.state, latest.quote, latest.leaderboard]
            .into_iter()
            .enumerate()
            .find_map(|(kind, value)| {
                value
                    .filter(|(id, _)| *id != sent[kind])
                    .map(|(id, text)| (kind, id, text))
            });
        let message = tokio::select! {
            biased;
            control = controls.recv() => match control { Some(message)=>message, None=>break },
            _ = std::future::ready(()), if next.is_some() => {
                let (kind,id,text) = next.unwrap(); sent[kind] = id;
                Message::Text(text.to_string().into())
            },
            changed = display.changed() => { if changed.is_err() { break; } continue; },
        };
        let close = matches!(message, Message::Close(_));
        if !tokio::time::timeout(Duration::from_secs(5), sink.send(message))
            .await
            .is_ok_and(|r| r.is_ok())
            || close
        {
            break;
        }
    }
    let _ = tokio::time::timeout(Duration::from_secs(2), sink.close()).await;
}
async fn serve_ws(
    socket: WebSocket,
    app: Arc<App>,
    name: String,
    token: String,
    _slot: SocketSlot,
) {
    let mut events = app.control_events();
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
    let (sink, mut source) = socket.split();
    let (control_tx, control_rx) = tokio::sync::mpsc::channel(64);
    let (display_tx, display_rx) = tokio::sync::watch::channel(Display::default());
    let mut writer = tokio::task::JoinSet::new();
    writer.spawn(ws_writer(sink, control_rx, display_rx));
    let mut clock = tokio::time::interval(Duration::from_millis(200));
    clock.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut view = View::default();
    let mut display = Display::default();
    let mut last_state: Arc<str> = Arc::from("");
    let mut last_account = String::new();
    let mut state_sequence = 0;
    let mut account_revision = 0;
    let mut account_at = tokio::time::Instant::now() - Duration::from_secs(2);
    let mut quote_at = account_at;
    let mut heartbeat_at = tokio::time::Instant::now();
    let mut force_state = true;
    loop {
        if app.shutting_down.load(std::sync::atomic::Ordering::SeqCst) {
            break;
        }
        tokio::select! {
            _ = writer.join_next() => return,
            incoming = source.next() => match incoming {
                Some(Ok(Message::Ping(data))) => {
                    if control_tx.send(Message::Pong(data)).await.is_err() { break; }
                },
                Some(Ok(Message::Text(text))) => {
                    if let Ok(value) = serde_json::from_str::<Value>(&text) && value["type"] == "view" {
                        if let Some(visible) = value["visible"].as_bool() { view.hidden = !visible; }
                        if let Some(visible) = value["leaderboard"].as_bool() { view.no_leaderboard = !visible; }
                        if let Some(low_power) = value["low_power"].as_bool() { view.low_power = low_power; }
                        if let Some(interval) = value["interval"].as_str()
                            && symbols::INTERVALS.iter().any(|(i,_)| *i == interval) {
                            view.interval = Some(interval.to_owned());
                        }
                        display = Display { epoch: display.epoch+1, ..Display::default() };
                        display_tx.send_replace(display.clone());
                        force_state = !view.hidden;
                        account_at = tokio::time::Instant::now() - Duration::from_secs(2);
                        quote_at = account_at;
                    }
                },
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                _ => {},
            },
            event = events.recv() => match event {
                Ok(Event::Message(text, _)) => {
                    if control_tx.send(Message::Text(text.to_string().into())).await.is_err() { break; }
                },
                Ok(Event::Deleted(owner)) => if owner == name { break; },
                Ok(Event::Shutdown) | Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    if app.store.authenticate(&token).await.ok().flatten().as_deref() != Some(&name) { break; }
                    // Display traffic cannot lag this channel. Recover controls from durable
                    // state/history rather than interpreting missed quotes as a disconnect.
                    let _ = control_tx.send(Message::Text(json!({"type":"resync","history":true,"chat":true}).to_string().into())).await;
                    if let Ok(mut settings) = app.store.settings().await {
                        settings["type"] = json!("symbols");
                        let _ = control_tx.send(Message::Text(settings.to_string().into())).await;
                    }
                    force_state = true;
                    account_at = tokio::time::Instant::now() - Duration::from_secs(2);
                },
            },
            _ = clock.tick() => {
                let mut changed = false;
                if account_at.elapsed() >= Duration::from_secs(1) {
                    account_at = tokio::time::Instant::now();
                    if app.store.authenticate(&token).await.ok().flatten().as_deref() != Some(&name) { break; }
                    let revision = app.store.revision();
                    let mut account_changed = false;
                    if revision != account_revision && let Ok(user) = app.store.user(&name).await {
                        let key = json!([user.account,user.symbol,user.last_trade]).to_string();
                        account_changed = key != last_account;
                        last_account = key;
                    }
                    if (!view.hidden || account_changed || force_state)
                        && let Ok(packet) = app.state_packet(&name).await {
                        if force_state || packet.key != last_state {
                            state_sequence += 1;
                            display.state = Some((state_sequence, packet.text.clone()));
                            last_state = packet.key.clone();
                            changed = true;
                        }
                        force_state = false;
                    }
                    account_revision = revision;
                }
                if !view.hidden && quote_at.elapsed() >= Duration::from_millis(if view.low_power { 1000 } else { 200 }) {
                    quote_at = tokio::time::Instant::now();
                    if let Some(symbol) = app.store.selected(&name)
                        && let Some(quote) = app.latest_quote(&symbol, view.interval.as_deref())
                        && display.quote.as_ref().is_none_or(|(id,_)| *id != quote.0) {
                        display.quote = Some(quote); changed = true;
                    }
                }
                if !view.hidden && !view.no_leaderboard
                    && let Some(board) = app.leaderboard_packet()
                    && display.leaderboard.as_ref().is_none_or(|(id,_)|*id != board.0) {
                    display.leaderboard = Some(board); changed = true;
                }
                if changed { display_tx.send_replace(display.clone()); }
                if heartbeat_at.elapsed() >= Duration::from_secs(15) {
                    heartbeat_at = tokio::time::Instant::now();
                    if control_tx.send(Message::Text(json!({"type":"heartbeat"}).to_string().into())).await.is_err() { break; }
                }
            }
        }
    }
    let _ = control_tx.send(Message::Close(None)).await;
    drop(control_tx);
    let _ = tokio::time::timeout(Duration::from_secs(2), writer.join_next()).await;
}
