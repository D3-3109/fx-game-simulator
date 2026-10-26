use super::*;
use crate::trading::{Holding, Order, Position};

async fn legacy_account(path: &Path) -> User {
    let store = Store::open_legacy(path).await.unwrap();
    store.register("legacy", "Password123").await.unwrap();
    store
        .transaction("legacy", |account, symbol, id| {
            *symbol = "FXSIM".into();
            let trade = account.open(
                "long",
                0.01,
                "FXSIM",
                &[("FXSIM".into(), 1.1)].into(),
                id,
                1,
            )?;
            account.positions.push(Position {
                id: id + 1,
                symbol: symbols::NSTAR.into(),
                side: "long".into(),
                lots: 100.0,
                entry: 100.0,
                tp: 120.0,
                sl: 80.0,
            });
            account.next_id = id + 2;
            account.orders = vec![
                Order {
                    id: 1,
                    symbol: "FXSIM".into(),
                    side: "long".into(),
                    spot: false,
                    kind: "limit".into(),
                    lots: 0.01,
                    price: 0.9,
                    limit: 0.0,
                    tp: 0.0,
                    sl: 0.0,
                    created: 1,
                },
                Order {
                    id: 2,
                    symbol: symbols::NSTAR.into(),
                    side: "long".into(),
                    spot: false,
                    kind: "limit".into(),
                    lots: 100.0,
                    price: 90.0,
                    limit: 0.0,
                    tp: 0.0,
                    sl: 0.0,
                    created: 1,
                },
            ];
            account.order_seq = 2;
            account.spot.insert(
                symbols::NSTAR.into(),
                Holding {
                    units: 2.0,
                    cost: 100.0,
                },
            );
            account.loan = 123.0;
            account.interest_at = 456;
            account.debt_since = 789;
            Ok(vec![trade])
        })
        .await
        .unwrap();
    sqlx::query("INSERT INTO equity_samples VALUES('legacy',1,10000,0)")
        .execute(&store.pool)
        .await
        .unwrap();
    let user = store.user("legacy").await.unwrap();
    store.close().await;
    user
}

#[tokio::test]
async fn production_store_defaults_to_nstar_and_rejects_other_mutations() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("test.db")).await.unwrap();
    assert!(store.is_nstar_only());
    store.register("player", "Password123").await.unwrap();
    assert_eq!(store.user("player").await.unwrap().symbol, symbols::NSTAR);
    store.ensure_nstar_only().await.unwrap();
    let settings = store.settings().await.unwrap();
    assert_eq!(settings["rows"].as_array().unwrap().len(), 1);
    assert_eq!(settings["rows"][0]["symbol"], symbols::NSTAR);
    assert!(store.select_symbol("player", "USDJPY").await.is_err());
    assert!(store.require_tradable("FXSIM").await.is_err());
    assert!(store.set_symbol("USDJPY", true, 1).await.is_err());
    assert!(
        store
            .transaction("player", |account, _, id| {
                Ok(vec![account.open(
                    "long",
                    0.01,
                    "FXSIM",
                    &[("FXSIM".into(), 1.1)].into(),
                    id,
                    1,
                )?])
            })
            .await
            .is_err()
    );
    assert!(
        store
            .user("player")
            .await
            .unwrap()
            .account
            .positions
            .is_empty()
    );
    assert!(store.history("player").await.unwrap().is_empty());
    store.close().await;
}

#[tokio::test]
async fn migration_backs_up_and_discards_legacy_exposure_without_settlement() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");
    let backup = dir.path().join("before.db");
    let before = legacy_account(&path).await;
    let store = Store::open(&path).await.unwrap();
    assert!(store.ensure_nstar_only().await.is_err());
    let report = store.migrate_nstar_only(&backup).await.unwrap();
    assert_eq!(
        (
            report.accounts,
            report.positions,
            report.orders,
            report.spot_assets
        ),
        (1, 1, 1, 0)
    );
    assert_eq!(report.backup.as_deref(), Some(backup.as_path()));
    store.ensure_nstar_only().await.unwrap();
    let user = store.user("legacy").await.unwrap();
    let mut expected = before.account.clone();
    expected.positions.retain(|p| p.symbol == symbols::NSTAR);
    expected.orders.retain(|o| o.symbol == symbols::NSTAR);
    assert_eq!(user.account, expected);
    assert_eq!(user.symbol, symbols::NSTAR);
    assert_eq!(user.last_trade, before.last_trade);
    assert_eq!(store.history("legacy").await.unwrap().len(), 1);
    assert_eq!(store.take_nstar_flow(), (0.0, 0.0));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM equity_samples")
            .fetch_one(&store.pool)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM admin_events WHERE kind='nstar_migration'"
        )
        .fetch_one(&store.pool)
        .await
        .unwrap(),
        1
    );
    let again = store.migrate_nstar_only(&backup).await.unwrap();
    assert_eq!(again.accounts, 0);
    assert!(again.backup.is_none());
    store.close().await;
    let restored = Store::open_legacy(&backup).await.unwrap();
    assert_eq!(
        restored.user("legacy").await.unwrap().account,
        before.account
    );
    assert_eq!(restored.user("legacy").await.unwrap().symbol, before.symbol);
    assert_eq!(restored.history("legacy").await.unwrap().len(), 1);
    restored.close().await;
    let reopened = Store::open(&path).await.unwrap();
    reopened.ensure_nstar_only().await.unwrap();
    assert_eq!(reopened.user("legacy").await.unwrap().account, expected);
    reopened.close().await;
}

async fn replace_account(path: &Path, account: &Account) {
    let pool = SqlitePool::connect_with(SqliteConnectOptions::new().filename(path))
        .await
        .unwrap();
    sqlx::query("UPDATE users SET account=? WHERE name='legacy'")
        .bind(serde_json::to_string(account).unwrap())
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
}

#[tokio::test]
async fn migration_accepts_legacy_spot_and_invalid_discarded_positions_and_orders() {
    for invalid_discarded in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy.db");
        let backup = dir.path().join("before.db");
        let mut before = legacy_account(&path).await;
        before.account.spot.insert(
            "BTCUSDT".into(),
            Holding {
                units: 1.25,
                cost: 60000.0,
            },
        );
        if invalid_discarded {
            before.account.positions[0].lots = -1.0;
            before.account.orders[0].spot = true;
            before.account.orders[0].side = "buy".into();
        }
        replace_account(&path, &before.account).await;
        assert!(Store::open(&path).await.is_err());
        assert!(Store::open_legacy(&path).await.is_err());
        let store = Store::open_for_migration(&path).await.unwrap();
        assert!(store.ensure_nstar_only().await.is_err());
        let report = store.migrate_nstar_only(&backup).await.unwrap();
        assert_eq!(
            (
                report.accounts,
                report.positions,
                report.orders,
                report.spot_assets
            ),
            (1, 1, 1, 1)
        );
        let mut expected = before.account.clone();
        expected.positions.retain(|p| p.symbol == symbols::NSTAR);
        expected.orders.retain(|o| o.symbol == symbols::NSTAR);
        expected.spot.retain(|s, _| s == symbols::NSTAR);
        let after = store.user("legacy").await.unwrap();
        assert_eq!(after.account, expected);
        assert_eq!(after.account.balance, before.account.balance);
        assert_eq!(after.symbol, symbols::NSTAR);
        assert_eq!(after.last_trade, before.last_trade);
        assert_eq!(store.take_nstar_flow(), (0.0, 0.0));
        store.ensure_nstar_only().await.unwrap();
        store.close().await;
        let pool = SqlitePool::connect_with(
            SqliteConnectOptions::new()
                .filename(&backup)
                .read_only(true),
        )
        .await
        .unwrap();
        let original: String = sqlx::query_scalar("SELECT account FROM users WHERE name='legacy'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Account>(&original).unwrap(),
            before.account
        );
        pool.close().await;
        let reopened = Store::open(&path).await.unwrap();
        assert_eq!(reopened.user("legacy").await.unwrap().account, expected);
        reopened.close().await;
    }
}

#[tokio::test]
async fn migration_rejects_invalid_retained_nstar_and_leaves_disk_and_cache_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.db");
    let backup = dir.path().join("before.db");
    let mut before = legacy_account(&path).await;
    before.account.positions[1].lots = -1.0;
    replace_account(&path, &before.account).await;
    let store = Store::open_for_migration(&path).await.unwrap();
    assert!(store.migrate_nstar_only(&backup).await.is_err());
    assert!(backup.is_file());
    assert_eq!(store.user("legacy").await.unwrap().account, before.account);
    let raw: String = sqlx::query_scalar("SELECT account FROM users WHERE name='legacy'")
        .fetch_one(&store.pool)
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Account>(&raw).unwrap(),
        before.account
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM admin_events WHERE kind='nstar_migration'"
        )
        .fetch_one(&store.pool)
        .await
        .unwrap(),
        0
    );
    store.close().await;
}

#[cfg(unix)]
#[tokio::test]
async fn migration_backup_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.db");
    let backup = dir.path().join("before.db");
    legacy_account(&path).await;
    let store = Store::open_for_migration(&path).await.unwrap();
    store.migrate_nstar_only(&backup).await.unwrap();
    assert_eq!(
        std::fs::metadata(&backup).unwrap().permissions().mode() & 0o777,
        0o600
    );
    store.close().await;
}

#[cfg(windows)]
#[tokio::test]
async fn migration_backup_has_protected_owner_and_system_acl() {
    use std::{os::windows::io::AsRawHandle, ptr::null_mut};
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::{
            ACCESS_ALLOWED_ACE,
            Authorization::{GetSecurityInfo, SE_FILE_OBJECT},
            DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetSecurityDescriptorControl,
            IsWellKnownSid, OWNER_SECURITY_INFORMATION, SE_DACL_PROTECTED, WinLocalSystemSid,
        },
        Storage::FileSystem::FILE_ALL_ACCESS,
        System::SystemServices::ACCESS_ALLOWED_ACE_TYPE,
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.db");
    let backup = dir.path().join("before.db");
    legacy_account(&path).await;
    let store = Store::open_for_migration(&path).await.unwrap();
    store.migrate_nstar_only(&backup).await.unwrap();
    let file = std::fs::File::open(&backup).unwrap();
    // SAFETY: GetSecurityInfo owns the descriptor until LocalFree; all SID/ACE
    // pointers remain inside that live descriptor and are read only.
    unsafe {
        let (mut owner, mut acl, mut descriptor) = (null_mut(), null_mut(), null_mut());
        assert_eq!(
            GetSecurityInfo(
                file.as_raw_handle(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut owner,
                null_mut(),
                &mut acl,
                null_mut(),
                &mut descriptor
            ),
            0
        );
        assert!(!acl.is_null());
        let (mut control, mut revision) = (0, 0);
        assert_ne!(
            GetSecurityDescriptorControl(descriptor, &mut control, &mut revision),
            0
        );
        assert_ne!(control & SE_DACL_PROTECTED, 0);
        assert_eq!((*acl).AceCount, 2);
        let (mut owner_grant, mut system_grant) = (false, false);
        for index in 0..2 {
            let mut ace = null_mut();
            assert_ne!(GetAce(acl, index, &mut ace), 0);
            let ace = &*ace.cast::<ACCESS_ALLOWED_ACE>();
            assert_eq!(u32::from(ace.Header.AceType), ACCESS_ALLOWED_ACE_TYPE);
            assert_eq!(ace.Header.AceFlags, 0);
            assert_eq!(ace.Mask, FILE_ALL_ACCESS);
            let sid = std::ptr::addr_of!(ace.SidStart).cast_mut().cast();
            let is_owner = EqualSid(sid, owner) != 0;
            let is_system = IsWellKnownSid(sid, WinLocalSystemSid) != 0;
            assert!(is_owner || is_system);
            owner_grant |= is_owner;
            system_grant |= is_system;
        }
        assert!(owner_grant && system_grant);
        LocalFree(descriptor);
    }
    store.close().await;
}

#[tokio::test]
async fn migration_fails_closed_for_existing_backup_and_rolls_back_audit_failure() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.db");
    let before = legacy_account(&path).await;
    let store = Store::open(&path).await.unwrap();
    let existing = dir.path().join("existing.db");
    std::fs::write(&existing, b"do not overwrite").unwrap();
    assert!(store.migrate_nstar_only(&existing).await.is_err());
    assert_eq!(std::fs::read(&existing).unwrap(), b"do not overwrite");
    assert_eq!(store.user("legacy").await.unwrap().account, before.account);
    assert!(
        store
            .migrate_nstar_only(&dir.path().join("missing/backup.db"))
            .await
            .is_err()
    );
    sqlx::raw_sql("CREATE TRIGGER fail_migration BEFORE INSERT ON admin_events WHEN NEW.kind='nstar_migration' BEGIN SELECT RAISE(ABORT,'test failure'); END;").execute(&store.pool).await.unwrap();
    let backup = dir.path().join("rollback.db");
    assert!(store.migrate_nstar_only(&backup).await.is_err());
    assert!(backup.is_file());
    assert_eq!(store.user("legacy").await.unwrap().account, before.account);
    assert!(store.ensure_nstar_only().await.is_err());
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM equity_samples")
            .fetch_one(&store.pool)
            .await
            .unwrap(),
        1
    );
    store.close().await;
    let reopened = Store::open_legacy(&path).await.unwrap();
    assert_eq!(
        reopened.user("legacy").await.unwrap().account,
        before.account
    );
    assert_eq!(reopened.user("legacy").await.unwrap().symbol, before.symbol);
    reopened.close().await;
}
