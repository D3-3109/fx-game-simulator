use serde_json::{Value, json};
use sqlx::{Row, sqlite::SqliteConnectOptions};
use std::{path::Path, process::Command};

async fn seed(path: &Path) -> Value {
    let pool = sqlx::SqlitePool::connect_with(
        SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true),
    )
    .await
    .unwrap();
    sqlx::raw_sql(include_str!("../src/schema.sql"))
        .execute(&pool)
        .await
        .unwrap();
    let account = json!({
        "balance":12345.0,"leverage":10,"next_id":3,
        "positions":[
            {"id":1,"side":"long","lots":0.01,"entry":1.1,"symbol":"FXSIM"},
            {"id":2,"side":"short","lots":100.0,"entry":100.0,"symbol":"NSTAR"}
        ],"orders":[],"spot":{},"loan":50.0,"interest_at":1,"debt_since":0
    });
    sqlx::query("INSERT INTO users(name,salt,hash,token,symbol,account,created) VALUES('legacy','','','token','USDJPY',?,1)")
        .bind(account.to_string()).execute(&pool).await.unwrap();
    pool.close().await;
    account
}

#[tokio::test]
async fn cli_requires_explicit_backup_and_preserves_original_data() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.db");
    let backup = dir.path().join("before.db");
    let original = seed(&path).await;
    let binary = env!("CARGO_BIN_EXE_fx-sim-rust");
    let rejected = Command::new(binary)
        .arg("--db")
        .arg(&path)
        .arg("--migrate-nstar-only")
        .output()
        .unwrap();
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("--migration-backup"));
    let migrated = Command::new(binary)
        .arg("--db")
        .arg(&path)
        .arg("--migrate-nstar-only")
        .arg("--migration-backup")
        .arg(&backup)
        .output()
        .unwrap();
    assert!(
        migrated.status.success(),
        "{}",
        String::from_utf8_lossy(&migrated.stderr)
    );
    let summary: Value = serde_json::from_slice(&migrated.stdout).unwrap();
    assert_eq!(summary["accounts"], 1);
    assert_eq!(summary["positions"], 1);
    for (db, expected_positions, symbol) in [(&path, 1, "NSTAR"), (&backup, 2, "USDJPY")] {
        let pool = sqlx::SqlitePool::connect_with(
            SqliteConnectOptions::new().filename(db).read_only(true),
        )
        .await
        .unwrap();
        let row = sqlx::query("SELECT symbol,account FROM users WHERE name='legacy'")
            .fetch_one(&pool)
            .await
            .unwrap();
        let account: Value = serde_json::from_str(row.get("account")).unwrap();
        assert_eq!(row.get::<String, _>("symbol"), symbol);
        assert_eq!(
            account["positions"].as_array().unwrap().len(),
            expected_positions
        );
        assert_eq!(account["balance"], original["balance"]);
        assert_eq!(account["loan"], original["loan"]);
        pool.close().await;
    }
    let repeated = Command::new(binary)
        .arg("--db")
        .arg(&path)
        .arg("--migrate-nstar-only")
        .arg("--migration-backup")
        .arg(&backup)
        .output()
        .unwrap();
    assert!(repeated.status.success());
    let summary: Value = serde_json::from_slice(&repeated.stdout).unwrap();
    assert_eq!(summary["accounts"], 0);
}

#[tokio::test]
async fn normal_startup_refuses_legacy_exposure_without_deleting_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.db");
    seed(&path).await;
    let admin = dir.path().join("admin.json");
    std::fs::write(
        &admin,
        json!({"salt":"00","hash":"00".repeat(32)}).to_string(),
    )
    .unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_fx-sim-rust"))
        .arg("--db")
        .arg(&path)
        .arg("--static-dir")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("static"))
        .arg("--admin-key")
        .arg(&admin)
        .arg("--listen")
        .arg("127.0.0.1:0")
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("--migrate-nstar-only"));
    let store = fx_sim_rust::storage::Store::open_legacy(&path)
        .await
        .unwrap();
    assert_eq!(
        store.user("legacy").await.unwrap().account.positions.len(),
        2
    );
    store.close().await;
}
