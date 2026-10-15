//! Cross-platform supervisor fixture.
//!
//! Starts a real headless server, opens a pane running the platform shell, and
//! drives agent state through `kelpie pane report-agent`, exactly as an agent
//! integration hook would. Everything goes through the `kelpie` CLI, so the
//! same tests run on Windows (named pipe) and Unix (socket).
//!
//! The `current_*` tests characterize the legacy `agent get/wait` surface,
//! which must not change: the supervisor facts (awaiting-human questions,
//! background work, completion) live in the separate `agents.digest` method
//! (`kelpie agent digest`), exercised by the remaining tests when the fixture
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

/// Fields kelpie already reports today that merely share a fragment above.
const EXISTING_KEYS: &[&str] = &["completion_seq"];

fn app_dir_name() -> &'static str {
    if cfg!(debug_assertions) {
        "kelpie-dev"
    } else {
        "kelpie"
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
    /// (e.g. the debug-build `KELPIE_DEV_SUBMIT_HOLD_MS` knob).
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
        // macOS limits a Unix socket path to 104 bytes and its per-user temp folder
        // alone is about 50, so the fixture's socket lives under /tmp there.
        let temp_root = if cfg!(target_os = "macos") {
            std::path::PathBuf::from("/tmp")
        } else {
            std::env::temp_dir()
        };
        let base = temp_root.join(format!("ksf-{}-{nanos}", std::process::id()));
        let config_home = base.join("config");
        let runtime_dir = base.join("runtime");
        let state_home = base.join("state");
        let socket_path = runtime_dir.join("kelpie.sock");
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

        let mut command = Command::new(env!("CARGO_BIN_EXE_kelpie"));
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

    /// Point a child process at this fixture's server and strip any kelpie
    /// context inherited from a surrounding session.
    fn isolate(&self, command: &mut Command) {
        command
            .env("XDG_CONFIG_HOME", &self.config_home)
            .env("XDG_RUNTIME_DIR", &self.runtime_dir)
            .env("XDG_STATE_HOME", &self.state_home)
            .env("KELPIE_SOCKET_PATH", &self.socket_path)
            .env_remove("KELPIE_CLIENT_SOCKET_PATH")
            .env_remove("KELPIE_ENV")
            .env_remove("KELPIE_SESSION")
            .env_remove("KELPIE_PANE_ID");
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_kelpie"));
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
            "kelpie {} failed: {}\n{}",
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
                "kelpie {} printed non-JSON ({err}): {}",
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

    /// Run a long-lived stand-in process that kelpie identifies as Claude, so
    /// `agent submit` sees the agent as the pane's foreground process. It is an
    /// interactive shell, so typed prompts are echoed and otherwise harmless.
    fn launch_stand_in_claude(&self, pane: &str) {
        self.launch_stand_in_agent(pane, "claude", "claude");
    }

    fn launch_stand_in_agent(&self, pane: &str, agent_label: &str, exe_name: &str) {
        let stand_in = self.write_stand_in_agent(agent_label, exe_name);
        let command = if cfg!(windows) {
            format!("\"{}\" /q /k", stand_in.display())
        } else {
            format!("'{}'", stand_in.display())
        };
        self.cli_json(&["pane", "run", pane, &command]);
        self.wait_identified_agent(pane, agent_label);
    }

    fn wait_identified_agent(&self, pane: &str, agent_label: &str) {
        // Detection only, no report yet: identify the actual foreground process.
        let identified = wait_until(Duration::from_secs(10), || {
            let output = self.cli(&["agent", "get", pane]);
            output.status.success()
                && serde_json::from_slice::<serde_json::Value>(&output.stdout)
                    .is_ok_and(|got| got["result"]["agent"]["agent"] == agent_label)
        });
        assert!(
            identified,
            "stand-in {agent_label} was not identified in {pane}"
        );
    }

    /// Write the stand-in Claude executable into `<base>/bin` and return its path. It
    /// ignores its arguments (so `claude --resume <id>` starts it too).
    fn write_stand_in_claude(&self) -> PathBuf {
        self.write_stand_in_agent("claude", "claude")
    }

    fn write_stand_in_agent(&self, agent_label: &str, exe_name: &str) -> PathBuf {
        let bin = self.base.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        if cfg!(windows) {
            let system_root =
                std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
            let stand_in = bin.join(format!("{exe_name}.exe"));
            if !stand_in.exists() {
                std::fs::copy(
                    PathBuf::from(system_root).join(r"System32\cmd.exe"),
                    &stand_in,
                )
                .unwrap();
            }
            stand_in
        } else if cfg!(target_os = "macos") {
            // macOS names the foreground process from argv[0] (KERN_PROCARGS2), and a
            // script that execs a shell would show up as that shell. Copying a system
            // shell does not work either (macOS kills relocated platform binaries), so
            // exec bash with argv[0] set to the agent's name.
            let stand_in = bin.join(exe_name);
            std::fs::write(
                &stand_in,
                format!(
                    "#!/bin/bash\nexport KELPIE_AGENT={agent_label}\nexec -a {exe_name} /bin/bash\n"
                ),
            )
            .unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&stand_in, std::fs::Permissions::from_mode(0o755))
                    .unwrap();
            }
            stand_in
        } else {
            let stand_in = bin.join(exe_name);
            std::fs::write(
                &stand_in,
                format!("#!/bin/sh\nexport KELPIE_AGENT={agent_label}\nexec /bin/sh\n"),
            )
            .unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&stand_in, std::fs::Permissions::from_mode(0o755))
                    .unwrap();
            }
            stand_in
        }
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
    /// `supervisor_v1` capability (`kelpie status` omits it), so this sends the
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

    /// `kelpie agent digest`, parsed. Errors print on stderr with exit 1.
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
/// and kelpie showed it as idle. Today there is no awaiting-human fact, so the
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
/// running, and kelpie showed the session as finished. Today background work
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
        .join("kelpie-server.log");
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

/// The receipt inside a submit/submit-status success response. The agent's
/// acknowledgement is `unsupported` unless its prompt-submit hook confirmed the
/// write (B1), and a receipt must never carry the submitted text.
fn receipt_of(response: &serde_json::Value, text: &str) -> serde_json::Value {
    assert_eq!(
        response["result"]["type"], "agent_submit_receipt",
        "{response}"
    );
    let receipt = response["result"]["receipt"].clone();
    assert!(
        matches!(
            receipt["acknowledged"].as_str(),
            Some("unsupported" | "confirmed")
        ),
        "{receipt}"
    );
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
            "kelpie {} should fail: {}{}",
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

    /// Run the shipped Claude hook script with action `prompt`, as Claude's
    /// `UserPromptSubmit` hook would, feeding it the hook JSON for `prompt`. The
    /// hook must exit 0 and print nothing (Claude adds hook stdout to the prompt).
    fn run_prompt_hook(&self, pane: &str, prompt: &str) {
        use std::io::Write as _;

        let assets = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("integration")
            .join("assets")
            .join("claude");
        let mut command = if cfg!(windows) {
            let mut command = Command::new("powershell");
            command
                .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
                .arg(assets.join("kelpie-agent-state.ps1"));
            command
        } else {
            let mut command = Command::new("sh");
            command.arg(assets.join("kelpie-agent-state.sh"));
            command
        };
        command.arg("prompt");
        self.isolate(&mut command);
        command
            .env("KELPIE_ENV", "1")
            .env("KELPIE_PANE_ID", pane)
            .env("KELPIE_BIN_PATH", env!("CARGO_BIN_EXE_kelpie"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let payload = serde_json::json!({
            "session_id": "fixture-session",
            "transcript_path": "fixture.jsonl",
            "cwd": self.base.to_str().unwrap(),
            "permission_mode": "default",
            "hook_event_name": "UserPromptSubmit",
            "prompt": prompt,
        });
        let mut child = command.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        let status = wait_child_exit(&mut child, Duration::from_secs(60));
        let output = child.wait_with_output().unwrap();
        assert!(status.success(), "prompt hook failed: {status:?}");
        assert!(
            output.stdout.iter().all(u8::is_ascii_whitespace),
            "prompt hook printed to stdout: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    /// `kelpie agent watch <args>` as a child; its stdout lines arrive parsed.
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

/// B1: the shipped Claude `UserPromptSubmit` hook, fed the hook JSON for the
/// submitted text, confirms the written receipt once. A different prompt
/// confirms nothing, a repeat of the same prompt changes nothing, and the
/// prompt text reaches neither the receipt nor the server log.
#[test]
fn b1_prompt_submit_hook_confirms_the_written_receipt_once() {
    let mut fixture = Fixture::start_with_supervisor();
    let pane = fixture.pane();
    let agent = fixture.ready_clean_claude(&pane, "b1");
    let target = target_ref_arg(&agent);
    let marker = "b1-prompt-ack-marker";
    let text = comment_line(marker);

    // An owner prompt before the submit: its report matches nothing. (A report that
    // matches nothing more than PROMPT_ACK_REORDER_SLACK_MS after a write retires that
    // receipt, and one hook run here can take longer than that: a cold PowerShell in
    // the isolated profile.)
    fixture.run_prompt_hook(&pane, "b1 a prompt the owner typed");

    fixture.submit_ok(&target, "b1-x", &text);
    let written = fixture.wait_receipt("b1-x", &text, "written", |receipt| {
        receipt["state"] == "written"
    });
    assert_eq!(written["acknowledged"], "unsupported", "{written}");

    // Claude's hook prompt for the submitted text, with a trailing newline.
    fixture.run_prompt_hook(&pane, &format!("{text}\n"));
    let confirmed = fixture.wait_receipt("b1-x", &text, "confirmed", |receipt| {
        receipt["acknowledged"] == "confirmed"
    });
    assert_eq!(confirmed["state"], "written", "{confirmed}");
    assert!(confirmed["acknowledged_at_ms"].is_u64(), "{confirmed}");

    // The owner retyping the same prompt changes nothing.
    fixture.run_prompt_hook(&pane, &text);
    assert_eq!(fixture.submit_status("b1-x", &text), confirmed);

    let log_path = fixture
        .config_home
        .join(app_dir_name())
        .join("kelpie-server.log");
    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    assert!(
        !log.contains(marker) && !log.contains("owner typed"),
        "server log contains prompt text: {}",
        log_path.display()
    );
}

// P10 uses real fixture PTYs and shipped hooks, without starting a model client.
const P10_OBSERVER_ENV: &str = "P10_FIXTURE_INPUT_OBSERVER_BASE";
const P10_INPUT_BARRIER: &str = "p10-input-barrier-end";

/// Only composer shape and styling are taken from the captured agent screens.
/// No paths, account data, or incidental transcript text belongs in this fixture.
fn p10_clean_screen(agent: &str) -> String {
    let composer = match agent {
        "codex" => {
            "\u{203a} \x1b[2mAsk Codex to do anything\x1b[0m\r\n\r\n  100% context left".to_string()
        }
        "cursor" => {
            let bg = "\x1b[48;2;21;21;21m";
            format!(
                "{bg}{pad}\x1b[0m\r\n{bg} \u{2192} \x1b[2mPlan, search, build anything\x1b[0m{bg}{rest}\x1b[0m\r\n{bg}{pad}\x1b[0m\r\n  / commands",
                pad = " ".repeat(60),
                rest = " ".repeat(30)
            )
        }
        "gemini" => "fixture gemini ready".to_string(),
        _ => panic!("unsupported fixture agent {agent}"),
    };
    // Disable bracketed paste so the captured delivery has an exact byte shape.
    format!("\x1b[?2004l\x1b[2J\x1b[HDone.\r\n\r\n{composer}")
}

fn p10_cursor_dirty_screen(text: &str) -> String {
    let bg = "\x1b[48;2;21;21;21m";
    format!(
        "\x1b[2J\x1b[HDone.\r\n\r\n{bg}          \x1b[0m\r\n{bg} \u{2192} {text}\x1b[0m\r\n{bg}          \x1b[0m\r\n  Auto"
    )
}

/// Quote only fixture-owned paths/assignments for the pane shell. Reject cmd
/// expansion characters rather than letting a local path become shell syntax.
fn p10_shell_quote(value: &str) -> String {
    #[cfg(windows)]
    {
        assert!(
            !value
                .chars()
                .any(|ch| matches!(ch, '"' | '%' | '!' | '\r' | '\n')),
            "unsupported shell character in fixture-owned path"
        );
        format!("\"{value}\"")
    }
    #[cfg(unix)]
    {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

/// Records only synthetic bytes delivered to this fixture's own terminal. Screen
/// updates use files, not keyboard input, so they cannot set the owner-input latch.
struct P10InputObserver {
    base: PathBuf,
    screen_seq: usize,
}

impl P10InputObserver {
    fn start(fixture: &Fixture, pane: &str, agent: &str, exe_name: &str) -> Self {
        fixture.launch_stand_in_agent(pane, agent, exe_name);
        let observer = Self {
            base: fixture.base.clone(),
            screen_seq: 0,
        };
        std::fs::write(
            observer.base.join("p10-screen-0.ansi"),
            p10_clean_screen(agent),
        )
        .unwrap();
        let executable = std::env::current_exe().unwrap();
        let base_assignment =
            p10_shell_quote(&format!("{P10_OBSERVER_ENV}={}", fixture.base.display()));
        let executable = p10_shell_quote(executable.to_str().unwrap());
        let stderr = p10_shell_quote(observer.base.join("p10-child.stderr").to_str().unwrap());
        #[cfg(windows)]
        let command = {
            // Windows shell setup reconstructs TEMP/TMP below USERPROFILE. The
            // fixture owns a different temp root, so carry it explicitly while
            // preserving the child's canonical direct-parent isolation check.
            let temp_root = fixture.base.parent().unwrap().display();
            let temp = p10_shell_quote(&format!("TEMP={temp_root}"));
            let tmp = p10_shell_quote(&format!("TMP={temp_root}"));
            format!(
                "set {base_assignment} & set {temp} & set {tmp} & {executable} --exact p10_fixture_input_observer_child --nocapture 2>{stderr}"
            )
        };
        #[cfg(unix)]
        let command = format!(
            "env {base_assignment} {executable} --exact p10_fixture_input_observer_child --nocapture 2>{stderr}"
        );
        fixture.cli_json(&["pane", "run", pane, &command]);
        observer.wait_screen(fixture, pane);
        // The observer is below the renamed shell in the process tree; prove that
        // it did not replace the detected agent with a fixture/report-only label.
        fixture.wait_identified_agent(pane, agent);
        observer
    }

    fn wait_screen(&self, fixture: &Fixture, pane: &str) {
        let ack = self
            .base
            .join(format!("p10-screen-{}.ready", self.screen_seq));
        assert!(
            wait_until(Duration::from_secs(10), || ack.exists()),
            "observer did not render screen {} in {}: {}",
            self.screen_seq,
            self.base.display(),
            self.diagnostics(fixture, pane)
        );
    }

    /// Preserve bounded, fixture-owned diagnostics in the test failure output
    /// before Fixture::drop removes the child files and terminal.
    fn diagnostics(&self, fixture: &Fixture, pane: &str) -> String {
        use std::io::Read;

        let mut details = Vec::new();
        for format in ["text", "ansi"] {
            let stdout = self.base.join(format!("p10-diagnostic-{format}.stdout"));
            let stderr = self.base.join(format!("p10-diagnostic-{format}.stderr"));
            let result = (|| -> std::io::Result<String> {
                let mut child = fixture
                    .command(&[
                        "agent",
                        "read",
                        pane,
                        "--source",
                        "detection",
                        "--format",
                        format,
                    ])
                    .stdout(std::fs::File::create(&stdout)?)
                    .stderr(std::fs::File::create(&stderr)?)
                    .spawn()?;
                let exited = wait_until(Duration::from_secs(5), || {
                    matches!(child.try_wait(), Ok(Some(_)))
                });
                if !exited {
                    let _ = child.kill();
                    let _ = child.wait();
                }
                Ok(format!("pane {format}: exited={exited}"))
            })();
            details.push(format!("{result:?}"));
        }
        for name in [
            "p10-diagnostic-text.stdout",
            "p10-diagnostic-text.stderr",
            "p10-diagnostic-ansi.stdout",
            "p10-diagnostic-ansi.stderr",
            "p10-child.stderr",
            "p10-input.bin",
            "p10-stopped",
        ] {
            let value = (|| -> std::io::Result<Vec<u8>> {
                let mut bytes = Vec::new();
                std::fs::File::open(self.base.join(name))?
                    .take(16384)
                    .read_to_end(&mut bytes)?;
                Ok(bytes)
            })();
            details.push(match value {
                Ok(bytes) => format!("{name}: {:?}", String::from_utf8_lossy(&bytes)),
                Err(error) => format!("{name}: {error}"),
            });
        }
        details.join("\n")
    }

    fn show(&mut self, fixture: &Fixture, pane: &str, screen: &str) {
        self.screen_seq += 1;
        let staged = self.base.join("p10-screen.staged");
        std::fs::write(&staged, screen).unwrap();
        std::fs::rename(
            staged,
            self.base
                .join(format!("p10-screen-{}.ansi", self.screen_seq)),
        )
        .unwrap();
        self.wait_screen(fixture, pane);
    }

    fn bytes(&self) -> Vec<u8> {
        std::fs::read(self.base.join("p10-input.bin")).unwrap_or_default()
    }

    fn wait_bytes(&self, fixture: &Fixture, pane: &str, expected: &[u8]) {
        assert!(
            wait_until(Duration::from_secs(10), || self.bytes().len()
                >= expected.len()),
            "observer expected {expected:?}, received {:?}: {}",
            self.bytes(),
            self.diagnostics(fixture, pane)
        );
        assert_eq!(
            self.bytes(),
            expected,
            "actual fixture PTY input: {}",
            self.diagnostics(fixture, pane)
        );
    }

    /// An in-band barrier orders this read after all earlier PTY writes. Call
    /// only after the final action: pane input deliberately sets the owner latch.
    fn assert_bytes_through_barrier(&self, fixture: &Fixture, pane: &str, expected: &[u8]) {
        fixture.cli_json(&["pane", "send-text", pane, P10_INPUT_BARRIER]);
        let mut through_barrier = expected.to_vec();
        through_barrier.extend_from_slice(P10_INPUT_BARRIER.as_bytes());
        self.wait_bytes(fixture, pane, &through_barrier);
        std::fs::write(self.base.join("p10-stop"), b"stop").unwrap();
        let stopped = wait_until(Duration::from_secs(5), || {
            self.base.join("p10-stopped").exists()
        });
        let completed =
            std::fs::read(self.base.join("p10-completed")).is_ok_and(|bytes| bytes == b"complete");
        let quiet = std::fs::metadata(self.base.join("p10-child.stderr"))
            .is_ok_and(|metadata| metadata.len() == 0);
        assert!(
            stopped && completed && quiet,
            "observer did not finish successfully: stopped={stopped}, completed={completed}, quiet={quiet}; {}",
            self.diagnostics(fixture, pane)
        );
    }
}

impl Drop for P10InputObserver {
    fn drop(&mut self) {
        let _ = std::fs::write(self.base.join("p10-stop"), b"stop");
        // Fixture::drop stops the owned server/PTY even if the observer failed.
        let _ = wait_until(Duration::from_secs(5), || {
            self.base.join("p10-stopped").exists()
        });
    }
}

/// Crossterm's Windows raw mode clears cooked input flags but does not enable
/// VT input. The byte observer needs console key events (including Escape) to
/// become bytes, and must restore the exact inherited mode when it exits.
struct P10RawInputMode {
    #[cfg(windows)]
    handle: windows_sys::Win32::Foundation::HANDLE,
    #[cfg(windows)]
    original_mode: u32,
    active: bool,
}

impl P10RawInputMode {
    fn enable() -> std::io::Result<Self> {
        #[cfg(windows)]
        {
            use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
            use windows_sys::Win32::System::Console::{
                GetConsoleMode, GetStdHandle, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT,
                ENABLE_PROCESSED_INPUT, ENABLE_VIRTUAL_TERMINAL_INPUT, STD_INPUT_HANDLE,
            };

            // SAFETY: these calls inspect the borrowed standard input handle;
            // the mode output points to a live u32 and no handle is closed.
            let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
            if handle.is_null() || handle == INVALID_HANDLE_VALUE {
                return Err(std::io::Error::other(format!(
                    "invalid fixture stdin handle {handle:?}: {}",
                    std::io::Error::last_os_error()
                )));
            }
            let mut original_mode = 0;
            if unsafe { GetConsoleMode(handle, &mut original_mode) } == 0 {
                return Err(std::io::Error::last_os_error());
            }
            let mode = Self {
                handle,
                original_mode,
                active: true,
            };
            let raw_mode = (original_mode
                & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT))
                | ENABLE_VIRTUAL_TERMINAL_INPUT;
            // Construct the guard first so a failed set/readback also restores.
            mode.set_windows_mode(raw_mode)?;
            Ok(mode)
        }
        #[cfg(unix)]
        {
            crossterm::terminal::enable_raw_mode()?;
            Ok(Self { active: true })
        }
    }

    #[cfg(windows)]
    fn set_windows_mode(&self, expected: u32) -> std::io::Result<()> {
        use windows_sys::Win32::System::Console::{GetConsoleMode, SetConsoleMode};

        // SAFETY: the validated console handle is borrowed for the observer's
        // lifetime, and the readback output points to a live u32.
        if unsafe { SetConsoleMode(self.handle, expected) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut actual = 0;
        if unsafe { GetConsoleMode(self.handle, &mut actual) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        if actual != expected {
            return Err(std::io::Error::other(format!(
                "fixture stdin {:?} mode: expected {expected:#x}, got {actual:#x}",
                self.handle
            )));
        }
        Ok(())
    }

    fn restore(&mut self) -> std::io::Result<()> {
        if self.active {
            #[cfg(windows)]
            self.set_windows_mode(self.original_mode)?;
            #[cfg(unix)]
            crossterm::terminal::disable_raw_mode()?;
            self.active = false;
        }
        Ok(())
    }
}

impl Drop for P10RawInputMode {
    fn drop(&mut self) {
        if let Err(error) = self.restore() {
            eprintln!("failed to restore fixture stdin mode: {error}");
        }
    }
}

/// Filtered subprocess entrypoint. A normal test run returns before touching the
/// terminal. The activation path must be a direct fixture child of this run's temp
/// directory; all I/O paths are fixed filenames beneath that canonical directory.
#[test]
fn p10_fixture_input_observer_child() {
    use std::io::{Read, Write};

    let Some(base) = std::env::var_os(P10_OBSERVER_ENV) else {
        return;
    };
    let args: Vec<_> = std::env::args().collect();
    assert!(args
        .windows(2)
        .any(|args| args == ["--exact", "p10_fixture_input_observer_child"]));
    let base = PathBuf::from(base).canonicalize().unwrap();
    // Same root the fixture chose for its base (see `start_with_config_env`).
    let child_temp = if cfg!(target_os = "macos") {
        PathBuf::from("/tmp")
    } else {
        std::env::temp_dir()
    };
    let child_temp = child_temp.canonicalize().unwrap_or_else(|error| {
        panic!(
            "observer temp directory {child_temp:?} cannot be resolved: {error}; TEMP={:?}; TMP={:?}",
            std::env::var_os("TEMP"),
            std::env::var_os("TMP")
        )
    });
    assert_eq!(base.parent(), Some(child_temp.as_path()));
    assert!(base
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .starts_with("ksf-"));
    assert!(base.join("config").is_dir() && base.join("runtime").is_dir());
    assert!(base.join("p10-screen-0.ansi").is_file());

    struct RawMode(PathBuf, P10RawInputMode);
    impl Drop for RawMode {
        fn drop(&mut self) {
            if let Err(error) = self.1.restore() {
                eprintln!("failed to restore fixture stdin mode: {error}");
            }
            let _ = std::fs::write(self.0.join("p10-stopped"), b"stopped");
        }
    }
    let input_mode = P10RawInputMode::enable().expect("enable fixture raw byte input");
    let mut raw_mode = RawMode(base.clone(), input_mode);
    let mut input_file = std::fs::File::create(base.join("p10-input.bin")).unwrap();
    let (send, receive) = std::sync::mpsc::sync_channel(16);
    // Blocking console reads stay in this disposable subprocess, never the test
    // parent. The bounded main loop exits the process even if stdin stays open.
    thread::spawn(move || {
        let mut input = std::io::stdin().lock();
        let mut buffer = [0; 1024];
        while let Ok(length) = input.read(&mut buffer) {
            if length == 0 || send.send(buffer[..length].to_vec()).is_err() {
                break;
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(90);
    let mut screen_seq = 0;
    let mut recorded = 0;
    while !base.join("p10-stop").exists() {
        assert!(
            Instant::now() < deadline,
            "observer exceeded its 90s lifetime"
        );
        let screen = base.join(format!("p10-screen-{screen_seq}.ansi"));
        if screen.exists() {
            let screen = std::fs::read(screen).unwrap();
            assert!(screen.len() <= 4096);
            let mut output = std::io::stdout().lock();
            output.write_all(&screen).unwrap();
            output.flush().unwrap();
            std::fs::write(
                base.join(format!("p10-screen-{screen_seq}.ready")),
                b"ready",
            )
            .unwrap();
            screen_seq += 1;
        }
        if let Ok(bytes) = receive.recv_timeout(Duration::from_millis(20)) {
            recorded += bytes.len();
            assert!(recorded <= 65536, "fixture input exceeded its bound");
            input_file.write_all(&bytes).unwrap();
            input_file.flush().unwrap();
        }
    }
    raw_mode.1.restore().expect("restore fixture stdin mode");
    std::fs::write(base.join("p10-completed"), b"complete").unwrap();
}

impl Fixture {
    fn p10_ready_agent(
        &mut self,
        pane: &str,
        agent: &str,
        exe_name: &str,
    ) -> (P10InputObserver, serde_json::Value) {
        let observer = P10InputObserver::start(self, pane, agent, exe_name);
        self.report(pane, agent, "idle");
        let entry = self.digest_agent_until(pane, "idle with a clean composer", |entry| {
            entry["agent_status"] == "idle"
                && (agent == "gemini" || entry["owner_input"]["draft"] == "clean")
        });
        (observer, entry)
    }

    fn p10_run_hook(&self, pane: &str, agent: &str, action: &str, payload: &[u8]) -> Output {
        use std::io::Write;
        let assets = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/integration/assets")
            .join(agent);
        #[cfg(windows)]
        let mut command = {
            let mut command = Command::new("powershell");
            command
                .args([
                    "-NoProfile",
                    "-NonInteractive",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-File",
                ])
                .arg(assets.join("kelpie-agent-state.ps1"));
            command
        };
        #[cfg(unix)]
        let mut command = {
            let mut command = Command::new("sh");
            command.arg(assets.join("kelpie-agent-state.sh"));
            command
        };
        command.arg(action);
        self.isolate(&mut command);
        command
            .env("KELPIE_ENV", "1")
            .env("KELPIE_PANE_ID", pane)
            .env("KELPIE_BIN_PATH", env!("CARGO_BIN_EXE_kelpie"))
            .env_remove("CODEX_THREAD_ID")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().unwrap();
        child.stdin.take().unwrap().write_all(payload).unwrap();
        let status = wait_child_exit(&mut child, Duration::from_secs(30));
        let output = child.wait_with_output().unwrap();
        assert!(status.success(), "{agent} {action} hook failed: {status:?}");
        assert!(
            output.stderr.iter().all(u8::is_ascii_whitespace),
            "{agent} {action} hook stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn p10_report_session(
        &mut self,
        pane: &str,
        agent: &str,
        session_id: &str,
        start_source: &str,
    ) {
        self.seq += 1;
        self.cli_json(&[
            "pane",
            "report-agent-session",
            pane,
            "--source",
            &format!("kelpie:{agent}"),
            "--agent",
            agent,
            "--agent-session-id",
            session_id,
            "--session-start-source",
            start_source,
            "--seq",
            &self.seq.to_string(),
        ]);
    }

    fn p10_no_receipt(&self, submit_id: &str) {
        let unknown = self.cli_error(&["agent", "submit-status", submit_id]);
        assert_eq!(unknown["error"]["code"], "submit_not_found", "{unknown}");
    }

    fn p10_no_prompt_log(&self, marker: &str) {
        let path = self
            .config_home
            .join(app_dir_name())
            .join("kelpie-server.log");
        let log = std::fs::read_to_string(&path).unwrap();
        assert!(
            !log.contains(marker),
            "server log leaked synthetic prompt: {}",
            path.display()
        );
    }
}

#[test]
fn b7_codex_prompt_hook_confirms_written_receipt() {
    let mut fixture = Fixture::start_with_supervisor();
    let pane = fixture.pane();
    let (observer, agent) = fixture.p10_ready_agent(&pane, "codex", "codex");
    let text = comment_line("p10-codex-prompt-marker");
    let target = target_ref_arg(&agent);
    fixture.submit_ok(&target, "p10-codex-prompt", &text);
    let written = fixture.wait_receipt("p10-codex-prompt", &text, "written", |r| {
        r["state"] == "written"
    });
    assert_eq!(written["acknowledged"], "unsupported");
    observer.wait_bytes(&fixture, &pane, format!("{text}\r").as_bytes());
    let payload = serde_json::to_vec(&serde_json::json!({
        "session_id": "p10-codex-session", "transcript_path": "fixture.jsonl",
        "cwd": fixture.base, "permission_mode": "default",
        "hook_event_name": "UserPromptSubmit", "prompt": format!("{text}\n")
    }))
    .unwrap();
    let output = fixture.p10_run_hook(&pane, "codex", "prompt", &payload);
    assert!(output.stdout.is_empty(), "Codex hook must remain silent");
    let confirmed = fixture.wait_receipt("p10-codex-prompt", &text, "confirmed", |r| {
        r["acknowledged"] == "confirmed"
    });
    assert_eq!(confirmed["state"], "written");
    assert_eq!(confirmed["target_ref"], agent["target_ref"]);
    assert!(confirmed["acknowledged_at_ms"].is_u64());
    let duplicate = fixture.p10_run_hook(&pane, "codex", "prompt", &payload);
    assert!(duplicate.stdout.is_empty());
    assert_eq!(fixture.submit_status("p10-codex-prompt", &text), confirmed);
    fixture.p10_no_prompt_log("p10-codex-prompt-marker");
    observer.assert_bytes_through_barrier(&fixture, &pane, format!("{text}\r").as_bytes());
}

#[test]
fn b7_codex_reset_bumps_once_and_verifies_lazily() {
    let mut fixture = Fixture::start_with_supervisor();
    let pane = fixture.pane();
    let (observer, _) = fixture.p10_ready_agent(&pane, "codex", "codex");
    fixture.p10_report_session(&pane, "codex", "p10-codex-old", "startup");
    let old = fixture.digest_agent_until(&pane, "old Codex session", |a| {
        a["agent_session"]["value"] == "p10-codex-old"
    });
    let generation = old["target_ref"]["generation"].as_u64().unwrap();
    let lineage_len = old["lineage"].as_array().unwrap().len();
    let target = target_ref_arg(&old);
    fixture.control_ok("reset", &target, "p10-codex-reset");
    let written = fixture.wait_receipt("p10-codex-reset", NO_TEXT, "written", |r| {
        r["state"] == "written"
    });
    assert_eq!(written["kind"], "reset");
    assert_eq!(written["reset"], "pending");
    assert!(written["new_session"].is_null() && written["new_generation"].is_null());
    observer.wait_bytes(&fixture, &pane, b"/clear\r");
    let pending = fixture.digest_agent_until(&pane, "reset-written generation", |a| {
        a["target_ref"]["generation"].as_u64() == Some(generation + 1)
    });
    let lineage = pending["lineage"].as_array().unwrap();
    assert_eq!(lineage.len(), lineage_len + 1);
    assert_eq!(lineage.last().unwrap()["reason"], "reset_written");
    fixture.p10_report_session(&pane, "codex", "p10-codex-new", "clear");
    let verified = fixture.wait_receipt("p10-codex-reset", NO_TEXT, "verified", |r| {
        r["reset"] == "verified"
    });
    assert_eq!(
        verified["new_session"],
        serde_json::json!({"kind": "id", "value": "p10-codex-new"})
    );
    assert_eq!(verified["new_generation"].as_u64(), Some(generation + 1));
    assert_eq!(verified["target_ref"], old["target_ref"]);
    let after = fixture.digest_agent_until(&pane, "new Codex session", |a| {
        a["agent_session"]["value"] == "p10-codex-new"
    });
    assert_eq!(
        after["target_ref"]["generation"],
        pending["target_ref"]["generation"]
    );
    assert_eq!(after["lineage"], pending["lineage"]);
    assert_eq!(
        fixture.control_ok("reset", &target, "p10-codex-reset"),
        verified
    );
    observer.assert_bytes_through_barrier(&fixture, &pane, b"/clear\r");
}

#[test]
fn b7_cursor_prompt_hook_confirms_and_refreshes_identity() {
    let mut fixture = Fixture::start_with_supervisor();
    let pane = fixture.pane();
    let (observer, _) = fixture.p10_ready_agent(&pane, "cursor", "cursor-agent");
    let startup = serde_json::to_vec(&serde_json::json!({"hook_event_name": "sessionStart", "conversation_id": "p10-cursor-old"})).unwrap();
    let output = fixture.p10_run_hook(&pane, "cursor", "session", &startup);
    #[cfg(windows)]
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap(),
        serde_json::json!({})
    );
    #[cfg(unix)]
    assert!(output.stdout.is_empty());
    let old = fixture.digest_agent_until(&pane, "legacy Cursor session hook identity", |a| {
        a["agent_session"]["value"] == "p10-cursor-old"
    });
    assert_eq!(old["agent_session"]["source"], "kelpie:cursor");
    assert_eq!(old["agent_session"]["agent"], "cursor");
    let text = comment_line("p10-cursor-prompt-marker");
    fixture.submit_ok(&target_ref_arg(&old), "p10-cursor-prompt", &text);
    let written = fixture.wait_receipt("p10-cursor-prompt", &text, "written", |r| {
        r["state"] == "written"
    });
    assert_eq!(written["acknowledged"], "unsupported");
    observer.wait_bytes(&fixture, &pane, format!("{text}\r").as_bytes());
    let mut payload = b"\xef\xbb\xbf".to_vec();
    payload.extend(serde_json::to_vec(&serde_json::json!({"hook_event_name": "beforeSubmitPrompt", "conversation_id": "p10-cursor-new", "prompt": text})).unwrap());
    let output = fixture.p10_run_hook(&pane, "cursor", "prompt", &payload);
    // from_slice rejects a second JSON value as trailing data.
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap(),
        serde_json::json!({"continue": true})
    );
    let confirmed = fixture.wait_receipt("p10-cursor-prompt", &text, "confirmed", |r| {
        r["acknowledged"] == "confirmed"
    });
    assert_eq!(confirmed["state"], "written");
    assert!(confirmed["acknowledged_at_ms"].is_u64());
    assert_eq!(
        confirmed["target_ref"], old["target_ref"],
        "acknowledgement precedes session replacement"
    );
    let refreshed = fixture.digest_agent_until(&pane, "prompt-refreshed Cursor session", |a| {
        a["agent_session"]["value"] == "p10-cursor-new"
    });
    assert_eq!(
        refreshed["agent_session"],
        serde_json::json!({"source": "kelpie:cursor", "agent": "cursor", "kind": "id", "value": "p10-cursor-new"})
    );
    assert_eq!(
        refreshed["target_ref"]["generation"].as_u64(),
        old["target_ref"]["generation"].as_u64().map(|g| g + 1)
    );
    let duplicate = fixture.p10_run_hook(&pane, "cursor", "prompt", &payload);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&duplicate.stdout).unwrap(),
        serde_json::json!({"continue": true})
    );
    assert_eq!(fixture.submit_status("p10-cursor-prompt", &text), confirmed);
    let after = fixture.digest_agent(&pane);
    assert_eq!(after["agent_session"], refreshed["agent_session"]);
    assert_eq!(after["target_ref"], refreshed["target_ref"]);
    assert_eq!(after["lineage"], refreshed["lineage"]);
    fixture.p10_no_prompt_log("p10-cursor-prompt-marker");
    observer.assert_bytes_through_barrier(&fixture, &pane, format!("{text}\r").as_bytes());
}

const P10_CURSOR_OWN_TEXT: &str = "p10 synthetic restored draft";

fn p10_interrupted_cursor(
    fixture: &mut Fixture,
    pane: &str,
    restored: &str,
) -> (P10InputObserver, serde_json::Value, Vec<u8>) {
    let (mut observer, clean) = fixture.p10_ready_agent(pane, "cursor", "cursor-agent");
    let target = target_ref_arg(&clean);
    fixture.submit_ok(&target, "p10-cursor-own", P10_CURSOR_OWN_TEXT);
    fixture.wait_receipt(
        "p10-cursor-own",
        P10_CURSOR_OWN_TEXT,
        "written submit",
        |r| r["state"] == "written",
    );
    let mut bytes = format!("{P10_CURSOR_OWN_TEXT}\r").into_bytes();
    observer.wait_bytes(fixture, pane, &bytes);
    fixture.report(pane, "cursor", "working");
    fixture.digest_agent_until(pane, "working Cursor", |a| a["agent_status"] == "working");
    fixture.control_ok("interrupt", &target, "p10-cursor-interrupt");
    let interrupted =
        fixture.wait_receipt("p10-cursor-interrupt", NO_TEXT, "written interrupt", |r| {
            r["state"] == "written"
        });
    assert_eq!(interrupted["kind"], "interrupt");
    bytes.push(0x1b);
    observer.wait_bytes(fixture, pane, &bytes);
    fixture.report(pane, "cursor", "idle");
    observer.show(fixture, pane, &p10_cursor_dirty_screen(restored));
    let dirty = fixture.digest_agent_until(pane, "idle restored dirty composer", |a| {
        a["agent_status"] == "idle" && a["owner_input"]["draft"] == "dirty"
    });
    assert_eq!(dirty["target_ref"], clean["target_ref"]);
    (observer, dirty, bytes)
}

#[test]
fn b6_cursor_clear_draft_writes_ctrl_u_for_own_text() {
    let mut fixture = Fixture::start_with_supervisor();
    let pane = fixture.pane();
    let (observer, dirty, mut bytes) =
        p10_interrupted_cursor(&mut fixture, &pane, P10_CURSOR_OWN_TEXT);
    let target = target_ref_arg(&dirty);
    fixture.control_ok("clear-draft", &target, "p10-cursor-clear");
    let written = fixture.wait_receipt("p10-cursor-clear", NO_TEXT, "written clear", |r| {
        r["state"] == "written"
    });
    assert_eq!(written["kind"], "clear_draft");
    assert_eq!(written["acknowledged"], "unsupported");
    bytes.push(0x15);
    observer.wait_bytes(&fixture, &pane, &bytes);
    assert_eq!(
        fixture.control_ok("clear-draft", &target, "p10-cursor-clear"),
        written
    );
    observer.assert_bytes_through_barrier(&fixture, &pane, &bytes);
}

#[test]
fn b6_cursor_clear_draft_refuses_owner_text() {
    let mut fixture = Fixture::start_with_supervisor();
    let pane = fixture.pane();
    let (observer, dirty, bytes) =
        p10_interrupted_cursor(&mut fixture, &pane, "p10 different owner draft");
    let error = fixture.control_error("clear-draft", &target_ref_arg(&dirty), "p10-cursor-refused");
    assert_eq!(error["error"]["code"], "draft_not_own", "{error}");
    fixture.p10_no_receipt("p10-cursor-refused");
    observer.assert_bytes_through_barrier(&fixture, &pane, &bytes);
}

#[test]
fn b7_gemini_submit_is_unsupported() {
    let mut fixture = Fixture::start_with_supervisor();
    let pane = fixture.pane();
    let (observer, agent) = fixture.p10_ready_agent(&pane, "gemini", "gemini");
    let error = fixture.submit_error(
        &target_ref_arg(&agent),
        "p10-gemini-submit",
        "p10 unsupported marker",
    );
    assert_eq!(error["error"]["code"], "unsupported_agent", "{error}");
    fixture.p10_no_receipt("p10-gemini-submit");
    observer.assert_bytes_through_barrier(&fixture, &pane, b"");
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
    let mut fixture = Fixture::start_with_supervisor_env(&[("KELPIE_DEV_SUBMIT_HOLD_MS", &hold)]);
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
/// the way an attached `kelpie` client does, so owner keys reach the server's
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
        let client_socket = fixture.runtime_dir.join("kelpie-client.sock");
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
    let mut fixture = Fixture::start_with_supervisor_env(&[("KELPIE_DEV_SUBMIT_HOLD_MS", &hold)]);
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
            "kelpie:claude",
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

// ---------------------------------------------------------------------------
// Local slice part 3 S10: `agent resume`.
// ---------------------------------------------------------------------------

impl Fixture {
    /// Put the stand-in Claude first on the pane shell's PATH, so the bare `claude`
    /// command `agent.start` types resolves to it.
    fn stand_in_claude_on_path(&self, pane: &str, tag: &str) {
        let stand_in = self.write_stand_in_claude();
        let bin = stand_in.parent().unwrap().display().to_string();
        let marker = format!("path-ready-{tag}");
        let command = if cfg!(windows) {
            format!("set \"PATH={bin};%PATH%\" & echo {marker}")
        } else {
            format!("export PATH='{bin}':\"$PATH\"; echo {marker}")
        };
        self.run_and_wait(pane, &command, &marker);
    }

    fn resume_args<'a>(pane: &'a str, session_id: &'a str, submit_id: &'a str) -> [&'a str; 8] {
        [
            "agent",
            "resume",
            "--pane",
            pane,
            "--session-id",
            session_id,
            "--submit-id",
            submit_id,
        ]
    }

    fn resume_ok(&self, pane: &str, session_id: &str, submit_id: &str) -> serde_json::Value {
        let response = self.cli_json(&Self::resume_args(pane, session_id, submit_id));
        receipt_of(&response, NO_TEXT)
    }

    fn resume_error(&self, pane: &str, session_id: &str, submit_id: &str) -> serde_json::Value {
        self.cli_error(&Self::resume_args(pane, session_id, submit_id))
    }

    /// Wait until the launched stand-in is the pane's detected Claude; returns the entry.
    fn launched_claude(&self, pane: &str) -> serde_json::Value {
        let launched = |agent: &serde_json::Value| {
            agent["agent"] == "claude"
                && agent["lineage"].as_array().is_some_and(|lineage| {
                    lineage
                        .iter()
                        .any(|entry| entry["reason"] == "agent_started")
                })
        };
        let mut last = serde_json::Value::Null;
        // Bound sized for the isolated runner: on Windows `agent.start` launches through
        // `powershell.exe ... Start-Process`, and with USERPROFILE redirected to an empty
        // sandbox dir the first Start-Process in each PowerShell process was measured at
        // 11-17 s (about 1.4 s with the real profile), more under a parallel fixture run.
        let ok = wait_until(Duration::from_secs(60), || {
            if let Some(agent) = Self::digest_entry(&self.digest(), pane) {
                let done = launched(&agent);
                last = agent;
                return done;
            }
            false
        });
        assert!(
            ok,
            "the launched stand-in claude was never detected: {last}\npane screen:\n{}\nagent get: {}",
            self.screen_text(pane),
            String::from_utf8_lossy(&self.cli(&["agent", "get", pane]).stdout)
        );
        last
    }

    fn agent_count(&self) -> usize {
        self.cli_json(&["agent", "list"])["result"]["agents"]
            .as_array()
            .map_or(0, Vec::len)
    }
}

/// S10: resume launches the stand-in through the agent start path (`launched`);
/// the reported session with the requested id and source `resume` verifies it
/// and the receipt carries the generation after the launch; a retry returns the
/// same receipt and launches nothing; a new id into the now-busy pane is
/// rejected `agent_running` before any launch.
///
/// Limit: the stand-in does not really resume a session; the evidence is driven
/// with `pane report-agent-session`. "No second launch" is observed as an
/// unchanged receipt, agent count and generation (on Windows the typed start
/// command is an encoded PowerShell line, so the argv is not on screen).
#[test]
fn s10_resume_launches_once_and_verifies_matching_session() {
    let mut fixture = Fixture::start_with_supervisor();
    let pane = fixture.pane();
    fixture.stand_in_claude_on_path(&pane, "s10");
    assert_eq!(fixture.agent_count(), 0);

    let accepted = fixture.resume_ok(&pane, "s10-session", "s10-r");
    assert_eq!(accepted["kind"], "resume", "{accepted}");
    assert_eq!(accepted["state"], "launched", "{accepted}");
    assert_eq!(accepted["resume"], "pending", "{accepted}");
    assert_eq!(accepted["session_id"], "s10-session", "{accepted}");
    assert!(accepted["launched_at_ms"].is_u64(), "{accepted}");

    let agent = fixture.launched_claude(&pane);
    let generation = agent["target_ref"]["generation"].as_u64().unwrap();
    let started = fixture.wait_receipt("s10-r", NO_TEXT, "the new generation", |receipt| {
        receipt["new_generation"].is_u64()
    });
    assert_eq!(
        started["new_generation"].as_u64(),
        Some(generation),
        "{started}"
    );
    assert_eq!(started["resume"], "pending", "{started}");

    fixture.report_session_start(&pane, "s10-session", "resume");
    let verified = fixture.wait_receipt("s10-r", NO_TEXT, "verified", |receipt| {
        receipt["resume"] == "verified"
    });
    assert_eq!(verified["state"], "launched", "{verified}");
    assert_eq!(verified["session_start_source"], "resume", "{verified}");
    assert!(
        verified.get("reported_session").is_none_or(|v| v.is_null()),
        "{verified}"
    );
    let current = fixture.digest_agent(&pane);
    assert_eq!(
        verified["new_generation"], current["target_ref"]["generation"],
        "receipt reports the current target generation: {current}"
    );
    let agents = fixture.agent_count();
    assert_eq!(agents, 1);

    let again = fixture.resume_ok(&pane, "s10-session", "s10-r");
    assert_eq!(again, verified, "retry returns the same receipt");
    thread::sleep(Duration::from_millis(1500));
    assert_eq!(fixture.agent_count(), agents, "retry launched again");
    assert_eq!(
        fixture.digest_agent(&pane)["target_ref"]["generation"],
        current["target_ref"]["generation"]
    );
    assert_eq!(fixture.submit_status("s10-r", NO_TEXT), verified);

    let busy = fixture.resume_error(&pane, "s10-session", "s10-busy");
    assert_eq!(busy["error"]["code"], "agent_running", "{busy}");
    let unknown = fixture.cli_error(&["agent", "submit-status", "s10-busy"]);
    assert_eq!(unknown["error"]["code"], "submit_not_found", "{unknown}");
}

/// S10: a different session id reported first is a `mismatch` that records the
/// reported session; the requested id reported afterwards does not turn it
/// into a success.
#[test]
fn s10_resume_different_session_is_mismatch() {
    let mut fixture = Fixture::start_with_supervisor();
    let pane = fixture.pane();
    fixture.stand_in_claude_on_path(&pane, "s10m");
    let accepted = fixture.resume_ok(&pane, "s10m-wanted", "s10m-r");
    assert_eq!(accepted["state"], "launched", "{accepted}");
    fixture.launched_claude(&pane);

    fixture.report_session_start(&pane, "s10m-fresh", "startup");
    let mismatch = fixture.wait_receipt("s10m-r", NO_TEXT, "mismatch", |receipt| {
        receipt["resume"] == "mismatch"
    });
    assert_eq!(
        mismatch["reported_session"]["value"], "s10m-fresh",
        "{mismatch}"
    );
    assert_eq!(mismatch["reported_session"]["kind"], "id", "{mismatch}");
    assert_eq!(mismatch["session_start_source"], "startup", "{mismatch}");
    assert_eq!(mismatch["session_id"], "s10m-wanted", "{mismatch}");

    fixture.report_session_start(&pane, "s10m-wanted", "resume");
    thread::sleep(Duration::from_millis(500));
    let after = fixture.submit_status("s10m-r", NO_TEXT);
    assert_eq!(after["resume"], "mismatch", "{after}");
}
