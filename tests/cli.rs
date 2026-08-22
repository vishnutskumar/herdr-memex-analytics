//! Black-box system tests: run the real `analytics` binary against seeded fake
//! memex roots. Every invocation passes explicit `--root`/`--state-dir` (and
//! `HERDR_BIN_PATH` where notifications could fire), so no test ever touches
//! `~/.memex`, `$HERDR_PLUGIN_STATE_DIR`, or any other host state.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::Connection;

static SEQ: AtomicU64 = AtomicU64::new(0);

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// Unique per-test scratch dir so tests never share state and can run in any order.
fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "analytics-cli-{tag}-{}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed),
        nanos
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct Fixture {
    root: PathBuf,
    state_dir: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.root).ok();
        std::fs::remove_dir_all(&self.state_dir).ok();
    }
}

fn fixture(tag: &str) -> Fixture {
    Fixture {
        root: temp_dir(&format!("{tag}-root")),
        state_dir: temp_dir(&format!("{tag}-state")),
    }
}

const SCHEMA: &str = "
CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE sessions(source TEXT NOT NULL, session_id TEXT NOT NULL, source_path TEXT NOT NULL, project TEXT NOT NULL, cwd TEXT, git_root TEXT, git_common_dir TEXT, repo_project TEXT, started_at INTEGER NOT NULL, last_at INTEGER NOT NULL, message_count INTEGER NOT NULL DEFAULT 0, resolution_status TEXT NOT NULL DEFAULT '', PRIMARY KEY(source, session_id, source_path));
";

struct Row {
    source: &'static str,
    session_id: &'static str,
    project: &'static str,
    repo_project: Option<&'static str>,
    started_at: u64,
    last_at: u64,
    message_count: u64,
}

/// Seed `<root>/state/analytics.sqlite` with the exact memex schema plus rows.
fn seed_db(root: &Path, rows: &[Row]) {
    let state = root.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let conn = Connection::open(state.join("analytics.sqlite")).unwrap();
    conn.execute_batch(SCHEMA).unwrap();
    for r in rows {
        conn.execute(
            "INSERT INTO sessions(source, session_id, source_path, project, cwd, git_root, git_common_dir, repo_project, started_at, last_at, message_count, resolution_status)
             VALUES(?1, ?2, ?3, ?4, NULL, NULL, NULL, ?5, ?6, ?7, ?8, '')",
            rusqlite::params![
                r.source,
                r.session_id,
                format!("/fake/{}", r.session_id),
                r.project,
                r.repo_project,
                r.started_at as i64,
                r.last_at as i64,
                r.message_count as i64,
            ],
        )
        .unwrap();
    }
}

/// Three projects: alpha (repo_project set, two sources, most recent), beta
/// (empty repo_project falls back to the raw project name), gamma (old — only
/// in the full-history window).
fn standard_rows(now: u64) -> Vec<Row> {
    let day: u64 = 24 * 3600 * 1000;
    vec![
        Row {
            source: "claude",
            session_id: "a1",
            project: "/w/alpha",
            repo_project: Some("alpha-repo"),
            started_at: now - day,
            last_at: now - 3_600_000,
            message_count: 10,
        },
        Row {
            source: "claude",
            session_id: "a2",
            project: "/elsewhere/alpha",
            repo_project: Some("alpha-repo"),
            started_at: now - 2 * day,
            last_at: now - 2 * day,
            message_count: 5,
        },
        Row {
            source: "codex",
            session_id: "a3",
            project: "/w/alpha/sub",
            repo_project: Some("alpha-repo"),
            started_at: now - 3 * day,
            last_at: now - 3 * day + 1_800_000,
            message_count: 7,
        },
        Row {
            source: "omp",
            session_id: "b1",
            project: "beta-dir",
            repo_project: Some(""),
            started_at: now - 4 * day,
            last_at: now - 4 * day + 300_000,
            message_count: 3,
        },
        Row {
            source: "claude",
            session_id: "g1",
            project: "gamma-old",
            repo_project: None,
            started_at: now - 60 * day,
            last_at: now - 60 * day + 60_000,
            message_count: 99,
        },
    ]
}

fn run(fx: &Fixture, args: &[&str]) -> Output {
    run_with_env(fx, args, &[])
}

fn run_with_env(fx: &Fixture, args: &[&str], env: &[(&str, &str)]) -> Output {
    let root = fx.root.to_string_lossy().into_owned();
    let state = fx.state_dir.to_string_lossy().into_owned();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_analytics"));
    cmd.args(["--root", &root, "--state-dir", &state]);
    cmd.args(args);
    cmd.env("HERDR_BIN_PATH", "/usr/bin/true"); // never emit real notifications
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().expect("spawn analytics")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[derive(serde::Deserialize)]
struct ReportJson {
    since_ms: Option<u64>,
    projects: Vec<ProjectJson>,
    usage: Option<serde_json::Value>,
    usage_note: Option<String>,
    #[serde(default)]
    project_usage: Vec<ProjectUsageJson>,
}

#[derive(serde::Deserialize)]
struct ProjectJson {
    project: String,
    sessions: u64,
    messages: u64,
    active_ms: u64,
    sources: BTreeMap<String, u64>,
}

fn report_json(fx: &Fixture, extra: &[&str]) -> ReportJson {
    let mut args = vec!["report", "--json"];
    args.extend_from_slice(extra);
    let out = run(fx, &args);
    assert!(
        out.status.success(),
        "report failed: {} {}",
        stderr(&out),
        stdout(&out)
    );
    serde_json::from_str(&stdout(&out)).expect("valid report JSON")
}

// ---------------------------------------------------------------- report

#[test]
fn report_aggregates_projects_and_sorts_by_recency() {
    let fx = fixture("aggregate");
    let now = now_ms();
    seed_db(&fx.root, &standard_rows(now));

    let rep = report_json(&fx, &["--all"]);

    assert_eq!(rep.since_ms, None, "--all means the full-history window");
    let names: Vec<&str> = rep.projects.iter().map(|p| p.project.as_str()).collect();
    assert_eq!(
        names,
        vec!["alpha-repo", "beta-dir", "gamma-old"],
        "sorted by last_at desc"
    );

    let alpha = &rep.projects[0];
    assert_eq!(alpha.sessions, 3);
    // a2 contributes 0; per-session wall times: 82_800_000 + 0 + 1_800_000.
    let expected_active: u64 = 82_800_000 + 1_800_000;
    assert_eq!(
        alpha.active_ms, expected_active,
        "sum of per-session wall time"
    );
    assert_eq!(alpha.sources.get("Claude"), Some(&2));
    assert_eq!(alpha.sources.get("Codex"), Some(&1));
    assert_eq!(rep.projects[1].sources.get("Omp"), Some(&1));
    assert!(
        rep.usage.is_none(),
        "token usage disabled without config.toml opt-in"
    );
    assert!(rep.usage_note.is_some());
}

#[test]
fn empty_repo_project_falls_back_to_raw_project_name() {
    let fx = fixture("fallback-name");
    let now = now_ms();
    seed_db(
        &fx.root,
        &[Row {
            source: "omp",
            session_id: "b1",
            project: "beta-dir",
            repo_project: Some(""),
            started_at: now - 1000,
            last_at: now,
            message_count: 3,
        }],
    );
    let rep = report_json(&fx, &["--all"]);
    assert_eq!(rep.projects.len(), 1);
    assert_eq!(rep.projects[0].project, "beta-dir");
}

#[test]
fn default_window_excludes_sessions_older_than_thirty_days() {
    let fx = fixture("default-window");
    let now = now_ms();
    seed_db(&fx.root, &standard_rows(now));

    let rep = report_json(&fx, &[]);
    let names: Vec<&str> = rep.projects.iter().map(|p| p.project.as_str()).collect();
    assert!(
        !names.contains(&"gamma-old"),
        "60-day-old session outside default window"
    );
    assert!(rep.since_ms.is_some(), "default window is a bounded since");

    let all = report_json(&fx, &["--all"]);
    assert!(all.projects.iter().any(|p| p.project == "gamma-old"));
}

#[test]
fn since_flag_filters_by_activity_age() {
    let fx = fixture("since");
    let now = now_ms();
    seed_db(&fx.root, &standard_rows(now));

    let rep = report_json(&fx, &["--since", "7d"]);
    let names: Vec<&str> = rep.projects.iter().map(|p| p.project.as_str()).collect();
    assert_eq!(
        names,
        vec!["alpha-repo", "beta-dir"],
        "only sessions active within 7 days"
    );

    let hours = report_json(&fx, &["--since", "2h"]);
    assert_eq!(
        hours
            .projects
            .iter()
            .map(|p| p.project.as_str())
            .collect::<Vec<_>>(),
        vec!["alpha-repo"],
        "2h window keeps only the session active an hour ago"
    );
}

#[test]
fn project_filter_matches_display_name_including_repo_project() {
    let fx = fixture("project-filter");
    let now = now_ms();
    seed_db(&fx.root, &standard_rows(now));

    let rep = report_json(&fx, &["--all", "--project", "alpha-repo"]);
    assert_eq!(rep.projects.len(), 1);
    assert_eq!(rep.projects[0].sessions, 3);

    let beta = report_json(&fx, &["--all", "--project", "beta-dir"]);
    assert_eq!(beta.projects.len(), 1);
    assert_eq!(beta.projects[0].project, "beta-dir");

    let none = report_json(&fx, &["--all", "--project", "no-such-project"]);
    assert!(none.projects.is_empty());
}

#[test]
fn text_report_shows_window_label_for_since_flag() {
    let fx = fixture("text-window");
    let now = now_ms();
    seed_db(&fx.root, &standard_rows(now));

    let out = run(&fx, &["report", "--since", "7d"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("herdr analytics — since 7d"), "{text}");
    assert!(text.contains("alpha-repo"));

    let all = run(&fx, &["report", "--all"]);
    assert!(stdout(&all).contains("herdr analytics — all history"));
}

#[test]
fn empty_database_yields_empty_report_not_a_crash() {
    let fx = fixture("empty-db");
    std::fs::create_dir_all(fx.root.join("state")).unwrap();
    let conn = Connection::open(fx.root.join("state").join("analytics.sqlite")).unwrap();
    conn.execute_batch(SCHEMA).unwrap();

    let rep = report_json(&fx, &["--all"]);
    assert!(rep.projects.is_empty());

    let text = run(&fx, &["report", "--all"]);
    assert!(text.status.success());
    assert!(stdout(&text).contains("(no indexed sessions in window"));
}

#[test]
fn malformed_since_values_are_rejected_with_clear_errors() {
    let fx = fixture("bad-since");
    seed_db(&fx.root, &standard_rows(now_ms()));

    for bad in ["banana", "5x", "2026-13-99"] {
        let out = run(&fx, &["report", "--since", bad]);
        assert!(!out.status.success(), "--since {bad} should fail");
        let err = stderr(&out);
        assert!(err.contains("analytics:"), "one-line refusal: {err}");
        assert!(err.contains("--since") || err.contains("bad"), "{err}");
    }

    // A negative count reaches the app's own parser (clap's `=` form) and is
    // rejected there rather than as an unknown flag.
    let out = run(&fx, &["report", "--since=-3d"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("bad --since"), "{}", stderr(&out));
}

// ---------------------------------------------------------------- snapshot

#[test]
fn snapshot_is_written_then_report_reads_it_without_the_database() {
    let fx = fixture("snapshot");
    let now = now_ms();
    seed_db(&fx.root, &standard_rows(now));

    let snap = run(&fx, &["snapshot", "--all"]);
    assert!(snap.status.success(), "{}", stderr(&snap));
    assert!(stderr(&snap).contains("snapshot written to"));
    assert!(fx.state_dir.join("snapshot.json").exists());

    // Remove the data source entirely: a fresh scan is impossible, so a
    // successful report proves the warm snapshot was served.
    std::fs::remove_file(fx.root.join("state").join("analytics.sqlite")).unwrap();
    let rep = report_json(&fx, &["--all"]);
    assert_eq!(rep.projects.len(), 3);
    assert_eq!(rep.projects[0].messages, 22);
}

#[test]
fn corrupt_snapshot_json_is_tolerated_by_rescanning() {
    let fx = fixture("corrupt-snapshot");
    seed_db(&fx.root, &standard_rows(now_ms()));
    std::fs::write(fx.state_dir.join("snapshot.json"), b"{ truncated").unwrap();

    let rep = report_json(&fx, &["--all"]);
    assert_eq!(rep.projects.len(), 3, "fell back to a live scan");
}

// ---------------------------------------------------------------- event-hook

fn event(fx: &Fixture, socket: &str, pane: &str, status: &str) -> Output {
    event_raw(
        fx,
        socket,
        &format!(r#"{{"pane_id":"{pane}","agent":"claude","agent_status":"{status}"}}"#),
    )
}

fn event_raw(fx: &Fixture, socket: &str, json: &str) -> Output {
    run_with_env(
        fx,
        &["event-hook", "--event-json", json],
        &[("HERDR_SOCKET_PATH", socket)],
    )
}

#[test]
fn working_then_idle_lifecycle_logs_one_completed_turn() {
    let fx = fixture("lifecycle");
    event(&fx, "sock-life", "w1:p1", "working");
    let out = event(&fx, "sock-life", "w1:p1", "idle");
    assert!(out.status.success(), "{}", stderr(&out));

    let turns = std::fs::read_to_string(fx.state_dir.join("turns.jsonl")).unwrap();
    let lines: Vec<&str> = turns.lines().collect();
    assert_eq!(lines.len(), 1, "exactly one completed turn: {turns}");
    let turn: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(turn["pane_id"], "w1:p1");
    assert_eq!(turn["agent"], "claude");
    assert!(turn["duration_ms"].is_u64(), "duration recorded: {turn}");
    assert!(turn["finished_at_ms"].is_u64());
}

#[test]
fn blocked_transition_persists_session_state_file() {
    let fx = fixture("blocked-state");
    let out = event(&fx, "sock-blocked", "w1:p2", "blocked");
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(fx.state_dir.join("agent-states-sock-blocked.json").exists());

    let states: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(fx.state_dir.join("agent-states-sock-blocked.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(states["w1:p2"]["status"], "blocked");
}

#[test]
fn envelope_wrapped_event_payload_is_accepted() {
    let fx = fixture("envelope");
    let wrapped = r#"{"type":"pane.agent_status_changed","data":{"pane_id":"w9:p9","agent":"codex","agent_status":"working"}}"#;
    let out = event_raw(&fx, "sock-envelope", wrapped);
    assert!(out.status.success(), "{}", stderr(&out));
    let out = event_raw(
        &fx,
        "sock-envelope",
        r#"{"type":"pane.agent_status_changed","data":{"pane_id":"w9:p9","agent_status":"done"}}"#,
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let turns = std::fs::read_to_string(fx.state_dir.join("turns.jsonl")).unwrap();
    assert_eq!(
        turns.lines().count(),
        1,
        "envelope lifecycle closed a turn: {turns}"
    );
}

#[test]
fn missing_pane_id_fails_loudly_with_message() {
    let fx = fixture("no-pane");
    let out = event_raw(
        &fx,
        "sock-nopane",
        r#"{"agent":"claude","agent_status":"idle"}"#,
    );
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains("pane_id"), "{err}");
}

#[test]
fn missing_event_json_env_fails_with_hint() {
    let fx = fixture("no-env");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_analytics"));
    cmd.args([
        "--root",
        &fx.root.to_string_lossy(),
        "--state-dir",
        &fx.state_dir.to_string_lossy(),
        "event-hook",
    ]);
    cmd.env("HERDR_BIN_PATH", "/usr/bin/true");
    cmd.env_remove("HERDR_PLUGIN_EVENT_JSON");
    cmd.env_remove("HERDR_SOCKET_PATH");
    let out = cmd.output().unwrap();
    assert!(!out.status.success());
    assert!(stderr(&out).contains("HERDR_PLUGIN_EVENT_JSON"));
}

#[test]
fn distinct_socket_paths_produce_distinct_state_files() {
    let fx = fixture("isolation");
    event(&fx, "/tmp/sess one/a", "w1:p1", "working");
    event(&fx, "/tmp/sess-two/b", "w1:p1", "working");

    // Non-alphanumeric socket chars sanitize to '-', trimmed at the edges.
    assert!(
        fx.state_dir
            .join("agent-states-tmp-sess-one-a.json")
            .exists()
    );
    assert!(
        fx.state_dir
            .join("agent-states-tmp-sess-two-b.json")
            .exists()
    );
    assert!(!fx.state_dir.join("agent-states-default.json").exists());

    // Same pane id in both sessions stayed isolated.
    let a: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(fx.state_dir.join("agent-states-tmp-sess-one-a.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(a["w1:p1"]["status"], "working");
}

#[test]
fn corrupt_state_file_is_tolerated_by_event_hook() {
    let fx = fixture("corrupt-state");
    std::fs::create_dir_all(&fx.state_dir).unwrap();
    std::fs::write(
        fx.state_dir.join("agent-states-sock-corrupt.json"),
        b"{{{ garbage",
    )
    .unwrap();

    let out = event(&fx, "sock-corrupt", "w1:p1", "working");
    assert!(out.status.success(), "{}", stderr(&out));
    let states: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(fx.state_dir.join("agent-states-sock-corrupt.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        states["w1:p1"]["status"], "working",
        "recovered with the new transition"
    );
}

// ---------------------------------------------------------------- watch

fn wait_for(paths: &[PathBuf], timeout: Duration) -> bool {
    let deadline = SystemTime::now() + timeout;
    loop {
        if paths.iter().all(|p| p.exists()) {
            return true;
        }
        if SystemTime::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn spawn_watch(fx: &Fixture) -> std::process::Child {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_analytics"));
    cmd.args([
        "--root",
        &fx.root.to_string_lossy(),
        "--state-dir",
        &fx.state_dir.to_string_lossy(),
        "watch",
        "--scan-interval-secs",
        "1",
    ]);
    cmd.env("HERDR_BIN_PATH", "/usr/bin/true");
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());
    cmd.spawn().expect("spawn watch daemon")
}

#[test]
fn watch_writes_snapshot_and_tips_then_keeps_running() {
    let fx = fixture("watch");
    seed_db(&fx.root, &standard_rows(now_ms()));

    let mut child = spawn_watch(&fx);
    let ok = wait_for(
        &[
            fx.state_dir.join("snapshot.json"),
            fx.state_dir.join("tips.json"),
        ],
        Duration::from_secs(15),
    );
    assert!(child.try_wait().unwrap().is_none(), "daemon exited early");
    {
        child.kill().ok();
        child.wait().ok();
    }

    assert!(ok, "snapshot.json and tips.json appeared within 15s");
    let snap: ReportJson =
        serde_json::from_str(&std::fs::read_to_string(fx.state_dir.join("snapshot.json")).unwrap())
            .unwrap();
    assert_eq!(snap.projects.len(), 3);
    let tips: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(fx.state_dir.join("tips.json")).unwrap())
            .unwrap();
    assert!(tips["items"].is_array());
}

#[test]
fn watch_survives_a_corrupted_database_scan() {
    let fx = fixture("watch-corrupt");
    // No valid sqlite here: gather fails every cycle, but the daemon must stay up.
    std::fs::create_dir_all(fx.root.join("state")).unwrap();
    std::fs::write(
        fx.root.join("state").join("analytics.sqlite"),
        b"not a database",
    )
    .unwrap();

    let mut child = spawn_watch(&fx);
    // Give it several scan cycles; tips are refreshed even when scans fail.
    let ok = wait_for(&[fx.state_dir.join("tips.json")], Duration::from_secs(15));
    let still_alive = child.try_wait().unwrap().is_none();
    {
        child.kill().ok();
        child.wait().ok();
    }

    assert!(ok, "tips.json written despite scan failures");
    assert!(still_alive, "daemon survived repeated scan failures");
}

// ---------------------------------------------------------------- usage intelligence

#[derive(serde::Deserialize)]
struct UsageJson {
    events: u64,
    total_tokens: u64,
    cache_read_tokens: u64,
    input_tokens: u64,
    cache_hit_rate: Option<f64>,
    by_model: Vec<ModelJson>,
}

#[derive(serde::Deserialize, Debug)]
struct ModelJson {
    model: String,
    events: u64,
    total_tokens: u64,
    known_cost_usd: f64,
}

#[derive(serde::Deserialize)]
struct ProjectUsageJson {
    project: String,
    events: u64,
    total_tokens: u64,
    known_cost_usd: f64,
    missed_cost_usd: f64,
}

struct UsageEventSpec {
    cwd: Option<&'static str>,
    model: Option<&'static str>,
    input: u64,
    cache_read: u64,
    cache_write: u64,
    output: u64,
    cost_usd: Option<f64>,
}

/// Enable token usage and seed one fake claude transcript; returns the
/// CLAUDE_CONFIG_DIR and a pinned HOME so the memex scanners see only the
/// fixture, never the developer's real agent logs.
fn seed_usage(fx: &Fixture, events: &[UsageEventSpec]) -> (String, String) {
    let claude_dir = fx.root.join("claude-config");
    let projects = claude_dir.join("projects").join("proj");
    std::fs::create_dir_all(&projects).unwrap();
    std::fs::write(fx.root.join("config.toml"), "token_usage = true\n").unwrap();
    std::fs::create_dir_all(fx.root.join("home")).unwrap();

    let ts = now_ms() - 60_000;
    let mut body = String::new();
    for (i, e) in events.iter().enumerate() {
        let cwd = match e.cwd {
            Some(cwd) => format!(r#""cwd":"{cwd}","#),
            None => String::new(),
        };
        let model = match e.model {
            Some(model) => format!(r#""model":"{model}","#),
            None => String::new(),
        };
        let cost = match e.cost_usd {
            Some(cost) => format!(r#","costUSD":{cost}"#),
            None => String::new(),
        };
        body.push_str(&format!(
            r#"{{"type":"assistant","sessionId":"s1",{cwd}"timestamp":{ts},"message":{{"id":"m{i}",{model}"usage":{{"input_tokens":{},"cache_read_input_tokens":{},"cache_creation_input_tokens":{},"output_tokens":{}}}}}{cost}}}"#,
            e.input, e.cache_read, e.cache_write, e.output
        ));
        body.push('\n');
    }
    std::fs::write(projects.join("s1.jsonl"), body).unwrap();
    (
        claude_dir.to_string_lossy().into_owned(),
        fx.root.join("home").to_string_lossy().into_owned(),
    )
}

fn usage_report_json(fx: &Fixture, env: &[(&str, &str)]) -> (ReportJson, UsageJson) {
    let out = run_with_env(fx, &["report", "--json", "--all"], env);
    assert!(
        out.status.success(),
        "report failed: {} {}",
        stderr(&out),
        stdout(&out)
    );
    let rep: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    let usage: UsageJson =
        serde_json::from_value(rep["usage"].clone()).expect("usage digest in report JSON");
    let rep: ReportJson = serde_json::from_value(rep).unwrap();
    (rep, usage)
}

#[test]
fn json_report_includes_model_mix_sorted_by_tokens() {
    let fx = fixture("model-mix");
    seed_db(&fx.root, &standard_rows(now_ms()));
    let env = seed_usage(
        &fx,
        &[
            UsageEventSpec {
                cwd: Some("/w/alpha"),
                model: Some("claude-sonnet-4-5"),
                input: 1_000,
                cache_read: 0,
                cache_write: 0,
                output: 500,
                cost_usd: None,
            },
            UsageEventSpec {
                cwd: Some("/w/alpha"),
                model: Some("claude-opus-4-6"),
                input: 200,
                cache_read: 100,
                cache_write: 50,
                output: 30,
                cost_usd: None,
            },
            UsageEventSpec {
                cwd: Some("/w/alpha"),
                model: Some("claude-haiku-4-5"),
                input: 10,
                cache_read: 5,
                cache_write: 0,
                output: 4,
                cost_usd: None,
            },
        ],
    );

    let (_, usage) = usage_report_json(&fx, &[("CLAUDE_CONFIG_DIR", &env.0), ("HOME", &env.1)]);
    let models: Vec<&str> = usage.by_model.iter().map(|m| m.model.as_str()).collect();
    assert_eq!(
        models,
        vec!["claude-sonnet-4-5", "claude-opus-4-6", "claude-haiku-4-5"],
        "models sorted by total tokens desc: {:?}",
        usage.by_model
    );
    assert_eq!(usage.by_model[0].total_tokens, 1_500);
    assert_eq!(usage.by_model[0].events, 1);
    assert_eq!(usage.by_model[1].total_tokens, 380);
    assert_eq!(usage.total_tokens, 1_899, "digest total matches model mix");
    assert_eq!(usage.by_model[0].known_cost_usd, 0.0);
    let model_events: u64 = usage.by_model.iter().map(|m| m.events).sum();
    assert_eq!(
        model_events, usage.events,
        "every event lands in one bucket"
    );
}

#[test]
fn unknown_model_events_bucketed_under_unknown() {
    let fx = fixture("unknown-model");
    seed_db(&fx.root, &standard_rows(now_ms()));
    let env = seed_usage(
        &fx,
        &[
            UsageEventSpec {
                cwd: Some("/w/alpha"),
                model: None,
                input: 100,
                cache_read: 0,
                cache_write: 0,
                output: 10,
                cost_usd: None,
            },
            UsageEventSpec {
                cwd: Some("/w/alpha"),
                model: Some(""),
                input: 50,
                cache_read: 0,
                cache_write: 0,
                output: 5,
                cost_usd: None,
            },
            UsageEventSpec {
                cwd: Some("/w/alpha"),
                model: Some("claude-sonnet-4-5"),
                input: 20,
                cache_read: 0,
                cache_write: 0,
                output: 2,
                cost_usd: None,
            },
        ],
    );

    let (_, usage) = usage_report_json(&fx, &[("CLAUDE_CONFIG_DIR", &env.0), ("HOME", &env.1)]);
    let unknown: Vec<&ModelJson> = usage
        .by_model
        .iter()
        .filter(|m| m.model == "unknown")
        .collect();
    assert_eq!(
        unknown.len(),
        1,
        "missing and empty models share one bucket"
    );
    assert_eq!(unknown[0].events, 2);
    assert_eq!(unknown[0].total_tokens, 165);
}

#[test]
fn cache_hit_rate_bounds_and_none_when_no_prompt_tokens() {
    let fx = fixture("hit-rate");
    seed_db(&fx.root, &standard_rows(now_ms()));
    let env = seed_usage(
        &fx,
        &[
            // prompt denominator = uncached 300 + read 600 + write 100 = 1000.
            UsageEventSpec {
                cwd: Some("/w/alpha"),
                model: Some("claude-sonnet-4-5"),
                input: 300,
                cache_read: 600,
                cache_write: 100,
                output: 50,
                cost_usd: None,
            },
            // Output-only request: no prompt tokens, must not dilute the rate.
            UsageEventSpec {
                cwd: Some("/w/alpha"),
                model: Some("claude-sonnet-4-5"),
                input: 0,
                cache_read: 0,
                cache_write: 0,
                output: 999,
                cost_usd: None,
            },
        ],
    );

    let (_, usage) = usage_report_json(&fx, &[("CLAUDE_CONFIG_DIR", &env.0), ("HOME", &env.1)]);
    assert_eq!(usage.input_tokens, 1_000);
    assert_eq!(usage.cache_read_tokens, 600);
    let rate = usage
        .cache_hit_rate
        .expect("rate defined with prompt tokens");
    assert!((0.0..=1.0).contains(&rate), "rate bounded: {rate}");
    assert!(
        (rate - 0.6).abs() < 1e-9,
        "reads over prompt tokens: {rate}"
    );

    // A window with only output-only traffic has no denominator at all.
    let fx = fixture("hit-rate-none");
    seed_db(&fx.root, &standard_rows(now_ms()));
    let env = seed_usage(
        &fx,
        &[UsageEventSpec {
            cwd: Some("/w/alpha"),
            model: Some("claude-sonnet-4-5"),
            input: 0,
            cache_read: 0,
            cache_write: 0,
            output: 42,
            cost_usd: None,
        }],
    );
    let (_, usage) = usage_report_json(&fx, &[("CLAUDE_CONFIG_DIR", &env.0), ("HOME", &env.1)]);
    assert_eq!(usage.input_tokens, 0);
    assert_eq!(usage.cache_read_tokens, 0);
    assert!(
        usage.cache_hit_rate.is_none(),
        "no prompt tokens means no rate"
    );
}

#[test]
fn project_usage_ranked_by_cost_with_unknown_fallback() {
    let fx = fixture("project-cost");
    seed_db(&fx.root, &standard_rows(now_ms()));
    let env = seed_usage(
        &fx,
        &[
            UsageEventSpec {
                cwd: Some("/w/alpha"),
                model: Some("claude-sonnet-4-5"),
                input: 100,
                cache_read: 0,
                cache_write: 0,
                output: 10,
                cost_usd: Some(0.50),
            },
            UsageEventSpec {
                cwd: Some("/w/alpha"),
                model: Some("claude-sonnet-4-5"),
                input: 100,
                cache_read: 0,
                cache_write: 0,
                output: 10,
                cost_usd: Some(0.25),
            },
            UsageEventSpec {
                cwd: Some("/w/beta"),
                model: Some("claude-sonnet-4-5"),
                input: 100,
                cache_read: 0,
                cache_write: 0,
                output: 10,
                cost_usd: Some(2.25),
            },
            UsageEventSpec {
                cwd: None,
                model: Some("claude-sonnet-4-5"),
                input: 100,
                cache_read: 0,
                cache_write: 0,
                output: 10,
                cost_usd: Some(9.99),
            },
        ],
    );
    let (rep, usage) = usage_report_json(&fx, &[("CLAUDE_CONFIG_DIR", &env.0), ("HOME", &env.1)]);
    let projects: Vec<&str> = rep
        .project_usage
        .iter()
        .map(|p| p.project.as_str())
        .collect();
    assert_eq!(
        projects,
        vec!["unknown", "/w/beta", "/w/alpha"],
        "ranked by known cost desc, missing cwd falls back to unknown"
    );
    assert_eq!(rep.project_usage[0].known_cost_usd, 9.99);
    assert_eq!(rep.project_usage[1].known_cost_usd, 2.25);
    assert_eq!(
        rep.project_usage[2].known_cost_usd, 0.75,
        "per-project costs aggregate"
    );
    assert_eq!(rep.project_usage[2].events, 2);
    assert_eq!(rep.project_usage[2].total_tokens, 220);
    assert!(
        (usage.by_model[0].known_cost_usd - 12.99).abs() < 1e-9,
        "per-model cost aggregates provider-reported figures"
    );
    assert!(
        rep.project_usage.iter().all(|p| p.missed_cost_usd == 0.0),
        "per-project missed cost is not fabricable from public per-event fields"
    );
}

#[test]
fn old_snapshot_without_new_fields_still_renders() {
    let fx = fixture("old-snapshot");
    std::fs::create_dir_all(&fx.state_dir).unwrap();
    let pre_feature = format!(
        r#"{{"generated_at_ms":{},"since_ms":null,"projects":[],"usage":{{"events":3,"total_tokens":1000,"known_cost_usd":0.5,"missed_tokens":10,"missed_cost_usd":0.01,"miss_count":1,"idle_misses":0,"model_switch_misses":0,"by_source":[]}},"usage_note":null}}"#,
        now_ms()
    );
    std::fs::write(fx.state_dir.join("snapshot.json"), pre_feature).unwrap();

    let rep = report_json(&fx, &["--all"]);
    let usage = rep.usage.expect("digest survives the round trip");
    let usage: serde_json::Value = serde_json::to_value(&usage).unwrap();
    assert_eq!(usage["events"], 3);
    assert!(
        usage["cache_hit_rate"].is_null(),
        "default is null: {usage}"
    );
    assert_eq!(usage["by_model"], serde_json::json!([]));
    assert_eq!(usage["input_tokens"], 0);
    assert!(rep.project_usage.is_empty());

    let text = run(&fx, &["report", "--all"]);
    assert!(text.status.success(), "{}", stderr(&text));
    assert!(
        stdout(&text).contains("cache hit-rate: n/a"),
        "{}",
        stdout(&text)
    );
}

#[test]
fn usage_disabled_leaves_usage_none_and_omits_new_sections() {
    let fx = fixture("usage-disabled");
    seed_db(&fx.root, &standard_rows(now_ms()));

    let rep = report_json(&fx, &["--all"]);
    assert!(rep.usage.is_none());
    assert!(rep.usage_note.is_some());
    assert!(
        rep.project_usage.is_empty(),
        "no attribution without usage tracking"
    );

    let text = stdout(&run(&fx, &["report", "--all"]));
    assert!(text.contains("Token usage: unavailable"), "{text}");
    assert!(!text.contains("cache hit-rate"), "{text}");
    assert!(!text.contains("model"), "no model table either: {text}");
}
