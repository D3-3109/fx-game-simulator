use futures_util::{SinkExt, StreamExt};
use fx_sim_rust::{
    api,
    app::App,
    limits::{Limits, Quota, Rate},
    market::Market,
    runtime,
    storage::{Store, now_ms},
};
use serde_json::{Value, json};
use std::{path::Path, sync::Arc, time::Duration};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::Message};
type Ws = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

struct Harness {
    dir: tempfile::TempDir,
    app: Arc<App>,
    base: String,
    client: reqwest::Client,
    server: tokio::task::JoinHandle<()>,
    tasks: tokio::task::JoinSet<()>,
}
impl Drop for Harness {
    fn drop(&mut self) {
        self.server.abort();
        self.tasks.abort_all();
    }
}
impl Harness {
    async fn new() -> Self {
        Self::limited(Limits::unlimited()).await
    }
    async fn limited(limits: Limits) -> Self {
        Self::configured(limits, false).await
    }
    async fn nstar_only() -> Self {
        Self::configured(Limits::unlimited(), true).await
    }
    async fn configured(limits: Limits, nstar_only: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let store = Arc::new(if nstar_only {
            Store::open(&path).await.unwrap()
        } else {
            Store::open_legacy(&path).await.unwrap()
        });
        let key = dir.path().join("admin.json");
        let mut hash = [0u8; 32];
        pbkdf2::pbkdf2_hmac::<sha2::Sha256>(b"test-key", &[0xab; 16], 200000, &mut hash);
        std::fs::write(
            &key,
            json!({"salt":hex::encode([0xab;16]),"hash":hex::encode(hash)}).to_string(),
        )
        .unwrap();
        let market = Arc::new(
            Market::offline(
                store.clone(),
                [
                    ("USDJPY".into(), 150.0),
                    ("EURJPY".into(), 165.0),
                    ("EURUSD".into(), 1.1),
                    ("FXSIM".into(), 1.1),
                    ("BTCUSDT".into(), 60000.0),
                ]
                .into(),
            )
            .await
            .unwrap(),
        );
        if nstar_only {
            market.tick_nstar().await.unwrap();
        }
        Self::with(store, market, key, limits, dir).await
    }
    async fn with(
        store: Arc<Store>,
        market: Arc<Market>,
        key: std::path::PathBuf,
        limits: Limits,
        dir: tempfile::TempDir,
    ) -> Self {
        let app = Arc::new(App::new(store, market, key).with_limits(limits));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let static_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("static");
        let router = api::router(app.clone(), static_dir);
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await
            .unwrap();
        });
        let tasks = runtime::start(app.clone());
        Self {
            dir,
            app,
            base,
            client: reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap(),
            server,
            tasks,
        }
    }
    async fn request(&self, path: &str, body: Option<Value>, token: &str) -> (u16, Value) {
        let request = if let Some(body) = body {
            self.client.post(format!("{}{path}", self.base)).json(&body)
        } else {
            self.client.get(format!("{}{path}", self.base))
        };
        let response = request.bearer_auth(token).send().await.unwrap();
        let status = response.status().as_u16();
        let value = response.json().await.unwrap();
        (status, value)
    }
    async fn register(&self, name: &str) -> String {
        let (status, value) = self
            .request(
                "/api/auth",
                Some(json!({"action":"register","name":name,"pwd":"password1"})),
                "",
            )
            .await;
        assert_eq!(status, 200, "{value}");
        value["token"].as_str().unwrap().into()
    }
    async fn admin(&self) -> String {
        let (status, value) = self
            .request("/api/admin/login", Some(json!({"psk":"test-key"})), "")
            .await;
        assert_eq!(status, 200, "{value}");
        value["token"].as_str().unwrap().into()
    }
    async fn ws(&self, token: &str) -> Ws {
        tokio_tungstenite::connect_async(format!(
            "{}/ws?token={token}",
            self.base.replace("http:", "ws:")
        ))
        .await
        .unwrap()
        .0
    }
    async fn sql(&self, sql: &str) {
        let pool = sqlx::SqlitePool::connect(&format!(
            "sqlite://{}",
            self.dir.path().join("test.db").display()
        ))
        .await
        .unwrap();
        sqlx::raw_sql(sql).execute(&pool).await.unwrap();
        pool.close().await;
    }
}
#[tokio::test]
async fn nstar_only_registration_whitelist_default_klines_and_rejections() {
    let h = Harness::nstar_only().await;
    let token = h.register("nstar-default").await;
    let admin = h.admin().await;
    let (status, state) = h.request("/api/state", None, &token).await;
    assert_eq!(status, 200, "{state}");
    assert_eq!(state["symbol"], "NSTAR");
    assert_eq!(
        h.app.store.user("nstar-default").await.unwrap().symbol,
        "NSTAR"
    );
    assert_eq!(state["quote_valid"], true);
    for (path, auth) in [("/api/symbols", ""), ("/api/admin/symbols", admin.as_str())] {
        let (status, value) = h.request(path, None, auth).await;
        assert_eq!(status, 200, "{value}");
        let rows = value["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 1, "{value}");
        assert_eq!(rows[0]["symbol"], "NSTAR");
        assert_eq!(rows[0]["enabled"], true);
    }
    let (status, default) = h.request("/api/klines", None, "").await;
    assert_eq!(status, 200, "{default}");
    assert!(!default["klines"].as_array().unwrap().is_empty());
    assert_eq!(
        default,
        h.request("/api/klines?symbol=NSTAR", None, "").await.1
    );
    assert_eq!(
        h.request("/api/klines?symbol=nstar&interval=1s", None, "")
            .await
            .0,
        200
    );
    for symbol in fx_sim_rust::symbols::all()
        .into_iter()
        .filter(|s| *s != "NSTAR")
        .chain(["UNKNOWN"])
    {
        for (path, body, auth) in [
            (
                "/api/symbol".to_owned(),
                Some(json!({"symbol":symbol})),
                token.as_str(),
            ),
            (format!("/api/klines?symbol={symbol}"), None, ""),
            (
                "/api/admin/symbols".to_owned(),
                Some(json!({"symbol":symbol,"enabled":true})),
                admin.as_str(),
            ),
        ] {
            let (status, value) = h.request(&path, body, auth).await;
            assert_eq!(status, 400, "{path} {symbol}: {value}");
            assert_eq!(value["ok"], false);
        }
    }
    for (path, body) in [
        (
            "/api/trade",
            json!({"symbol":"BTCUSDT","side":"buy","lots":1}),
        ),
        (
            "/api/order",
            json!({"symbol":"USDJPY","side":"buy","kind":"limit","lots":1,"price":1}),
        ),
        (
            "/api/spot",
            json!({"symbol":"FXSIM","side":"buy","units":1}),
        ),
    ] {
        assert_eq!(h.request(path, Some(body), &token).await.0, 400);
    }
    let (_, after) = h.request("/api/state", None, &token).await;
    assert_eq!(after["symbol"], "NSTAR");
    assert_eq!(after["balance"], state["balance"]);
    assert_eq!(after["positions"], json!([]));
    assert_eq!(after["spot"], json!([]));
    assert_eq!(after["orders"], json!([]));
    assert_eq!(
        h.request("/api/history", None, &token).await.1["rows"],
        json!([])
    );
    assert_eq!(h.app.market.snapshot().await.len(), 1);
    assert!(h.app.risk_symbols().await.unwrap().is_empty());
}

#[tokio::test]
async fn nstar_only_spot_perpetual_and_websocket_are_tradeable_by_default() {
    let h = Harness::nstar_only().await;
    let token = h.register("nstar-trader").await;
    let mut socket = h.ws(&token).await;
    assert_eq!(packet(&mut socket, "state").await["symbol"], "NSTAR");
    let (status, buy) = h
        .request("/api/spot", Some(json!({"side":"buy","units":5})), &token)
        .await;
    assert_eq!(status, 200, "{buy}");
    assert_eq!(buy["state"]["spot"][0]["sym"], "NSTAR");
    near(&buy["state"]["spot"][0]["units"], 5.0);
    // No symbol selection is needed before opening perpetual positions.
    let (status, opened) = h
        .request(
            "/api/trade",
            Some(json!({"side":"buy","lots":1000})),
            &token,
        )
        .await;
    assert_eq!(status, 200, "{opened}");
    assert_eq!(opened["state"]["positions"][0]["sym"], "NSTAR");
    assert_eq!(opened["state"]["positions"][0]["side"], "long");
    let (status, closed) = h.request("/api/close", Some(json!({})), &token).await;
    assert_eq!(status, 200, "{closed}");
    assert_eq!(closed["state"]["positions"], json!([]));
    let (status, sold) = h
        .request("/api/spot", Some(json!({"side":"sell","units":5})), &token)
        .await;
    assert_eq!(status, 200, "{sold}");
    assert_eq!(sold["state"]["spot"], json!([]));
    let history = h.request("/api/history", None, &token).await.1;
    let rows = history["rows"].as_array().unwrap();
    assert!(rows.iter().all(|r| r["symbol"] == "NSTAR"));
    for kind in ["spot", "open", "close"] {
        assert!(rows.iter().any(|r| r["kind"] == kind), "{history}");
    }
    let tick = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Message::Text(text) = socket.next().await.unwrap().unwrap() {
                let value: Value = serde_json::from_str(&text).unwrap();
                if value.get("type").is_none() && value["sym"] == "NSTAR" {
                    break value;
                }
            }
        }
    })
    .await
    .unwrap();
    assert!(tick["price"].as_f64().unwrap() > 0.0);
    socket.close(None).await.unwrap();
}

#[tokio::test]
async fn fx_source_sides_match_state_fills_marks_closes_and_closed_market_rejection() {
    let h = Harness::new().await;
    let token = h.register("quotes").await;
    let q = fx_sim_rust::market::Quote {
        price: 150.015,
        at: now_ms(),
        received_at: now_ms(),
        bid_ask: Some((150.009, 150.021)),
        open: true,
        anchor: None,
        mark: None,
    };
    h.app.market.on_market_quote("USDJPY", q).await.unwrap();
    let state = h.request("/api/state", None, &token).await.1;
    near(&state["price"], 150.015);
    near(&state["bid"], 150.009);
    near(&state["ask"], 150.021);
    assert_eq!(state["quote_valid"], true);
    for (side, entry, exit) in [("buy", 150.021, 150.009), ("sell", 150.009, 150.021)] {
        let (status, reply) = h
            .request("/api/trade", Some(json!({"side":side,"lots":0.01})), &token)
            .await;
        assert_eq!(status, 200, "{reply}");
        let state = h.request("/api/state", None, &token).await.1;
        near(&state["positions"][0]["entry"], entry);
        near(&state["positions"][0]["now"], exit);
        let pnl = state["positions"][0]["pnl"].as_f64().unwrap();
        near(&state["positions"][0]["margin"], 100.0);
        let (status, reply) = h.request("/api/close", Some(json!({})), &token).await;
        assert_eq!(status, 200, "{reply}");
        let history = h.request("/api/history", None, &token).await.1;
        near(&history["rows"][0]["price"], exit);
        near(&history["rows"][0]["pnl"], pnl);
    }
    let mut closed = q;
    closed.open = false;
    h.app
        .market
        .on_market_quote("USDJPY", closed)
        .await
        .unwrap();
    let before = h.request("/api/state", None, &token).await.1;
    assert_eq!(before["quote_valid"], false);
    let (status, reply) = h
        .request(
            "/api/trade",
            Some(json!({"side":"buy","lots":0.01})),
            &token,
        )
        .await;
    assert_eq!(status, 400);
    assert!(
        reply["err"]
            .as_str()
            .unwrap()
            .contains("报价已过期或市场已关闭")
    );
    let after = h.request("/api/state", None, &token).await.1;
    assert_eq!(after["balance"], before["balance"]);
    assert_eq!(after["positions"], before["positions"]);
}

fn near(a: &Value, b: f64) {
    assert!((a.as_f64().unwrap() - b).abs() < 1e-7, "{a} != {b}");
}
async fn packet(ws: &mut Ws, kind: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let msg = ws.next().await.unwrap().unwrap();
            if let Message::Text(t) = msg {
                let v: Value = serde_json::from_str(&t).unwrap();
                if v["type"] == kind {
                    return v;
                }
            }
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn http_account_trading_cross_pair_currency_and_auth_contract() {
    let h = Harness::new().await;
    let token = h.register("player").await;
    let other = h.register("other").await;
    for path in [
        "/api/state",
        "/api/history",
        "/api/me",
        "/api/chat",
        "/api/admin/users",
    ] {
        assert_eq!(h.request(path, None, "bad").await.0, 401, "{path}");
    }
    assert_eq!(
        h.request("/api/me", None, &token).await.1,
        json!({"ok":true,"name":"player"})
    );
    assert_eq!(
        h.request(
            "/api/auth",
            Some(json!({"name":"player","pwd":"wrong"})),
            ""
        )
        .await
        .0,
        400
    );
    let state = h.request("/api/state", None, &token).await.1;
    assert_eq!(state["currency"], "USD");
    assert_eq!(state["positions"], json!([]));
    assert!(state["margin_level"].is_null());
    assert_eq!(
        h.request("/api/trade", Some(json!({"side":"buy","lots":0.1})), &token)
            .await
            .0,
        200
    );
    let state = h.request("/api/state", None, &token).await.1;
    near(&state["used"], 1_000.0);
    near(&state["positions"][0]["pnl"], -100.0 / 150.0);
    let id = state["positions"][0]["id"].clone();
    assert_eq!(
        h.request("/api/leverage", Some(json!({"leverage":50})), &token)
            .await
            .0,
        400
    );
    assert_eq!(
        h.request("/api/symbol", Some(json!({"symbol":"FXSIM"})), &token)
            .await
            .0,
        200
    );
    h.app
        .market
        .on_quote("USDJPY", 151.0, now_ms())
        .await
        .unwrap();
    assert_eq!(
        h.request("/api/close", Some(json!({"id":id})), &token)
            .await
            .0,
        200
    );
    let history = h.request("/api/history", None, &token).await.1;
    near(&history["rows"][0]["pnl"], 9800.0 / 151.0);
    assert_eq!(history["rows"][0]["currency"], "USD");
    assert_eq!(
        h.request("/api/history", None, &other).await.1["rows"],
        json!([])
    );
    assert_eq!(
        h.request("/api/leverage", Some(json!({"leverage":10})), &token)
            .await
            .0,
        200
    );
    for (path, body) in [
        ("/api/trade", json!({"side":"invalid","lots":0.1})),
        ("/api/trade", json!({"side":"buy","lots":"NaN"})),
        ("/api/leverage", json!({"leverage":true})),
        ("/api/leverage", json!({"leverage":0})),
        ("/api/leverage", json!({"leverage":1.5})),
        ("/api/symbol", json!({"symbol":"FAKE"})),
    ] {
        assert_eq!(h.request(path, Some(body), &token).await.0, 400);
    }
    let r = h
        .client
        .post(format!("{}/api/trade", h.base))
        .body("[")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    let r = h
        .client
        .post(format!("{}/api/chat", h.base))
        .body("x".repeat(65537))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 413);
    assert_eq!(r.json::<Value>().await.unwrap()["ok"], false);
}

#[tokio::test]
async fn admin_deposits_failure_rollback_symbol_rules_delete_and_replay() {
    let h = Harness::new().await;
    let token = h.register("player").await;
    let admin = h.admin().await;
    let body = json!({"name":"player","amount":100.01,"request_id":"a".repeat(32)});
    assert_eq!(
        h.request("/api/admin/deposit", Some(body.clone()), &token)
            .await
            .0,
        401
    );
    let first = h
        .request("/api/admin/deposit", Some(body.clone()), &admin)
        .await;
    assert_eq!(first.0, 200);
    assert_eq!(
        first,
        h.request("/api/admin/deposit", Some(body.clone()), &admin)
            .await
    );
    assert_eq!(
        h.request("/api/leaderboard", None, "").await.1["rows"][0]["ret"],
        0.0
    );
    h.sql("CREATE TRIGGER fail_admin BEFORE INSERT ON admin_events BEGIN SELECT RAISE(ABORT,'fixture failure'); END").await;
    let mut second = body.clone();
    second["request_id"] = json!("b".repeat(32));
    assert_eq!(
        h.request("/api/admin/deposit", Some(second), &admin)
            .await
            .0,
        503
    );
    near(
        &h.request("/api/state", None, &token).await.1["balance"],
        10100.01,
    );
    assert_eq!(
        h.request(
            "/api/admin/symbols",
            Some(json!({"symbol":"USDJPY","enabled":false})),
            &admin
        )
        .await
        .0,
        503
    );
    h.sql("DROP TRIGGER fail_admin").await;
    assert_eq!(
        h.request("/api/trade", Some(json!({"side":"buy","lots":0.1})), &token)
            .await
            .0,
        200
    );
    assert_eq!(
        h.request(
            "/api/admin/symbols",
            Some(json!({"symbol":"USDJPY","enabled":false})),
            &admin
        )
        .await
        .0,
        200
    );
    assert_eq!(
        h.request("/api/trade", Some(json!({"side":"buy","lots":0.1})), &token)
            .await
            .0,
        400
    );
    assert_eq!(
        h.request("/api/symbol", Some(json!({"symbol":"USDJPY"})), &token)
            .await
            .0,
        400
    );
    assert_eq!(
        h.request("/api/close", Some(json!({})), &token).await.0,
        200
    );
    // Disabled conversion pairs still provide USD rates for cross trades.
    assert_eq!(
        h.request("/api/symbol", Some(json!({"symbol":"EURJPY"})), &token)
            .await
            .0,
        200
    );
    assert_eq!(
        h.request(
            "/api/trade",
            Some(json!({"side":"sell","lots":0.1})),
            &token
        )
        .await
        .0,
        200
    );
    assert_eq!(
        h.request(
            "/api/admin/delete",
            Some(json!({"name":"player","confirmation":"wrong"})),
            &admin
        )
        .await
        .0,
        400
    );
    h.sql("CREATE TRIGGER fail_delete BEFORE DELETE ON users BEGIN SELECT RAISE(ABORT,'fixture failure'); END").await;
    assert_eq!(
        h.request(
            "/api/admin/delete",
            Some(json!({"name":"player","confirmation":"player"})),
            &admin
        )
        .await
        .0,
        503
    );
    assert_eq!(h.request("/api/me", None, &token).await.0, 200);
    h.sql("DROP TRIGGER fail_delete").await;
    assert_eq!(
        h.request(
            "/api/admin/delete",
            Some(json!({"name":"player","confirmation":"player"})),
            &admin
        )
        .await
        .0,
        200
    );
    assert_eq!(h.request("/api/me", None, &token).await.0, 401);
    h.register("player").await;
    assert_eq!(
        h.request("/api/admin/deposit", Some(body), &admin).await.0,
        400
    );
    let events = h.request("/api/admin/deposits", None, &admin).await.1;
    assert!(
        events["rows"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["kind"] == "delete")
    );
}

#[tokio::test]
async fn admin_session_expiry_logout_rate_limits_and_bad_input() {
    let h = Harness::new().await;
    let token = h.register("player").await;
    let admin = h.admin().await;
    for value in [
        Value::Null,
        json!(true),
        json!(0),
        json!(-1),
        json!(0.001),
        json!(1000000001),
        json!("NaN"),
    ] {
        assert_eq!(
            h.request(
                "/api/admin/deposit",
                Some(json!({"name":"player","amount":value,"request_id":"c".repeat(32)})),
                &admin
            )
            .await
            .0,
            400
        );
    }
    for value in [json!(0), json!("false"), Value::Null] {
        assert_eq!(
            h.request(
                "/api/admin/symbols",
                Some(json!({"symbol":"USDJPY","enabled":value})),
                &admin
            )
            .await
            .0,
            400
        );
    }
    let expired = h
        .app
        .admin_login("test-key", now_ms() - 3601000)
        .await
        .unwrap();
    assert_eq!(h.request("/api/admin/users", None, &expired).await.0, 401);
    assert_eq!(
        h.request("/api/admin/logout", Some(json!({})), &admin)
            .await
            .0,
        200
    );
    assert_eq!(h.request("/api/admin/users", None, &admin).await.0, 401);
    for _ in 0..11 {
        assert_eq!(
            h.request("/api/admin/login", Some(json!({"psk":"wrong"})), "")
                .await
                .0,
            400
        );
    }
    assert!(
        h.request("/api/admin/login", Some(json!({"psk":"test-key"})), "")
            .await
            .1["err"]
            .as_str()
            .unwrap()
            .contains("一分钟")
    );
    assert_eq!(h.request("/api/me", None, &token).await.0, 200);
}

#[tokio::test]
async fn websocket_isolation_presence_chat_symbols_and_deletion() {
    let h = Harness::new().await;
    let token = h.register("player").await;
    let other = h.register("other").await;
    let admin = h.admin().await;
    assert!(
        tokio_tungstenite::connect_async(format!(
            "{}/ws?token=bad",
            h.base.replace("http:", "ws:")
        ))
        .await
        .is_err()
    );
    let mut a = h.ws(&token).await;
    let mut b = h.ws(&token).await;
    let mut c = h.ws(&other).await;
    assert_eq!(packet(&mut a, "state").await["name"], "player");
    assert_eq!(packet(&mut b, "state").await["name"], "player");
    assert_eq!(packet(&mut c, "state").await["name"], "other");
    assert_eq!(
        h.app
            .online()
            .iter()
            .filter(|n| n.as_str() == "player")
            .count(),
        2
    );
    assert_eq!(
        h.request(
            "/api/chat",
            Some(json!({"text":" <script>x</script> ","name":"impostor"})),
            &token
        )
        .await
        .0,
        200
    );
    let chat = packet(&mut c, "chat").await;
    assert_eq!(chat["row"]["name"], "player");
    assert_eq!(chat["row"]["text"], "<script>x</script>");
    assert_eq!(
        h.request("/api/chat", Some(json!({"text":"too quick"})), &token)
            .await
            .0,
        400
    );
    assert_eq!(
        h.request(
            "/api/admin/symbols",
            Some(json!({"symbol":"FXSIM","enabled":false})),
            &admin
        )
        .await
        .0,
        200
    );
    assert_eq!(
        packet(&mut c, "symbols").await["rows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["symbol"] == "FXSIM")
            .unwrap()["enabled"],
        false
    );
    a.close(None).await.unwrap();
    drop(a);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        h.app
            .online()
            .iter()
            .filter(|n| n.as_str() == "player")
            .count(),
        1
    );
    // Ping/pong remains protocol-correct.
    c.send(Message::Ping(vec![1, 2, 3].into())).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(Ok(Message::Pong(data))) = c.next().await {
                assert_eq!(data.as_ref(), &[1, 2, 3]);
                break;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(
        packet(&mut c, "lb").await["rows"].as_array().unwrap().len(),
        2
    );
    h.request(
        "/api/admin/delete",
        Some(json!({"name":"player","confirmation":"player"})),
        &admin,
    )
    .await;
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match b.next().await {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    assert!(!h.app.online().iter().any(|n| n == "player"));
    let mut again = h.ws(&other).await;
    assert_eq!(packet(&mut again, "state").await["name"], "other");
}

#[tokio::test]
async fn atomic_trade_failure_stays_visible_as_503_and_no_browser_stopout() {
    let h = Harness::new().await;
    let token = h.register("player").await;
    h.request("/api/symbol", Some(json!({"symbol":"FXSIM"})), &token)
        .await;
    h.sql("CREATE TRIGGER fail_trade BEFORE INSERT ON trades BEGIN SELECT RAISE(ABORT,'fixture failure'); END").await;
    assert_eq!(
        h.request("/api/trade", Some(json!({"side":"buy","lots":0.1})), &token)
            .await
            .0,
        503
    );
    assert_eq!(
        h.request("/api/state", None, &token).await.1["positions"],
        json!([])
    );
    assert_eq!(
        h.request("/api/history", None, &token).await.1["rows"],
        json!([])
    );
    h.sql("DROP TRIGGER fail_trade").await;
    h.app
        .store
        .transaction("player", |a, _, _| {
            a.balance = 50.0;
            a.positions = vec![fx_sim_rust::trading::Position {
                id: 1,
                side: "long".into(),
                lots: 0.1,
                entry: 1.0,
                symbol: "FXSIM".into(),
                tp: 0.0,
                sl: 0.0,
            }];
            Ok(vec![])
        })
        .await
        .unwrap();
    assert!(h.app.online().is_empty());
    h.app
        .market
        .on_quote("FXSIM", 0.9999, now_ms())
        .await
        .unwrap();
    assert_eq!(
        h.request("/api/state", None, &token).await.1["positions"],
        json!([])
    );
    near(
        &h.request("/api/state", None, &token).await.1["balance"],
        48.0,
    );
    assert_eq!(
        h.request("/api/history", None, &token).await.1["rows"][0]["kind"],
        "stopout"
    );
}

#[tokio::test]
async fn per_ip_rate_limits_registration_quotas_and_credential_rules() {
    let free = Rate {
        burst: 1e9,
        per_sec: 1e9,
    };
    let h = Harness::limited(Limits {
        api: Rate {
            burst: 40.0,
            per_sec: 0.01,
        },
        auth: free,
        ws: free,
        ws_per_client: 2,
        register: Quota {
            per_client_hour: 2,
            per_client_day: 5,
            global_hour: 30,
        },
    })
    .await;
    let reg = |name: &str, pwd: &str| json!({"action":"register","name":name,"pwd":pwd});
    for (name, pwd) in [
        ("a", "password1"),
        ("bad name", "password1"),
        ("12345", "password1"),
        ("player", "short1"),
        ("player", "onlyletters"),
        ("player", "11111111a"),
        ("player1", "PLAYER1"),
    ] {
        let (status, v) = h.request("/api/auth", Some(reg(name, pwd)), "").await;
        assert_eq!(status, 400, "{name}/{pwd}: {v}");
    }
    let (status, v) = h
        .request("/api/auth", Some(reg("玩家一", "trade2026")), "")
        .await;
    assert_eq!(status, 200, "{v}");
    let (status, v) = h
        .request("/api/auth", Some(reg("PLAYER", "trade2026")), "")
        .await;
    assert_eq!(status, 200, "{v}");
    // Same name in another case is taken; the third account this hour is refused even
    // after deleting one, because quota rows outlive the account.
    let (status, v) = h
        .request("/api/auth", Some(reg("player", "trade2026")), "")
        .await;
    assert_eq!(
        (status, v["err"].as_str()),
        (400, Some("该网络注册过于频繁，请稍后再试"))
    );
    let admin = h.admin().await;
    let (status, _) = h
        .request(
            "/api/admin/delete",
            Some(json!({"name":"PLAYER","confirmation":"PLAYER"})),
            &admin,
        )
        .await;
    assert_eq!(status, 200);
    let (status, _) = h
        .request("/api/auth", Some(reg("again", "trade2026")), "")
        .await;
    assert_eq!(status, 400);
    // Existing accounts keep logging in regardless of the new password rules.
    let (status, v) = h
        .request(
            "/api/auth",
            Some(json!({"name":"玩家一","pwd":"trade2026"})),
            "",
        )
        .await;
    assert_eq!(status, 200, "{v}");
    let token = v["token"].as_str().unwrap().to_owned();
    // Two sockets per client; the third handshake is refused with 429.
    let _one = h.ws(&token).await;
    let _two = h.ws(&token).await;
    let third = tokio_tungstenite::connect_async(format!(
        "{}/ws?token={token}",
        h.base.replace("http:", "ws:")
    ))
    .await;
    assert!(third.is_err());
    // Burn the remaining API burst; X-Forwarded-For from an untrusted peer is ignored.
    let mut limited = None;
    for _ in 0..60 {
        let response = h
            .client
            .get(format!("{}/api/symbols", h.base))
            .header("x-forwarded-for", "198.51.100.9")
            .send()
            .await
            .unwrap();
        if response.status() == 429 {
            limited = Some(response);
            break;
        }
    }
    let limited = limited.expect("API burst never ran out");
    assert!(
        limited.headers()["retry-after"]
            .to_str()
            .unwrap()
            .parse::<u64>()
            .unwrap()
            >= 1
    );
    let body: Value = limited.json().await.unwrap();
    assert_eq!(body["ok"], false);
    // Static assets bypass the limiter.
    let page = h.client.get(format!("{}/", h.base)).send().await.unwrap();
    assert_ne!(page.status(), 429);
}

#[test]
fn forwarded_for_counts_only_through_trusted_proxies() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&dir.path().join("t.db")).await.unwrap());
        let market = Arc::new(Market::offline(store.clone(), [].into()).await.unwrap());
        let proxy: std::net::IpAddr = "10.0.0.2".parse().unwrap();
        let docker = "172.16.0.0/12".parse().unwrap();
        let app = App::new(store, market, dir.path().join("k.json"))
            .with_trusted_proxies(vec!["10.0.0.2".parse().unwrap(), docker]);
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            "6.6.6.6, 203.0.113.5, 10.0.0.2".parse().unwrap(),
        );
        let ip = |s: &str| Some(s.parse::<std::net::IpAddr>().unwrap());
        // A spoofed left-most hop is ignored; the right-most untrusted hop is the client.
        assert_eq!(app.client_ip(Some(proxy), &headers), ip("203.0.113.5"));
        // Direct clients cannot claim another address.
        assert_eq!(
            app.client_ip(ip("198.51.100.1"), &headers),
            ip("198.51.100.1")
        );
        // Any address in a trusted CIDR counts as the proxy, so container restarts are safe.
        assert_eq!(app.client_ip(ip("172.18.0.7"), &headers), ip("203.0.113.5"));
        let mapped = "::ffff:10.0.0.2".parse::<std::net::IpAddr>().ok();
        assert_eq!(app.client_ip(mapped, &headers), ip("203.0.113.5"));
    });
}

#[tokio::test]
async fn resting_limit_orders_fill_into_a_push_before_it_is_printed() {
    let h = Harness::new().await;
    let maker = h.register("maker").await;
    let taker = h.register("taker").await;
    let admin = h.admin().await;
    for (name, request_id) in [
        ("maker", "00000000000000000000000000000001"),
        ("taker", "00000000000000000000000000000002"),
    ] {
        let (status, v) = h
            .request(
                "/api/admin/deposit",
                Some(json!({"name":name,"amount":100_000.0,"request_id":request_id})),
                &admin,
            )
            .await;
        assert_eq!(status, 200, "{v}");
    }
    for token in [&maker, &taker] {
        h.request("/api/symbol", Some(json!({"symbol":"NSTAR"})), token)
            .await;
    }
    // Seed the offline NSTAR book.
    let (_, v) = h
        .request("/api/spot", Some(json!({"side":"buy","units":1})), &maker)
        .await;
    let mid = v["state"]["nstar_price"].as_f64().unwrap();
    // Maker rests a sell limit 1% up (1,000 NSTAR) with TP/SL; it does not fill yet.
    let (status, v) = h
        .request(
            "/api/order",
            Some(
                json!({"kind":"limit","side":"sell","lots":100_000,"price":mid*1.01,
                "tp":mid*0.9,"sl":mid*1.2}),
            ),
            &maker,
        )
        .await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["state"]["orders"].as_array().unwrap().len(), 1, "{v}");
    assert!(v["state"]["positions"].as_array().unwrap().is_empty());
    // Bad orders are refused: a sell stop above the market, an unknown kind.
    for body in [
        json!({"kind":"stop","side":"sell","lots":100,"price":mid*1.05}),
        json!({"kind":"iceberg","side":"buy","lots":100,"price":mid}),
    ] {
        let (status, v) = h.request("/api/order", Some(body), &maker).await;
        assert_eq!(status, 400, "{v}");
    }
    // Taker lifts the book ~3% with 1,500 NSTAR at market, with its own TP/SL.
    let (status, v) = h
        .request(
            "/api/trade",
            Some(json!({"side":"buy","lots":150_000,"tp":mid*1.5,"sl":mid*0.8})),
            &taker,
        )
        .await;
    assert_eq!(status, 200, "{v}");
    let p = &v["state"]["positions"][0];
    assert_eq!(
        (p["tp"].as_f64(), p["sl"].as_f64()),
        (Some(mid * 1.5), Some(mid * 0.8))
    );
    // The maker filled inside the same request, at or above its limit, keeping its TP/SL.
    let state = h.request("/api/state", None, &maker).await.1;
    let filled = &state["positions"][0];
    assert_eq!(filled["side"], "short", "{state}");
    assert!(
        filled["entry"].as_f64().unwrap() >= mid * 1.01 - 1e-9,
        "{state}"
    );
    assert_eq!(filled["tp"].as_f64(), Some(mid * 0.9));
    // It sold only until the bid was back at its limit; any remainder keeps resting.
    let left = state["orders"]
        .as_array()
        .unwrap()
        .first()
        .map_or(0.0, |o| o["lots"].as_f64().unwrap());
    // Several matching rounds may each open a slice.
    let sold: f64 = state["positions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["lots"].as_f64().unwrap())
        .sum();
    assert!((sold + left - 100_000.0).abs() < 0.011, "{sold} + {left}");
    let bid = state["bid"].as_f64().unwrap();
    assert!(
        left == 0.0 || bid <= mid * 1.01 + 1e-6,
        "rests while bid {bid} is above it"
    );
    // ...so the +3% never printed: price and the 1s candle high stay near the limit.
    let (_, k) = h
        .request("/api/klines?symbol=NSTAR&interval=1s&limit=5", None, "")
        .await;
    let high = k["klines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b[2].as_f64().unwrap())
        .fold(0.0, f64::max);
    assert!(high < mid * 1.02, "wick printed: {high} vs mid {mid}");
    // Cancelling: unknown ids fail, real ones are recorded.
    let (status, _) = h
        .request("/api/cancel", Some(json!({"id":999})), &maker)
        .await;
    assert_eq!(status, 400);
    let (_, v) = h
        .request(
            "/api/order",
            Some(json!({"kind":"limit","side":"buy","lots":100,"price":mid*0.5})),
            &maker,
        )
        .await;
    let orders = v["state"]["orders"].as_array().unwrap();
    let id = orders.last().unwrap()["id"].as_u64().unwrap();
    let (status, v) = h
        .request("/api/cancel", Some(json!({"id":id})), &maker)
        .await;
    assert_eq!(status, 200, "{v}");
    assert!(
        !v["state"]["orders"]
            .as_array()
            .unwrap()
            .iter()
            .any(|o| o["id"] == id)
    );
    let rows = h.request("/api/history", None, &maker).await.1;
    assert!(
        rows["rows"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["kind"] == "cancel")
    );
}

#[tokio::test]
async fn order_and_position_ids_are_not_limited_by_leverage() {
    let h = Harness::new().await;
    let token = h.register("large_ids").await;
    h.request("/api/symbol", Some(json!({"symbol":"NSTAR"})), &token)
        .await;
    for _ in 0..11 {
        let (status, v) = h
            .request("/api/trade", Some(json!({"side":"buy","lots":1.0})), &token)
            .await;
        assert_eq!(status, 200, "{v}");
    }
    let state = h.request("/api/state", None, &token).await.1;
    let position = state["positions"].as_array().unwrap().last().unwrap();
    assert_eq!(position["id"], 11);
    let mid = state["price"].as_f64().unwrap();
    let (status, v) = h
        .request(
            "/api/tpsl",
            Some(json!({"id":11,"tp":mid*1.5,"sl":mid*0.5})),
            &token,
        )
        .await;
    assert_eq!(status, 200, "{v}");
    for _ in 0..11 {
        let (status, v) = h
            .request(
                "/api/order",
                Some(json!({"kind":"limit","side":"buy","lots":1.0,"price":1.0})),
                &token,
            )
            .await;
        assert_eq!(status, 200, "{v}");
    }
    let (status, v) = h
        .request("/api/cancel", Some(json!({"id":11})), &token)
        .await;
    assert_eq!(status, 200, "{v}");
    assert!(
        v["state"]["orders"]
            .as_array()
            .unwrap()
            .iter()
            .all(|o| o["id"] != 11)
    );
}

#[tokio::test]
async fn nstar_spot_limit_and_stop_orders_use_the_shared_book() {
    let h = Harness::new().await;
    let token = h.register("spot_orders").await;
    h.request("/api/symbol", Some(json!({"symbol":"NSTAR"})), &token)
        .await;
    let market = h.request("/api/market", None, "").await.1;
    let mid = market["fair"].as_f64().unwrap();
    let (status, v) = h
        .request(
            "/api/order",
            Some(json!({"market":"spot","kind":"limit","side":"buy","units":2.0,"price":mid*0.9})),
            &token,
        )
        .await;
    assert_eq!(status, 200, "{v}");
    assert!(v["state"]["orders"].is_array(), "{v}");
    assert_eq!(v["state"]["orders"].as_array().unwrap().len(), 1, "{v}");
    assert_eq!(v["state"]["orders"][0]["spot"], true);
    assert!(v["state"]["positions"].as_array().unwrap().is_empty());

    let quote = |price, now| fx_sim_rust::market::Quote {
        price,
        at: now,
        received_at: now,
        bid_ask: None,
        open: true,
        anchor: Some(price.ln()),
        mark: Some(price),
    };
    h.app
        .market
        .on_market_quote("NSTAR", quote(mid * 0.89, now_ms()))
        .await
        .unwrap();
    let state = h.request("/api/state", None, &token).await.1;
    assert!(
        (state["spot"][0]["units"].as_f64().unwrap() - 2.0).abs() < 1e-9,
        "{state}"
    );
    assert!(state["orders"].as_array().unwrap().is_empty());
    assert!(state["positions"].as_array().unwrap().is_empty());

    let (status, v) = h.request("/api/order", Some(json!({"market":"spot","kind":"stop","side":"sell","units":1.0,"price":mid*0.95,"limit":mid*0.935})), &token).await;
    assert_eq!(status, 200, "{v}");
    h.app
        .market
        .on_market_quote("NSTAR", quote(mid * 0.96, now_ms()))
        .await
        .unwrap();
    let state = h.request("/api/state", None, &token).await.1;
    assert_eq!(state["orders"].as_array().unwrap().len(), 1);
    h.app
        .market
        .on_market_quote("NSTAR", quote(mid * 0.939, now_ms()))
        .await
        .unwrap();
    let state = h.request("/api/state", None, &token).await.1;
    assert!(
        (state["spot"][0]["units"].as_f64().unwrap() - 1.0).abs() < 1e-9,
        "{state}"
    );
    assert!(state["orders"].as_array().unwrap().is_empty());
    let history = h.request("/api/history", None, &token).await.1;
    assert!(
        history["rows"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["kind"] == "spot" && r["side"] == "sell")
    );
}

#[tokio::test]
async fn nstar_spot_wallet_tpsl_and_public_market_panel() {
    let h = Harness::new().await;
    let token = h.register("spotter").await;
    let (status, market) = h.request("/api/market", None, "").await;
    assert_eq!(status, 200, "{market}");
    for key in [
        "high_24h",
        "low_24h",
        "volume_24h",
        "fair",
        "funding_rate",
        "tape",
    ] {
        assert!(!market[key].is_null(), "{key} missing: {market}");
    }
    let (status, v) = h
        .request("/api/spot", Some(json!({"side":"buy","units":5})), &token)
        .await;
    assert_eq!(status, 200, "{v}");
    // The reply carries the post-trade account, so the page needs no follow-up GET.
    assert_eq!(v["state"]["type"], "state");
    assert!((v["state"]["spot"][0]["units"].as_f64().unwrap() - 5.0).abs() < 1e-9);
    let state = h.request("/api/state", None, &token).await.1;
    let spot = &state["spot"][0];
    assert_eq!(spot["sym"], "NSTAR");
    assert!((spot["units"].as_f64().unwrap() - 5.0).abs() < 1e-9);
    assert!(state["balance"].as_f64().unwrap() < 10_000.0);
    assert!(state["nstar_price"].as_f64().unwrap() > 0.0);
    let (status, v) = h
        .request("/api/spot", Some(json!({"side":"sell","units":6})), &token)
        .await;
    assert_eq!(status, 400, "oversell: {v}");
    let (status, v) = h
        .request("/api/spot", Some(json!({"side":"sell","units":5})), &token)
        .await;
    assert_eq!(status, 200, "{v}");
    let history = h.request("/api/history", None, &token).await.1;
    assert!(
        history["rows"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["kind"] == "spot")
    );
    // A trade print reaches the public tape.
    let market = h.request("/api/market", None, "").await.1;
    assert!(!market["tape"].as_array().unwrap().is_empty());
    // Perpetual NSTAR with take-profit / stop-loss.
    let (status, _) = h
        .request("/api/symbol", Some(json!({"symbol":"NSTAR"})), &token)
        .await;
    assert_eq!(status, 200);
    let (status, v) = h
        .request(
            "/api/trade",
            Some(json!({"side":"buy","lots":1000})),
            &token,
        )
        .await;
    assert_eq!(status, 200, "{v}");
    let state = h.request("/api/state", None, &token).await.1;
    let p = &state["positions"][0];
    let (id, mid) = (p["id"].as_u64().unwrap(), state["price"].as_f64().unwrap());
    assert!(p["liq"].is_null() || p["liq"].as_f64().unwrap() < mid);
    assert_eq!(p["leverage"], 10);
    let (status, v) = h
        .request(
            "/api/tpsl",
            Some(json!({"id":id,"tp":mid*0.5,"sl":0})),
            &token,
        )
        .await;
    assert_eq!(status, 400, "tp below long: {v}");
    let (status, v) = h
        .request(
            "/api/tpsl",
            Some(json!({"id":id,"tp":mid*1.5,"sl":mid*0.5})),
            &token,
        )
        .await;
    assert_eq!(status, 200, "{v}");
    let p = &h.request("/api/state", None, &token).await.1["positions"][0];
    assert!((p["tp"].as_f64().unwrap() - mid * 1.5).abs() < 1e-6);
}

#[tokio::test]
async fn leaderboard_cold_and_warm_http_are_read_only_top50_self_and_bounded_pages() {
    let mut h = Harness::new().await;
    h.tasks.shutdown().await;
    for index in 0..125 {
        h.app
            .store
            .register(&format!("rank{index:03}"), "pw")
            .await
            .unwrap();
    }
    let token = h.app.store.login("rank124", "pw").await.unwrap();
    h.sql("CREATE TRIGGER no_equity_insert BEFORE INSERT ON equity_samples BEGIN SELECT RAISE(ABORT,'GET must not sample'); END;
        CREATE TRIGGER no_equity_update BEFORE UPDATE ON equity_samples BEGIN SELECT RAISE(ABORT,'GET must not sample'); END;
        CREATE TRIGGER no_equity_delete BEFORE DELETE ON equity_samples BEGIN SELECT RAISE(ABORT,'GET must not prune'); END;").await;
    let before = h.app.store.persistence_stats();
    let responses = futures_util::future::join_all(
        (0..16).map(|_| h.request("/api/leaderboard", None, &token)),
    )
    .await;
    let first = &responses[0].1;
    for (status, value) in &responses {
        assert_eq!(*status, 200, "{value}");
        assert_eq!(value, first, "cold concurrent readers share one snapshot");
    }
    assert_eq!(first["rows"].as_array().unwrap().len(), 50);
    assert_eq!(first["total"], 125);
    assert_eq!(first["me"]["name"], "rank124");
    assert_eq!(first["me"]["rank"], 125);
    assert_eq!(first["rows"][0]["rank"], 1);
    assert!(first["as_of"].as_i64().unwrap() > 0);
    let public = h.request("/api/leaderboard", None, "").await.1;
    assert!(public["me"].is_null());
    let page = h
        .request("/api/leaderboard?offset=10&limit=999999", None, &token)
        .await
        .1;
    assert_eq!(page["rows"].as_array().unwrap().len(), 100);
    assert_eq!(page["rows"][0]["rank"], 11);
    assert_eq!(page["as_of"], first["as_of"]);
    assert_eq!(
        h.request("/api/leaderboard?offset=999&limit=100", None, "")
            .await
            .1["rows"],
        json!([])
    );
    for query in ["?offset=-1", "?limit=nope"] {
        assert_eq!(
            h.request(&format!("/api/leaderboard{query}"), None, "")
                .await
                .0,
            400
        );
    }
    assert_eq!(
        h.app.store.persistence_stats()["commits"],
        before["commits"]
    );
    // Warm GETs do not recalculate after account changes; only the producer refreshes.
    h.app
        .store
        .transaction("rank124", |a, _, _| {
            a.balance = 20000.0;
            Ok(vec![])
        })
        .await
        .unwrap();
    assert_eq!(h.request("/api/leaderboard", None, &token).await.1, *first);
    let refresh = h.app.refresh_leaderboard().await.unwrap();
    assert_eq!(refresh["rows"][0]["name"], "rank124");
    assert!(refresh["rows"][0]["ret_5m"].is_null());
    let pool = sqlx::SqlitePool::connect(&format!(
        "sqlite://{}",
        h.dir.path().join("test.db").display()
    ))
    .await
    .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM equity_samples")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    pool.close().await;
    // Exactly the explicit test transaction may write; display work never adds commits.
    let after = h.app.store.persistence_stats();
    assert_eq!(after["failures"], before["failures"]);
}

async fn view(ws: &mut Ws, visible: bool, leaderboard: bool, interval: &str, low_power: bool) {
    ws.send(Message::Text(
        json!({"type":"view","visible":visible,"leaderboard":leaderboard,
        "interval":interval,"low_power":low_power})
        .to_string()
        .into(),
    ))
    .await
    .unwrap();
    ws.send(Message::Ping(vec![9, 8, 7].into())).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(Ok(Message::Pong(bytes))) = ws.next().await
                && bytes.as_ref() == [9, 8, 7]
            {
                break;
            }
        }
    })
    .await
    .unwrap();
}
async fn quote_packet(ws: &mut Ws) -> Value {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Some(Ok(Message::Text(text))) = ws.next().await {
                let value: Value = serde_json::from_str(&text).unwrap();
                if value.get("type").is_none() && value.get("sym").is_some() {
                    return value;
                }
            }
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn websocket_hidden_restore_interval_low_power_and_auth_survive_quote_flood() {
    let mut h = Harness::new().await;
    h.tasks.shutdown().await;
    let token = h.register("viewer").await;
    let mut ws = h.ws(&token).await;
    packet(&mut ws, "state").await;
    view(&mut ws, false, true, "1m", false).await;
    for price in 1..=2000 {
        h.app.broadcast(
            json!({"sym":"USDJPY","price":price,"klines":{"1m":[[1,2,3,4,5]],"5m":[[6,7,8,9,10]]}}),
        );
    }
    h.app.refresh_leaderboard().await.unwrap();
    // No market or leaderboard packets while hidden, even after overflowing the old ring.
    let quiet = tokio::time::sleep(Duration::from_millis(350));
    tokio::pin!(quiet);
    loop {
        tokio::select! {
            _ = &mut quiet => break,
            message = ws.next() => {
                if let Some(Ok(Message::Text(text))) = message {
                    let value: Value = serde_json::from_str(&text).unwrap();
                    assert!(value.get("sym").is_none() && value["type"] != "lb", "hidden display: {value}");
                }
            }
        }
    }
    h.app.broadcast(json!({"type":"symbols","rows":[]}));
    assert_eq!(packet(&mut ws, "symbols").await["rows"], json!([]));
    h.app
        .store
        .transaction("viewer", |a, _, _| {
            a.balance += 100.0;
            Ok(vec![])
        })
        .await
        .unwrap();
    assert_eq!(packet(&mut ws, "state").await["balance"], 10100.0);
    view(&mut ws, true, true, "1m", true).await;
    let tick = quote_packet(&mut ws).await;
    assert_eq!(tick["price"], 2000);
    assert_eq!(tick["klines"].as_object().unwrap().len(), 1);
    assert!(tick["klines"]["1m"].is_array());
    for price in 2001..=2200 {
        h.app.broadcast(json!({"sym":"USDJPY","price":price}));
    }
    let start = tokio::time::Instant::now();
    let tick = quote_packet(&mut ws).await;
    assert_eq!(
        tick["price"], 2200,
        "latest quote wins instead of FIFO backlog"
    );
    assert!(
        start.elapsed() >= Duration::from_millis(650),
        "low power must limit quotes to 1Hz"
    );
    view(&mut ws, false, false, "1m", true).await;
    // Deliberately omit App::deleted: periodic authentication is the independent safeguard.
    h.app
        .store
        .delete("viewer", "viewer", now_ms())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                _ => {}
            }
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn performance_endpoint_is_admin_only() {
    let h = Harness::new().await;
    assert_eq!(h.request("/api/admin/performance", None, "").await.0, 401);
    let admin = h.admin().await;
    let (status, value) = h.request("/api/admin/performance", None, &admin).await;
    assert_eq!(status, 200);
    assert!(value["persistence"].is_object());
    assert!(value["pending_bars"].is_u64());
}
