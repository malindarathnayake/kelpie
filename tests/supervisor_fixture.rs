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

    /// Supervisor server with extra environment for the server process only
    /// (e.g. the debug-build `HERDR_DEV_SUBMIT_HOLD_MS` knob).
    fn start_with_supervisor_env(server_env: &[(&str, &str)]) -> Self {
        Self::start_with_config_env("[supervisor]\nenabled = true\n", server_env)
    }

    fn start_with_config(extra_config: &str) -> Self {
        Self::start_with_config_env(extra_config, &[])
    }

    fn start_with_config_env(extra_config: &str, server_env: &[(&str, &str)]) -> Self {
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
        command.envs(server_env.iter().copied());
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
        let mut args = vec![
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
        ];
        // A zero needs a live reporter TTL to pass the freshness fence; the digest still
        // reads it Unknown while C18b is open (no reporter correlation).
        if jobs == 0 {
            args.extend(["--ttl-ms", "600000"]);
        }
        self.cli_json(&args);
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

    // The server log lives under this fixture's own config dir (data_dir() =
    // $XDG_CONFIG_HOME/<app dir>), and its startup line names this fixture's
    // spawned server. Only this path is read; no other log is touched.
    let server_pid = fixture.server.as_ref().expect("server child").id();
    let log_path = fixture
        .config_home
        .join(app_dir_name())
        .join("herdr-server.log");
    assert!(log_path.starts_with(&fixture.base));
    let mut startup_pids = Vec::new();
    let logged = wait_until(Duration::from_secs(10), || {
        let Ok(log) = std::fs::read_to_string(&log_path) else {
            return false;
        };
        startup_pids = log
            .lines()
            .filter(|line| line.contains("app.startup") && line.contains("server"))
            .filter_map(|line| {
                let rest = &line[line.find(" pid=")? + 5..];
                let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
                digits.parse::<u32>().ok()
            })
            .collect();
        !startup_pids.is_empty()
    });
    assert!(
        logged,
        "no server startup line in {} (server log not scoped to the fixture)",
        log_path.display()
    );
    assert_eq!(
        startup_pids,
        vec![server_pid],
        "startup line pid in {} is not the fixture's server",
        log_path.display()
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

    // An uncorrelated zero reads Unknown (C18b open); the open question is kept.
    let agent = fixture.digest_agent_until(&pane, "idle after the next turn", |agent| {
        agent["agent_status"] == "idle"
    });
    let questions = agent["pending_questions"].as_array().unwrap();
    assert_eq!(questions.len(), 1, "{agent}");
    assert_eq!(questions[0]["question_id"], id, "{agent}");
    assert!(questions[0].get("resolution").is_none_or(|r| r.is_null()));
    assert_eq!(agent["background"]["value"], "unknown", "{agent}");
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

// ---------------------------------------------------------------------------
// Local slice S6: `agent watch`, `agent submit`, `agent submit-status`.
// ---------------------------------------------------------------------------

/// Debug-build hold applied to every accepted submission by the fence tests.
const SUBMIT_HOLD_MS: u64 = 3000;
/// Paste-to-Enter delay the server uses after the submission text.
const SUBMIT_ENTER_DELAY_MS: u64 = 300;

/// A harmless line for the stand-in shell: a comment, so executing it prints
/// nothing and the marker can appear on screen only as the echoed input.
fn comment_line(marker: &str) -> String {
    if cfg!(windows) {
        format!("rem {marker}")
    } else {
        format!("# {marker}")
    }
}

/// `<server_epoch>:<pane_id>:<generation>` from a digest entry.
fn target_ref_arg(agent: &serde_json::Value) -> String {
    let target = &agent["target_ref"];
    format!(
        "{}:{}:{}",
        target["server_epoch"].as_str().unwrap(),
        target["pane_id"].as_str().unwrap(),
        target["generation"].as_u64().unwrap()
    )
}

/// The receipt inside a submit/submit-status success response. Every receipt
/// must say the agent's acknowledgement is unsupported (S6h) and must never
/// carry the submitted text.
fn receipt_of(response: &serde_json::Value, text: &str) -> serde_json::Value {
    assert_eq!(
        response["result"]["type"], "agent_submit_receipt",
        "{response}"
    );
    let receipt = response["result"]["receipt"].clone();
    assert_eq!(receipt["acknowledged"], "unsupported", "{receipt}");
    assert!(
        !receipt.to_string().contains(text),
        "receipt leaked the submitted text: {receipt}"
    );
    receipt
}

impl Fixture {
    /// Create a workspace, focus it, and return its root pane id, so an
    /// attached client's default view is this pane's tab.
    fn focused_pane(&self) -> String {
        let created = self.cli_json(&[
            "workspace",
            "create",
            "--cwd",
            self.base.to_str().unwrap(),
            "--focus",
        ]);
        created["result"]["root_pane"]["pane_id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// Show a Claude-shaped prompt box (rule / input row / rule) with `marker`
    /// as its footer row.
    fn show_prompt_box(&self, pane: &str, name: &str, input_row: &str, marker: &str) {
        let rule = "\u{2500}".repeat(40);
        self.show_block(
            pane,
            name,
            &["w2 box above", &rule, input_row, &rule, marker],
            marker,
        );
    }

    /// Stand-in Claude reported idle with an empty prompt box on screen; returns
    /// the digest entry once the derived draft reads clean.
    fn ready_clean_claude(&mut self, pane: &str, tag: &str) -> serde_json::Value {
        self.launch_stand_in_claude(pane);
        self.report(pane, "claude", "idle");
        self.show_prompt_box(
            pane,
            &format!("{tag}-box.txt"),
            "\u{276f} ",
            &format!("footer-{tag}"),
        );
        self.digest_agent_until(pane, "a clean draft", |agent| {
            agent["owner_input"]["draft"] == "clean"
        })
    }

    /// The pane's recent text, unwrapped so a marker is never split.
    fn screen_text(&self, pane: &str) -> String {
        let output = self.cli(&[
            "pane",
            "read",
            pane,
            "--source",
            "recent-unwrapped",
            "--lines",
            "500",
        ]);
        assert!(
            output.status.success(),
            "pane read failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn screen_count(&self, pane: &str, needle: &str) -> usize {
        self.screen_text(pane).matches(needle).count()
    }

    fn wait_output(&self, pane: &str, marker: &str) -> bool {
        self.cli(&[
            "pane",
            "wait-output",
            pane,
            "--match",
            marker,
            "--timeout",
            "10000",
        ])
        .status
        .success()
    }

    /// A CLI call that must fail with exit 1 and a JSON error on stderr.
    fn cli_error(&self, args: &[&str]) -> serde_json::Value {
        let output = self.cli(args);
        assert_eq!(
            output.status.code(),
            Some(1),
            "herdr {} should fail: {}{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stdout.iter().all(u8::is_ascii_whitespace));
        serde_json::from_slice(&output.stderr)
            .unwrap_or_else(|_| panic!("stderr: {}", String::from_utf8_lossy(&output.stderr)))
    }

    fn submit_args<'a>(target_ref: &'a str, submit_id: &'a str, text: &'a str) -> [&'a str; 7] {
        [
            "agent",
            "submit",
            "--target-ref",
            target_ref,
            "--submit-id",
            submit_id,
            text,
        ]
    }

    fn submit_ok(&self, target_ref: &str, submit_id: &str, text: &str) -> serde_json::Value {
        let response = self.cli_json(&Self::submit_args(target_ref, submit_id, text));
        receipt_of(&response, text)
    }

    fn submit_error(&self, target_ref: &str, submit_id: &str, text: &str) -> serde_json::Value {
        let error = self.cli_error(&Self::submit_args(target_ref, submit_id, text));
        assert!(
            !error.to_string().contains(text),
            "error leaked the submitted text: {error}"
        );
        error
    }

    fn submit_status(&self, submit_id: &str, text: &str) -> serde_json::Value {
        let response = self.cli_json(&["agent", "submit-status", submit_id]);
        receipt_of(&response, text)
    }

    /// Poll `agent submit-status` until `done` accepts the receipt.
    fn wait_receipt(
        &self,
        submit_id: &str,
        text: &str,
        what: &str,
        done: impl Fn(&serde_json::Value) -> bool,
    ) -> serde_json::Value {
        let mut last = serde_json::Value::Null;
        let ok = wait_until(Duration::from_secs(10), || {
            last = self.submit_status(submit_id, text);
            done(&last)
        });
        assert!(ok, "receipt never showed {what}: {last}");
        last
    }

    /// Stop the fixture server and wait for its process to exit.
    fn stop_server(&mut self) {
        let stopped = self.cli(&["server", "stop"]);
        assert!(
            stopped.status.success(),
            "server stop failed: {}",
            String::from_utf8_lossy(&stopped.stderr)
        );
        if let Some(mut server) = self.server.take() {
            let exited = wait_until(Duration::from_secs(10), || {
                matches!(server.try_wait(), Ok(Some(_)))
            });
            if !exited {
                let _ = server.kill();
                let _ = server.wait();
            }
        }
    }

    /// `herdr agent watch <args>` as a child; its stdout lines arrive parsed.
    fn spawn_watch(&self, args: &[&str]) -> (Child, std::sync::mpsc::Receiver<serde_json::Value>) {
        use std::io::{BufRead, BufReader};

        let mut command_args = vec!["agent", "watch"];
        command_args.extend_from_slice(args);
        let mut child = self
            .command(&command_args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let value = serde_json::from_str(&line)
                    .unwrap_or_else(|_| serde_json::json!({ "unparsed": line }));
                if tx.send(value).is_err() {
                    break;
                }
            }
        });
        (child, rx)
    }
}

/// Wait for `child` to exit; kill it and fail if it outlives `timeout`.
fn wait_child_exit(child: &mut Child, timeout: Duration) -> std::process::ExitStatus {
    let mut status = None;
    let exited = wait_until(timeout, || {
        status = child.try_wait().ok().flatten();
        status.is_some()
    });
    if !exited {
        let _ = child.kill();
        let _ = child.wait();
        panic!("child did not exit within {timeout:?}");
    }
    status.unwrap()
}

/// Receive watch lines until `done` accepts one; returns everything received.
fn recv_watch_until(
    rx: &std::sync::mpsc::Receiver<serde_json::Value>,
    lines: &mut Vec<serde_json::Value>,
    what: &str,
    done: impl Fn(&serde_json::Value) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(line) => {
                let matched = done(&line);
                lines.push(line);
                if matched {
                    return;
                }
            }
            Err(_) => panic!("watch never printed {what}; got {lines:?}"),
        }
    }
}

/// S4/S6a: watch prints the initial state and then one line per bounded
/// change, and exits 0 once `--max-changes` lines are out.
#[test]
fn s6a_watch_prints_bounded_changes_then_exits() {
    const MAX_CHANGES: usize = 3;
    let mut fixture = Fixture::start_with_supervisor();
    let pane = fixture.pane();
    fixture.report(&pane, "claude", "working");
    fixture.digest_agent_until(&pane, "working", |agent| agent["agent_status"] == "working");

    let (mut watch, rx) = fixture.spawn_watch(&[
        "--pane",
        &pane,
        "--interval-ms",
        "100",
        "--max-changes",
        "3",
        "--timeout-ms",
        "30000",
    ]);
    let mut lines = Vec::new();
    recv_watch_until(&rx, &mut lines, "the initial state", |_| true);
    assert_eq!(lines[0]["agent_status"], "working", "{lines:?}");

    fixture.report(&pane, "claude", "idle");
    recv_watch_until(&rx, &mut lines, "the idle change", |line| {
        line["agent_status"] == "idle"
    });
    if lines.len() < MAX_CHANGES {
        fixture.report(&pane, "claude", "working");
        while lines.len() < MAX_CHANGES {
            recv_watch_until(&rx, &mut lines, "the last change", |_| true);
        }
    }

    let status = wait_child_exit(&mut watch, Duration::from_secs(10));
    assert_eq!(status.code(), Some(0), "{lines:?}");
    // Nothing after the bound: the reader thread hits EOF once the child exits.
    lines.extend(rx.iter());
    assert_eq!(lines.len(), MAX_CHANGES, "{lines:?}");
    let mut last_seq = 0;
    for line in &lines {
        assert_eq!(line["type"], "change", "{line}");
        assert_eq!(line["pane_id"], pane.as_str(), "{line}");
        assert_eq!(line["target_ref"]["pane_id"], pane.as_str(), "{line}");
        let seq = line["as_of_seq"].as_u64().expect("as_of_seq");
        assert!(seq >= last_seq, "as_of_seq went backwards: {lines:?}");
        last_seq = seq;
        for key in ["foreground", "background", "completion", "draft"] {
            assert!(line.get(key).is_some(), "missing {key}: {line}");
        }
    }
}

/// S4/S6a: a request failure prints one `stale` line per outage and polling
/// continues until `--timeout-ms`, which ends with a `timeout` line and exit 0.
#[test]
fn s6a_watch_reports_stale_when_server_stops() {
    let mut fixture = Fixture::start_with_supervisor();
    let pane = fixture.pane();
    fixture.report(&pane, "claude", "working");
    fixture.digest_agent_until(&pane, "working", |agent| agent["agent_status"] == "working");

    let (mut watch, rx) = fixture.spawn_watch(&["--interval-ms", "100", "--timeout-ms", "15000"]);
    let mut lines = Vec::new();
    recv_watch_until(&rx, &mut lines, "the initial state", |line| {
        line["type"] == "change"
    });

    fixture.stop_server();
    recv_watch_until(&rx, &mut lines, "a stale line", |line| {
        line["type"] == "stale"
    });
    let stale = lines.last().unwrap().clone();
    assert!(stale["last_as_of_seq"].is_u64(), "{stale}");
    assert!(stale["last_ok_ms"].is_u64(), "{stale}");
    let reason = stale["reason"].as_str().expect("reason");
    assert!(
        !reason.is_empty() && reason.chars().count() <= 160,
        "{stale}"
    );

    let status = wait_child_exit(&mut watch, Duration::from_secs(25));
    assert_eq!(status.code(), Some(0), "{lines:?}");
    lines.extend(rx.iter());
    assert_eq!(
        lines.iter().filter(|line| line["type"] == "stale").count(),
        1,
        "one stale line per outage: {lines:?}"
    );
    assert_eq!(lines.last().unwrap()["type"], "timeout", "{lines:?}");
}

/// S5/S6b+c+h: one submit is written exactly once, its receipt is readable by
/// id without waiting for the turn to finish, and a retry with the same id
/// returns the same receipt without a second delivery. A later Working report
/// shows in the digest but never moves the receipt past `written`.
#[test]
fn s6b_submit_once_then_retry_writes_once() {
    let mut fixture = Fixture::start_with_supervisor();
    let pane = fixture.pane();
    let agent = fixture.ready_clean_claude(&pane, "s6b");
    let target = target_ref_arg(&agent);
    let marker = "w2-submit-once-marker";
    let text = comment_line(marker);

    let accepted = fixture.submit_ok(&target, "s6b-x", &text);
    assert!(
        matches!(accepted["state"].as_str(), Some("accepted" | "written")),
        "{accepted}"
    );
    assert_eq!(accepted["submit_id"], "s6b-x");
    assert_eq!(accepted["target_ref"], agent["target_ref"], "{accepted}");
    let accepted_at = accepted["accepted_at_ms"].as_u64().expect("accepted_at_ms");

    // No completion or idle wait: the receipt reaches written on its own.
    let written = fixture.wait_receipt("s6b-x", &text, "written", |receipt| {
        receipt["state"] == "written"
    });
    assert!(written["written_at_ms"].is_u64(), "{written}");
    assert_eq!(written["accepted_at_ms"], accepted_at, "{written}");
    assert!(
        fixture.wait_output(&pane, marker),
        "submitted text never shown"
    );
    assert_eq!(fixture.screen_count(&pane, marker), 1);

    fixture.report(&pane, "claude", "working");
    fixture.digest_agent_until(&pane, "working after the write", |agent| {
        agent["agent_status"] == "working"
    });
    let after_working = fixture.submit_status("s6b-x", &text);
    assert_eq!(after_working["state"], "written", "{after_working}");
    assert!(
        after_working.get("started_at_ms").is_none(),
        "{after_working}"
    );
    assert_eq!(
        after_working["acknowledged"], "unsupported",
        "{after_working}"
    );

    let again = fixture.submit_ok(&target, "s6b-x", &text);
    assert_eq!(again["submit_id"], "s6b-x");
    assert_eq!(again["accepted_at_ms"], accepted_at, "{again}");
    assert_eq!(
        again["text_sha256_16"], written["text_sha256_16"],
        "{again}"
    );
    thread::sleep(Duration::from_millis(SUBMIT_ENTER_DELAY_MS + 1200));
    assert_eq!(
        fixture.screen_count(&pane, marker),
        1,
        "retry delivered the text again"
    );
}

/// S6d(i): after a generation bump the old full target_ref is stale and
/// nothing is written.
#[test]
fn s6d_stale_generation_is_rejected_without_write() {
    let mut fixture = Fixture::start_with_supervisor();
    let pane = fixture.pane();
    let agent = fixture.ready_clean_claude(&pane, "s6d");
    let old_target = target_ref_arg(&agent);
    let generation = agent["target_ref"]["generation"].as_u64().unwrap();

    fixture.cli_json(&["pane", "move", &pane, "--new-tab"]);
    fixture.digest_agent_until(&pane, "a bumped generation", |agent| {
        agent["target_ref"]["generation"].as_u64() == Some(generation + 1)
    });

    let marker = "w2-stale-generation-marker";
    let text = comment_line(marker);
    let error = fixture.submit_error(&old_target, "s6d-x", &text);
    assert_eq!(error["error"]["code"], "stale_target", "{error}");
    let message = error["error"]["message"].as_str().unwrap();
    assert!(
        message.contains(&format!("current_generation={}", generation + 1)),
        "{error}"
    );
    thread::sleep(Duration::from_millis(SUBMIT_ENTER_DELAY_MS + 1000));
    assert_eq!(fixture.screen_count(&pane, marker), 0);
    let unknown = fixture.cli_error(&["agent", "submit-status", "s6d-x"]);
    assert_eq!(unknown["error"]["code"], "submit_not_found", "{unknown}");
}

/// S6d(ii): a server restart changes server_epoch, so a retry with the old
/// full target_ref is stale even against a comparable live pane; the same
/// pane and generation under the new epoch is accepted (positive control).
#[test]
fn s6d_server_restart_epoch_is_rejected_without_write() {
    let (old_target, old_epoch) = {
        let mut first = Fixture::start_with_supervisor();
        let pane = first.pane();
        let agent = first.ready_clean_claude(&pane, "s6d-old");
        let epoch = agent["target_ref"]["server_epoch"]
            .as_str()
            .unwrap()
            .to_string();
        (target_ref_arg(&agent), epoch)
        // `first` drops here: its server stops.
    };

    let mut fixture = Fixture::start_with_supervisor();
    let pane = fixture.pane();
    let agent = fixture.ready_clean_claude(&pane, "s6d-new");
    let new_epoch = agent["target_ref"]["server_epoch"].as_str().unwrap();
    assert_ne!(new_epoch, old_epoch, "restart kept the server epoch");

    let marker = "w2-stale-epoch-marker";
    let text = comment_line(marker);
    let verbatim = fixture.submit_error(&old_target, "s6d-old-ref", &text);
    assert_eq!(verbatim["error"]["code"], "stale_target", "{verbatim}");
    let swapped = format!(
        "{old_epoch}:{}:{}",
        agent["target_ref"]["pane_id"].as_str().unwrap(),
        agent["target_ref"]["generation"].as_u64().unwrap()
    );
    let epoch_only = fixture.submit_error(&swapped, "s6d-old-epoch", &text);
    assert_eq!(epoch_only["error"]["code"], "stale_target", "{epoch_only}");
    assert!(
        epoch_only["error"]["message"]
            .as_str()
            .unwrap()
            .contains("current_generation=none"),
        "{epoch_only}"
    );
    thread::sleep(Duration::from_millis(SUBMIT_ENTER_DELAY_MS + 1000));
    assert_eq!(fixture.screen_count(&pane, marker), 0);

    let control_marker = "w2-new-epoch-control";
    let control = fixture.submit_ok(
        &target_ref_arg(&agent),
        "s6d-new-ref",
        &comment_line(control_marker),
    );
    assert!(
        matches!(control["state"].as_str(), Some("accepted" | "written")),
        "{control}"
    );
}

/// S6e: a dirty draft and an unknown draft are both rejected before any write.
#[test]
fn s6e_dirty_and_unknown_drafts_are_rejected_without_write() {
    let mut fixture = Fixture::start_with_supervisor();
    let pane = fixture.pane();
    fixture.launch_stand_in_claude(&pane);
    fixture.report(&pane, "claude", "idle");
    fixture.show_prompt_box(
        &pane,
        "s6e-dirty-box.txt",
        "\u{276f} owner typed draft",
        "footer-s6e-dirty",
    );
    let dirty = fixture.digest_agent_until(&pane, "a dirty draft", |agent| {
        agent["owner_input"]["draft"] == "dirty"
    });

    let marker = "w2-draft-marker";
    let text = comment_line(marker);
    let error = fixture.submit_error(&target_ref_arg(&dirty), "s6e-dirty", &text);
    assert_eq!(error["error"]["code"], "draft_not_clean", "{error}");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("draft=dirty"),
        "{error}"
    );

    // Push the box out of reach: more than six text rows below its bottom rule.
    let rows: Vec<String> = (1..=8).map(|n| format!("w2 plain row {n}")).collect();
    let mut block: Vec<&str> = rows.iter().map(String::as_str).collect();
    block.push("footer-s6e-unknown");
    fixture.show_block(&pane, "s6e-plain.txt", &block, "footer-s6e-unknown");
    let unknown = fixture.digest_agent_until(&pane, "an unknown draft", |agent| {
        agent["owner_input"]["draft"] == "unknown"
    });
    let error = fixture.submit_error(&target_ref_arg(&unknown), "s6e-unknown", &text);
    assert_eq!(error["error"]["code"], "draft_not_clean", "{error}");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("draft=unknown"),
        "{error}"
    );

    thread::sleep(Duration::from_millis(SUBMIT_ENTER_DELAY_MS + 1000));
    assert_eq!(fixture.screen_count(&pane, marker), 0);
    for id in ["s6e-dirty", "s6e-unknown"] {
        let unknown = fixture.cli_error(&["agent", "submit-status", id]);
        assert_eq!(unknown["error"]["code"], "submit_not_found", "{unknown}");
    }
}

/// S6f: a generation change while the accepted submission is parked (debug
/// hold) fences it: zero bytes, receipt `fenced{generation_changed}`.
#[test]
fn s6f_generation_change_during_hold_fences_zero_bytes() {
    let hold = SUBMIT_HOLD_MS.to_string();
    let mut fixture = Fixture::start_with_supervisor_env(&[("HERDR_DEV_SUBMIT_HOLD_MS", &hold)]);
    let pane = fixture.pane();
    let agent = fixture.ready_clean_claude(&pane, "s6f");
    let marker = "w2-held-generation-marker";
    let text = comment_line(marker);

    let submitted_at = Instant::now();
    let accepted = fixture.submit_ok(&target_ref_arg(&agent), "s6f-x", &text);
    assert_eq!(accepted["state"], "accepted", "{accepted}");

    fixture.cli_json(&["pane", "move", &pane, "--new-tab"]);
    let fenced = fixture.wait_receipt("s6f-x", &text, "fenced", |receipt| {
        receipt["state"] == "fenced"
    });
    assert_eq!(fenced["reason"], "generation_changed", "{fenced}");
    assert!(fenced.get("written_at_ms").is_none_or(|v| v.is_null()));

    let settle = Duration::from_millis(SUBMIT_HOLD_MS + SUBMIT_ENTER_DELAY_MS + 1500);
    thread::sleep(settle.saturating_sub(submitted_at.elapsed()));
    assert_eq!(fixture.screen_count(&pane, marker), 0);
    assert_eq!(fixture.submit_status("s6f-x", &text)["state"], "fenced");
}

/// Speaks the client-shell endpoint protocol over the fixture's client socket
/// the way an attached `herdr` client does, so owner keys reach the server's
/// real attached-client pane-input path (`ClientShellPaneInput`, the one that
/// trips submit fences) rather than an API writer.
struct OwnerClient {
    frames: Option<std::sync::mpsc::Sender<Vec<u8>>>,
    worker: Option<thread::JoinHandle<()>>,
}

mod owner_wire {
    //! Minimal bincode-varint framing for the client-shell protocol, matching
    //! `tests/support` (unix-only) so it also runs over Windows named pipes.

    pub const CLIENT_DETACH: u32 = 4;
    pub const CLIENT_SHELL_PANE_INPUT: u32 = 13;
    pub const CLIENT_ENDPOINT_CONTROL: u32 = 20;
    pub const SERVER_ENDPOINT_CONTROL: u32 = 20;
    pub const SERVER_PANE_SURFACE: u32 = 13;
    const PANE_EVENT_KEY: u32 = 0;
    const PANE_EVENT_TEXT_COMMIT: u32 = 1;
    const KEY_CODE_ENTER: u32 = 1;
    const KEY_KIND_PRESS: u32 = 0;

    pub fn varint_u32(v: u32) -> Vec<u8> {
        if v < 251 {
            vec![v as u8]
        } else if v < 65536 {
            let mut buf = vec![251u8];
            buf.extend_from_slice(&(v as u16).to_le_bytes());
            buf
        } else {
            let mut buf = vec![252u8];
            buf.extend_from_slice(&v.to_le_bytes());
            buf
        }
    }

    pub fn string(value: &str) -> Vec<u8> {
        let mut out = varint_u32(value.len() as u32);
        out.extend_from_slice(value.as_bytes());
        out
    }

    pub fn frame(payload: &[u8]) -> Vec<u8> {
        let mut out = (payload.len() as u32).to_le_bytes().to_vec();
        out.extend_from_slice(payload);
        out
    }

    pub fn decode_varint(payload: &[u8], offset: &mut usize) -> Option<u32> {
        let first = *payload.get(*offset)?;
        let (value, used) = match first {
            0..=250 => (u32::from(first), 1),
            251 => (
                u32::from(u16::from_le_bytes(
                    payload.get(*offset + 1..*offset + 3)?.try_into().ok()?,
                )),
                3,
            ),
            252 => (
                u32::from_le_bytes(payload.get(*offset + 1..*offset + 5)?.try_into().ok()?),
                5,
            ),
            _ => return None,
        };
        *offset += used;
        Some(value)
    }

    pub fn decode_string(payload: &[u8], offset: &mut usize) -> Option<String> {
        let len = decode_varint(payload, offset)? as usize;
        let bytes = payload.get(*offset..*offset + len)?;
        *offset += len;
        String::from_utf8(bytes.to_vec()).ok()
    }

    /// Pops one complete `[u32 len][payload]` frame off the front of `buf`.
    pub fn take_frame(buf: &mut Vec<u8>) -> Option<Vec<u8>> {
        let len = u32::from_le_bytes(buf.get(..4)?.try_into().ok()?) as usize;
        let payload = buf.get(4..4 + len)?.to_vec();
        buf.drain(..4 + len);
        Some(payload)
    }

    pub fn hello(cols: u16, rows: u16) -> Vec<u8> {
        let data = serde_json::json!({
            "generation": 1,
            "cell_width_px": 8,
            "cell_height_px": 16,
            "surface_size": {"cols": cols, "rows": rows},
            "pixel_mouse": false,
            "direct_graphics": false,
            "endpoint_keybindings": false,
            "mouse_capture": false,
            "snapshot_codecs": ["shell.snapshot.v1"],
            "surface_codecs": ["shell.surface.v1"],
            "input_codecs": ["shell.input.semantic.v1"],
            "blob_codecs": ["shell.blob.v1"]
        })
        .to_string();
        let mut payload = varint_u32(CLIENT_ENDPOINT_CONTROL);
        payload.extend(string("endpoint.hello.v1"));
        payload.extend(string(&data));
        payload
    }

    /// `EndpointControl { "fork.supervisor.handoff.v1", pane_id }`: the message an
    /// attached client sends when the operator presses the `supervisor_handoff` key.
    pub fn handoff(pane_id: &str) -> Vec<u8> {
        let mut payload = varint_u32(CLIENT_ENDPOINT_CONTROL);
        payload.extend(string("fork.supervisor.handoff.v1"));
        payload.extend(string(pane_id));
        payload
    }

    /// `ClientShellPaneInput { pane_id, [TextCommit(text), Key(Enter)?] }`.
    pub fn pane_text(pane_id: &str, text: &str, enter: bool) -> Vec<u8> {
        let mut payload = varint_u32(CLIENT_SHELL_PANE_INPUT);
        payload.extend(string(pane_id));
        payload.extend(varint_u32(if enter { 2 } else { 1 }));
        payload.extend(varint_u32(PANE_EVENT_TEXT_COMMIT));
        payload.extend(string(text));
        if enter {
            payload.extend(varint_u32(PANE_EVENT_KEY));
            payload.extend(varint_u32(KEY_CODE_ENTER));
            payload.push(0); // no modifiers
            payload.extend(varint_u32(KEY_KIND_PRESS));
            payload.extend(varint_u32(1)); // repeat_count (u16 varint)
            payload.push(0); // no shifted codepoint
            payload.push(0); // no generated text
            payload.push(0); // does not track release
            payload.push(0); // no physical key id
            payload.push(0); // no Windows key record
        }
        payload
    }
}

impl OwnerClient {
    fn connect(fixture: &Fixture) -> Self {
        let client_socket = fixture.runtime_dir.join("herdr-client.sock");
        let (frames_tx, frames_rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
        let worker = thread::spawn(move || {
            Self::run(&client_socket, &frames_rx, &ready_tx);
        });
        match ready_rx.recv_timeout(Duration::from_secs(20)) {
            Ok(Ok(())) => {}
            Ok(Err(err)) => panic!("attached-client path unreachable: {err}"),
            Err(_) => panic!("attached-client handshake timed out"),
        }
        Self {
            frames: Some(frames_tx),
            worker: Some(worker),
        }
    }

    /// Type `text` (and optionally Enter) into `pane` as the owner.
    fn type_text(&self, pane: &str, text: &str, enter: bool) {
        self.frames
            .as_ref()
            .unwrap()
            .send(owner_wire::pane_text(pane, text, enter))
            .expect("owner client worker exited");
    }

    /// Operator handoff for `pane` through the attached-client control message
    /// (what the `supervisor_handoff` keybinding sends).
    fn hand_off(&self, pane: &str) {
        self.frames
            .as_ref()
            .unwrap()
            .send(owner_wire::handoff(pane))
            .expect("owner client worker exited");
    }

    #[cfg(windows)]
    fn readable(stream: &interprocess::local_socket::Stream) -> std::io::Result<usize> {
        use std::os::windows::io::{AsHandle, AsRawHandle};
        let interprocess::local_socket::Stream::NamedPipe(pipe) = stream;
        let mut available = 0u32;
        // SAFETY: PeekNamedPipe only reads the pipe's queued byte count into
        // `available`; the handle stays owned and alive for the call.
        let ok = unsafe {
            windows_sys::Win32::System::Pipes::PeekNamedPipe(
                pipe.as_handle().as_raw_handle(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut available,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(available as usize)
    }

    /// Append whatever the server has sent to `buf` without blocking long.
    fn drain(
        stream: &mut interprocess::local_socket::Stream,
        buf: &mut Vec<u8>,
    ) -> std::io::Result<()> {
        use std::io::Read;
        let mut chunk = vec![0u8; 64 * 1024];
        #[cfg(windows)]
        {
            let available = Self::readable(stream)?.min(chunk.len());
            if available > 0 {
                let n = stream.read(&mut chunk[..available])?;
                buf.extend_from_slice(&chunk[..n]);
            }
            Ok(())
        }
        #[cfg(not(windows))]
        {
            match stream.read(&mut chunk) {
                Ok(0) => Err(std::io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    Ok(())
                }
                Err(err)
                    if matches!(
                        err.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    Ok(())
                }
                Err(err) => Err(err),
            }
        }
    }

    fn run(
        client_socket: &std::path::Path,
        frames: &std::sync::mpsc::Receiver<Vec<u8>>,
        ready: &std::sync::mpsc::Sender<Result<(), String>>,
    ) {
        use interprocess::local_socket::{prelude::*, Stream};
        use std::io::Write;

        let connect = || -> std::io::Result<Stream> {
            #[cfg(unix)]
            let name = {
                use interprocess::local_socket::GenericFilePath;
                client_socket.to_fs_name::<GenericFilePath>()?
            };
            #[cfg(windows)]
            let name = {
                use interprocess::local_socket::GenericNamespaced;
                client_socket
                    .to_string_lossy()
                    .to_string()
                    .to_ns_name::<GenericNamespaced>()?
            };
            let stream = Stream::connect(name)?;
            #[cfg(not(windows))]
            stream.set_recv_timeout(Some(Duration::from_millis(10)))?;
            Ok(stream)
        };
        let mut stream = match connect() {
            Ok(stream) => stream,
            Err(err) => {
                let _ = ready.send(Err(format!("connect {}: {err}", client_socket.display())));
                return;
            }
        };
        let send = |stream: &mut Stream, payload: &[u8]| -> std::io::Result<()> {
            stream.write_all(&owner_wire::frame(payload))?;
            stream.flush()
        };
        if let Err(err) = send(&mut stream, &owner_wire::hello(100, 30)) {
            let _ = ready.send(Err(format!("hello: {err}")));
            return;
        }

        // Handshake reply, then the bootstrap snapshot and first pane surface.
        let mut buf = Vec::new();
        let mut welcomed = false;
        let deadline = Instant::now() + Duration::from_secs(15);
        let bootstrap = loop {
            if Instant::now() >= deadline {
                break Err(format!(
                    "timed out waiting for {}",
                    if welcomed {
                        "the first pane surface"
                    } else {
                        "endpoint welcome"
                    }
                ));
            }
            if let Err(err) = Self::drain(&mut stream, &mut buf) {
                break Err(format!("read: {err}"));
            }
            let mut outcome = None;
            while let Some(frame) = owner_wire::take_frame(&mut buf) {
                let mut offset = 0;
                let variant = owner_wire::decode_varint(&frame, &mut offset);
                if !welcomed {
                    let kind = owner_wire::decode_string(&frame, &mut offset);
                    let data = owner_wire::decode_string(&frame, &mut offset).unwrap_or_default();
                    let error = serde_json::from_str::<serde_json::Value>(&data)
                        .ok()
                        .and_then(|value| value.get("error").cloned())
                        .filter(|error| !error.is_null());
                    if variant != Some(owner_wire::SERVER_ENDPOINT_CONTROL)
                        || kind.as_deref() != Some("endpoint.welcome.v1")
                        || error.is_some()
                    {
                        outcome = Some(Err(format!(
                            "unexpected handshake reply: variant={variant:?} kind={kind:?} error={error:?}"
                        )));
                        break;
                    }
                    welcomed = true;
                } else if variant == Some(owner_wire::SERVER_PANE_SURFACE) {
                    outcome = Some(Ok(()));
                    break;
                }
            }
            if let Some(outcome) = outcome {
                break outcome;
            }
            thread::sleep(Duration::from_millis(5));
        };
        let failed = bootstrap.is_err();
        let _ = ready.send(bootstrap);
        if failed {
            return;
        }

        // Keep reading so the server never sees a stalled client; forward
        // owner input frames until the fixture drops the client.
        loop {
            if Self::drain(&mut stream, &mut buf).is_err() {
                return;
            }
            while owner_wire::take_frame(&mut buf).is_some() {}
            match frames.recv_timeout(Duration::from_millis(5)) {
                Ok(payload) => {
                    if send(&mut stream, &payload).is_err() {
                        return;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    let _ = send(
                        &mut stream,
                        &owner_wire::varint_u32(owner_wire::CLIENT_DETACH),
                    );
                    return;
                }
            }
        }
    }
}

impl Drop for OwnerClient {
    fn drop(&mut self) {
        self.frames.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// S6g: owner keys typed through a real attached client while the
/// accepted submission is parked fence it: receipt `fenced{owner_input}`, the
/// owner's keys stay on screen, and the submission contributes zero text and
/// zero Enter bytes.
#[test]
fn s6g_owner_keys_during_hold_fence_via_attached_client() {
    let hold = SUBMIT_HOLD_MS.to_string();
    let mut fixture = Fixture::start_with_supervisor_env(&[("HERDR_DEV_SUBMIT_HOLD_MS", &hold)]);
    let pane = fixture.focused_pane();
    fixture.launch_stand_in_claude(&pane);
    fixture.report(&pane, "claude", "idle");

    // Prove the path first: a probe typed by the attached client reaches the pane.
    let owner = OwnerClient::connect(&fixture);
    owner.type_text(&pane, "echo w2-owner-probe", true);
    assert!(
        fixture.wait_output(&pane, "w2-owner-probe"),
        "attached-client keys never reached {pane}"
    );
    let probed = fixture.digest_agent_until(&pane, "an owner input stamp", |agent| {
        agent["owner_input"]["last_owner_input_at_ms"].is_u64()
    });
    let probe_ms = probed["owner_input"]["last_owner_input_at_ms"]
        .as_u64()
        .unwrap();

    // The probe latched the draft for this PTY: fresh output never clears it,
    // so the operator hands one submit over explicitly.
    fixture.show_prompt_box(&pane, "s6g-box.txt", "\u{276f} ", "footer-s6g");
    let latched = fixture.digest_agent(&pane);
    assert_eq!(latched["owner_input"]["draft"], "unknown", "{latched}");
    owner.hand_off(&pane);
    let agent = fixture.digest_agent_until(&pane, "an armed handoff", |agent| {
        agent["owner_input"]["handoff_armed"] == true
    });
    assert_eq!(agent["owner_input"]["draft"], "unknown", "{agent}");

    let marker = "w2-held-owner-marker";
    let text = comment_line(marker);
    let owner_keys = "zq7owner";
    let submitted_at = Instant::now();
    let accepted = fixture.submit_ok(&target_ref_arg(&agent), "s6g-x", &text);
    assert_eq!(accepted["state"], "accepted", "{accepted}");
    assert_eq!(accepted["handoff"], true, "{accepted}");

    owner.type_text(&pane, owner_keys, false);
    let fenced = fixture.wait_receipt("s6g-x", &text, "fenced", |receipt| {
        receipt["state"] == "fenced"
    });
    assert_eq!(fenced["reason"], "owner_input", "{fenced}");
    assert!(
        fixture.wait_output(&pane, owner_keys),
        "owner keys not shown"
    );
    let after = fixture.digest_agent(&pane);
    assert!(
        after["owner_input"]["last_owner_input_at_ms"]
            .as_u64()
            .is_some_and(|ms| ms > probe_ms),
        "{after}"
    );

    let settle = Duration::from_millis(SUBMIT_HOLD_MS + SUBMIT_ENTER_DELAY_MS + 1500);
    thread::sleep(settle.saturating_sub(submitted_at.elapsed()));
    assert_eq!(fixture.submit_status("s6g-x", &text)["state"], "fenced");
    let screen = fixture.screen_text(&pane);
    assert_eq!(
        screen.matches(marker).count(),
        0,
        "submit text written: {screen}"
    );
    // An Enter from the held submission would make the shell run the owner's
    // keys as a command, whose not-found error repeats them; exactly one copy
    // means the owner's input is still unsubmitted on the prompt line.
    assert_eq!(
        screen.matches(owner_keys).count(),
        1,
        "owner keys were submitted or lost: {screen}"
    );
    drop(owner);
}

/// S6h: the operator's handoff through the real
/// attached-client control message lets exactly one submit through the
/// owner-input latch. `agent watch` shows `handoff_armed` go true, then false
/// once the accepted submit consumed it; a second submit is rejected
/// `draft_not_clean` and the draft never reads clean.
#[test]
fn s6h_operator_handoff_allows_exactly_one_submit() {
    let mut fixture = Fixture::start_with_supervisor();
    let pane = fixture.focused_pane();
    fixture.launch_stand_in_claude(&pane);
    fixture.report(&pane, "claude", "idle");

    let owner = OwnerClient::connect(&fixture);
    owner.type_text(&pane, "echo w2-handoff-probe", true);
    assert!(
        fixture.wait_output(&pane, "w2-handoff-probe"),
        "attached-client keys never reached {pane}"
    );
    fixture.show_prompt_box(&pane, "s6h-box.txt", "\u{276f} ", "footer-s6h");
    let latched = fixture.digest_agent_until(&pane, "an owner input stamp", |agent| {
        agent["owner_input"]["last_owner_input_at_ms"].is_u64()
    });
    assert_eq!(latched["owner_input"]["draft"], "unknown", "{latched}");
    assert_eq!(latched["owner_input"]["handoff_armed"], false, "{latched}");

    let (mut watch, rx) = fixture.spawn_watch(&[
        "--pane",
        &pane,
        "--interval-ms",
        "100",
        "--timeout-ms",
        "60000",
    ]);
    let mut lines = Vec::new();
    recv_watch_until(&rx, &mut lines, "the unarmed state", |line| {
        line["type"] == "change" && line["handoff_armed"] == false
    });

    owner.hand_off(&pane);
    recv_watch_until(&rx, &mut lines, "handoff_armed true", |line| {
        line["handoff_armed"] == true
    });
    let armed = lines.last().unwrap().clone();
    assert_eq!(armed["draft"], "unknown", "{armed}");
    let agent = fixture.digest_agent(&pane);
    assert_eq!(agent["owner_input"]["handoff_armed"], true, "{agent}");

    let marker = "w2-handoff-marker";
    let text = comment_line(marker);
    let accepted = fixture.submit_ok(&target_ref_arg(&agent), "s6h-1", &text);
    assert_eq!(accepted["state"], "accepted", "{accepted}");
    assert_eq!(accepted["handoff"], true, "{accepted}");
    recv_watch_until(
        &rx,
        &mut lines,
        "handoff_armed false after consumption",
        |line| line["handoff_armed"] == false,
    );
    fixture.wait_receipt("s6h-1", &text, "written", |receipt| {
        receipt["state"] == "written"
    });

    let second = comment_line("w2-handoff-second");
    let error = fixture.submit_error(&target_ref_arg(&agent), "s6h-2", &second);
    assert_eq!(error["error"]["code"], "draft_not_clean", "{error}");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("draft=unknown"),
        "{error}"
    );
    let after = fixture.digest_agent(&pane);
    assert_eq!(after["owner_input"]["draft"], "unknown", "{after}");
    assert_eq!(after["owner_input"]["handoff_armed"], false, "{after}");
    let unknown = fixture.cli_error(&["agent", "submit-status", "s6h-2"]);
    assert_eq!(unknown["error"]["code"], "submit_not_found", "{unknown}");
    assert!(
        lines.iter().all(|line| line["draft"] != "clean"),
        "draft read clean: {lines:?}"
    );

    let _ = watch.kill();
    let _ = watch.wait();
    thread::sleep(Duration::from_millis(SUBMIT_ENTER_DELAY_MS + 1000));
    assert_eq!(fixture.screen_count(&pane, "w2-handoff-second"), 0);
    drop(owner);
}

// ---------------------------------------------------------------------------
// Local slice part 2 S7-S9: `agent answer`, `agent interrupt`, `agent reset`.
// ---------------------------------------------------------------------------

/// Stand-in for the receipt text check of verbs that send no caller text.
const NO_TEXT: &str = "w3-no-caller-text";

impl Fixture {
    fn answer_args<'a>(
        target_ref: &'a str,
        question_id: &'a str,
        submit_id: &'a str,
        text: &'a str,
    ) -> [&'a str; 9] {
        [
            "agent",
            "answer",
            "--target-ref",
            target_ref,
            "--question-id",
            question_id,
            "--submit-id",
            submit_id,
            text,
        ]
    }

    fn answer_ok(
        &self,
        target_ref: &str,
        question_id: &str,
        submit_id: &str,
        text: &str,
    ) -> serde_json::Value {
        let response = self.cli_json(&Self::answer_args(target_ref, question_id, submit_id, text));
        receipt_of(&response, text)
    }

    fn answer_error(
        &self,
        target_ref: &str,
        question_id: &str,
        submit_id: &str,
        text: &str,
    ) -> serde_json::Value {
        let error = self.cli_error(&Self::answer_args(target_ref, question_id, submit_id, text));
        assert!(
            !error.to_string().contains(text),
            "error leaked the answer text: {error}"
        );
        error
    }

    /// `agent interrupt|reset --target-ref T --submit-id ID`, success receipt.
    fn control_ok(&self, verb: &str, target_ref: &str, submit_id: &str) -> serde_json::Value {
        let response = self.cli_json(&[
            "agent",
            verb,
            "--target-ref",
            target_ref,
            "--submit-id",
            submit_id,
        ]);
        receipt_of(&response, NO_TEXT)
    }

    fn control_error(&self, verb: &str, target_ref: &str, submit_id: &str) -> serde_json::Value {
        self.cli_error(&[
            "agent",
            verb,
            "--target-ref",
            target_ref,
            "--submit-id",
            submit_id,
        ])
    }

    /// Report a Claude SessionStart the way the shipped hook does, with an explicit
    /// session id and start source.
    fn report_session_start(&mut self, pane: &str, session_id: &str, start_source: &str) {
        self.seq += 1;
        let seq = self.seq.to_string();
        self.cli_json(&[
            "pane",
            "report-agent-session",
            pane,
            "--source",
            "herdr:claude",
            "--agent",
            "claude",
            "--agent-session-id",
            session_id,
            "--session-start-source",
            start_source,
            "--seq",
            &seq,
        ]);
    }

    /// The open question ids of a digest entry, oldest first.
    fn open_question_ids(agent: &serde_json::Value) -> Vec<String> {
        agent["pending_questions"]
            .as_array()
            .map(|questions| {
                questions
                    .iter()
                    .filter(|q| q.get("resolution").is_none_or(|r| r.is_null()))
                    .filter_map(|q| q["question_id"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// S7: a prose question shown with chrome and an empty box is answered once
/// (the text reaches the stand-in once; a retry writes nothing); once a newer
/// question is shown, the old question id is rejected `question_not_current`
/// before any write.
#[test]
fn s7_answer_newest_question_once_then_old_id_is_not_current() {
    let mut fixture = Fixture::start_with_supervisor();
    let pane = fixture.pane();
    fixture.launch_stand_in_claude(&pane);
    fixture.report(&pane, "claude", "working");
    fixture.show_question_with_chrome(&pane, "Should I start the brief?", "footer-s7-q1");
    fixture.report(&pane, "claude", "idle");
    let asked = fixture.digest_agent_until(&pane, "an open question and a clean draft", |agent| {
        Fixture::open_question_ids(agent).len() == 1 && agent["owner_input"]["draft"] == "clean"
    });
    let question = Fixture::open_question_ids(&asked).remove(0);
    let target = target_ref_arg(&asked);

    let marker = "w3-answer-once-marker";
    let text = comment_line(marker);
    let accepted = fixture.answer_ok(&target, &question, "s7-a", &text);
    assert_eq!(accepted["kind"], "answer", "{accepted}");
    assert_eq!(accepted["question_id"], question.as_str(), "{accepted}");
    let written = fixture.wait_receipt("s7-a", &text, "written", |receipt| {
        receipt["state"] == "written"
    });
    assert!(fixture.wait_output(&pane, marker), "answer never shown");
    assert_eq!(fixture.screen_count(&pane, marker), 1);
    let again = fixture.answer_ok(&target, &question, "s7-a", &text);
    assert_eq!(
        again["accepted_at_ms"], written["accepted_at_ms"],
        "{again}"
    );
    thread::sleep(Duration::from_millis(SUBMIT_ENTER_DELAY_MS + 1200));
    assert_eq!(
        fixture.screen_count(&pane, marker),
        1,
        "retry delivered the answer again"
    );

    // A newer question: the old id is no longer the newest open question.
    fixture.report(&pane, "claude", "working");
    fixture.show_question_with_chrome(&pane, "Do you want the long version?", "footer-s7-q2");
    fixture.report(&pane, "claude", "idle");
    let newer = fixture.digest_agent_until(&pane, "a second open question", |agent| {
        Fixture::open_question_ids(agent).len() == 2
    });
    assert_eq!(
        Fixture::open_question_ids(&newer)[0],
        question,
        "an answer does not resolve the question: {newer}"
    );
    let stale_marker = "w3-answer-stale-marker";
    let stale_text = comment_line(stale_marker);
    let error = fixture.answer_error(&target_ref_arg(&newer), &question, "s7-old", &stale_text);
    assert_eq!(error["error"]["code"], "question_not_current", "{error}");
    thread::sleep(Duration::from_millis(SUBMIT_ENTER_DELAY_MS + 1000));
    assert_eq!(fixture.screen_count(&pane, stale_marker), 0);
    let unknown = fixture.cli_error(&["agent", "submit-status", "s7-old"]);
    assert_eq!(unknown["error"]["code"], "submit_not_found", "{unknown}");
}

/// S8: interrupt is accepted only while working; the Esc write completes once
/// (`written`), the next idle report records `turn_end_observed_after_write_at_ms`
/// (an uncorrelated observation), a retry
/// returns the same receipt without a second write, and an idle agent is
/// rejected `agent_not_working`.
///
/// Limit: Esc has no visible echo in the stand-in shell, so "reached the
/// stand-in" is the writer's own completion (`written`), not screen evidence.
#[test]
fn s8_interrupt_working_agent_once_then_turn_end_on_idle() {
    let mut fixture = Fixture::start_with_supervisor();
    let pane = fixture.pane();
    fixture.ready_clean_claude(&pane, "s8");
    fixture.report(&pane, "claude", "working");
    let working = fixture.digest_agent_until(&pane, "working with a clean draft", |agent| {
        agent["agent_status"] == "working" && agent["owner_input"]["draft"] == "clean"
    });
    let target = target_ref_arg(&working);

    let accepted = fixture.control_ok("interrupt", &target, "s8-i");
    assert_eq!(accepted["kind"], "interrupt", "{accepted}");
    assert!(
        matches!(accepted["state"].as_str(), Some("accepted" | "written")),
        "{accepted}"
    );
    let written = fixture.wait_receipt("s8-i", NO_TEXT, "written", |receipt| {
        receipt["state"] == "written"
    });
    assert!(written["written_at_ms"].is_u64(), "{written}");
    assert!(
        written
            .get("turn_end_observed_after_write_at_ms")
            .is_none_or(|v| v.is_null()),
        "{written}"
    );

    fixture.report(&pane, "claude", "idle");
    let effect = fixture.wait_receipt("s8-i", NO_TEXT, "an observed turn end", |receipt| {
        receipt["turn_end_observed_after_write_at_ms"].is_u64()
    });
    assert_eq!(effect["state"], "written", "{effect}");

    let again = fixture.control_ok("interrupt", &target, "s8-i");
    assert_eq!(again, effect, "retry returns the same receipt");

    let idle = fixture.digest_agent(&pane);
    let error = fixture.control_error("interrupt", &target_ref_arg(&idle), "s8-idle");
    assert_eq!(error["error"]["code"], "agent_not_working", "{error}");
    let unknown = fixture.cli_error(&["agent", "submit-status", "s8-idle"]);
    assert_eq!(unknown["error"]["code"], "submit_not_found", "{unknown}");
}

/// S9: reset submits `/clear` once; a reported SessionStart with a new session
/// id and source `clear` verifies it with the new session and generation; a
/// retry with the original target writes nothing.
#[test]
fn s9_reset_writes_clear_once_and_verifies_on_clear_session() {
    let mut fixture = Fixture::start_with_supervisor();
    let pane = fixture.pane();
    fixture.ready_clean_claude(&pane, "s9");
    fixture.report_session_start(&pane, "s9-session-old", "startup");
    let agent =
        fixture.digest_agent_until(&pane, "a clean draft after the session report", |agent| {
            agent["owner_input"]["draft"] == "clean"
        });
    let generation = agent["target_ref"]["generation"].as_u64().unwrap();
    let target = target_ref_arg(&agent);
    let before = fixture.screen_count(&pane, "/clear");

    let accepted = fixture.control_ok("reset", &target, "s9-r");
    assert_eq!(accepted["kind"], "reset", "{accepted}");
    assert_eq!(accepted["reset"], "pending", "{accepted}");
    let written = fixture.wait_receipt("s9-r", NO_TEXT, "written", |receipt| {
        receipt["state"] == "written"
    });
    assert_eq!(written["reset"], "pending", "{written}");
    assert!(fixture.wait_output(&pane, "/clear"), "/clear never shown");
    let echoed = fixture.screen_count(&pane, "/clear");
    assert!(echoed > before, "/clear was not echoed");

    fixture.report_session_start(&pane, "s9-session-new", "clear");
    let verified = fixture.wait_receipt("s9-r", NO_TEXT, "verified", |receipt| {
        receipt["reset"] == "verified"
    });
    assert_eq!(
        verified["new_session"]["value"], "s9-session-new",
        "{verified}"
    );
    assert_eq!(verified["new_session"]["kind"], "id", "{verified}");
    assert_eq!(
        verified["new_generation"].as_u64(),
        Some(generation + 1),
        "{verified}"
    );
    let bumped = fixture.digest_agent(&pane);
    assert_eq!(
        bumped["target_ref"]["generation"].as_u64(),
        Some(generation + 1),
        "{bumped}"
    );

    let again = fixture.control_ok("reset", &target, "s9-r");
    assert_eq!(again, verified, "retry returns the same receipt");
    thread::sleep(Duration::from_millis(SUBMIT_ENTER_DELAY_MS + 1200));
    assert_eq!(
        fixture.screen_count(&pane, "/clear"),
        echoed,
        "retry wrote /clear again"
    );
}

/// S9: without a clear SessionStart report the reset stays `pending` and
/// never carries a session or generation.
#[test]
fn s9_reset_without_clear_session_stays_pending() {
    let mut fixture = Fixture::start_with_supervisor();
    let pane = fixture.pane();
    let agent = fixture.ready_clean_claude(&pane, "s9p");
    let accepted = fixture.control_ok("reset", &target_ref_arg(&agent), "s9p-r");
    assert_eq!(accepted["reset"], "pending", "{accepted}");
    fixture.wait_receipt("s9p-r", NO_TEXT, "written", |receipt| {
        receipt["state"] == "written"
    });
    thread::sleep(Duration::from_millis(SUBMIT_ENTER_DELAY_MS + 1000));
    let receipt = fixture.submit_status("s9p-r", NO_TEXT);
    assert_eq!(receipt["reset"], "pending", "{receipt}");
    assert!(
        receipt.get("new_session").is_none_or(|v| v.is_null()),
        "{receipt}"
    );
    assert!(
        receipt.get("new_generation").is_none_or(|v| v.is_null()),
        "{receipt}"
    );
}
