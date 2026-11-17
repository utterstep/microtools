//! The forwarder against a stand-in HEY: what a poll reads, what it sends, and what it
//! remembers. The stand-in answers with the wire shapes the HEY SDK is generated from.

use chrono::{DateTime, TimeDelta, Utc};
use hey_forward::config::Config;
use hey_forward::forwarder::Forwarder;
use hey_forward::state::{State, StateFile};
use hey_sdk::{Client, StaticTokenProvider};
use serde_json::{Value, json};
use wiremock::matchers::{method, path, path_regex, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, ResponseTemplate};

const RULES: &str = r#"
    [[forward]]
    alias = "me+family@hey.com"
    to = ["partner@example.com"]

    [[forward]]
    alias = "me+bills@hey.com"
    to = ["partner@example.com", "books@example.com"]
"#;

/// The moment the tests take for "now".
fn now() -> DateTime<Utc> {
    "2026-11-17T12:00:00Z".parse().unwrap()
}

fn minutes_ago(minutes: i64) -> DateTime<Utc> {
    now() - TimeDelta::minutes(minutes)
}

/// A state whose first run was a day ago and whose last poll was a minute ago.
fn running_state() -> StateFile {
    let mut state = State::starting_at(now() - TimeDelta::days(1));
    state.checkpoint = minutes_ago(1);
    StateFile::in_memory(state)
}

fn client(server: &MockServer) -> Client {
    Client::builder(hey_sdk::Config::default().with_base_url(server.uri()))
        .token_provider(StaticTokenProvider::new("token"))
        .max_retries(0)
        .build()
        .unwrap()
}

fn forwarder(server: &MockServer) -> Forwarder {
    Forwarder::new(client(server), Config::parse(RULES).unwrap(), false)
}

fn topic(id: i64, active_at: DateTime<Utc>) -> Value {
    json!({ "id": id, "name": "A thread", "active_at": active_at })
}

fn entry(id: i64, created_at: DateTime<Utc>) -> Value {
    json!({ "id": id, "kind": "message", "created_at": created_at })
}

async fn everything(server: &MockServer, topics: Vec<Value>) {
    Mock::given(method("GET"))
        .and(path("/topics/everything.json"))
        .and(query_param_is_missing("page"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "topics": topics })))
        .mount(server)
        .await;
}

async fn entries(server: &MockServer, topic_id: i64, entries: Vec<Value>) {
    Mock::given(method("GET"))
        .and(path_regex(format!(
            r"^/topics/{topic_id}/entries(\.json)?$"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(entries))
        .mount(server)
        .await;
}

/// A message as it arrives through `via`, with `to` on its To line.
async fn message(server: &MockServer, id: i64, to: &str, via: Option<&str>) {
    let mut body = json!({
        "id": id,
        "subject": "Parents' evening",
        "content": "<div>See you there</div>",
        "addressed": { "directly": [{ "id": 7, "email_address": to }] },
    });
    if let Some(via) = via {
        body["received_via"] = json!([{ "email_address": via }]);
    }
    Mock::given(method("GET"))
        .and(path_regex(format!(r"^/messages/{id}(\.json)?$")))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

async fn forward_prefill(server: &MockServer, entry_id: i64) {
    Mock::given(method("GET"))
        .and(path(format!("/entries/{entry_id}/forwards/new.json")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "subject": "Fwd: Parents' evening",
            "content": "<div>---------- Forwarded message ----------</div><div>See you there</div>",
        })))
        .mount(server)
        .await;
}

async fn identity(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/identity.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 1,
            "senders": [{ "id": 41 }, { "id": 42, "default": true }],
        })))
        .mount(server)
        .await;
}

async fn accept_sends(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/messages.json"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "id": 900, "topic_id": 90 })),
        )
        .mount(server)
        .await;
}

/// One thread with one fresh message, sent to `to` and arriving through `via`.
async fn one_fresh_message(server: &MockServer, to: &str, via: Option<&str>) {
    everything(server, vec![topic(10, minutes_ago(1))]).await;
    entries(server, 10, vec![entry(100, minutes_ago(1))]).await;
    message(server, 100, to, via).await;
    forward_prefill(server, 100).await;
    identity(server).await;
}

/// The bodies of the messages the stand-in was asked to send.
async fn sent(server: &MockServer) -> Vec<Value> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| request.method.as_str() == "POST")
        .map(|request| serde_json::from_slice(&request.body).unwrap())
        .collect()
}

async fn requests_to(server: &MockServer, path_prefix: &str) -> usize {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| request.url.path().starts_with(path_prefix))
        .count()
}

#[tokio::test]
async fn mail_that_arrived_through_an_alias_is_forwarded_once() {
    let server = MockServer::start().await;
    one_fresh_message(&server, "me@hey.com", Some("me+family@hey.com")).await;
    accept_sends(&server).await;
    let mut forwarder = forwarder(&server);
    let mut state = running_state();

    let report = forwarder.poll(&mut state, now(), false).await.unwrap();

    assert_eq!(report.forwarded.len(), 1);
    assert_eq!(report.forwarded[0].aliases, ["me+family@hey.com"]);
    let sent_bodies = sent(&server).await;
    assert_eq!(sent_bodies.len(), 1);
    assert_eq!(sent_bodies[0]["acting_sender_id"], 42);
    assert_eq!(
        sent_bodies[0]["message"],
        json!({
            "subject": "Fwd: Parents' evening",
            "content": "<div>---------- Forwarded message ----------</div><div>See you there</div>",
        })
    );
    assert_eq!(
        sent_bodies[0]["entry"],
        json!({ "addressed": { "directly": ["partner@example.com"] } })
    );

    // The next polls — quick or deep — find nothing new, and do not read the thread again.
    let later = now() + TimeDelta::minutes(1);
    assert!(
        forwarder
            .poll(&mut state, later, false)
            .await
            .unwrap()
            .forwarded
            .is_empty()
    );
    assert!(
        forwarder
            .poll(&mut state, later, true)
            .await
            .unwrap()
            .forwarded
            .is_empty()
    );
    assert_eq!(sent(&server).await.len(), 1);
    assert_eq!(requests_to(&server, "/topics/10/entries").await, 1);
}

#[tokio::test]
async fn an_alias_on_the_to_line_matches_without_a_delivery_record() {
    let server = MockServer::start().await;
    one_fresh_message(&server, "Me+Bills@hey.com", None).await;
    accept_sends(&server).await;

    let report = forwarder(&server)
        .poll(&mut running_state(), now(), false)
        .await
        .unwrap();

    assert_eq!(
        report.forwarded[0].to,
        ["partner@example.com", "books@example.com"]
    );
    assert_eq!(
        sent(&server).await[0]["entry"]["addressed"]["directly"],
        json!(["partner@example.com", "books@example.com"])
    );
}

#[tokio::test]
async fn mail_to_any_other_address_is_left_alone() {
    let server = MockServer::start().await;
    one_fresh_message(&server, "me@hey.com", Some("me+shopping@hey.com")).await;
    let mut forwarder = forwarder(&server);
    let mut state = running_state();

    let report = forwarder.poll(&mut state, now(), false).await.unwrap();

    assert_eq!(report.messages, 1);
    assert!(report.forwarded.is_empty());
    assert!(sent(&server).await.is_empty());
    // It is remembered all the same, so it is fetched only the once.
    assert!(state.state.handled.contains_key(&100));
}

#[tokio::test]
async fn mail_from_before_the_first_run_never_goes_out() {
    let server = MockServer::start().await;
    // First run a minute ago; the message is older than that, on a thread still listed.
    let mut state = StateFile::in_memory(State::starting_at(minutes_ago(1)));
    everything(&server, vec![topic(11, now())]).await;
    entries(&server, 11, vec![entry(101, minutes_ago(5))]).await;
    message(&server, 101, "me@hey.com", Some("me+family@hey.com")).await;

    let report = forwarder(&server)
        .poll(&mut state, now(), true)
        .await
        .unwrap();

    assert!(report.forwarded.is_empty());
    assert_eq!(requests_to(&server, "/messages").await, 0);
}

#[tokio::test]
async fn a_dry_run_reports_matches_and_sends_nothing() {
    let server = MockServer::start().await;
    one_fresh_message(&server, "me@hey.com", Some("me+family@hey.com")).await;
    let mut forwarder = Forwarder::new(client(&server), Config::parse(RULES).unwrap(), true);

    let report = forwarder
        .poll(&mut running_state(), now(), true)
        .await
        .unwrap();

    assert_eq!(report.forwarded.len(), 1);
    assert_eq!(report.forwarded[0].subject, "Parents' evening");
    assert_eq!(report.forwarded[0].received_at, minutes_ago(1));
    assert!(sent(&server).await.is_empty());
}

#[tokio::test]
async fn a_forward_hey_refuses_is_tried_again_on_the_next_deep_poll() {
    let server = MockServer::start().await;
    one_fresh_message(&server, "me@hey.com", Some("me+family@hey.com")).await;
    Mock::given(method("POST"))
        .and(path("/messages.json"))
        .respond_with(ResponseTemplate::new(422).set_body_json(json!({ "errors": ["Nope"] })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    accept_sends(&server).await;
    let mut forwarder = forwarder(&server);
    let mut state = running_state();

    let refused = forwarder.poll(&mut state, now(), false).await.unwrap();
    assert_eq!((refused.failed, refused.forwarded.len()), (1, 0));
    assert!(!state.state.handled.contains_key(&100));

    // Quick polls leave it be; the deep one sends it.
    let later = now() + TimeDelta::minutes(1);
    let quick = forwarder.poll(&mut state, later, false).await.unwrap();
    assert_eq!((quick.failed, quick.forwarded.len()), (0, 0));
    let deep = forwarder.poll(&mut state, later, true).await.unwrap();
    assert_eq!((deep.failed, deep.forwarded.len()), (0, 1));
    assert_eq!(sent(&server).await.len(), 2);
}

#[tokio::test]
async fn a_poll_cut_short_by_an_outage_loses_nothing() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/topics/10/entries(\.json)?$"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    one_fresh_message(&server, "me@hey.com", Some("me+family@hey.com")).await;
    accept_sends(&server).await;
    let mut forwarder = forwarder(&server);
    let mut state = running_state();

    assert!(forwarder.poll(&mut state, now(), false).await.is_err());
    // The failed poll did not count as a look, so the next one starts from the same place.
    assert_eq!(state.state.checkpoint, minutes_ago(1));

    let later = now() + TimeDelta::minutes(1);
    let report = forwarder.poll(&mut state, later, false).await.unwrap();
    assert_eq!(report.forwarded.len(), 1);
    assert_eq!(state.state.checkpoint, later);
}

#[tokio::test]
async fn a_restart_does_not_send_anything_twice() {
    let server = MockServer::start().await;
    one_fresh_message(&server, "me@hey.com", Some("me+family@hey.com")).await;
    accept_sends(&server).await;
    let directory = tempfile::tempdir().unwrap();
    let state_path = directory.path().join("state.json");
    let first_run = now() - TimeDelta::days(1);

    let mut state = StateFile::load_or_start(state_path.clone(), first_run).unwrap();
    forwarder(&server)
        .poll(&mut state, now(), true)
        .await
        .unwrap();
    assert_eq!(sent(&server).await.len(), 1);

    // A new process: nothing in memory, only the file.
    let mut state = StateFile::load_or_start(state_path, now()).unwrap();
    assert_eq!(state.state.baseline, first_run);
    let report = forwarder(&server)
        .poll(&mut state, now() + TimeDelta::minutes(1), true)
        .await
        .unwrap();
    assert!(report.forwarded.is_empty());
    assert_eq!(sent(&server).await.len(), 1);
}

#[tokio::test]
async fn a_new_message_on_a_thread_already_read_is_picked_up() {
    let server = MockServer::start().await;
    one_fresh_message(&server, "me@hey.com", Some("me+shopping@hey.com")).await;
    accept_sends(&server).await;
    let mut forwarder = forwarder(&server);
    let mut state = running_state();
    forwarder.poll(&mut state, now(), false).await.unwrap();

    // A reply lands on the same thread, this time through a watched alias.
    server.reset().await;
    let later = now() + TimeDelta::minutes(1);
    everything(&server, vec![topic(10, now())]).await;
    entries(
        &server,
        10,
        vec![entry(100, minutes_ago(1)), entry(102, now())],
    )
    .await;
    message(&server, 102, "me@hey.com", Some("me+family@hey.com")).await;
    forward_prefill(&server, 102).await;
    identity(&server).await;
    accept_sends(&server).await;

    let report = forwarder.poll(&mut state, later, false).await.unwrap();

    assert_eq!(report.forwarded.len(), 1);
    assert_eq!(report.forwarded[0].entry_id, 102);
    // The message already looked at was not fetched again.
    assert_eq!(requests_to(&server, "/messages/100").await, 0);
}

#[tokio::test]
async fn a_poll_reads_on_through_the_pages_until_the_horizon() {
    let server = MockServer::start().await;
    // Page one is all recent threads with nothing in them; the match is on page two,
    // which ends past the horizon, so page three is never asked for.
    Mock::given(method("GET"))
        .and(path("/topics/everything.json"))
        .and(query_param_is_missing("page"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Link", r#"</topics/everything.json?page=two>; rel="next""#)
                .set_body_json(json!({ "topics": [topic(20, minutes_ago(1))] })),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/topics/everything.json"))
        .and(query_param("page", "two"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header(
                    "Link",
                    r#"</topics/everything.json?page=three>; rel="next""#,
                )
                .set_body_json(json!({
                    "topics": [topic(10, minutes_ago(2)), topic(30, minutes_ago(600))],
                })),
        )
        .mount(&server)
        .await;
    entries(&server, 20, vec![]).await;
    entries(&server, 10, vec![entry(100, minutes_ago(2))]).await;
    message(&server, 100, "me@hey.com", Some("me+family@hey.com")).await;
    forward_prefill(&server, 100).await;
    identity(&server).await;
    accept_sends(&server).await;

    let report = forwarder(&server)
        .poll(&mut running_state(), now(), false)
        .await
        .unwrap();

    assert_eq!(report.forwarded.len(), 1);
    assert_eq!(requests_to(&server, "/topics/everything").await, 2);
    // The thread past the horizon was not opened on a quick poll.
    assert_eq!(requests_to(&server, "/topics/30/entries").await, 0);
}

#[tokio::test]
async fn a_deep_poll_finds_mail_that_surfaced_late() {
    let server = MockServer::start().await;
    // Ten hours old — held in the Screener, say — and only now on the list.
    everything(&server, vec![topic(10, minutes_ago(600))]).await;
    entries(&server, 10, vec![entry(100, minutes_ago(600))]).await;
    message(&server, 100, "me@hey.com", Some("me+family@hey.com")).await;
    forward_prefill(&server, 100).await;
    identity(&server).await;
    accept_sends(&server).await;
    let mut forwarder = forwarder(&server);
    let mut state = running_state();

    let quick = forwarder.poll(&mut state, now(), false).await.unwrap();
    assert!(quick.forwarded.is_empty());

    let deep = forwarder.poll(&mut state, now(), true).await.unwrap();
    assert_eq!(deep.forwarded.len(), 1);
}
