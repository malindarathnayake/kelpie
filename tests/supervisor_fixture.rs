//! Cross-platform supervisor fixture.
//!
//! Starts a real headless server, opens a pane running the platform shell, and
//! drives agent state through `herdr pane report-agent`, exactly as an agent
//! integration hook would. Everything goes through the `herdr` CLI, so the
//! same tests run on Windows (named pipe) and Unix (socket).
//!
//! The `current_*` tests characterize the legacy `agent get/wait` surface,
//! which must not change: the supervisor facts (awaiting-human questions,
//! background work, completion) live in the separate `agents.digest` method
//! (`herdr agent digest`), exercised by the remaining tests when the fixture
//! server runs with `[supervisor] enabled = true`.

use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Key fragments the planned status facts and receipts would introduce. Their
/// absence anywhere in a response is what "not a separate fact today" means.
const FUTURE_FACT_KEYS: &[&str] = &[
    "await",
    "question",
    "background",
    "completion",
    "receipt",
    "deliver",
    "command_id",
];

/// Fields herdr already reports today that merely share a fragment above.
const EXISTING_KEYS: &[&str] = &["completion_seq"];

fn app_dir_name() -> &'static str {
    if cfg!(debug_assertions) {
        "herdr-dev"
    } else {
        "herdr"
    }
}

fn default_shell() -> &'static str {
    if cfg!(windows) {
        "cmd.exe"
    } else {
        "/bin/sh"
    }
}

struct Fixture {
    base: PathBuf,
    config_home: PathBuf,
    runtime_dir: PathBuf,
    state_home: PathBuf,
    socket_path: PathBuf,
    server: Option<Child>,
    seq: u64,
}

impl Fixture {
    fn start() -> Self {
        Self::start_with_config("")
    }

    /// Start a server whose own temp config enables the supervisor surface.
    fn start_with_supervisor() -> Self {
        Self::start_with_config("[supervisor]\nenabled = true\n")
    }

    fn start_with_config(extra_config: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        let base = std::env::temp_dir().join(format!(
            "herdr-supervisor-fixture-{}-{nanos}",
            std::process::id()
        ));
        let config_home = base.join("config");
        let runtime_dir = base.join("runtime");
        let state_home = base.join("state");
        let socket_path = runtime_dir.join("herdr.sock");
        let app_config = config_home.join(app_dir_name());
        std::fs::create_dir_all(&app_config).unwrap();
        std::fs::create_dir_all(&runtime_dir).unwrap();
        std::fs::write(
            app_config.join("config.toml"),
            format!(
                "onboarding = false\n[terminal]\ndefault_shell = {:?}\nshell_mode = \"non_login\"\n{extra_config}",
                default_shell()
            ),
        )
        .unwrap();

        let mut command = Command::new(env!("CARGO_BIN_EXE_herdr"));
        command
            .arg("server")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let fixture = Self {
            base,
            config_home,
            runtime_dir,
            state_home,
            socket_path,
            server: None,
            seq: 0,
        };
        fixture.isolate(&mut command);
        let mut fixture = fixture;
        fixture.server = Some(command.spawn().unwrap());

        let ready = wait_until(Duration::from_secs(15), || {
            fixture.cli(&["workspace", "list"]).status.success()
        });
        assert!(ready, "server did not become ready");
        fixture
    }

    /// Point a child process at this fixture's server and strip any herdr
    /// context inherited from a surrounding session.
    fn isolate(&self, command: &mut Command) {
        command
            .env("XDG_CONFIG_HOME", &self.config_home)
            .env("XDG_RUNTIME_DIR", &self.runtime_dir)
            .env("XDG_STATE_HOME", &self.state_home)
            .env("HERDR_SOCKET_PATH", &self.socket_path)
            .env_remove("HERDR_CLIENT_SOCKET_PATH")
            .env_remove("HERDR_ENV")
            .env_remove("HERDR_SESSION")
            .env_remove("HERDR_PANE_ID");
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_herdr"));
        command.args(args);
        self.isolate(&mut command);
        command
    }

    fn cli(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    fn cli_json(&self, args: &[&str]) -> serde_json::Value {
        let output = self.cli(args);
        assert!(
            output.status.success(),
            "herdr {} failed: {}\n{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        );
        // Some commands (pane run, report-agent) succeed with empty stdout.
        if output.stdout.iter().all(u8::is_ascii_whitespace) {
            return serde_json::Value::Null;
        }
        serde_json::from_slice(&output.stdout).unwrap_or_else(|err| {
            panic!(
                "herdr {} printed non-JSON ({err}): {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stdout)
            )
        })
    }

    /// Create a workspace and return its root pane id.
    fn pane(&self) -> String {
        let created = self.cli_json(&["workspace", "create", "--cwd", self.base.to_str().unwrap()]);
        created["result"]["root_pane"]["pane_id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// Run `command` in the pane and wait until `marker` is visible.
    fn run_and_wait(&self, pane: &str, command: &str, marker: &str) {
        self.cli_json(&["pane", "run", pane, command]);
        let waited = self.cli(&[
            "pane",
            "wait-output",
            pane,
            "--match",
            marker,
            "--timeout",
            "10000",
        ]);
        assert!(
            waited.status.success(),
            "{marker:?} never appeared: {}",
            String::from_utf8_lossy(&waited.stderr)
        );
    }

    /// Print `text` on the pane's screen and wait until it is visible.
    fn show(&self, pane: &str, text: &str) {
        self.run_and_wait(pane, &format!("echo {text}"), text);
    }

    /// Start a real long-running background process in the pane's shell, the
    /// way an agent leaves a build or review running after its turn ends.
    fn start_background_job(&self, pane: &str) {
        let command = if cfg!(windows) {
            "start /b ping -n 300 127.0.0.1 >nul & echo background-started"
        } else {
            "sleep 300 & echo background-started"
        };
        self.run_and_wait(pane, command, "background-started");
    }

    /// Run a long-lived stand-in process that herdr identifies as Claude, so
    /// `agent prompt` sees the agent as the pane's foreground process. It is an
    /// interactive shell, so typed prompts are echoed and otherwise harmless.
    fn launch_stand_in_claude(&self, pane: &str) {
        let bin = self.base.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let command = if cfg!(windows) {
            let system_root =
                std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
            let stand_in = bin.join("claude.exe");
            std::fs::copy(
                PathBuf::from(system_root).join(r"System32\cmd.exe"),
                &stand_in,
            )
            .unwrap();
            format!("\"{}\" /q /k", stand_in.display())
        } else {
            let stand_in = bin.join("claude");
            std::fs::write(
                &stand_in,
                "#!/bin/sh\nexport HERDR_AGENT=claude\nexec /bin/sh\n",
            )
            .unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&stand_in, std::fs::Permissions::from_mode(0o755))
                    .unwrap();
            }
            format!("'{}'", stand_in.display())
        };
        self.cli_json(&["pane", "run", pane, &command]);
        // Detection only, no report yet: the pane's own agent must be Claude.
        let identified = wait_until(Duration::from_secs(10), || {
            let output = self.cli(&["agent", "get", pane]);
            output.status.success()
                && serde_json::from_slice::<serde_json::Value>(&output.stdout)
                    .is_ok_and(|got| got["result"]["agent"]["agent"] == "claude")
        });
        assert!(identified, "stand-in claude was not identified in {pane}");
    }

    /// Report agent state the way an integration hook does.
    fn report(&mut self, pane: &str, agent: &str, state: &str) {
        self.seq += 1;
        let seq = self.seq.to_string();
        self.cli_json(&[
            "pane",
            "report-agent",
            pane,
            "--source",
            "custom:supervisor-fixture",
            "--agent",
            agent,
            "--state",
            state,
            "--seq",
            &seq,
        ]);
    }

    fn agent_status(&self, pane: &str) -> serde_json::Value {
        let waited = self.cli_json(&[
            "agent",
            "wait",
            pane,
            "--until",
            "idle",
            "--until",
            "working",
            "--until",
            "blocked",
            "--until",
            "done",
            "--timeout",
            "5000",
        ]);
        waited["result"]["agent"].clone()
    }
}

impl Fixture {
    /// Raw `ping` over the fixture's own socket. No CLI prints the
    /// `supervisor_v1` capability (`herdr status` omits it), so this sends the
    /// same wire request the CLI's status probe sends.
    fn ping(&self) -> serde_json::Value {
        let socket_path = self.socket_path.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        // Connect, write and read all block; bound the whole exchange.
        thread::spawn(move || {
            use interprocess::local_socket::{prelude::*, Stream};
            use std::io::{BufRead, BufReader, Write};

            #[cfg(unix)]
            let name = {
                use interprocess::local_socket::GenericFilePath;
                socket_path.as_path().to_fs_name::<GenericFilePath>()
            };
            #[cfg(windows)]
            let name = {
                use interprocess::local_socket::GenericNamespaced;
                socket_path
                    .to_string_lossy()
                    .to_string()
                    .to_ns_name::<GenericNamespaced>()
            };
            let reply = (|| -> std::io::Result<String> {
                let mut stream = Stream::connect(name?)?;
                stream.write_all(
                    b"{\"id\":\"fixture:ping\",\"method\":\"ping\",\"params\":{}}
",
                )?;
                stream.flush()?;
                let mut line = String::new();
                BufReader::new(stream).read_line(&mut line)?;
                Ok(line)
            })();
            let _ = tx.send(reply);
        });
        let line = match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(line)) => line,
            Ok(Err(err)) => panic!("ping exchange failed: {err}"),
            Err(_) => panic!("ping exchange timed out after 10s"),
        };
        serde_json::from_str(&line).unwrap_or_else(|err| panic!("ping reply ({err}): {line}"))
    }

    /// `herdr agent digest`, parsed. Errors print on stderr with exit 1.
    fn digest(&self) -> serde_json::Value {
        self.cli_json(&["agent", "digest"])["result"].clone()
    }

    fn digest_entry(digest: &serde_json::Value, pane: &str) -> Option<serde_json::Value> {
        digest["agents"].as_array().and_then(|agents| {
            agents
                .iter()
                .find(|agent| agent["target_ref"]["pane_id"] == pane)
                .cloned()
        })
    }

    /// The digest entry for `pane`.
    fn digest_agent(&self, pane: &str) -> serde_json::Value {
        let digest = self.digest();
        Self::digest_entry(&digest, pane)
            .unwrap_or_else(|| panic!("pane {pane} missing from digest: {digest}"))
    }

    /// Poll the digest entry for `pane` until `done` accepts it.
    fn digest_agent_until(
        &self,
        pane: &str,
        what: &str,
        done: impl Fn(&serde_json::Value) -> bool,
    ) -> serde_json::Value {
        let mut last = serde_json::Value::Null;
        let ok = wait_until(Duration::from_secs(10), || {
            let digest = self.digest();
            if let Some(agent) = Self::digest_entry(&digest, pane) {
                last = agent.clone();
                return done(&agent);
            }
            false
        });
        assert!(ok, "digest never showed {what}: {last}");
        last
    }

    /// Print a block of UTF-8 lines on the pane's screen exactly as written
    /// (the console code page would mangle `echo` of box-drawing glyphs), and
    /// wait for the ASCII `marker` that the block ends with.
    fn show_block(&self, pane: &str, name: &str, lines: &[&str], marker: &str) {
        let file = self.base.join(name);
        std::fs::write(&file, format!("{}\n", lines.join("\n"))).unwrap();
        let command = if cfg!(windows) {
            // Its own command first: the code page must be in force before
            // `type` starts, or the glyph bytes are decoded as cp437.
            self.run_and_wait(pane, "chcp 65001", "Active code page: 65001");
            format!("type \"{}\"", file.display())
        } else {
            format!("cat '{}'", file.display())
        };
        self.run_and_wait(pane, &command, marker);
    }

    /// Report a hook-supplied `background_jobs` count for the pane's agent.
    fn report_background_jobs(&mut self, pane: &str, jobs: u32) {
        self.seq += 1;
        let seq = self.seq.to_string();
        let token = format!("background_jobs={jobs}");
        self.cli_json(&[
            "pane",
            "report-metadata",
            pane,
            "--source",
            "custom:supervisor-fixture",
            "--agent",
            "claude",
            "--token",
            &token,
            "--seq",
            &seq,
        ]);
    }

    /// Show a prose question followed by a chrome block shaped like the real
    /// agent layout: rule, prompt row, rule, footer row.
    fn show_question_with_chrome(&self, pane: &str, question: &str, marker: &str) {
        let rule = "\u{2500}".repeat(40);
        self.show_block(
            pane,
            "question-block.txt",
            &[question, &rule, "\u{276f} ", &rule, marker],
            marker,
        );
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Bounded: a hung CLI must not hang the test process in cleanup.
        if let Ok(mut stop) = self
            .command(&["server", "stop"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            if !wait_until(Duration::from_secs(10), || {
                matches!(stop.try_wait(), Ok(Some(_)))
            }) {
                let _ = stop.kill();
                let _ = stop.wait();
            }
        }
        if let Some(mut server) = self.server.take() {
            if !wait_until(Duration::from_secs(5), || {
                matches!(server.try_wait(), Ok(Some(_)))
            }) {
                let _ = server.kill();
                let _ = server.wait();
            }
        }
        // Pane processes may hold files in `base` briefly after the server exits.
        let removed = wait_until(Duration::from_secs(5), || {
            std::fs::remove_dir_all(&self.base).is_ok() || !self.base.exists()
        });
        if !removed {
            eprintln!(
                "supervisor fixture: could not remove {}",
                self.base.display()
            );
        }
    }
}

fn wait_until(timeout: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if done() {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    false
}

/// Every object key anywhere in `value`, lowercased.
fn all_keys(value: &serde_json::Value, keys: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map {
                keys.push(key.to_ascii_lowercase());
                all_keys(child, keys);
            }
        }
        serde_json::Value::Array(items) => items.iter().for_each(|item| all_keys(item, keys)),
        _ => {}
    }
}

fn assert_no_future_facts(value: &serde_json::Value) {
    let mut keys = Vec::new();
    all_keys(value, &mut keys);
    let found: Vec<_> = keys
        .iter()
        .filter(|key| !EXISTING_KEYS.contains(&key.as_str()))
        .filter(|key| FUTURE_FACT_KEYS.iter().any(|fact| key.contains(fact)))
        .collect();
    assert!(
        found.is_empty(),
        "status facts appeared ({found:?}); flip this characterization test: {value}"
    );
}

/// Incident: the agent asked a prose question ("Should I start the brief?")
/// and herdr showed it as idle. Today there is no awaiting-human fact, so the
/// only status available is the plain agent state.
#[test]
fn current_prose_question_reports_plain_idle() {
    let mut fixture = Fixture::start();
    let pane = fixture.pane();
    fixture.report(&pane, "claude", "working");
    fixture.show(&pane, "Should I start the brief?");
    fixture.report(&pane, "claude", "idle");

    let agent = fixture.agent_status(&pane);

    assert_eq!(agent["agent"], "claude");
    assert_eq!(agent["agent_status"], "idle");
    assert_no_future_facts(&agent);
}

/// Incident: the foreground turn finished while a background shell was still
/// running, and herdr showed the session as finished. Today background work
/// is not a separate fact.
#[test]
fn current_background_job_is_not_a_separate_fact() {
    let mut fixture = Fixture::start();
    let pane = fixture.pane();
    fixture.report(&pane, "claude", "working");
    fixture.start_background_job(&pane);
    fixture.show(&pane, "1 shell still running");
    fixture.report(&pane, "claude", "idle");

    let agent = fixture.agent_status(&pane);

    assert_eq!(agent["agent_status"], "idle");
    assert_no_future_facts(&agent);
}

/// Incident: `agent prompt --wait` exited 1 after its timeout even though the
/// prompt had been delivered and the agent had started working. Today the
/// failure carries no delivery fact.
#[test]
fn current_prompt_wait_timeout_hides_delivery() {
    let mut fixture = Fixture::start();
    let pane = fixture.pane();
    fixture.launch_stand_in_claude(&pane);
    fixture.report(&pane, "claude", "idle");

    let prompt = fixture
        .command(&[
            "agent",
            "prompt",
            &pane,
            "cover the 429 retry path",
            "--wait",
            "--until",
            "idle",
            "--timeout",
            "8000",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    // The prompt text reaches the pane, then the agent starts working and
    // keeps working past the caller's timeout. The delivery wait is shorter
    // than the prompt timeout, so "working" is always reported before it.
    let delivered = fixture.cli(&[
        "pane",
        "wait-output",
        &pane,
        "--match",
        "cover the 429 retry path",
        "--timeout",
        "4000",
    ]);
    if !delivered.status.success() {
        let output = prompt.wait_with_output().unwrap();
        panic!(
            "prompt text never reached the pane; agent prompt exited {:?}: {}{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        );
    }
    fixture.report(&pane, "claude", "working");

    let output = prompt.wait_with_output().unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(
        output.stdout.iter().all(u8::is_ascii_whitespace),
        "unexpected stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let error: serde_json::Value = serde_json::from_slice(&output.stderr)
        .unwrap_or_else(|_| panic!("stderr: {}", String::from_utf8_lossy(&output.stderr)));
    assert_eq!(error["error"]["code"], "timeout", "{error}");
    assert_no_future_facts(&error);
    assert_eq!(fixture.agent_status(&pane)["agent_status"], "working");
}

/// `isolate()` must point every child, server included, at the fixture's own
/// state dir. The headless server writes nothing under the state dir in a
/// short run (sessions and logs live under the config dir), so the write is
/// forced through `plugin config-dir`, which creates the plugin state dir via
/// `state_dir()` in a CLI child. A leak to the platform state dir would leave
/// `<base>/state` empty; only the fixture's own paths are inspected.
#[test]
fn fixture_state_dir_is_scoped() {
    let fixture = Fixture::start();

    let output = fixture.cli(&["plugin", "config-dir", "fixture.state-scope"]);
    assert!(
        output.status.success(),
        "plugin config-dir failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let scoped = fixture.base.join("state").join(app_dir_name());
    let plugin_state = scoped.join("plugins");
    let created = std::fs::read_dir(&plugin_state)
        .map(|mut entries| entries.next().is_some())
        .unwrap_or(false);
    assert!(
        created,
        "no plugin state under {} (state dir not scoped to the fixture)",
        plugin_state.display()
    );
}

#[test]
fn digest_disabled_by_default() {
    let fixture = Fixture::start();

    let pong = fixture.ping();
    assert_eq!(pong["result"]["type"], "pong", "{pong}");
    assert_eq!(pong["result"]["capabilities"]["supervisor_v1"], false);

    let output = fixture.cli(&["agent", "digest"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.iter().all(u8::is_ascii_whitespace));
    let error: serde_json::Value = serde_json::from_slice(&output.stderr)
        .unwrap_or_else(|_| panic!("stderr: {}", String::from_utf8_lossy(&output.stderr)));
    assert_eq!(error["error"]["code"], "supervisor_disabled", "{error}");
}

#[test]
fn supervisor_v1_advertised_when_enabled() {
    let fixture = Fixture::start_with_supervisor();

    let pong = fixture.ping();
    assert_eq!(pong["result"]["type"], "pong", "{pong}");
    assert_eq!(pong["result"]["capabilities"]["supervisor_v1"], true);

    let digest = fixture.digest();
    assert_eq!(digest["type"], "agents_digest", "{digest}");
}

fn has_open_question(agent: &serde_json::Value) -> bool {
    agent["pending_questions"]
        .as_array()
        .is_some_and(|questions| !questions.is_empty())
}

#[test]
fn t1a_prose_question_is_awaiting_human() {
    let mut fixture = Fixture::start_with_supervisor();
    let pane = fixture.pane();
    fixture.report(&pane, "claude", "working");
    fixture.show_question_with_chrome(&pane, "Should I start the brief?", "footer-t1a");
    fixture.report(&pane, "claude", "idle");

    let agent = fixture.digest_agent_until(&pane, "an open question", has_open_question);

    assert_eq!(agent["foreground"]["value"], "awaiting_human", "{agent}");
    assert_eq!(agent["foreground"]["source"], "screen_heuristic");
    assert_eq!(agent["foreground"]["confidence"], "low");
    assert_eq!(agent["completion"], "incomplete");
    assert_eq!(agent["blocking_reason"], "awaiting_human");
    let questions = agent["pending_questions"].as_array().unwrap();
    assert_eq!(questions.len(), 1, "{agent}");
    assert_eq!(questions[0]["kind"], "prose_question");
    assert_eq!(questions[0]["excerpt"], "Should I start the brief?");
    assert!(questions[0].get("resolution").is_none_or(|r| r.is_null()));
    let id = questions[0]["question_id"].clone();
    assert!(id.is_string(), "{agent}");

    let again = fixture.digest_agent(&pane);
    assert_eq!(again["pending_questions"][0]["question_id"], id, "{again}");
}

#[test]
fn t1a_question_survives_next_turn() {
    let mut fixture = Fixture::start_with_supervisor();
    let pane = fixture.pane();
    fixture.report(&pane, "claude", "working");
    fixture.show_question_with_chrome(&pane, "Should I start the brief?", "footer-t1a-survive");
    fixture.report(&pane, "claude", "idle");
    let asked = fixture.digest_agent_until(&pane, "an open question", has_open_question);
    let id = asked["pending_questions"][0]["question_id"].clone();

    fixture.report(&pane, "claude", "working");
    fixture.show(&pane, "next-turn-output");
    fixture.report(&pane, "claude", "idle");
    fixture.report_background_jobs(&pane, 0);

    let agent = fixture.digest_agent_until(&pane, "background_jobs=0", |agent| {
        agent["background"]["value"] == "inactive"
    });
    let questions = agent["pending_questions"].as_array().unwrap();
    assert_eq!(questions.len(), 1, "{agent}");
    assert_eq!(questions[0]["question_id"], id, "{agent}");
    assert!(questions[0].get("resolution").is_none_or(|r| r.is_null()));
    assert_eq!(agent["completion"], "incomplete", "{agent}");
}

#[test]
fn t1b_idle_with_background_unknown_is_incomplete() {
    let mut fixture = Fixture::start_with_supervisor();
    let pane = fixture.pane();
    fixture.report(&pane, "claude", "working");
    fixture.show(&pane, "turn-finished");
    fixture.report(&pane, "claude", "idle");

    let agent = fixture.digest_agent_until(&pane, "idle foreground", |agent| {
        agent["foreground"]["value"] == "idle"
    });
    assert_eq!(agent["background"]["value"], "unknown", "{agent}");
    assert_eq!(agent["completion"], "incomplete", "{agent}");
    assert_eq!(agent["blocking_reason"], "background_unknown", "{agent}");

    fixture.report_background_jobs(&pane, 1);
    let agent = fixture.digest_agent_until(&pane, "active background", |agent| {
        agent["background"]["value"] == "active"
    });
    assert_eq!(agent["background"]["source"], "hook", "{agent}");
    assert_eq!(agent["background"]["confidence"], "high", "{agent}");
    assert_eq!(agent["background_jobs"], 1, "{agent}");
    assert_eq!(agent["completion"], "incomplete", "{agent}");
    assert_eq!(agent["blocking_reason"], "background_active", "{agent}");
}

#[test]
fn t4b_pane_move_bumps_generation() {
    let mut fixture = Fixture::start_with_supervisor();
    let pane = fixture.pane();
    fixture.report(&pane, "claude", "working");
    fixture.report(&pane, "claude", "idle");
    let before = fixture.digest_agent(&pane);
    let generation = before["target_ref"]["generation"].as_u64().unwrap();

    fixture.cli_json(&["pane", "move", &pane, "--new-tab"]);

    let after = fixture.digest_agent_until(&pane, "a bumped generation", |agent| {
        agent["target_ref"]["generation"].as_u64() == Some(generation + 1)
    });
    let lineage = after["lineage"].as_array().unwrap();
    assert_eq!(lineage.last().unwrap()["reason"], "pane_moved", "{after}");
}

/// Covers the spawn-path `on_read` stamp only. The pane handoff `on_read`
/// hook (src/pane.rs handoff constructor) is NOT exercised here because the
/// fixture performs no live handoff; it stays inspection-only.
#[test]
fn q1_output_stamps_last_output_at() {
    let mut fixture = Fixture::start_with_supervisor();
    let pane = fixture.pane();
    fixture.report(&pane, "claude", "working");

    // (1) Settle: the stamp is unchanged across 3 consecutive digests, so
    // late shell-startup output cannot be mistaken for our marker.
    let mut last_seen: Option<u64> = None;
    let mut stable = 0;
    let settled = wait_until(Duration::from_secs(15), || {
        let stamp = fixture.digest_agent(&pane)["last_output_at_ms"].as_u64();
        if stamp.is_some_and(|ms| ms > 0) && stamp == last_seen {
            stable += 1;
        } else {
            stable = 0;
            last_seen = stamp;
        }
        stable >= 2
    });
    assert!(settled, "last_output_at_ms never settled: {last_seen:?}");

    // (2) T0 is the digest clock after settling; wait for the clock to pass it.
    let t0 = fixture.digest()["generated_at_ms"].as_u64().unwrap();
    let clock_passed = wait_until(Duration::from_secs(10), || {
        fixture.digest()["generated_at_ms"]
            .as_u64()
            .is_some_and(|now| now > t0)
    });
    assert!(clock_passed, "digest clock never passed {t0}");

    // (3) Fresh unique output, (4) a stamp strictly after T0 must follow.
    fixture.show(&pane, "q1-fresh-output-marker");
    let newer = fixture.digest_agent_until(&pane, "last_output_at_ms after T0", |agent| {
        agent["last_output_at_ms"]
            .as_u64()
            .is_some_and(|ms| ms > t0)
    });
    assert!(newer["last_output_at_ms"].as_u64().unwrap() > t0);
}
