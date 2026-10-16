use chrono::DateTime;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;

fn note(severity: Severity) -> Notification {
    Notification {
        ts: DateTime::from_timestamp(0, 0).expect("ts"),
        severity,
        title: "watchdog · paper session not responding".into(),
        body: "last heartbeat 25 min ago → check the process".into(),
    }
}

fn settings(server: &str, min: Severity) -> PhoneSettings {
    PhoneSettings {
        enabled: true,
        server: server.into(),
        min_severity: min,
    }
}

fn keys(topic: &str, token: Option<&str>) -> KeySource {
    let keys = PhoneKeys {
        topic: topic.into(),
        token: token.map(str::to_owned),
    };
    Box::new(move || Some(keys.clone()))
}

#[tokio::test]
async fn an_alert_is_one_json_post_with_title_priority_and_token() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .and(header("Authorization", "Bearer tk_secret"))
        .and(body_partial_json(serde_json::json!({
            "topic": "quorum-x7f3k9q2",
            "title": "watchdog · paper session not responding",
            "message": "last heartbeat 25 min ago → check the process",
            "priority": 5,
            "tags": ["rotating_light"],
        })))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    let phone = PhoneNotifier::new(
        &settings(&format!("{}/", server.uri()), Severity::Warning),
        keys("quorum-x7f3k9q2", Some("tk_secret")),
    )
    .expect("notifier");
    phone.notify(&note(Severity::Critical)).await.expect("sent");
}

#[tokio::test]
async fn below_the_minimum_nothing_is_sent() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let phone = PhoneNotifier::new(&settings(&server.uri(), Severity::Warning), keys("t", None))
        .expect("notifier");
    phone
        .notify(&note(Severity::Info))
        .await
        .expect("skipped quietly");
}

#[tokio::test]
async fn failures_are_errors_that_never_show_the_topic() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&server)
        .await;
    let phone = PhoneNotifier::new(
        &settings(&server.uri(), Severity::Info),
        keys("secret-topic-123", None),
    )
    .expect("notifier");
    let err = phone
        .notify(&note(Severity::Warning))
        .await
        .expect_err("403");
    let text = format!("{err:#} {err:?} {phone:?}");
    assert!(!text.contains("secret-topic-123"), "{text}");

    let missing = PhoneNotifier::new(&settings(&server.uri(), Severity::Info), Box::new(|| None))
        .expect("notifier");
    let err = missing
        .notify(&note(Severity::Warning))
        .await
        .expect_err("no topic");
    assert!(
        err.to_string().contains("quorum secrets set ntfy-topic"),
        "{err}"
    );

    let bad = PhoneNotifier::new(&settings(&server.uri(), Severity::Info), keys("a/b", None))
        .expect("notifier");
    assert!(
        bad.notify(&note(Severity::Warning)).await.is_err(),
        "path characters refused"
    );
}

#[test]
fn topics_and_priorities() {
    assert!(valid_topic("quorum-alerts_2026"));
    assert!(!valid_topic("") && !valid_topic("has space") && !valid_topic(&"x".repeat(65)));
    assert_eq!(message("t", &note(Severity::Info))["priority"], 3);
    assert_eq!(message("t", &note(Severity::Warning))["priority"], 4);
    assert_eq!(
        format!(
            "{:?}",
            PhoneKeys {
                topic: "s".into(),
                token: None
            }
        ),
        "PhoneKeys(<secret>)"
    );
}
