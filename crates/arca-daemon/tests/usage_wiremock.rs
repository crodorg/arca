//! Wiremock fixtures for Phase 3 usage providers.

use std::sync::Arc;

use arca_core::db::{Db, ProviderRow};
use arca_core::ids::ProviderId;
use arca_core::provider::{Ctx, Provider};
use serde_json::json;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use arca_daemon::providers::gold_spot::GoldSpotProvider;
use arca_daemon::providers::openai_usage::OpenAiUsageProvider;
use arca_daemon::providers::openrouter::OpenRouterProvider;
use arca_daemon::providers::postmark::PostmarkProvider;
use arca_daemon::providers::scrapecreators::ScrapeCreatorsProvider;
use arca_daemon::secrets::Secrets;

#[tokio::test]
async fn openrouter_credits_snapshot() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/credits"))
        .and(header("authorization", "Bearer sk-or-test"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": { "total_credits": 100.0, "total_usage": 23.45 }
        })))
        .mount(&server)
        .await;

    let db = Arc::new(Db::open_memory().unwrap());
    let secrets = Secrets::for_test(&[("openrouter_main_key", "sk-or-test")]);
    let pid = db
        .upsert_provider(&ProviderRow {
            id: ProviderId(0),
            kind: "openrouter".into(),
            label: "main".into(),
            config_json: "{}".into(),
            secret_ref: Some("openrouter_main_key".into()),
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
    let provider = OpenRouterProvider::build(&row, &secrets, Arc::clone(&db))
        .unwrap()
        .with_base_url(server.uri());

    let ctx = Ctx::new(Arc::clone(&db));
    let r = provider.refresh_balances(&ctx).await.unwrap();
    assert_eq!(r.rows_written, 1);

    let bal = db.latest_balances().unwrap();
    assert_eq!(bal[0].1.as_i64(), 2345); // 23.45 USD → 2345 cents
}

#[tokio::test]
async fn scrapecreators_credits_snapshot() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/v1/account/credit-balance"))
        .and(header("x-api-key", "sc-test"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "creditCount": 12345
        })))
        .mount(&server)
        .await;

    let db = Arc::new(Db::open_memory().unwrap());
    let secrets = Secrets::for_test(&[("scrapecreators_main_key", "sc-test")]);
    let pid = db
        .upsert_provider(&ProviderRow {
            id: ProviderId(0),
            kind: "scrapecreators".into(),
            label: "main".into(),
            config_json: "{}".into(),
            secret_ref: Some("scrapecreators_main_key".into()),
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
    let provider = ScrapeCreatorsProvider::build(&row, &secrets, Arc::clone(&db))
        .unwrap()
        .with_base_url(server.uri());

    let ctx = Ctx::new(Arc::clone(&db));
    let r = provider.refresh_balances(&ctx).await.unwrap();
    assert_eq!(r.rows_written, 1);

    let accounts = db.list_active_accounts().unwrap();
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].currency, "CREDITS");
    let bal = db.latest_balances().unwrap();
    assert_eq!(bal[0].1.as_i64(), 12_345);
}

#[tokio::test]
async fn postmark_mtd_messages() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/servers"))
        .and(header("X-Postmark-Account-Token", "acct-tok"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "Servers": [
                { "Name": "primary", "ApiTokens": ["srv-tok-1"] },
                { "Name": "secondary", "ApiTokens": ["srv-tok-2"] }
            ]
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/stats/outbound"))
        .and(header("X-Postmark-Server-Token", "srv-tok-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "Sent": 120 })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/stats/outbound"))
        .and(header("X-Postmark-Server-Token", "srv-tok-2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "Sent": 80 })))
        .mount(&server)
        .await;

    let db = Arc::new(Db::open_memory().unwrap());
    let secrets = Secrets::for_test(&[("postmark_main_account_token", "acct-tok")]);
    let pid = db
        .upsert_provider(&ProviderRow {
            id: ProviderId(0),
            kind: "postmark".into(),
            label: "main".into(),
            config_json: "{}".into(),
            secret_ref: Some("postmark_main_account_token".into()),
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
    let provider = PostmarkProvider::build(&row, &secrets, Arc::clone(&db))
        .unwrap()
        .with_base_url(server.uri());

    let ctx = Ctx::new(Arc::clone(&db));
    let r = provider.refresh_balances(&ctx).await.unwrap();
    assert_eq!(r.rows_written, 1);

    let bal = db.latest_balances().unwrap();
    assert_eq!(bal[0].1.as_i64(), 200); // 120 + 80
    let accounts = db.list_active_accounts().unwrap();
    assert_eq!(accounts[0].currency, "MESSAGES");
}

#[tokio::test]
async fn gold_spot_snapshot() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/price/XAU"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "name": "Gold",
            "price": 2348.71,
            "symbol": "XAU"
        })))
        .mount(&server)
        .await;

    let db = Arc::new(Db::open_memory().unwrap());
    let secrets = Secrets::for_test(&[]);
    let pid = db
        .upsert_provider(&ProviderRow {
            id: ProviderId(0),
            kind: "gold_spot".into(),
            label: "main".into(),
            config_json: "{}".into(),
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
    let provider = GoldSpotProvider::build(&row, &secrets, Arc::clone(&db))
        .unwrap()
        .with_base_url(server.uri());

    let ctx = Ctx::new(Arc::clone(&db));
    let r = provider.refresh_balances(&ctx).await.unwrap();
    assert_eq!(r.rows_written, 1);

    // Should write to price_snapshots, NOT balance_snapshots (gold_spot is a
    // market reference, not an account).
    assert!(db.list_active_accounts().unwrap().is_empty());
    let p = db.latest_price("XAU").unwrap().expect("XAU snapshot");
    assert_eq!(p.as_i64(), 234_871); // $2348.71 → 234871 cents
}

#[tokio::test]
async fn openai_usage_mtd_cost_snapshot() {
    let server = MockServer::start().await;

    // `amount.value` is dollars: 0.06 + 1.94 = $2.00 → 200¢.
    Mock::given(method("GET"))
        .and(path("/v1/organization/costs"))
        .and(header("authorization", "Bearer sk-admin-test"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{
                "object": "bucket",
                "start_time": 1_777_000_000,
                "end_time": 1_777_086_400,
                "results": [
                    { "object": "organization.costs.result",
                      "amount": { "value": 0.06, "currency": "usd" } },
                    { "object": "organization.costs.result",
                      "amount": { "value": 1.94, "currency": "usd" } }
                ]
            }],
            "has_more": false,
            "next_page": null
        })))
        .mount(&server)
        .await;

    let db = Arc::new(Db::open_memory().unwrap());
    let secrets = Secrets::for_test(&[("openai_main_admin_key", "sk-admin-test")]);
    let pid = db
        .upsert_provider(&ProviderRow {
            id: ProviderId(0),
            kind: "openai_usage".into(),
            label: "main".into(),
            config_json: "{}".into(),
            secret_ref: Some("openai_main_admin_key".into()),
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
    let provider = OpenAiUsageProvider::build(&row, &secrets, Arc::clone(&db))
        .unwrap()
        .with_base_url(server.uri());

    let ctx = Ctx::new(Arc::clone(&db));
    let r = provider.refresh_balances(&ctx).await.unwrap();
    assert_eq!(r.rows_written, 1);

    let bal = db.latest_balances().unwrap();
    assert_eq!(bal[0].1.as_i64(), 200); // $2.00 → 200¢
}
