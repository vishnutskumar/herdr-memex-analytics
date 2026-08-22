//! PTY-driven behavior tests for the `analytics ui` dashboard.
//!
//! Written against the shared v2 contract only (tester-blind): each test
//! spawns the real binary behind a 100x30 pseudo-terminal, drives it with raw
//! key/mouse bytes, and asserts on ANSI-stripped screen text plus the raw
//! escape sequences emitted on exit.

use std::fs;
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use rusqlite::Connection;

const COLS: u16 = 100;
const ROWS: u16 = 30;
/// Generous first-paint deadline: the binary may do a full scan on startup.
const PAINT_TIMEOUT: Duration = Duration::from_secs(20);
const KEY_TIMEOUT: Duration = Duration::from_secs(10);
/// Quiet window before we treat the screen as settled (clamp tests).
const QUIESCE_IDLE: Duration = Duration::from_millis(250);

static SEQ: AtomicU32 = AtomicU32::new(0);

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "analytics-tui-{tag}-{}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed),
        nanos
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

struct Fixture {
    root: PathBuf,
    state_dir: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).ok();
        fs::remove_dir_all(&self.state_dir).ok();
    }
}

fn fixture(tag: &str) -> Fixture {
    Fixture {
        root: temp_dir(&format!("{tag}-root")),
        state_dir: temp_dir(&format!("{tag}-state")),
    }
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS sessions(source TEXT NOT NULL, session_id TEXT NOT NULL, source_path TEXT NOT NULL, project TEXT NOT NULL, cwd TEXT, git_root TEXT, git_common_dir TEXT, repo_project TEXT, started_at INTEGER NOT NULL, last_at INTEGER NOT NULL, message_count INTEGER NOT NULL DEFAULT 0, resolution_status TEXT NOT NULL DEFAULT '', PRIMARY KEY(source, session_id, source_path));
";

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

static ROW_SEQ: AtomicU32 = AtomicU32::new(0);

/// Seed `<root>/state/analytics.sqlite` with one row per entry.
/// `rows`: (project, repo_project, started_at_ms, last_at_ms).
fn seed_db(root: &Path, rows: &[(&str, Option<&str>, u64, u64)]) {
    let state = root.join("state");
    fs::create_dir_all(&state).unwrap();
    let conn = Connection::open(state.join("analytics.sqlite")).unwrap();
    conn.execute_batch(SCHEMA).unwrap();
    for (project, repo_project, started_at, last_at) in rows {
        let id = format!("s{}", ROW_SEQ.fetch_add(1, Ordering::Relaxed));
        conn.execute(
            "INSERT INTO sessions(source, session_id, source_path, project, cwd, git_root, git_common_dir, repo_project, started_at, last_at, message_count, resolution_status)
             VALUES('claude', ?1, ?2, ?3, NULL, NULL, NULL, ?4, ?5, ?6, 4, '')",
            rusqlite::params![
                id,
                format!("/fake/{id}"),
                project,
                repo_project,
                *started_at as i64,
                *last_at as i64,
            ],
        )
        .unwrap();
    }
}

/// Three distinct projects, all inside any default window.
fn three_projects(root: &Path) {
    let now = now_ms();
    seed_db(
        root,
        &[
            (
                "/w/alpha",
                Some("alpha-repo"),
                now - 3_600_000,
                now - 60_000,
            ),
            (
                "/w/alpha/sub",
                Some("alpha-repo"),
                now - 7_200_000,
                now - 300_000,
            ),
            ("/w/beta", None, now - 1_800_000, now - 120_000),
            ("/w/gamma", None, now - 900_000, now - 30_000),
        ],
    );
}

/// Serialize forks: a forked child must not run while other test threads may
/// hold allocator locks, or it deadlocks before exec.
static SPAWN_LOCK: Mutex<()> = Mutex::new(());

// ---------------------------------------------------------------------------
// PTY harness
// ---------------------------------------------------------------------------

struct Tui {
    master: fs::File,
    pid: libc::pid_t,
    /// Every byte ever received from the child PTY (raw, escapes included).
    raw: String,
}

impl Tui {
    fn spawn(fx: &Fixture) -> Tui {
        let _guard = SPAWN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Build everything (allocation, env) BEFORE fork: the child must not
        // touch the allocator, which may be locked by another test thread.
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_analytics"));
        cmd.arg("ui")
            .arg("--root")
            .arg(&fx.root)
            .arg("--state-dir")
            .arg(&fx.state_dir)
            .env("TERM", "xterm-256color")
            .env("HERDR_BIN_PATH", "/usr/bin/true");

        let mut ws = libc::winsize {
            ws_col: COLS,
            ws_row: ROWS,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let mut master_fd: libc::c_int = -1;
        let pid = unsafe {
            libc::forkpty(
                &mut master_fd,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut ws,
            )
        };
        assert!(pid >= 0, "forkpty failed: {pid}");
        if pid == 0 {
            // Child: exec on the fresh controlling tty; never return.
            unsafe {
                let _ = cmd.exec();
                libc::_exit(127);
            }
        }
        unsafe {
            let flags = libc::fcntl(master_fd, libc::F_GETFL);
            libc::fcntl(master_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
        Tui {
            master: unsafe { fs::File::from_raw_fd(master_fd) },
            pid,
            raw: String::new(),
        }
    }

    fn feed(&mut self, bytes: &[u8]) {
        self.master.write_all(bytes).unwrap();
        self.master.flush().unwrap();
    }

    fn mark(&self) -> usize {
        self.raw.len()
    }

    /// Read whatever is currently available from the PTY into `raw`.
    fn drain_available(&mut self) {
        let mut buf = [0u8; 16_384];
        loop {
            let n =
                unsafe { libc::read(self.master.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            if n > 0 {
                self.raw
                    .push_str(&String::from_utf8_lossy(&buf[..n as usize]));
            } else {
                break;
            }
        }
    }

    /// Poll until the ANSI-stripped output after `mark` contains `needle`.
    fn wait_text_after(&mut self, mark: usize, needle: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut fds = [libc::pollfd {
            fd: self.master.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        }];
        loop {
            self.drain_available();
            if normalize_screen(&self.raw[mark..]).contains(needle) {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            let ms = ((deadline - now).as_millis() as i32).min(200);
            let r = unsafe { libc::poll(fds.as_mut_ptr(), 1, ms) };
            if r < 0 && errno_is_interrupt() {
                continue;
            }
        }
    }

    fn wait_text(&mut self, needle: &str, timeout: Duration) -> bool {
        self.wait_text_after(self.mark(), needle, timeout)
    }

    /// Wait until no new output arrives for `idle`, within `budget`.
    fn quiesce(&mut self, idle: Duration, budget: Duration) {
        let deadline = Instant::now() + budget;
        while Instant::now() < deadline {
            let fds = [libc::pollfd {
                fd: self.master.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            }];
            let r = unsafe { libc::poll(fds.as_ptr().cast_mut(), 1, idle.as_millis() as i32) };
            if r == 0 {
                return;
            }
            self.drain_available();
        }
    }

    /// Normalized screen text received after `mark`.
    fn screen_from(&self, mark: usize) -> String {
        normalize_screen(&self.raw[mark..])
    }

    /// Reap the child; returns its exit code (negative if killed by a signal).
    /// Drains the PTY while polling so the child never blocks writing frames.
    fn exit_status(&mut self, timeout: Duration) -> Option<i32> {
        let deadline = Instant::now() + timeout;
        loop {
            self.drain_available();
            let mut status: libc::c_int = 0;
            let r = unsafe { libc::waitpid(self.pid, &mut status, libc::WNOHANG) };
            if r == self.pid {
                self.drain_available();
                return Some(if libc::WIFEXITED(status) {
                    libc::WEXITSTATUS(status)
                } else if libc::WIFSIGNALED(status) {
                    -libc::WTERMSIG(status)
                } else {
                    -255
                });
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn quit_and_expect_clean_exit(&mut self, key: &[u8]) {
        self.feed(key);
        let status = self
            .exit_status(KEY_TIMEOUT)
            .expect("child did not exit after quit key");
        assert_eq!(status, 0, "quit must exit with status 0");
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        // Every test kills its child, even on assertion failure mid-drive.
        // Bounded nonblocking reap: never hang the suite on a stuck child.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let mut status: libc::c_int = 0;
            let r = unsafe { libc::waitpid(self.pid, &mut status, libc::WNOHANG) };
            if r == self.pid {
                return;
            }
            if r < 0 {
                // ECHILD: already reaped by exit_status; anything else is
                // unexpected, but never hang the suite on a stuck child.
                return;
            }
            if Instant::now() >= deadline {
                return;
            }
            unsafe { libc::kill(self.pid, libc::SIGKILL) };
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

fn errno_is_interrupt() -> bool {
    std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
}

/// Remove CSI/OSC escape sequences; each sequence collapses to one space so
/// text split across cursor moves reassembles once whitespace is normalized.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            if c != '\r' {
                out.push(c);
            }
            continue;
        }
        match chars.peek() {
            Some('[') => {
                chars.next();
                // Consume through the final byte of the CSI (0x40..=0x7E).
                for n in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&n) {
                        break;
                    }
                }
            }
            Some(']') => {
                chars.next();
                // OSC runs to BEL or ST (ESC \).
                loop {
                    match chars.next() {
                        Some('\x07') | None => break,
                        Some('\x1b') => {
                            chars.next();
                            break;
                        }
                        _ => {}
                    }
                }
            }
            Some(_) => {
                chars.next();
            }
            None => {}
        }
        out.push(' ');
    }
    out
}

/// Canonical screen text: escapes gone, runs of whitespace collapsed.
fn normalize_screen(s: &str) -> String {
    strip_ansi(s)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Every literal `Sessions <sel>/<total>` occurrence in screen text.
fn titles_in(screen: &str) -> Vec<(u64, u64)> {
    let mut found = Vec::new();
    let mut rest = screen;
    while let Some(idx) = rest.find("Sessions ") {
        rest = &rest[idx + "Sessions ".len()..];
        let bytes = rest.as_bytes();
        let mut i = 0;
        let mut sel = 0u64;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            sel = sel * 10 + u64::from(bytes[i] - b'0');
            i += 1;
        }
        if i == 0 || i >= bytes.len() || bytes[i] != b'/' {
            continue;
        }
        i += 1;
        let mut total = 0u64;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            total = total * 10 + u64::from(bytes[i] - b'0');
            i += 1;
        }
        if i < bytes.len() && bytes[i] == b' ' {
            found.push((sel, total));
        }
    }
    found
}

const BRAILLE_RAMP: [char; 4] = ['\u{28c0}', '\u{28e4}', '\u{28f6}', '\u{28ff}'];

/// Mode word ("tokens"|"cost"|"sessions") of the activity chart. The harness
/// normalizes the screen to a single whitespace-joined line, so the discovery
/// protocol's "mode word within two lines above the braille row" collapses
/// into that line; in the fixture dashboard (no usage data) the chart title is
/// the only carrier of these words. A braille ramp row must be present.
fn chart_mode_word(screen: &str) -> Option<String> {
    if !screen.chars().any(|c| BRAILLE_RAMP.contains(&c)) {
        return None;
    }
    let lower = screen.to_ascii_lowercase();
    ["tokens", "cost", "sessions"]
        .iter()
        .find(|word| lower.contains(*word))
        .map(|word| (*word).to_string())
}

fn wheel_down(col: u32, row: u32) -> Vec<u8> {
    format!("\x1b[<65;{col};{row}M").into_bytes()
}

fn wheel_up(col: u32, row: u32) -> Vec<u8> {
    format!("\x1b[<64;{col};{row}M").into_bytes()
}

impl Tui {
    fn set_rows(&self, rows: u16) {
        let ws = libc::winsize {
            ws_col: COLS,
            ws_row: rows,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &ws) };
    }

    /// ratatui redraws only changed cells, so a title transition like
    /// `Sessions 1/3` -> `Sessions 2/3` arrives as a one-cell diff. Bounce the
    /// PTY height (SIGWINCH makes crossterm wake and ratatui fully repaint),
    /// let output settle at each size, then return the normalized screen text
    /// accumulated since `mark`.
    fn screen_after_input(&mut self, mark: usize, settle: Duration) -> String {
        self.set_rows(ROWS - 1);
        self.quiesce(QUIESCE_IDLE, settle);
        self.set_rows(ROWS);
        self.quiesce(QUIESCE_IDLE, settle);
        self.screen_from(mark)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn j_k_keys_move_selection_with_title_updates() {
    let fx = fixture("jk-move");
    three_projects(&fx.root);
    let mut tui = Tui::spawn(&fx);

    assert!(
        tui.wait_text("Sessions 1/3", PAINT_TIMEOUT),
        "initial paint should select the first of 3 projects"
    );

    let m = tui.mark();
    tui.feed(b"j");
    let screen = tui.screen_after_input(m, KEY_TIMEOUT);
    assert!(
        screen.contains("Sessions 2/3"),
        "j should advance the selection to 2/3; screen: {screen}"
    );

    let m = tui.mark();
    tui.feed(b"k");
    let screen = tui.screen_after_input(m, KEY_TIMEOUT);
    assert!(
        screen.contains("Sessions 1/3"),
        "k should move the selection back to 1/3; screen: {screen}"
    );
}

#[test]
fn arrow_keys_move_selection_like_j_k() {
    let fx = fixture("arrows-move");
    three_projects(&fx.root);
    let mut tui = Tui::spawn(&fx);

    assert!(tui.wait_text("Sessions 1/3", PAINT_TIMEOUT));

    let m = tui.mark();
    tui.feed(b"\x1b[B"); // Down
    let screen = tui.screen_after_input(m, KEY_TIMEOUT);
    assert!(
        screen.contains("Sessions 2/3"),
        "Down arrow should advance the selection; screen: {screen}"
    );

    let m = tui.mark();
    tui.feed(b"\x1b[A"); // Up
    let screen = tui.screen_after_input(m, KEY_TIMEOUT);
    assert!(
        screen.contains("Sessions 1/3"),
        "Up arrow should move the selection back; screen: {screen}"
    );
}

#[test]
fn selection_clamps_at_top_and_bottom_bounds() {
    let fx = fixture("clamping");
    three_projects(&fx.root);
    let mut tui = Tui::spawn(&fx);

    assert!(tui.wait_text("Sessions 1/3", PAINT_TIMEOUT));

    // Hammer k well past the top bound; selection must stay at 1/3.
    let m = tui.mark();
    for _ in 0..10 {
        tui.feed(b"k");
    }
    let screen = tui.screen_after_input(m, KEY_TIMEOUT);
    assert_eq!(
        titles_in(&screen).into_iter().next_back(),
        Some((1, 3)),
        "selection must clamp at the top bound; screen: {screen}"
    );

    // Hammer j well past the bottom bound; selection must stop at 3/3.
    let m = tui.mark();
    for _ in 0..10 {
        tui.feed(b"j");
    }
    let screen = tui.screen_after_input(m, KEY_TIMEOUT);
    assert_eq!(
        titles_in(&screen).into_iter().next_back(),
        Some((3, 3)),
        "selection must clamp at the bottom bound; screen: {screen}"
    );
}

#[test]
fn mouse_wheel_scroll_moves_selection_and_clamps() {
    let fx = fixture("wheel-scroll");
    three_projects(&fx.root);
    let mut tui = Tui::spawn(&fx);

    assert!(tui.wait_text("Sessions 1/3", PAINT_TIMEOUT));

    // Two wheel-down events land on 3/3.
    let m = tui.mark();
    tui.feed(&wheel_down(10, 5));
    tui.feed(&wheel_down(10, 5));
    let screen = tui.screen_after_input(m, KEY_TIMEOUT);
    assert!(
        screen.contains("Sessions 3/3"),
        "wheel down should scroll the selection forward; screen: {screen}"
    );

    // One wheel-up steps back to 2/3.
    let m = tui.mark();
    tui.feed(&wheel_up(10, 5));
    let screen = tui.screen_after_input(m, KEY_TIMEOUT);
    assert!(
        screen.contains("Sessions 2/3"),
        "wheel up should scroll the selection back; screen: {screen}"
    );

    // Hammering wheel-up past the top clamps at 1/3.
    let m = tui.mark();
    for _ in 0..10 {
        tui.feed(&wheel_up(10, 5));
    }
    let screen = tui.screen_after_input(m, KEY_TIMEOUT);
    assert_eq!(
        titles_in(&screen).into_iter().next_back(),
        Some((1, 3)),
        "wheel scrolling must clamp at the top bound; screen: {screen}"
    );
}

#[test]
fn q_quits_with_success_status_and_restores_terminal() {
    let fx = fixture("q-exit");
    three_projects(&fx.root);
    let mut tui = Tui::spawn(&fx);

    assert!(tui.wait_text("Sessions 1/3", PAINT_TIMEOUT));
    tui.quit_and_expect_clean_exit(b"q");

    // Disable mouse capture: SGR-mode off (and friends), leave alt screen.
    assert!(
        tui.raw.contains("\x1b[?1006l"),
        "must emit disable-mouse-capture (SGR \\e[?1006l) on exit"
    );
    assert!(
        tui.raw.contains("\x1b[?1049l"),
        "must emit leave-alt-screen (\\e[?1049l) on exit"
    );
}

#[test]
fn esc_key_also_quits_cleanly() {
    let fx = fixture("esc-exit");
    three_projects(&fx.root);
    let mut tui = Tui::spawn(&fx);

    assert!(tui.wait_text("Sessions 1/3", PAINT_TIMEOUT));
    tui.quit_and_expect_clean_exit(b"\x1b");
}

#[test]
fn r_rescan_picks_up_new_sessions() {
    let fx = fixture("rescan");
    three_projects(&fx.root);
    let mut tui = Tui::spawn(&fx);

    assert!(tui.wait_text("Sessions 1/3", PAINT_TIMEOUT));

    // A session lands in a brand-new project between scans; only a real
    // rescan can make the title reflect it.
    let now = now_ms();
    seed_db(&fx.root, &[("/w/delta", None, now - 600_000, now - 10_000)]);
    let m = tui.mark();
    tui.feed(b"r");
    let screen = tui.screen_after_input(m, KEY_TIMEOUT);
    assert!(
        screen.contains("Sessions 1/4"),
        "r must trigger a rescan that picks up the new project; screen: {screen}"
    );
}

#[test]
fn empty_memex_renders_empty_dashboard_without_panicking() {
    let fx = fixture("empty-state");
    // Root exists but has no state dir / database at all.
    let mut tui = Tui::spawn(&fx);

    assert!(
        tui.wait_text("Sessions 0/0", PAINT_TIMEOUT),
        "empty memex root must render Sessions 0/0"
    );
    tui.quit_and_expect_clean_exit(b"q");
}

#[test]
fn c_cycles_chart_mode_tokens_cost_sessions() {
    let fx = fixture("chart-cycle");
    three_projects(&fx.root);
    let mut tui = Tui::spawn(&fx);

    assert!(tui.wait_text("Sessions 1/3", PAINT_TIMEOUT));
    // The first frame streams row by row; wait for it to settle so the
    // activity chart below the usage panel is included.
    tui.quiesce(QUIESCE_IDLE, PAINT_TIMEOUT);
    let s0 = tui.screen_from(0);
    assert_eq!(
        chart_mode_word(&s0),
        Some("tokens".into()),
        "chart starts in tokens mode; screen: {s0}"
    );

    for expected in ["cost", "sessions", "tokens"] {
        let m = tui.mark();
        tui.feed(b"c");
        let screen = tui.screen_after_input(m, KEY_TIMEOUT);
        assert_eq!(
            chart_mode_word(&screen),
            Some(expected.into()),
            "c should cycle chart mode to {expected}; screen: {screen}"
        );
    }
}
