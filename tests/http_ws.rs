use futures_util::{SinkExt, StreamExt};
use fx_sim_rust::{
    api,
    app::App,
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
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&dir.path().join("test.db")).await.unwrap());
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
        let app = Arc::new(App::new(store, market, key));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let static_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("static");
        let router = api::router(app.clone(), static_dir);
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
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
                Some(json!({"action":"register","name":name,"pwd":"pw"})),
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
    near(&state["used"], 100.0);
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
        h.request("/api/leverage", Some(json!({"leverage":200})), &token)
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
