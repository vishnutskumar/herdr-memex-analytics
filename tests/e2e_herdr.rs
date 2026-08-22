//! Black-box herdr-integration E2E tests: run the real `analytics` binary
//! against seeded fake memex roots, exactly as a herdr session would drive it.
//! Every invocation passes explicit `--root`/`--state-dir` (and
//! `HERDR_BIN_PATH` where the binary could fire notifications), so no test
//! touches `~/.memex`, `$HERDR_PLUGIN_STATE_DIR`, or any other host state.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
        "analytics-e2e-{tag}-{}-{}-{}",
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

/// Seed `<root>/state/analytics.sqlite` with the exact memex schema plus rows.
fn seed_db(root: &Path, rows: &[(&str, &str, &str, u64, u64)]) {
    let state = root.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let conn = rusqlite::Connection::open(state.join("analytics.sqlite")).unwrap();
    conn.execute_batch(SCHEMA).unwrap();
    for (source, session_id, project, started_at, last_at) in rows {
        conn.execute(
            "INSERT INTO sessions(source, session_id, source_path, project, cwd, git_root, git_common_dir, repo_project, started_at, last_at, message_count, resolution_status)
             VALUES(?1, ?2, ?3, ?4, NULL, NULL, NULL, ?4, ?5, ?6, 3, '')",
            rusqlite::params![
                source,
                session_id,
                format!("/fake/{session_id}"),
                project,
                *started_at as i64,
                *last_at as i64,
            ],
        )
        .unwrap();
    }
}

/// Enable token usage and seed one fake claude transcript; returns the
/// CLAUDE_CONFIG_DIR and a pinned HOME so the memex scanners see only the
/// fixture, never the developer's real agent logs. Each event is
/// `(minutes_ago, uncached_input, output, cost_usd)`.
fn seed_usage(fx: &Fixture, events: &[(u64, u64, u64, f64)]) -> (String, String) {
    let claude_dir = fx.root.join("claude-config");
    let projects = claude_dir.join("projects").join("proj");
    std::fs::create_dir_all(&projects).unwrap();
    std::fs::write(fx.root.join("config.toml"), "token_usage = true\n").unwrap();
    std::fs::create_dir_all(fx.root.join("home")).unwrap();

    let mut body = String::new();
    for (i, (mins_ago, input, output, cost)) in events.iter().enumerate() {
        let ts = now_ms() - mins_ago * 60_000;
        body.push_str(&format!(
            r#"{{"type":"assistant","sessionId":"s1","cwd":"/w/alpha","timestamp":{ts},"message":{{"id":"m{i}","model":"claude-sonnet-4-5","usage":{{"input_tokens":{input},"cache_read_input_tokens":0,"cache_creation_input_tokens":0,"output_tokens":{output}}}}},"costUSD":{cost}}}"#
        ));
        body.push('\n');
    }
    std::fs::write(projects.join("s1.jsonl"), body).unwrap();
    (
        claude_dir.to_string_lossy().into_owned(),
        fx.root.join("home").to_string_lossy().into_owned(),
    )
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

fn report_json_value(fx: &Fixture, extra: &[&str], env: &[(&str, &str)]) -> serde_json::Value {
    let mut args = vec!["report", "--json"];
    args.extend_from_slice(extra);
    let out = run_with_env(fx, &args, env);
    assert!(
        out.status.success(),
        "report failed: {} {}",
        stderr(&out),
        stdout(&out)
    );
    serde_json::from_str(&stdout(&out)).expect("valid report JSON")
}

/// Poll `pred` until it returns true or the deadline passes; never a fixed sleep.
fn poll_until(deadline: Duration, mut pred: impl FnMut() -> bool) -> bool {
    let end = SystemTime::now() + deadline;
    loop {
        if pred() {
            return true;
        }
        if SystemTime::now() > end {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Watch daemon child that is always killed on drop, so a failing assert never
/// leaves a stray process behind.
struct WatchChild(Child);

impl Drop for WatchChild {
    fn drop(&mut self) {
        self.0.kill().ok();
        self.0.wait().ok();
    }
}

fn spawn_watch(fx: &Fixture, env: &[(&str, &str)]) -> WatchChild {
    let root = fx.root.to_string_lossy().into_owned();
    let state = fx.state_dir.to_string_lossy().into_owned();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_analytics"));
    cmd.args([
        "--root",
        &root,
        "--state-dir",
        &state,
        "watch",
        "--scan-interval-secs",
        "1",
    ]);
    cmd.env("HERDR_BIN_PATH", "/usr/bin/true");
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::null());
    WatchChild(cmd.spawn().expect("spawn watch daemon"))
}

/// One pane event as herdr's `[[events]]` hook would deliver it (envelope form).
fn send_event(fx: &Fixture, socket: &str, pane: &str, status: &str) {
    let json = format!(
        r#"{{"type":"pane.agent_status_changed","data":{{"pane_id":"{pane}","agent":"claude","agent_status":"{status}"}}}}"#
    );
    let out = run_with_env(
        fx,
        &["event-hook", "--event-json", &json],
        &[("HERDR_SOCKET_PATH", socket)],
    );
    assert!(
        out.status.success(),
        "event-hook {status} failed: {}",
        stderr(&out)
    );
}

fn read_json(path: &Path) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap())
        .unwrap_or_else(|e| panic!("valid JSON in {}: {e}", path.display()))
}

// ------------------------------------------------------- snapshot + report JSON

#[test]
fn snapshot_and_report_json_expose_contract_fields() {
    let fx = fixture("pipeline");
    let now = now_ms();
    let day = 24 * 3600 * 1000;
    seed_db(
        &fx.root,
        &[("claude", "a1", "/w/alpha", now - day, now - 3_600_000)],
    );
    // Three strictly increasing uncached inputs in one session (bloat: last
    // >= the 100k default threshold), all costed and inside the last hour
    // (burn rate) and today (today cost).
    let usage_env = seed_usage(
        &fx,
        &[
            (10, 50_000, 500, 0.10),
            (5, 90_000, 300, 0.05),
            (2, 120_000, 200, 0.02),
        ],
    );
    let env = [
        ("CLAUDE_CONFIG_DIR", usage_env.0.as_str()),
        ("HOME", usage_env.1.as_str()),
    ];

    // WoW appears only when the window is bounded: with no daemon snapshot on
    // disk, `--all` must scan directly and omit wow entirely.
    let all = report_json_value(&fx, &["--all"], &env);
    assert!(
        all["wow"].is_null(),
        "no wow for the unbounded --all window: {}",
        all["wow"]
    );

    // snapshot writes a valid snapshot.json.
    let out = run_with_env(&fx, &["snapshot"], &env);
    assert!(out.status.success(), "snapshot failed: {}", stderr(&out));
    let snap_path = fx.state_dir.join("snapshot.json");
    assert!(snap_path.exists(), "snapshot.json written");
    let snap = read_json(&snap_path);
    assert!(snap["projects"].is_array(), "snapshot carries projects");

    let rep = report_json_value(&fx, &[], &env);

    // Daily points: one entry for the active day, oldest first.
    let daily = rep["daily"].as_array().expect("daily array present");
    assert!(!daily.is_empty(), "activity in window yields daily points");
    assert!(
        daily
            .windows(2)
            .all(|w| w[0]["date"].as_str() < w[1]["date"].as_str())
    );
    for point in daily {
        assert!(point["date"].is_string());
        assert!(point["tokens"].is_u64());
        assert!(point["cost_usd"].is_number());
        assert!(point["sessions"].is_u64());
    }

    // Heatmap: 7 rows (last 7 local days) of 24 hourly token buckets.
    let heatmap = rep["activity_heatmap"]
        .as_array()
        .expect("activity_heatmap present");
    assert_eq!(heatmap.len(), 7, "7 day rows");
    assert!(
        heatmap
            .iter()
            .all(|row| row.as_array().unwrap().len() == 24)
    );

    // Burn rate: costed events landed within the last hour.
    let burn = rep["burn_rate_usd_per_hr"].as_f64().expect("burn rate set");
    assert!(burn > 0.0, "costed recent events give a positive burn rate");

    // Today cost: usage is available and dated today.
    let today = rep["today_cost_usd"].as_f64().expect("today cost set");
    assert!(today > 0.0);

    // Reasoning fields exist; share is a ratio (None only when output is 0).
    assert!(rep["reasoning_tokens"].is_u64());
    let share = rep["reasoning_share"].as_f64();
    if let Some(share) = share {
        assert!((0.0..=1.0).contains(&share), "share is a ratio: {share}");
    }

    // Bloat: the session's uncached input climbed monotonically past 100k.
    let bloat = rep["bloating_sessions"]
        .as_array()
        .expect("bloating_sessions present");
    let hit = bloat
        .iter()
        .find(|s| s["session_id"] == "s1")
        .expect("seeded session flagged as bloating");
    assert_eq!(hit["last_uncached_input"], 120_000);

    // WoW appears only when the window is bounded.
    let with_since = report_json_value(&fx, &["--since", "7d"], &env);
    let wow = with_since["wow"]
        .as_object()
        .expect("wow with bounded window");
    assert!(wow.contains_key("cost_usd"));
    assert!(wow.contains_key("missed_cost_usd"));
}

// ------------------------------------------------------- event-hook transitions

#[test]
fn working_blocked_working_lifecycle_records_turns_gaps_and_states() {
    let fx = fixture("hook-lifecycle");
    let now = now_ms();
    seed_db(
        &fx.root,
        &[("claude", "a1", "/w/alpha", now - 3_600_000, now - 60_000)],
    );
    send_event(&fx, "sock-e2e", "w1:p1", "working");
    send_event(&fx, "sock-e2e", "w1:p1", "blocked");
    send_event(&fx, "sock-e2e", "w1:p1", "working");
    send_event(&fx, "sock-e2e", "w1:p1", "idle");

    // Two completed turns: the working->blocked pause and the resumed turn.
    let turns_path = fx.state_dir.join("turns.jsonl");
    let turns = std::fs::read_to_string(&turns_path).unwrap();
    let lines: Vec<&str> = turns.lines().collect();
    assert_eq!(lines.len(), 2, "pause + resumed turn recorded: {turns}");
    let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    let second: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(
        first["ended_by"], "blocked",
        "first turn ended by the block"
    );
    assert_eq!(second["ended_by"], "idle", "resumed turn ended by idle");
    assert!(first["duration_ms"].is_u64());
    assert!(second["duration_ms"].is_u64());

    // The blocked->working resume lands a gap record with the block start.
    let gaps_path = fx.state_dir.join("gaps.jsonl");
    let gaps = std::fs::read_to_string(&gaps_path).unwrap();
    let gap_lines: Vec<&str> = gaps.lines().collect();
    assert_eq!(gap_lines.len(), 1, "exactly one blocked gap: {gaps}");
    let gap: serde_json::Value = serde_json::from_str(gap_lines[0]).unwrap();
    assert_eq!(gap["pane_id"], "w1:p1");
    assert_eq!(gap["agent"], "claude");
    assert!(gap["started_at_ms"].is_u64());
    assert!(gap["duration_ms"].is_u64());
    assert!(
        gap["started_at_ms"].as_u64().unwrap() <= first["finished_at_ms"].as_u64().unwrap(),
        "gap starts when the block started"
    );

    // Session state file reflects the final status.
    let states: serde_json::Value = read_json(&fx.state_dir.join("agent-states-sock-e2e.json"));
    assert_eq!(states["w1:p1"]["status"], "idle");
    assert_eq!(states["w1:p1"]["agent"], "claude");

    // The report surfaces turn statistics derived from those records.
    let rep = report_json_value(&fx, &[], &[]);
    let turns_stats = rep["turns"].as_object().expect("turns stats in report");
    assert_eq!(turns_stats["completed"], 2);
    assert_eq!(
        turns_stats["intervention_rate"].as_f64().expect("IR set"),
        0.5,
        "one of two turns ended by a block"
    );
    assert_eq!(
        turns_stats["zero_intervention_rate"]
            .as_f64()
            .expect("zIR set"),
        0.5
    );
    assert_eq!(
        turns_stats["rework_turns"], 1,
        "resumed turn is short and follows the block"
    );
    assert!(
        turns_stats["human_latency_total_ms"]
            .as_u64()
            .expect("latency total")
            > 0,
        "the blocked gap counts as human latency"
    );
    let by_agent = turns_stats["by_agent"]
        .as_array()
        .expect("by_agent present");
    assert!(
        by_agent
            .iter()
            .any(|a| a["agent"] == "claude" && a["completed"] == 2),
        "claude's turns aggregated: {by_agent:?}"
    );
    assert!(turns_stats["p50_ms"].is_u64());
    assert!(turns_stats["p95_ms"].is_u64());
}

// ------------------------------------------------------- fleet injection

/// Install a fake `herdr` executable that prints a canned api-snapshot and
/// returns its dir; the watch daemon must pick fleet data up through it.
fn install_fake_herdr(fx: &Fixture) -> String {
    let bin_dir = fx.root.join("fake-bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let canned = r#"{"result":{"snapshot":{"agents":[
        {"agent":"claude","agent_status":"working","pane_id":"w1:p1","revision":42,"workspace_id":"ws"},
        {"agent":"codex","agent_status":"blocked","pane_id":"w1:p2","revision":7,"workspace_id":"ws"},
        {"agent":"omp","agent_status":"idle","pane_id":"w2:p1","revision":3,"workspace_id":"ws"}
    ]}}}"#;
    let script = bin_dir.join("herdr");
    std::fs::write(&script, format!("#!/bin/sh\ncat <<'EOF'\n{canned}\nEOF\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    bin_dir.to_string_lossy().into_owned()
}

#[test]
fn watch_daemon_publishes_fleet_snapshot_from_herdr_binary() {
    let fx = fixture("fleet");
    let now = now_ms();
    seed_db(
        &fx.root,
        &[("claude", "a1", "/w/alpha", now - 3_600_000, now - 60_000)],
    );
    let fake_bin = install_fake_herdr(&fx);
    let path_env = std::env::var("PATH").unwrap_or_default();
    let env = [
        ("PATH", format!("{fake_bin}:{path_env}")),
        (
            "HERDR_BIN_PATH",
            std::path::Path::new(&fake_bin)
                .join("herdr")
                .to_string_lossy()
                .into_owned(),
        ),
    ];
    let env_refs: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let mut child = spawn_watch(&fx, &env_refs);
    let snap_path = fx.state_dir.join("snapshot.json");
    let landed = poll_until(Duration::from_secs(20), || {
        let Ok(text) = std::fs::read_to_string(&snap_path) else {
            return false;
        };
        let Ok(snap) = serde_json::from_str::<serde_json::Value>(&text) else {
            return false;
        };
        let Some(fleet) = snap["fleet"].as_object() else {
            return false;
        };
        fleet["working"] == 1 && fleet["blocked"] == 1 && fleet["idle"] == 1
    });
    let still_alive = child.0.try_wait().unwrap().is_none();
    drop(child); // always killed, success or failure

    assert!(landed, "fleet counts landed in snapshot.json within 20s");
    assert!(
        still_alive,
        "daemon kept running after publishing the fleet"
    );

    let snap = read_json(&snap_path);
    let fleet = &snap["fleet"];
    assert!(
        fleet["sampled_at_ms"].is_u64(),
        "fleet timestamped: {fleet}"
    );
    let churn = fleet["churn"].as_array().expect("churn list present");
    assert_eq!(churn.len(), 3, "one churn entry per pane");
    let p1 = churn
        .iter()
        .find(|c| c["pane_id"] == "w1:p1")
        .expect("working pane in churn");
    assert_eq!(p1["agent"], "claude");
    assert_eq!(p1["status"], "working");
}

// ------------------------------------------------------- retry-loop tips

#[test]
fn watch_flags_fresh_retry_loop_as_urgent_tip() {
    let fx = fixture("retry-loop");
    let now = now_ms();
    seed_db(
        &fx.root,
        &[("claude", "a1", "/w/alpha", now - 3_600_000, now - 60_000)],
    );
    // Three output_matched hits on one pane, all within the fresh 10-minute
    // window: the daemon must surface an urgent retry-loop tip.
    let alerts = format!(
        r#"{{"w9:p9":{{"count":3,"first_at_ms":{},"last_at_ms":{}}}}}"#,
        now - 120_000,
        now - 30_000
    );
    std::fs::create_dir_all(&fx.state_dir).unwrap();
    std::fs::write(fx.state_dir.join("loop-alerts.json"), alerts).unwrap();

    let mut child = spawn_watch(&fx, &[]);
    let tips_path = fx.state_dir.join("tips.json");
    let flagged = poll_until(Duration::from_secs(20), || {
        let Ok(text) = std::fs::read_to_string(&tips_path) else {
            return false;
        };
        let Ok(tips) = serde_json::from_str::<serde_json::Value>(&text) else {
            return false;
        };
        tips["items"].as_array().is_some_and(|items| {
            items.iter().any(|t| {
                t["pane_id"] == "w9:p9"
                    && t["urgent"] == true
                    && t["message"]
                        .as_str()
                        .is_some_and(|m| m.to_lowercase().contains("retry"))
            })
        })
    });
    let still_alive = child.0.try_wait().unwrap().is_none();
    drop(child); // always killed, success or failure

    assert!(flagged, "urgent retry-loop tip published within 20s");
    assert!(still_alive, "daemon kept running after publishing the tip");

    let tips = read_json(&tips_path);
    let tip = tips["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["pane_id"] == "w9:p9")
        .expect("tip for the looping pane");
    assert_eq!(tip["urgent"], true);
    assert!(
        tip["message"]
            .as_str()
            .unwrap()
            .to_lowercase()
            .contains("retry loop"),
        "message names the retry loop: {}",
        tip["message"]
    );
}
