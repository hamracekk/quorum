//! The `quorum` binary end to end, without network: config errors, status, stop.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn workspace(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("quorum-cli-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir); // a leftover from an earlier run, if any
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn quorum(dir: &Path, args: &[&str]) -> Output {
    let config = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config");
    Command::new(env!("CARGO_BIN_EXE_quorum"))
        .arg("--config")
        .arg(config)
        .arg("--data")
        .arg(dir.join("data"))
        .args(args)
        .output()
        .expect("runs")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn status_without_a_session_says_so() {
    let dir = workspace("status");
    let out = quorum(&dir, &["paper", "status", "--slot", "crypto-spot"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(text(&out.stdout).contains("no paper session yet"));
}

#[test]
fn stop_leaves_a_marker_for_the_running_session() {
    let dir = workspace("stop");
    let out = quorum(&dir, &["paper", "stop", "--slot", "crypto-spot"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(dir.join("data/slots/crypto-spot/STOP").exists());
}

#[test]
fn an_unknown_slot_is_a_readable_error() {
    let dir = workspace("unknown");
    let out = quorum(&dir, &["paper", "run", "--slot", "equities"]);
    assert!(!out.status.success());
    let err = text(&out.stderr);
    assert!(
        err.contains("slot `equities`") && err.contains("cannot read slot config"),
        "{err}"
    );
}

#[test]
fn an_invalid_slot_id_is_refused() {
    let dir = workspace("invalid");
    let out = quorum(&dir, &["paper", "status", "--slot", "../etc"]);
    assert!(!out.status.success());
    assert!(text(&out.stderr).contains("slot id"));
}

#[test]
fn the_recorder_refuses_a_config_naming_an_unknown_slot() {
    let dir = workspace("recorder");
    let config = dir.join("config");
    std::fs::create_dir_all(config.join("slots")).expect("config dir");
    std::fs::write(
        config.join("recorder.toml"),
        "slots = [\"equities\"]\nbook_every_secs = 300\nderivs_every_secs = 300\nliquidations = true\n",
    )
    .expect("recorder.toml");
    let out = Command::new(env!("CARGO_BIN_EXE_quorum"))
        .arg("--config")
        .arg(&config)
        .arg("--data")
        .arg(dir.join("data"))
        .arg("record")
        .output()
        .expect("runs");
    assert!(!out.status.success());
    let err = text(&out.stderr);
    assert!(err.contains("slot `equities`"), "{err}");
}
