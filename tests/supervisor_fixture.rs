//! Cross-platform supervisor fixture.
//!
//! Starts a real headless server, opens a pane running the platform shell, and
//! drives agent state through `herdr pane report-agent`, exactly as an agent
//! integration hook would. Everything goes through the `herdr` CLI, so the
//! same tests run on Windows (named pipe) and Unix (socket).
//!
//! The `current_*` tests characterize behaviour that the supervisor slice of
//! the fork plan changes (status facts and delivery receipts). They pin what
//! herdr reports today so the incidents are reproducible; when that slice
//! lands, these assertions flip to the new facts.

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
    socket_path: PathBuf,
    server: Option<Child>,
    seq: u64,
}

impl Fixture {
    fn start() -> Self {
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
        let socket_path = runtime_dir.join("herdr.sock");
        let app_config = config_home.join(app_dir_name());
        std::fs::create_dir_all(&app_config).unwrap();
        std::fs::create_dir_all(&runtime_dir).unwrap();
        std::fs::write(
            app_config.join("config.toml"),
            format!(
                "onboarding = false\n[terminal]\ndefault_shell = {:?}\nshell_mode = \"non_login\"\n",
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
