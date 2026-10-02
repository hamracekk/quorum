use trader_core::InstrumentId;
use trader_core::agents::{
    AgentControl, Evidence, FindingKind, FindingQuery, FindingStatus, Hypothesis, MessageQuery,
    NewFinding, NewMessage, OPERATOR, Verdict,
};
use trader_core::signals::Signal;

use super::*;

fn at(hours: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(hours * 3_600, 0).expect("valid timestamp")
}

fn btc() -> InstrumentId {
    InstrumentId::spot("binance", "BTCUSDT").expect("valid instrument")
}

fn edge(claim: &str) -> NewFinding {
    NewFinding {
        kind: FindingKind::SignalEdge,
        instruments: vec![btc()],
        claim: claim.into(),
        hypothesis: Some(Hypothesis::SignalEdge {
            signal: Signal::RsiOversold,
            instruments: vec![btc()],
            interval: "1h".into(),
            horizon: 24,
        }),
        evidence: vec![
            Evidence::Run { id: "r1".into() },
            Evidence::Url {
                url: "https://example.com/study".into(),
            },
        ],
        supersedes: None,
        confidence: None,
    }
}

fn note(claim: &str) -> NewFinding {
    NewFinding {
        kind: FindingKind::Observation,
        instruments: vec![],
        claim: claim.into(),
        hypothesis: None,
        evidence: vec![],
        supersedes: None,
        confidence: None,
    }
}

fn message(recipient: &str, body: &str, thread: Option<i64>) -> NewMessage {
    NewMessage {
        recipient: recipient.into(),
        body: body.into(),
        thread,
        refs: vec!["finding:1".into()],
    }
}

fn query(limit: usize) -> FindingQuery {
    FindingQuery {
        limit,
        ..FindingQuery::default()
    }
}

#[tokio::test]
async fn findings_round_trip_with_typed_hypotheses_and_evidence() {
    let journal = SqliteJournal::in_memory().await.expect("journal");
    let stored = journal
        .post_finding(
            "researcher",
            "signal-researcher",
            &edge("RSI oversold bounces on BTC within a day"),
            at(1),
        )
        .await
        .expect("post");
    assert_eq!(stored.id, 1);
    assert_eq!(stored.status, FindingStatus::Hypothesis);
    assert_eq!((stored.created_at, stored.updated_at), (at(1), at(1)));

    let listed = journal.findings(&query(10)).await.expect("list");
    assert_eq!(listed, vec![stored.clone()]);
    assert_eq!(listed[0].hypothesis, edge("x").hypothesis);
    assert_eq!(listed[0].evidence.len(), 2);
}

#[tokio::test]
async fn superseding_keeps_both_and_links_them() {
    let journal = SqliteJournal::in_memory().await.expect("journal");
    let first = journal
        .post_finding("researcher", "r", &note("first version of a note"), at(1))
        .await
        .expect("post");
    let mut second = note("second version, with more data");
    second.supersedes = Some(first.id);
    let second = journal
        .post_finding("researcher", "r", &second, at(2))
        .await
        .expect("supersede");

    let listed = journal.findings(&query(10)).await.expect("list");
    assert_eq!(
        listed.iter().map(|f| f.id).collect::<Vec<_>>(),
        vec![second.id, first.id],
        "newest first"
    );
    assert_eq!(listed[1].superseded_by, Some(second.id));
    assert_eq!(listed[0].supersedes, Some(first.id));

    let mut dangling = note("supersedes something missing");
    dangling.supersedes = Some(99);
    let err = journal
        .post_finding("researcher", "r", &dangling, at(3))
        .await
        .expect_err("unknown finding");
    assert!(matches!(err, PortError::Invalid(_)), "{err}");
}

#[tokio::test]
async fn findings_filter_by_agent_status_and_full_text() {
    let journal = SqliteJournal::in_memory().await.expect("journal");
    journal
        .post_finding(
            "researcher",
            "r",
            &edge("MACD crosses lead SOL rallies"),
            at(1),
        )
        .await
        .expect("post");
    journal
        .post_finding(
            "risk-reviewer",
            "c",
            &note("the MACD sample is too small"),
            at(2),
        )
        .await
        .expect("post");
    journal
        .post_finding(
            "researcher",
            "r",
            &note("volume dries up on weekends"),
            at(3),
        )
        .await
        .expect("post");

    let by_agent = FindingQuery {
        agent: Some("researcher".into()),
        ..query(10)
    };
    assert_eq!(journal.findings(&by_agent).await.expect("list").len(), 2);

    let text = FindingQuery {
        text: Some("macd".into()),
        ..query(10)
    };
    let hits = journal.findings(&text).await.expect("search");
    assert_eq!(hits.len(), 2, "FTS is case-insensitive");

    let both = FindingQuery {
        agent: Some("risk-reviewer".into()),
        text: Some("MACD sample".into()),
        ..query(10)
    };
    assert_eq!(journal.findings(&both).await.expect("search").len(), 1);

    // FTS operators are treated as plain words, never as syntax.
    let hostile = FindingQuery {
        text: Some("macd\" OR NEAR( *".into()),
        ..query(10)
    };
    assert!(journal.findings(&hostile).await.is_ok());
    let nothing = FindingQuery {
        text: Some("  ** ".into()),
        ..query(10)
    };
    assert!(journal.findings(&nothing).await.expect("search").is_empty());

    let confirmed = FindingQuery {
        status: Some(FindingStatus::Confirmed),
        ..query(10)
    };
    assert!(journal.findings(&confirmed).await.expect("list").is_empty());
    assert_eq!(journal.findings(&query(1)).await.expect("list").len(), 1);
}

#[tokio::test]
async fn messages_thread_and_unread_tracking_per_reader() {
    let journal = SqliteJournal::in_memory().await.expect("journal");
    let root = journal
        .send_message(
            "researcher",
            &message("risk-reviewer", "please review finding 1", None),
            at(1),
        )
        .await
        .expect("send");
    assert_eq!(root.thread, root.id, "a new message starts its own thread");

    let reply = journal
        .send_message(
            "risk-reviewer",
            &message("researcher", "sample too small", Some(root.id)),
            at(2),
        )
        .await
        .expect("reply");
    assert_eq!(reply.thread, root.id);
    // Replying to a reply still lands in the original thread.
    let deeper = journal
        .send_message(
            "researcher",
            &message(OPERATOR, "operator, we disagree", Some(reply.id)),
            at(3),
        )
        .await
        .expect("reply");
    assert_eq!(deeper.thread, root.id);
    let broadcast = journal
        .send_message(OPERATOR, &message("all", "pause new findings", None), at(4))
        .await
        .expect("broadcast");

    assert_eq!(journal.unread_count("researcher").await.expect("count"), 2);
    let unread = journal.take_unread("researcher", 10).await.expect("unread");
    assert_eq!(
        unread.iter().map(|m| m.id).collect::<Vec<_>>(),
        vec![reply.id, broadcast.id]
    );
    assert_eq!(journal.unread_count("researcher").await.expect("count"), 0);
    assert!(
        journal
            .take_unread("researcher", 10)
            .await
            .expect("unread")
            .is_empty()
    );
    // Reading is per reader: the broadcast is still unread for the reviewer.
    assert_eq!(
        journal.unread_count("risk-reviewer").await.expect("count"),
        2
    );

    let thread = MessageQuery {
        thread: Some(root.id),
        limit: 10,
        ..MessageQuery::default()
    };
    let in_thread = journal.messages(&thread).await.expect("thread");
    assert_eq!(
        in_thread.iter().map(|m| m.id).collect::<Vec<_>>(),
        vec![root.id, reply.id, deeper.id],
        "oldest first"
    );
    let by_reply = MessageQuery {
        thread: Some(deeper.id),
        ..thread.clone()
    };
    assert_eq!(
        journal.messages(&by_reply).await.expect("thread").len(),
        3,
        "any message id selects its whole thread"
    );
    let operator = MessageQuery {
        participant: Some(OPERATOR.into()),
        limit: 10,
        ..MessageQuery::default()
    };
    assert_eq!(journal.messages(&operator).await.expect("inbox").len(), 2);

    let err = journal
        .send_message("researcher", &message(OPERATOR, "x", Some(99)), at(5))
        .await
        .expect_err("unknown thread");
    assert!(matches!(err, PortError::Invalid(_)));
}

fn verdict(by: &str, outcome: FindingStatus, hour: i64) -> Verdict {
    Verdict {
        evaluated_at: at(hour),
        by: by.into(),
        method: "signal_study_holdout".into(),
        outcome,
        summary: "oos net +0.4%, t 3.4 over 41 events".into(),
        metrics: std::iter::once(("t_stat".to_owned(), 3.4)).collect(),
        window: Some((at(0), at(hour))),
    }
}

#[tokio::test]
async fn verdicts_move_findings_and_are_kept_in_history() {
    let journal = SqliteJournal::in_memory().await.expect("journal");
    let mut claim = edge("RSI oversold bounces on BTC within a day");
    claim.confidence = Some(0.7);
    let posted = journal
        .post_finding("researcher", "r", &claim, at(1))
        .await
        .expect("post");
    assert_eq!(posted.confidence, Some(0.7));
    assert!(posted.verdict.is_none());

    let confirmed = journal
        .record_verdict(
            posted.id,
            &verdict("risk-reviewer", FindingStatus::Confirmed, 5),
        )
        .await
        .expect("verdict");
    assert_eq!(confirmed.status, FindingStatus::Confirmed);
    assert_eq!(confirmed.updated_at, at(5));
    assert_eq!(
        confirmed.verdict,
        Some(verdict("risk-reviewer", FindingStatus::Confirmed, 5))
    );
    assert_eq!(
        journal.finding(posted.id).await.expect("get"),
        Some(confirmed)
    );

    journal
        .record_verdict(posted.id, &verdict(OPERATOR, FindingStatus::Retired, 6))
        .await
        .expect("operator retires");
    let err = journal
        .record_verdict(
            posted.id,
            &verdict("researcher", FindingStatus::Confirmed, 7),
        )
        .await
        .expect_err("retired findings change only by the operator");
    assert!(matches!(err, PortError::Invalid(_)), "{err}");
    journal
        .record_verdict(posted.id, &verdict(OPERATOR, FindingStatus::Hypothesis, 8))
        .await
        .expect("operator reopens");

    let history: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM finding_evaluations")
        .fetch_one(&journal.pool)
        .await
        .expect("count");
    assert_eq!(history, 3, "every decision is kept");
    assert!(
        journal
            .record_verdict(99, &verdict(OPERATOR, FindingStatus::Rejected, 9))
            .await
            .is_err()
    );
    assert_eq!(journal.finding(99).await.expect("get"), None);
}

#[tokio::test]
async fn controls_upsert_per_agent() {
    let journal = SqliteJournal::in_memory().await.expect("journal");
    let control = |agent: &str, enabled: bool, hour: i64| AgentControl {
        agent: agent.into(),
        enabled,
        reason: (!enabled).then(|| "misbehaving".into()),
        updated_at: at(hour),
    };
    journal
        .set_control(&control("researcher", false, 1))
        .await
        .expect("set");
    journal
        .set_control(&control("*", true, 2))
        .await
        .expect("set");
    journal
        .set_control(&control("researcher", true, 3))
        .await
        .expect("update");
    assert_eq!(
        journal.controls().await.expect("controls"),
        vec![control("*", true, 2), control("researcher", true, 3)]
    );
}

#[test]
fn fts_queries_quote_every_word() {
    assert_eq!(
        fts_query("MACD near-term").as_deref(),
        Some("\"MACD\" \"near\" \"term\"")
    );
    assert_eq!(fts_query("\"* OR"), Some("\"OR\"".into()));
    assert_eq!(fts_query(" ** "), None);
}

#[tokio::test]
async fn agent_runs_are_recorded_finished_and_counted_for_budgets() {
    let journal = SqliteJournal::in_memory().await.expect("journal");
    let mut run = journal
        .start_agent_run("scout", "watch whales", "manual", at(0))
        .await
        .expect("start");
    assert_eq!(run.status, "running");
    run.status = "done".into();
    run.finished_at = Some(at(1));
    run.steps = 4;
    run.input_tokens = 3_000;
    run.output_tokens = 500;
    run.summary = Some("two traders opened BTC longs".into());
    journal.finish_agent_run(&run).await.expect("finish");
    journal
        .start_agent_run("news", "read news", "schedule", at(2))
        .await
        .expect("start");

    let runs = journal.agent_runs(None, 10).await.expect("runs");
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0].agent, "news", "newest first");
    assert_eq!(runs[1], run);
    assert_eq!(
        journal
            .agent_runs(Some("scout"), 10)
            .await
            .expect("runs")
            .len(),
        1
    );
    assert_eq!(
        journal.agent_tokens("scout", at(0)).await.expect("tokens"),
        3_500
    );
    assert_eq!(
        journal.agent_tokens("scout", at(1)).await.expect("tokens"),
        0
    );
    let missing = AgentRun { id: 99, ..run };
    assert!(journal.finish_agent_run(&missing).await.is_err());
}
