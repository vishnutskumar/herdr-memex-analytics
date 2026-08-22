//! Black-box tests for the live fleet layer: run the real `analytics` binary
//! as the watch daemon against a fake `herdr` executable (via HERDR_BIN_PATH)
//! that serves canned `api snapshot` JSON. Every invocation passes an explicit
//! `--root`/`--state-dir`, so no test touches host state.

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusqlite::Connection;

static SEQ: AtomicU64 = AtomicU64::new(0);

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "analytics-live-{tag}-{}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed),
        nanos
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

const SCHEMA: &str = "
CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE sessions(source TEXT NOT NULL, session_id TEXT NOT NULL, source_path TEXT NOT NULL, project TEXT NOT NULL, cwd TEXT, git_root TEXT, git_common_dir TEXT, repo_project TEXT, started_at INTEGER NOT NULL, last_at INTEGER NOT NULL, message_count INTEGER NOT NULL DEFAULT 0, resolution_status TEXT NOT NULL DEFAULT '', PRIMARY KEY(source, session_id, source_path));
";

/// Minimal memex store so a daemon gather cycle succeeds.
fn seed_db(root: &Path) {
    let state = root.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let conn = Connection::open(state.join("analytics.sqlite")).unwrap();
    conn.execute_batch(SCHEMA).unwrap();
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    conn.execute(
        "INSERT INTO sessions(source, session_id, source_path, project, cwd, git_root, git_common_dir, repo_project, started_at, last_at, message_count, resolution_status)
         VALUES('claude', 'a1', '/fake/a1', '/w/alpha', NULL, NULL, NULL, NULL, ?1, ?2, 3, '')",
        rusqlite::params![now_ms - 3_600_000, now_ms],
    )
    .unwrap();
}

/// Fake herdr: every invocation cats whatever JSON the test staged in
/// FAKE_HERDR_SNAPSHOT_FILE, so cycles can see revisions move.
struct FakeHerdr {
    bin: PathBuf,
    snapshot_file: PathBuf,
}

impl FakeHerdr {
    fn spawn(dir: &Path) -> Self {
        let snapshot_file = dir.join("canned-snapshot.json");
        std::fs::write(&snapshot_file, "{}").unwrap();
        let bin = dir.join("herdr");
        std::fs::write(
            &bin,
            format!("#!/bin/sh\ncat \"{}\"\n", snapshot_file.display()),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        Self { bin, snapshot_file }
    }

    fn set_snapshot(&self, agents_json: &str) {
        std::fs::write(
            &self.snapshot_file,
            format!(
                r#"{{"id":"cli:api:snapshot","result":{{"snapshot":{{"agents":[{}]}}}}}}"#,
                agents_json
            ),
        )
        .unwrap();
    }
}

struct WatchDaemon {
    child: Child,
    state_dir: PathBuf,
}

impl Drop for WatchDaemon {
    fn drop(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
        std::fs::remove_dir_all(&self.state_dir).ok();
    }
}

fn start_watch_daemon(root: &Path, fake: &FakeHerdr) -> WatchDaemon {
    let state_dir = temp_dir("state");
    let child = Command::new(env!("CARGO_BIN_EXE_analytics"))
        .args([
            "--root",
            root.to_str().unwrap(),
            "--state-dir",
            state_dir.to_str().unwrap(),
        ])
        .args(["watch", "--scan-interval-secs", "1"])
        .env("HERDR_BIN_PATH", &fake.bin)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn analytics watch");
    WatchDaemon { child, state_dir }
}

fn read_json(path: &Path) -> Option<serde_json::Value> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn wait_for<F>(timeout: Duration, mut check: F) -> Option<serde_json::Value>
where
    F: FnMut() -> Option<serde_json::Value>,
{
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(v) = check() {
            return Some(v);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[test]
fn daemon_publishes_fleet_counts_and_churn_from_fake_herdr() {
    let tmp = temp_dir("fleet");
    let root = tmp.join("memex");
    seed_db(&root);
    let fake = FakeHerdr::spawn(&tmp);
    fake.set_snapshot(
        r#"{"agent":"omp","agent_status":"working","pane_id":"w1:p1","revision":100,"workspace_id":"w1"},
           {"agent":"claude","agent_status":"blocked","pane_id":"w1:p2","revision":7,"workspace_id":"w1"}"#,
    );
    let daemon = start_watch_daemon(&root, &fake);
    let snapshot_path = daemon.state_dir.join("snapshot.json");

    let first = wait_for(Duration::from_secs(15), || {
        read_json(&snapshot_path)?
            .get("fleet")
            .and_then(|f| f.is_object().then(|| f.clone()))
    })
    .expect("snapshot.json with fleet within timeout");
    let fleet = &first;
    assert_eq!(fleet["working"].as_u64(), Some(1));
    assert_eq!(fleet["blocked"].as_u64(), Some(1));
    assert_eq!(fleet["idle"].as_u64(), Some(0));
    assert_eq!(fleet["churn"].as_array().unwrap().len(), 2);

    // Second cycle after a revision bump: churn delta shows up, counts persist.
    fake.set_snapshot(
        r#"{"agent":"omp","agent_status":"working","pane_id":"w1:p1","revision":160,"workspace_id":"w1"},
           {"agent":"claude","agent_status":"idle","pane_id":"w1:p2","revision":7,"workspace_id":"w1"}"#,
    );
    let bumped = wait_for(Duration::from_secs(15), || {
        let v = read_json(&snapshot_path)?;
        let delta = v["fleet"]["churn"]
            .as_array()?
            .iter()
            .find(|c| c["pane_id"] == "w1:p1")?["revision_delta"]
            .as_u64()?;
        (delta == 60).then_some(v)
    })
    .expect("second cycle publishes revision_delta for the bumped pane");
    let fleet = &bumped["fleet"];
    assert_eq!(fleet["working"].as_u64(), Some(1));
    assert_eq!(fleet["blocked"].as_u64(), Some(0));
    assert_eq!(fleet["idle"].as_u64(), Some(1));
}

#[test]
fn daemon_survives_a_dead_herdr_and_keeps_scanning() {
    let tmp = temp_dir("noherdr");
    let root = tmp.join("memex");
    seed_db(&root);
    // HERDR_BIN_PATH points at something that always fails.
    let bad_bin = tmp.join("herdr-broken");
    std::fs::write(&bad_bin, "#!/bin/sh\nexit 3\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bad_bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let state_dir = temp_dir("state");
    let mut child = Command::new(env!("CARGO_BIN_EXE_analytics"))
        .args([
            "--root",
            root.to_str().unwrap(),
            "--state-dir",
            state_dir.to_str().unwrap(),
        ])
        .args(["watch", "--scan-interval-secs", "1"])
        .env("HERDR_BIN_PATH", &bad_bin)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn analytics watch");

    let snapshot_path = state_dir.join("snapshot.json");
    let ok = wait_for(Duration::from_secs(10), || {
        read_json(&snapshot_path).and_then(|v| {
            matches!(v.get("fleet"), None | Some(serde_json::Value::Null)).then_some(v)
        })
    })
    .is_some();
    assert!(
        ok,
        "daemon should keep publishing snapshots without fleet when herdr is down"
    );
    child.kill().ok();
    child.wait().ok();
    std::fs::remove_dir_all(&state_dir).ok();
}

#[test]
fn daemon_folds_fresh_loop_alerts_into_published_tips() {
    let tmp = temp_dir("loops");
    let root = tmp.join("memex");
    seed_db(&root);
    let fake = FakeHerdr::spawn(&tmp);
    fake.set_snapshot(
        r#"{"agent":"omp","agent_status":"working","pane_id":"w1:p1","revision":1,"workspace_id":"w1"}"#,
    );
    let daemon = start_watch_daemon(&root, &fake);
    let state_dir = &daemon.state_dir;

    // Simulate what the event hook accumulates for pane.output_matched hits.
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    std::fs::create_dir_all(state_dir).ok();
    std::fs::write(
        state_dir.join("loop-alerts.json"),
        format!(r#"{{"w1:p1":{{"count":3,"first_at_ms":{now_ms},"last_at_ms":{now_ms}}}}}"#),
    )
    .unwrap();

    let tips_path = state_dir.join("tips.json");
    let tip = wait_for(Duration::from_secs(15), || {
        read_json(&tips_path)?["items"]
            .as_array()?
            .iter()
            .find(|t| {
                t["message"]
                    .as_str()
                    .is_some_and(|m| m.contains("retry loop"))
            })
            .cloned()
    })
    .expect("retry-loop tip published within timeout");
    assert_eq!(tip["pane_id"].as_str(), Some("w1:p1"));
    assert_eq!(tip["urgent"].as_bool(), Some(true));
}
