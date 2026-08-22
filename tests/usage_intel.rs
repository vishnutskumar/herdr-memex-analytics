//! Black-box tests for the report data layer's usage intelligence: daily
//! series, activity heatmap, burn rate, today cost, reasoning share, bloat
//! detection, week-over-week, and the daemon's memoized scan path. Every test
//! seeds a self-contained fake memex root (fake claude transcripts and/or a
//! fake hermes state.db) plus the sessions sqlite, and runs the real binary
//! with pinned HOME/CLAUDE_CONFIG_DIR so no host state is ever touched.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusqlite::Connection;

static SEQ: AtomicU64 = AtomicU64::new(0);

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "analytics-usage-{tag}-{}-{}-{}",
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

// ---------------------------------------------------------------- local time

/// Local UTC offset in seconds, via the system `date` (tests run on a host
/// with a POSIX date; no chrono dev-dep is available here).
fn tz_offset_secs() -> i64 {
    let out = Command::new("date").arg("+%z").output().unwrap();
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let sign = if s.starts_with('-') { -1 } else { 1 };
    let digits: String = s.chars().filter(|c| c.is_ascii_digit()).collect();
    let hours: i64 = digits[0..2].parse().unwrap();
    let mins: i64 = digits[2..4].parse().unwrap();
    sign * (hours * 3600 + mins * 60)
}

fn floor_div(a: i64, b: i64) -> i64 {
    let q = a / b;
    if (a % b != 0) && ((a < 0) != (b < 0)) {
        q - 1
    } else {
        q
    }
}

/// Local calendar date of `ms`, as YYYY-MM-DD (proleptic Gregorian civil
/// conversion, so no date library is needed).
fn local_date(ms: u64) -> String {
    let off = tz_offset_secs() * 1000;
    let days = floor_div(ms as i64 + off, 86_400_000);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

fn local_hour(ms: u64) -> u32 {
    let off = tz_offset_secs() * 1000;
    let secs = floor_div(ms as i64 + off, 1000).rem_euclid(86_400);
    (secs / 3600) as u32
}

/// Epoch ms of today's local midnight.
fn local_midnight(now_ms: u64) -> u64 {
    let off = tz_offset_secs() * 1000;
    (floor_div(now_ms as i64 + off, 86_400_000) * 86_400_000 - off) as u64
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = floor_div(z, 146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// ------------------------------------------------------------------ fixtures

const SCHEMA: &str = "
CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE sessions(source TEXT NOT NULL, session_id TEXT NOT NULL, source_path TEXT NOT NULL, project TEXT NOT NULL, cwd TEXT, git_root TEXT, git_common_dir TEXT, repo_project TEXT, started_at INTEGER NOT NULL, last_at INTEGER NOT NULL, message_count INTEGER NOT NULL DEFAULT 0, resolution_status TEXT NOT NULL DEFAULT '', PRIMARY KEY(source, session_id, source_path));
";

struct SessionRow {
    session_id: &'static str,
    started_at: u64,
}

fn seed_db(root: &Path, rows: &[SessionRow]) {
    let state = root.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let conn = Connection::open(state.join("analytics.sqlite")).unwrap();
    conn.execute_batch(SCHEMA).unwrap();
    for r in rows {
        conn.execute(
            "INSERT INTO sessions(source, session_id, source_path, project, cwd, git_root, git_common_dir, repo_project, started_at, last_at, message_count, resolution_status)
             VALUES('claude', ?1, ?2, ?3, NULL, NULL, NULL, ?3, ?4, ?4, 1, '')",
            rusqlite::params![
                r.session_id,
                format!("/fake/{}", r.session_id),
                format!("/w/{}", r.session_id),
                r.started_at as i64,
            ],
        )
        .unwrap();
    }
}

struct UsageEventSpec {
    session_id: &'static str,
    cwd: &'static str,
    /// Epoch ms of the assistant message.
    ts: u64,
    input: u64,
    cache_read: u64,
    cache_write: u64,
    output: u64,
    cost_usd: Option<f64>,
}

/// Seed fake claude transcripts (one file per session) and enable
/// token_usage in the memex root config. Returns the pinned env.
fn seed_claude(fx: &Fixture, events: &[UsageEventSpec]) -> Vec<(String, String)> {
    let claude_dir = fx.root.join("claude-config");
    let projects = claude_dir.join("projects").join("proj");
    std::fs::create_dir_all(&projects).unwrap();
    std::fs::write(fx.root.join("config.toml"), "token_usage = true\n").unwrap();
    std::fs::create_dir_all(fx.root.join("home")).unwrap();

    let mut by_session: std::collections::BTreeMap<&str, Vec<&UsageEventSpec>> =
        std::collections::BTreeMap::new();
    for e in events {
        by_session.entry(e.session_id).or_default().push(e);
    }
    for (sid, list) in by_session {
        let mut body = String::new();
        for (i, e) in list.iter().enumerate() {
            let cost = match e.cost_usd {
                Some(cost) => format!(r#","costUSD":{cost:?}"#),
                None => String::new(),
            };
            body.push_str(&format!(
                r#"{{"type":"assistant","sessionId":"{sid}","cwd":"{}","timestamp":{},"message":{{"id":"{sid}-m{i}","model":"claude-sonnet-4-5","usage":{{"input_tokens":{},"cache_read_input_tokens":{},"cache_creation_input_tokens":{},"output_tokens":{}}}}}{cost}}}"#,
                e.cwd, e.ts, e.input, e.cache_read, e.cache_write, e.output
            ));
            body.push('\n');
        }
        std::fs::write(projects.join(format!("{sid}.jsonl")), body).unwrap();
    }
    pinned_env(fx)
}

fn pinned_env(fx: &Fixture) -> Vec<(String, String)> {
    vec![
        (
            "CLAUDE_CONFIG_DIR".into(),
            fx.root.join("claude-config").to_string_lossy().into_owned(),
        ),
        (
            "HOME".into(),
            fx.root.join("home").to_string_lossy().into_owned(),
        ),
    ]
}

struct HermesRow {
    id: &'static str,
    started_at: u64,
    input: u64,
    output: u64,
    reasoning: u64,
}

/// Seed a fake hermes state.db under the pinned HOME; memex discovers it at
/// $HOME/.hermes/state.db and reports reasoning tokens per session.
fn seed_hermes(fx: &Fixture, rows: &[HermesRow]) -> Vec<(String, String)> {
    let hermes_dir = fx.root.join("home").join(".hermes");
    std::fs::create_dir_all(&hermes_dir).unwrap();
    std::fs::write(fx.root.join("config.toml"), "token_usage = true\n").unwrap();
    let conn = Connection::open(hermes_dir.join("state.db")).unwrap();
    conn.execute_batch(
        "CREATE TABLE sessions (id TEXT, model TEXT, started_at INTEGER, input_tokens INTEGER, output_tokens INTEGER, cache_read_tokens INTEGER, cache_write_tokens INTEGER, reasoning_tokens INTEGER, billing_provider TEXT, estimated_cost_usd REAL, cwd TEXT, git_repo_root TEXT, profile_name TEXT);
         CREATE TABLE messages (content TEXT, system_prompt TEXT);",
    )
    .unwrap();
    for r in rows {
        conn.execute(
            "INSERT INTO sessions VALUES (?1, 'claude-sonnet-4-6', ?2, ?3, ?4, 0, 0, ?5, 'anthropic', NULL, '/w/hermes', NULL, 'p')",
            rusqlite::params![r.id, r.started_at as i64, r.input as i64, r.output as i64, r.reasoning as i64],
        )
        .unwrap();
    }
    pinned_env(fx)
}

// ---------------------------------------------------------------- runner

#[derive(serde::Deserialize)]
struct ReportJson {
    #[serde(default)]
    daily: Vec<DayJson>,
    #[serde(default)]
    activity_heatmap: Vec<[u64; 24]>,
    #[serde(default)]
    burn_rate_usd_per_hr: Option<f64>,
    #[serde(default)]
    today_cost_usd: Option<f64>,
    #[serde(default)]
    reasoning_tokens: u64,
    #[serde(default)]
    reasoning_share: Option<f64>,
    #[serde(default)]
    bloating_sessions: Vec<BloatJson>,
    #[serde(default)]
    wow: Option<WowJson>,
    #[serde(default)]
    usage: Option<serde_json::Value>,
}

#[derive(serde::Deserialize, Debug)]
struct DayJson {
    date: String,
    tokens: u64,
    cost_usd: f64,
    events: u64,
    sessions: u64,
}

#[derive(serde::Deserialize)]
struct BloatJson {
    session_id: String,
    project: String,
    last_uncached_input: u64,
}

#[derive(serde::Deserialize)]
struct WowJson {
    cost_usd: f64,
    missed_cost_usd: f64,
}

fn run(fx: &Fixture, args: &[&str], env: &[(String, String)]) -> Output {
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

fn report_json(fx: &Fixture, extra: &[&str], env: &[(String, String)]) -> ReportJson {
    let mut args = vec!["report", "--json"];
    args.extend_from_slice(extra);
    let out = run(fx, &args, env);
    assert!(
        out.status.success(),
        "report failed: {} {}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout)
    );
    serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).expect("valid report JSON")
}

// ------------------------------------------------------------------- tests

#[test]
fn daily_series_buckets_by_local_date_across_midnight() {
    let fx = fixture("daily-midnight");
    let midnight = local_midnight(now_ms());
    seed_db(
        &fx.root,
        &[
            SessionRow {
                session_id: "yesterday",
                started_at: midnight - 3_600_000,
            },
            SessionRow {
                session_id: "today",
                started_at: midnight + 3_600_000,
            },
        ],
    );
    let env = seed_claude(
        &fx,
        &[
            UsageEventSpec {
                session_id: "s1",
                cwd: "/w/alpha",
                ts: midnight - 60_000, // yesterday 23:59 local
                input: 100,
                cache_read: 0,
                cache_write: 0,
                output: 0,
                cost_usd: Some(1.0),
            },
            UsageEventSpec {
                session_id: "s1",
                cwd: "/w/alpha",
                ts: midnight + 60_000, // today 00:01 local
                input: 200,
                cache_read: 0,
                cache_write: 0,
                output: 0,
                cost_usd: Some(2.0),
            },
        ],
    );

    let rep = report_json(&fx, &["--all"], &env);
    assert_eq!(rep.daily.len(), 2, "one point per active local day");
    assert_eq!(rep.daily[0].date, local_date(midnight - 60_000));
    assert_eq!(rep.daily[1].date, local_date(midnight + 60_000));
    assert_eq!(rep.daily[0].tokens, 100, "daily: {:?}", rep.daily);
    assert_eq!(rep.daily[0].events, 1);
    assert!(
        (rep.daily[0].cost_usd - 1.0).abs() < 1e-9,
        "cost attribution: {:?}",
        rep.daily
    );
    assert_eq!(rep.daily[0].sessions, 1, "session started yesterday");
    assert_eq!(rep.daily[1].tokens, 200);
    assert_eq!(rep.daily[1].events, 1);
    assert!((rep.daily[1].cost_usd - 2.0).abs() < 1e-9);
    assert_eq!(rep.daily[1].sessions, 1, "session started today");
}

#[test]
fn heatmap_is_seven_by_24_and_buckets_tokens_by_hour() {
    let fx = fixture("heatmap");
    let now = now_ms();
    let midnight = local_midnight(now);
    let hour = local_hour(now);
    // Two events today in distinct hours when possible; the expected cells
    // are derived from the placement, not hardcoded clock times.
    let ts1 = midnight + hour as u64 * 3_600_000 + 5_000;
    let h2 = if hour >= 8 { hour - 7 } else { hour };
    let ts2 = midnight + h2 as u64 * 3_600_000 + 35_000;
    seed_db(&fx.root, &[]);
    let env = seed_claude(
        &fx,
        &[
            UsageEventSpec {
                session_id: "s1",
                cwd: "/w/alpha",
                ts: ts1,
                input: 300,
                cache_read: 0,
                cache_write: 0,
                output: 0,
                cost_usd: None,
            },
            UsageEventSpec {
                session_id: "s1",
                cwd: "/w/alpha",
                ts: ts2,
                input: 700,
                cache_read: 0,
                cache_write: 0,
                output: 0,
                cost_usd: None,
            },
            // Yesterday 23:58 local: lands in row 6 of 7, col 23.
            UsageEventSpec {
                session_id: "s1",
                cwd: "/w/alpha",
                ts: midnight - 120_000,
                input: 50,
                cache_read: 0,
                cache_write: 0,
                output: 0,
                cost_usd: None,
            },
            // Ten days ago: outside the 7-day grid entirely.
            UsageEventSpec {
                session_id: "s1",
                cwd: "/w/alpha",
                ts: midnight - 10 * 86_400_000,
                input: 9_999,
                cache_read: 0,
                cache_write: 0,
                output: 0,
                cost_usd: None,
            },
        ],
    );

    let rep = report_json(&fx, &["--all"], &env);
    assert_eq!(rep.activity_heatmap.len(), 7, "7 rows: last 7 local days");
    assert!(
        rep.activity_heatmap.iter().all(|row| row.len() == 24),
        "every row has 24 hour cells"
    );
    let total: u64 = rep.activity_heatmap.iter().flatten().sum();
    assert_eq!(total, 300 + 700 + 50, "old events stay outside the grid");
    let today = rep.activity_heatmap[6];
    assert_eq!(today[local_hour(ts1) as usize], 300, "event 1 cell");
    assert_eq!(today[local_hour(ts2) as usize], 700, "event 2 cell");
    assert_eq!(rep.activity_heatmap[5][23], 50, "yesterday 23:58 cell");
    assert_eq!(
        rep.activity_heatmap[5].iter().sum::<u64>(),
        50,
        "yesterday row holds only that event"
    );
}

#[test]
fn burn_rate_counts_only_the_trailing_hour() {
    let fx = fixture("burn");
    let now = now_ms();
    seed_db(&fx.root, &[]);
    let env = seed_claude(
        &fx,
        &[
            UsageEventSpec {
                session_id: "s1",
                cwd: "/w/alpha",
                ts: now - 30 * 60_000,
                input: 10,
                cache_read: 0,
                cache_write: 0,
                output: 0,
                cost_usd: Some(2.0),
            },
            UsageEventSpec {
                session_id: "s1",
                cwd: "/w/alpha",
                ts: now - 59 * 60_000,
                input: 10,
                cache_read: 0,
                cache_write: 0,
                output: 0,
                cost_usd: Some(0.5),
            },
            UsageEventSpec {
                session_id: "s1",
                cwd: "/w/alpha",
                ts: now - 61 * 60_000,
                input: 10,
                cache_read: 0,
                cache_write: 0,
                output: 0,
                cost_usd: Some(5.0),
            },
        ],
    );

    let rep = report_json(&fx, &["--all"], &env);
    let burn = rep.burn_rate_usd_per_hr.expect("burn rate present");
    assert!(
        (burn - 2.5).abs() < 1e-9,
        "trailing hour sums 2.0 + 0.5, got {burn}"
    );
}

#[test]
fn today_cost_uses_the_local_date_boundary() {
    let fx = fixture("today");
    let midnight = local_midnight(now_ms());
    seed_db(&fx.root, &[]);
    let env = seed_claude(
        &fx,
        &[
            UsageEventSpec {
                session_id: "s1",
                cwd: "/w/alpha",
                ts: midnight + 60_000,
                input: 10,
                cache_read: 0,
                cache_write: 0,
                output: 0,
                cost_usd: Some(1.5),
            },
            UsageEventSpec {
                session_id: "s1",
                cwd: "/w/alpha",
                ts: midnight - 60_000,
                input: 10,
                cache_read: 0,
                cache_write: 0,
                output: 0,
                cost_usd: Some(2.5),
            },
        ],
    );

    let rep = report_json(&fx, &["--all"], &env);
    let today = rep.today_cost_usd.expect("today cost present");
    assert!(
        (today - 1.5).abs() < 1e-9,
        "only today's local-date spend counts, got {today}"
    );
}

#[test]
fn reasoning_share_is_none_when_output_is_zero() {
    let fx = fixture("reasoning-zero");
    seed_db(&fx.root, &[]);
    let env = seed_claude(
        &fx,
        &[UsageEventSpec {
            session_id: "s1",
            cwd: "/w/alpha",
            ts: now_ms() - 60_000,
            input: 100,
            cache_read: 0,
            cache_write: 0,
            output: 0,
            cost_usd: None,
        }],
    );

    let rep = report_json(&fx, &["--all"], &env);
    assert_eq!(rep.reasoning_tokens, 0);
    assert!(
        rep.reasoning_share.is_none(),
        "no output tokens means no share"
    );
}

#[test]
fn reasoning_share_is_reasoning_over_output() {
    let fx = fixture("reasoning-share");
    seed_db(&fx.root, &[]);
    let env = seed_hermes(
        &fx,
        &[HermesRow {
            id: "h1",
            started_at: now_ms() - 60_000,
            input: 100,
            output: 100,
            reasoning: 40,
        }],
    );

    let rep = report_json(&fx, &["--all"], &env);
    assert_eq!(rep.reasoning_tokens, 40);
    let share = rep.reasoning_share.expect("share defined with output");
    assert!((share - 0.4).abs() < 1e-9, "40/100, got {share}");
}

#[test]
fn bloat_detection_needs_monotonic_growth_threshold_and_three_events() {
    let fx = fixture("bloat");
    let now = now_ms();
    std::fs::create_dir_all(&fx.state_dir).unwrap();
    std::fs::write(
        fx.state_dir.join("config.toml"),
        "context_bloat_tokens = 250\n",
    )
    .unwrap();
    seed_db(&fx.root, &[]);
    let mut events = Vec::new();
    let mut push = |sid: &'static str, inputs: &[u64]| {
        for (i, input) in inputs.iter().enumerate() {
            events.push(UsageEventSpec {
                session_id: sid,
                cwd: "/w/bloat",
                ts: now - 3_600_000 + (i as u64) * 60_000,
                input: *input,
                cache_read: 0,
                cache_write: 0,
                output: 0,
                cost_usd: None,
            });
        }
    };
    push("s_bloat", &[100, 200, 300]); // monotonic, ends >= 250: flagged
    push("s_bigger", &[200, 300, 400]); // monotonic, ends higher: ranked first
    push("s_dip", &[100, 400, 300]); // ends >= 250 but not monotonic
    push("s_short", &[100, 900]); // high but only two events
    push("s_low", &[50, 100, 200]); // monotonic but below threshold
    let env = seed_claude(&fx, &events);

    let rep = report_json(&fx, &["--all"], &env);
    let ids: Vec<&str> = rep
        .bloating_sessions
        .iter()
        .map(|b| b.session_id.as_str())
        .collect();
    assert_eq!(
        ids,
        vec!["s_bigger", "s_bloat"],
        "only monotonic >=3-event sessions past the threshold, worst first: {ids:?}"
    );
    assert_eq!(rep.bloating_sessions[0].last_uncached_input, 400);
    assert_eq!(rep.bloating_sessions[1].last_uncached_input, 300);
    assert_eq!(rep.bloating_sessions[0].project, "/w/bloat");
}

#[test]
fn wow_appears_only_with_a_since_window_and_reports_the_prior_window() {
    let fx = fixture("wow");
    let now = now_ms();
    seed_db(&fx.root, &[]);
    let env = seed_claude(
        &fx,
        &[
            // Inside the prior 1d window of `--since 1d` ([now-2d, now-1d)).
            UsageEventSpec {
                session_id: "s1",
                cwd: "/w/alpha",
                ts: now - 36 * 3_600_000,
                input: 10,
                cache_read: 0,
                cache_write: 0,
                output: 0,
                cost_usd: Some(3.0),
            },
            // Inside the current window ([now-1d, now)).
            UsageEventSpec {
                session_id: "s1",
                cwd: "/w/alpha",
                ts: now - 12 * 3_600_000,
                input: 10,
                cache_read: 0,
                cache_write: 0,
                output: 0,
                cost_usd: Some(4.0),
            },
        ],
    );

    let full = report_json(&fx, &["--all"], &env);
    assert!(
        full.wow.is_none(),
        "unbounded history has no preceding window to compare"
    );

    let rep = report_json(&fx, &["--since", "1d"], &env);
    let wow = rep.wow.expect("wow present for a bounded window");
    assert!(
        (wow.cost_usd - 3.0).abs() < 1e-9,
        "prior-window cost only, got {}",
        wow.cost_usd
    );
    assert!(
        (wow.missed_cost_usd - 0.0).abs() < 1e-9,
        "single-event prior window has no cache-miss cost"
    );
}

#[test]
fn daemon_cycle_with_memo_ttl_produces_a_correct_snapshot() {
    let fx = fixture("memo");
    let midnight = local_midnight(now_ms());
    seed_db(&fx.root, &[]);
    let env = seed_claude(
        &fx,
        &[UsageEventSpec {
            session_id: "s1",
            cwd: "/w/alpha",
            ts: midnight + 60_000,
            input: 100,
            cache_read: 0,
            cache_write: 0,
            output: 0,
            cost_usd: Some(1.25),
        }],
    );

    // The daemon passes a nonzero memo TTL; the snapshot it writes must be
    // identical in content to the direct-scan path.
    let root = fx.root.to_string_lossy().into_owned();
    let state = fx.state_dir.to_string_lossy().into_owned();
    let mut child = Command::new(env!("CARGO_BIN_EXE_analytics"))
        .args([
            "--root",
            &root,
            "--state-dir",
            &state,
            "watch",
            "--scan-interval-secs",
            "1",
        ])
        .env("HERDR_BIN_PATH", "/usr/bin/true")
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .spawn()
        .expect("spawn analytics watch");

    let snapshot = fx.state_dir.join("snapshot.json");
    let deadline = Instant::now() + Duration::from_secs(30);
    let parsed = loop {
        std::thread::sleep(Duration::from_millis(200));
        if let Some(rep) = std::fs::read_to_string(&snapshot)
            .ok()
            .and_then(|text| serde_json::from_str::<ReportJson>(&text).ok())
        {
            break rep;
        }
        assert!(
            Instant::now() < deadline,
            "daemon never wrote a parsable snapshot"
        );
    };
    child.kill().ok();
    child.wait().ok();

    let today = local_date(now_ms());
    let point = parsed
        .daily
        .iter()
        .find(|d| d.date == today)
        .expect("today's activity in the daemon snapshot");
    assert_eq!(point.tokens, 100);
    assert!((point.cost_usd - 1.25).abs() < 1e-9);
    let usage = parsed.usage.expect("usage digest in daemon snapshot");
    assert_eq!(usage["events"], 1);
}

#[test]
fn old_snapshot_without_new_fields_deserializes_with_defaults() {
    let fx = fixture("old-snapshot");
    std::fs::create_dir_all(&fx.state_dir).unwrap();
    let pre_feature = format!(
        r#"{{"generated_at_ms":{},"since_ms":null,"projects":[],"usage":null,"usage_note":"disabled"}}"#,
        now_ms()
    );
    std::fs::write(fx.state_dir.join("snapshot.json"), pre_feature).unwrap();

    // `report --json --all` prefers a fresh snapshot; it must parse the
    // pre-feature file and default every new field.
    let rep = report_json(&fx, &["--all"], &[]);
    assert!(rep.daily.is_empty(), "missing daily defaults to empty");
    assert!(
        rep.activity_heatmap.is_empty(),
        "missing heatmap defaults to empty"
    );
    assert!(rep.burn_rate_usd_per_hr.is_none());
    assert!(rep.today_cost_usd.is_none());
    assert_eq!(rep.reasoning_tokens, 0);
    assert!(rep.reasoning_share.is_none());
    assert!(rep.bloating_sessions.is_empty());
    assert!(rep.wow.is_none());
}
