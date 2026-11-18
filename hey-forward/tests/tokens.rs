//! The stored credentials against a stand-in for HEY's token endpoint: when they are
//! renewed, and what ends up on disk.

use std::path::{Path, PathBuf};

use chrono::{TimeDelta, Utc};
use hey_forward::auth::{Credentials, StoredTokens};
use hey_sdk::TokenProvider;
use serde_json::json;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn credentials(access: &str, refresh: &str, expires_in: TimeDelta) -> Credentials {
    serde_json::from_value(json!({
        "install_id": "install-1",
        "token": {
            "access_token": access,
            "refresh_token": refresh,
            "token_type": "bearer",
            "expires_at": Utc::now() + expires_in,
        },
    }))
    .unwrap()
}

fn saved(directory: &tempfile::TempDir, credentials: &Credentials) -> PathBuf {
    let path = directory.path().join("credentials.json");
    credentials.save(&path).unwrap();
    path
}

fn tokens(path: &Path, server: &MockServer) -> StoredTokens {
    let hey = hey_sdk::Config::default().with_base_url(server.uri());
    StoredTokens::open(path.to_path_buf(), &hey).unwrap()
}

async fn requests(server: &MockServer) -> usize {
    server.received_requests().await.unwrap().len()
}

#[tokio::test]
async fn a_token_with_time_left_is_handed_out_as_it_is() {
    let server = MockServer::start().await;
    let directory = tempfile::tempdir().unwrap();
    let path = saved(
        &directory,
        &credentials("access-1", "refresh-1", TimeDelta::hours(1)),
    );

    assert_eq!(
        tokens(&path, &server).access_token().await.unwrap(),
        "access-1"
    );
    assert_eq!(requests(&server).await, 0);
}

#[tokio::test]
async fn a_token_about_to_expire_is_renewed_and_the_rotation_saved() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/tokens"))
        .and(body_string_contains("grant_type=refresh_token"))
        .and(body_string_contains("refresh_token=refresh-1"))
        .and(body_string_contains("install_id=install-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "access-2",
            "refresh_token": "refresh-2",
            "token_type": "bearer",
            "expires_in": 3600,
        })))
        .mount(&server)
        .await;
    let directory = tempfile::tempdir().unwrap();
    let path = saved(
        &directory,
        &credentials("access-1", "refresh-1", TimeDelta::seconds(30)),
    );
    let tokens = tokens(&path, &server);

    assert_eq!(tokens.access_token().await.unwrap(), "access-2");
    // And again, without asking HEY a second time.
    assert_eq!(tokens.access_token().await.unwrap(), "access-2");
    assert_eq!(requests(&server).await, 1);

    let on_disk = Credentials::load(&path).unwrap();
    assert_eq!(on_disk.install_id, "install-1");
    assert_eq!(on_disk.token.access_token.expose(), "access-2");
    assert_eq!(on_disk.token.refresh_token.unwrap().expose(), "refresh-2");
    assert!(on_disk.token.expires_at.unwrap() > Utc::now() + TimeDelta::minutes(50));
}

#[tokio::test]
async fn a_renewal_that_names_no_refresh_token_keeps_the_one_held() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/tokens"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "access-2",
            "token_type": "bearer",
            "expires_in": 3600,
        })))
        .mount(&server)
        .await;
    let directory = tempfile::tempdir().unwrap();
    let path = saved(
        &directory,
        &credentials("access-1", "refresh-1", TimeDelta::hours(1)),
    );

    // HEY refused the token outright, time left or not.
    assert!(tokens(&path, &server).refresh().await);

    let on_disk = Credentials::load(&path).unwrap();
    assert_eq!(on_disk.token.refresh_token.unwrap().expose(), "refresh-1");
}

#[tokio::test]
async fn a_refused_renewal_is_not_hammered() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/tokens"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({ "error": "invalid_grant" })))
        .mount(&server)
        .await;
    let directory = tempfile::tempdir().unwrap();
    let path = saved(
        &directory,
        &credentials("access-1", "refresh-1", TimeDelta::seconds(30)),
    );
    let tokens = tokens(&path, &server);

    // The token held still goes out: HEY has the last word on it.
    assert_eq!(tokens.access_token().await.unwrap(), "access-1");
    assert!(!tokens.refresh().await);
    assert_eq!(tokens.access_token().await.unwrap(), "access-1");
    assert_eq!(requests(&server).await, 1);
    assert_eq!(
        Credentials::load(&path)
            .unwrap()
            .token
            .access_token
            .expose(),
        "access-1"
    );
}

#[tokio::test]
async fn credentials_replaced_on_disk_are_picked_up_instead_of_renewing() {
    let server = MockServer::start().await;
    let directory = tempfile::tempdir().unwrap();
    let path = saved(
        &directory,
        &credentials("access-1", "refresh-1", TimeDelta::hours(1)),
    );
    let tokens = tokens(&path, &server);

    // A second `hey-forward login` while the daemon runs.
    credentials("access-9", "refresh-9", TimeDelta::hours(1))
        .save(&path)
        .unwrap();

    assert!(tokens.refresh().await);
    assert_eq!(tokens.access_token().await.unwrap(), "access-9");
    assert_eq!(requests(&server).await, 0);
}

#[tokio::test]
async fn without_credentials_the_way_to_get_them_is_named() {
    let server = MockServer::start().await;
    let hey = hey_sdk::Config::default().with_base_url(server.uri());
    let missing = PathBuf::from("/nonexistent/credentials.json");

    let error = StoredTokens::open(missing, &hey).err().unwrap();

    assert!(error.to_string().contains("hey-forward login"));
}
