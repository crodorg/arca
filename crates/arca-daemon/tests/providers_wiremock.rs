//! Provider integration tests using wiremock fixtures.
//! These spin up a local mock HTTP server, point each provider at it, and
//! assert what lands in the DB.

use std::sync::Arc;

use arca_core::db::{Db, ProviderRow};
use arca_core::ids::ProviderId;
use arca_core::provider::{Ctx, Provider};
use serde_json::json;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use arca_daemon::providers::mercury::MercuryProvider;
use arca_daemon::providers::plaid::PlaidProvider;
use arca_daemon::providers::stripe::StripeProvider;
use arca_daemon::providers::xmr_spot::XmrSpotProvider;
use arca_daemon::secrets::Secrets;

#[tokio::test]
async fn plaid_sync_roundtrip() {
    let server = MockServer::start().await;

    // /accounts/balance/get
    Mock::given(method("POST"))
        .and(path("/accounts/balance/get"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "accounts": [{
                "account_id": "plaid_acct_1",
                "name": "Plaid Checking",
                "type": "depository",
                "subtype": "checking",
                "balances": { "current": 1234.56, "iso_currency_code": "USD" }
            }]
        })))
        .mount(&server)
        .await;

    // /transactions/sync — single page, no more
    Mock::given(method("POST"))
        .and(path("/transactions/sync"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "added": [{
                "transaction_id": "plaid_txn_1",
                "account_id": "plaid_acct_1",
                "amount": 12.34,
                "date": "2026-05-20",
                "merchant_name": "Coffee Shop",
                "name": "COFFEE",
                "category": ["Food and Drink"],
                "iso_currency_code": "USD"
            }],
            "modified": [],
            "removed": [],
            "next_cursor": "cur-1",
            "has_more": false
        })))
        .mount(&server)
        .await;

    let db = Arc::new(Db::open_memory().unwrap());
    // Build a no-op Secrets store with the keys plaid expects.
    let secrets = Secrets::for_test(&[
        ("plaid_client_id", "cid"),
        ("plaid_sandbox_secret", "sec"),
        ("plaid_navy_fed_access_token", "tok"),
    ]);
    let provider_id = db
        .upsert_provider(&ProviderRow {
            id: ProviderId(0),
            kind: "plaid".into(),
            label: "navy_fed".into(),
            config_json: r#"{"plaid_env":"sandbox"}"#.into(),
            secret_ref: Some("plaid_navy_fed_access_token".into()),
            poll_cadence: "manual".into(),
            last_poll_at: None,
            last_status: None,
        })
        .unwrap();
    let row = db
        .list_providers()
        .unwrap()
        .into_iter()
        .find(|r| r.id == provider_id)
        .unwrap();
    let provider = PlaidProvider::build(&row, &secrets, Arc::clone(&db))
        .unwrap()
        .with_base_url(server.uri());

    let ctx = Ctx::new(Arc::clone(&db));
    let bal = provider.refresh_balances(&ctx).await.unwrap();
    assert_eq!(bal.rows_written, 1);

    let txr = provider.refresh_transactions(&ctx, None).await.unwrap();
    assert_eq!(txr.rows_written, 1);

    // Verify DB has the snapshot + tx.
    let accounts = db.list_active_accounts().unwrap();
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].name, "Plaid Checking");
    assert_eq!(accounts[0].external_id.as_deref(), Some("plaid_acct_1"));
    let balances = db.latest_balances().unwrap();
    assert_eq!(balances[0].1.as_i64(), 123_456);

    let txns = db.list_transactions(None, None, 100).unwrap();
    assert_eq!(txns.len(), 1);
    // Plaid amount was +12.34 (outflow); arca stores -1234 cents.
    assert_eq!(txns[0].amount_cents.as_i64(), -1234);
    assert_eq!(txns[0].external_id.as_deref(), Some("plaid_txn_1"));

    // raw responses persisted
    let raw_count: i64 = db
        .with_conn(|c| {
            c.query_row("SELECT COUNT(*) FROM provider_raw", [], |r| r.get(0))
                .map_err(Into::into)
        })
        .unwrap();
    assert_eq!(raw_count, 2);
}

#[tokio::test]
async fn mercury_roundtrip() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/accounts"))
        .and(header("authorization", "Bearer merc-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "accounts": [{
                "id": "merc_acct_1",
                "name": "Mercury Checking",
                "currency": "USD",
                "currentBalance": 5000.00
            }]
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/account/merc_acct_1/transactions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "transactions": [{
                "id": "merc_txn_1",
                "amount": -100.00,
                "postedAt": "2026-05-20T12:00:00Z",
                "counterpartyName": "AWS",
                "kind": "card_charge"
            }]
        })))
        .mount(&server)
        .await;

    let db = Arc::new(Db::open_memory().unwrap());
    let secrets = Secrets::for_test(&[("mercury_main_token", "merc-token")]);
    let pid = db
        .upsert_provider(&ProviderRow {
            id: ProviderId(0),
            kind: "mercury".into(),
            label: "main".into(),
            config_json: r#"{"business_tag":"main"}"#.into(),
            secret_ref: Some("mercury_main_token".into()),
            poll_cadence: "manual".into(),
            last_poll_at: None,
            last_status: None,
        })
        .unwrap();
    let row = db
        .list_providers()
        .unwrap()
        .into_iter()
        .find(|r| r.id == pid)
        .unwrap();
    let provider = MercuryProvider::build(&row, &secrets, Arc::clone(&db))
        .unwrap()
        .with_base_url(server.uri());

    let ctx = Ctx::new(Arc::clone(&db));
    assert_eq!(
        provider.refresh_balances(&ctx).await.unwrap().rows_written,
        1
    );
    assert_eq!(
        provider
            .refresh_transactions(&ctx, None)
            .await
            .unwrap()
            .rows_written,
        1
    );

    let txns = db.list_transactions(None, None, 100).unwrap();
    assert_eq!(txns.len(), 1);
    assert_eq!(txns[0].amount_cents.as_i64(), -10_000);
    assert_eq!(txns[0].tag.as_deref(), Some("business"));
}

#[tokio::test]
async fn stripe_roundtrip() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/v1/balance"))
        .and(header("authorization", "Bearer rk_test_stripe"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "available": [{ "amount": 250_000, "currency": "usd" }],
            "pending":   [{ "amount": 50_000,  "currency": "usd" }]
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/v1/balance_transactions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{
                "id": "txn_1",
                "amount": 100_00,
                "created": 1_716_000_000_i64,
                "type": "charge",
                "description": "customer payment",
                "currency": "usd"
            }]
        })))
        .mount(&server)
        .await;

    let db = Arc::new(Db::open_memory().unwrap());
    let secrets = Secrets::for_test(&[("stripe_main_key", "rk_test_stripe")]);
    let pid = db
        .upsert_provider(&ProviderRow {
            id: ProviderId(0),
            kind: "stripe".into(),
            label: "main".into(),
            config_json: r#"{"business_tag":"main"}"#.into(),
            secret_ref: Some("stripe_main_key".into()),
            poll_cadence: "manual".into(),
            last_poll_at: None,
            last_status: None,
        })
        .unwrap();
    let row = db
        .list_providers()
        .unwrap()
        .into_iter()
        .find(|r| r.id == pid)
        .unwrap();
    let provider = StripeProvider::build(&row, &secrets, Arc::clone(&db))
        .unwrap()
        .with_base_url(server.uri());

    let ctx = Ctx::new(Arc::clone(&db));
    let bal = provider.refresh_balances(&ctx).await.unwrap();
    assert_eq!(bal.rows_written, 1);

    let balances = db.latest_balances().unwrap();
    assert_eq!(balances[0].1.as_i64(), 300_000); // 2500.00 + 500.00

    let txr = provider.refresh_transactions(&ctx, None).await.unwrap();
    assert_eq!(txr.rows_written, 1);

    let txns = db.list_transactions(None, None, 100).unwrap();
    assert_eq!(txns[0].tag.as_deref(), Some("income"));
    assert_eq!(txns[0].amount_cents.as_i64(), 10_000);
}

#[tokio::test]
async fn xmr_spot_values_holding() {
    let server = MockServer::start().await;

    // Kraken public ticker shape; `result` keyed "XXMRZUSD", `c[0]` = last price.
    Mock::given(method("GET"))
        .and(path("/0/public/Ticker"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "error": [],
            "result": {
                "XXMRZUSD": {
                    "a": ["200.10", "1", "1.000"],
                    "b": ["199.90", "2", "2.000"],
                    "c": ["200.00", "0.5"]
                }
            }
        })))
        .mount(&server)
        .await;

    let db = Arc::new(Db::open_memory().unwrap());
    let secrets = Secrets::for_test(&[]);
    let pid = db
        .upsert_provider(&ProviderRow {
            id: ProviderId(0),
            kind: "xmr_spot".into(),
            label: "Monero".into(),
            config_json: r#"{"quantity":3.5}"#.into(),
            secret_ref: None,
            poll_cadence: "daily".into(),
            last_poll_at: None,
            last_status: None,
        })
        .unwrap();
    let row = db
        .list_providers()
        .unwrap()
        .into_iter()
        .find(|r| r.id == pid)
        .unwrap();
    let provider = XmrSpotProvider::build(&row, &secrets, Arc::clone(&db))
        .unwrap()
        .with_base_url(server.uri());

    let ctx = Ctx::new(Arc::clone(&db));
    assert_eq!(
        provider.refresh_balances(&ctx).await.unwrap().rows_written,
        1
    );

    // 3.5 XMR × $200.00 = $700.00 = 70_000 cents, snapshotted on `xmr_wallet`.
    let accts = db.list_active_accounts().unwrap();
    let xmr = accts
        .iter()
        .find(|a| a.name == "xmr_wallet")
        .expect("xmr_wallet account created");
    assert_eq!(xmr.asset_class.as_deref(), Some("xmr"));
    assert_eq!(xmr.tier.as_deref(), Some("t1"));
    assert_eq!(xmr.currency, "USD");

    let balances = db.latest_balances().unwrap();
    let (_, bal) = balances
        .iter()
        .find(|(aid, _)| Some(*aid) == xmr.id)
        .expect("xmr_wallet snapshot");
    assert_eq!(bal.as_i64(), 70_000);

    // Reference price series also recorded (USD/coin).
    assert_eq!(
        db.latest_price("XMR").unwrap(),
        Some(arca_core::money::Cents(20_000))
    );
}
