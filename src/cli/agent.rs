use std::collections::BTreeMap;
use std::io::Write;
use std::time::{Duration, Instant};

use crate::api::schema::supervisor::{
    AgentAnswerParams, AgentControlParams, AgentResumeParams, AgentSubmitParams,
    AgentSubmitStatusParams, AgentUndelegateParams, AgentsDigestParams, DigestFilter, TargetRef,
};
use crate::api::schema::{
    AgentReadParams, AgentRenameParams, AgentStartParams, AgentTarget, AgentWaitParams,
    EmptyParams, ErrorBody, ErrorResponse, Method, PaneProcessInfoParams, PaneTarget, ReadFormat,
    ReadSource, Request,
};

const AGENT_START_POLL_INTERVAL: Duration = Duration::from_millis(100);
const PANE_SHELL_READINESS_RETRY_TIMEOUT: Duration = Duration::from_secs(2);

pub(super) fn run_agent_command(args: &[String]) -> std::io::Result<i32> {
    let Some(subcommand) = args.first().map(|arg| arg.as_str()) else {
        print_agent_help();
        return Ok(2);
    };

    match subcommand {
        "list" => agent_list(&args[1..]),
        "digest" => agent_digest(&args[1..]),
        "watch" => agent_watch(&args[1..]),
        "submit" => agent_submit(&args[1..]),
        "answer" => agent_answer(&args[1..]),
        "interrupt" => agent_interrupt(&args[1..]),
        "reset" => agent_reset(&args[1..]),
        "clear-draft" => agent_clear_draft(&args[1..]),
        "resume" => agent_resume(&args[1..]),
        "submit-status" => agent_submit_status(&args[1..]),
        "undelegate" => agent_undelegate(&args[1..]),
        "get" => agent_get(&args[1..]),
        "read" => agent_read(&args[1..]),
        "rename" => agent_rename(&args[1..]),
        "focus" => agent_focus(&args[1..]),
        "wait" => agent_wait(&args[1..]),
        "attach" => agent_attach(&args[1..]),
        "start" => agent_start(&args[1..]),
        "explain" => agent_explain(&args[1..]),
        "help" | "--help" | "-h" => {
            print_agent_help();
            Ok(0)
        }
        _ => {
            print_agent_help();
            Ok(2)
        }
    }
}

fn agent_explain(args: &[String]) -> std::io::Result<i32> {
    let mut file = None;
    let mut agent = None;
    let mut json = false;
    let mut verbose = false;
    let mut target = None;

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--file" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --file");
                    return Ok(2);
                };
                file = Some(value.clone());
                index += 2;
            }
            "--agent" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --agent");
                    return Ok(2);
                };
                agent = Some(value.clone());
                index += 2;
            }
            "--json" => {
                json = true;
                index += 1;
            }
            "--format" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --format");
                    return Ok(2);
                };
                match value.as_str() {
                    "json" => json = true,
                    "text" => json = false,
                    other => {
                        eprintln!("invalid --format: {other} (expected text or json)");
                        return Ok(2);
                    }
                }
                index += 2;
            }
            "--verbose" | "-v" => {
                verbose = true;
                index += 1;
            }
            "help" | "--help" | "-h" => {
                eprintln!("usage: kelpie agent explain <target> [--json|--verbose]");
                eprintln!(
                    "usage: kelpie agent explain --file PATH --agent LABEL [--json|--verbose]"
                );
                return Ok(0);
            }
            value if value.starts_with('-') => {
                eprintln!("unknown option: {value}");
                return Ok(2);
            }
            value => {
                if target.is_some() {
                    eprintln!("usage: kelpie agent explain <target> [--json]");
                    return Ok(2);
                }
                target = Some(value.to_string());
                index += 1;
            }
        }
    }

    let explain = if let Some(path) = file {
        if target.is_some() {
            eprintln!("usage: kelpie agent explain --file PATH --agent LABEL [--json]");
            return Ok(2);
        }
        let Some(agent_label) = agent else {
            eprintln!("kelpie agent explain --file requires --agent LABEL");
            return Ok(2);
        };
        let content = match std::fs::read_to_string(&path) {
            Ok(content) => content,
            Err(err) => {
                let response = ErrorResponse {
                    id: "cli:agent:explain".into(),
                    error: ErrorBody {
                        code: "agent_explain_file_read_failed".into(),
                        message: format!("failed to read agent explain file {path}: {err}"),
                    },
                };
                let response = serde_json::to_string(&response).map_err(std::io::Error::other)?;
                eprintln!("{response}");
                return Ok(1);
            }
        };
        crate::detect::manifest::explain_to_json_value(&crate::detect::manifest::explain_for_label(
            &agent_label,
            &content,
        ))
    } else {
        let Some(target) = target else {
            eprintln!("usage: kelpie agent explain <target> [--json]");
            eprintln!("usage: kelpie agent explain --file PATH --agent LABEL [--json]");
            return Ok(2);
        };
        if agent.is_some() {
            eprintln!("--agent is only valid with --file");
            return Ok(2);
        }

        let response = super::send_request(&Request {
            id: "cli:agent:explain".into(),
            method: Method::AgentExplain(AgentTarget {
                target: target.to_owned(),
            }),
        })?;
        if response.get("error").is_some() {
            eprintln!("{}", serde_json::to_string(&response).unwrap());
            return Ok(1);
        }
        response["result"]["explain"].clone()
    };

    if json {
        println!("{explain}");
    } else {
        print_agent_explain_text(&explain, verbose);
    }
    Ok(0)
}

fn print_agent_explain_text(explain: &serde_json::Value, verbose: bool) {
    println!("agent: {}", explain["agent"].as_str().unwrap_or("unknown"));
    println!("state: {}", explain["state"].as_str().unwrap_or("unknown"));
    println!(
        "manifest: {} {}",
        explain["manifest_source"].as_str().unwrap_or("none"),
        explain["manifest_version"].as_str().unwrap_or("unknown")
    );
    if let Some(rule) = explain["matched_rule"].as_object() {
        let rule_id = rule
            .get("id")
            .and_then(|value| value.as_str())
            .unwrap_or("-");
        println!(
            "rule: {} (region={} priority={})",
            rule_id,
            rule.get("region")
                .and_then(|value| value.as_str())
                .unwrap_or("-"),
            rule.get("priority")
                .and_then(|value| value.as_i64())
                .unwrap_or(0),
        );
        if let Some(preview) = matched_rule_region_preview(explain, rule_id) {
            println!("evidence: {preview:?}");
        }
    } else {
        println!("rule: none");
    }
    if let Some(reason) = explain["fallback_reason"].as_str() {
        println!("fallback_reason: {reason}");
    }
    if let Some(reason) = explain["screen_detection_skip_reason"].as_str() {
        println!("screen_detection_skip_reason: {reason}");
    }
    if let Some(reason) = explain["skipped_update_reason"].as_str() {
        println!("skipped_update_reason: {reason}");
    }
    if let Some(warning) = explain["warning"].as_str() {
        println!("warning: {warning}");
    }

    if !verbose {
        return;
    }

    println!(
        "visible: idle={} blocker={} working={}",
        explain["visible_idle"].as_bool().unwrap_or(false),
        explain["visible_blocker"].as_bool().unwrap_or(false),
        explain["visible_working"].as_bool().unwrap_or(false)
    );
    println!(
        "cached_remote_version: {}",
        explain["cached_remote_version"].as_str().unwrap_or("none")
    );
    println!(
        "local_override_shadowing_remote: {}",
        explain["local_override_shadowing_remote"]
            .as_bool()
            .unwrap_or(false)
    );
    if let Some(status) = explain["remote_update_status"].as_str() {
        println!("remote_update_status: {status}");
    }
    if let Some(error) = explain["remote_update_error"].as_str() {
        println!("remote_update_error: {error}");
    }
    if let Some(evaluated_rules) = explain["evaluated_rules"]
        .as_array()
        .filter(|rules| !rules.is_empty())
    {
        println!("evaluated_rules:");
        for rule in evaluated_rules {
            println!(
                "  {} {} priority={} region={} state={}",
                if rule["matched"].as_bool().unwrap_or(false) {
                    "✓"
                } else {
                    "✗"
                },
                rule["id"].as_str().unwrap_or("-"),
                rule["priority"].as_i64().unwrap_or(0),
                rule["region"].as_str().unwrap_or("-"),
                rule["state"].as_str().unwrap_or("unknown")
            );
            let evidence = &rule["evidence"];
            println!(
                "    matchers: contains={:?} regex={:?} line_regex={:?} all={} any={} not={}",
                evidence["contains"],
                evidence["regex"],
                evidence["line_regex"],
                evidence["all_count"].as_u64().unwrap_or(0),
                evidence["any_count"].as_u64().unwrap_or(0),
                evidence["not_count"].as_u64().unwrap_or(0)
            );
            println!(
                "    region: bytes={} preview={:?}",
                evidence["region_bytes"].as_u64().unwrap_or(0),
                evidence["region_preview"].as_str().unwrap_or("")
            );
        }
    }
}

fn matched_rule_region_preview<'a>(
    explain: &'a serde_json::Value,
    rule_id: &str,
) -> Option<&'a str> {
    explain["evaluated_rules"]
        .as_array()?
        .iter()
        .find(|rule| rule["id"].as_str() == Some(rule_id))?["evidence"]["region_preview"]
        .as_str()
        .filter(|preview| !preview.is_empty())
}

fn agent_start(args: &[String]) -> std::io::Result<i32> {
    let Some(name) = args.first() else {
        eprintln!("usage: kelpie agent start <name> --kind KIND --pane ID [--timeout MS] [-- <agent-args...>]");
        return Ok(2);
    };
    let separator = args
        .iter()
        .position(|arg| arg == "--")
        .unwrap_or(args.len());
    let mut kind = None;
    let mut pane_id = None;
    let mut timeout_ms = None;
    let mut index = 1;
    while index < separator {
        match args[index].as_str() {
            "--kind" => {
                let Some(value) = args.get(index + 1).filter(|_| index + 1 < separator) else {
                    eprintln!("missing value for --kind");
                    return Ok(2);
                };
                kind = Some(value.clone());
                index += 2;
            }
            "--pane" => {
                let Some(value) = args.get(index + 1).filter(|_| index + 1 < separator) else {
                    eprintln!("missing value for --pane");
                    return Ok(2);
                };
                pane_id = Some(super::normalize_pane_id(value));
                index += 2;
            }
            "--timeout" => {
                let Some(value) = args.get(index + 1).filter(|_| index + 1 < separator) else {
                    eprintln!("missing value for --timeout");
                    return Ok(2);
                };
                timeout_ms = match parse_timeout(value) {
                    Ok(timeout_ms) => Some(timeout_ms),
                    Err(exit_code) => return Ok(exit_code),
                };
                index += 2;
            }
            other => {
                eprintln!("unknown option: {other}");
                return Ok(2);
            }
        }
    }
    let Some(kind) = kind else {
        eprintln!("missing required --kind");
        return Ok(2);
    };
    let Some(pane_id) = pane_id else {
        eprintln!("missing required --pane");
        return Ok(2);
    };
    let Some(expected_kind) = crate::detect::parse_agent_label(&kind) else {
        eprintln!("unsupported interactive agent kind: {kind}");
        return Ok(2);
    };
    let expected_kind = crate::detect::agent_label(expected_kind).to_string();
    let agent_args = if separator < args.len() {
        args[separator + 1..].to_vec()
    } else {
        Vec::new()
    };
    let timeout = Duration::from_millis(timeout_ms.unwrap_or(30_000));
    let retryable_timeout = timeout > crate::app::AGENT_START_SETTLE_DELAY
        && timeout <= crate::app::MAX_AGENT_START_TIMEOUT;
    let pinned_terminal_id = pane_terminal_id(&pane_id)?;
    let mut retry_deadline = None;
    let mut previous_busy_response = None;
    let mut response = loop {
        if let Some(previous_busy_response) = previous_busy_response.as_ref() {
            let retry_expired = retry_deadline.is_some_and(|deadline| Instant::now() >= deadline);
            if retry_expired
                || pane_terminal_id(&pane_id)? != pinned_terminal_id
                || !pane_shell_is_initializing(&pane_id)?
            {
                return super::print_response(previous_busy_response);
            }
        }

        let response = super::send_request(&Request {
            id: "cli:agent:start".into(),
            method: Method::AgentStart(AgentStartParams {
                name: name.clone(),
                kind: kind.clone(),
                pane_id: pane_id.clone(),
                args: agent_args.clone(),
                timeout_ms,
            }),
        })?;
        if response.get("error").is_none() {
            break response;
        }
        if response["error"]["code"].as_str() != Some("agent_pane_busy")
            || !retryable_timeout
            || pinned_terminal_id.is_none()
            || pane_terminal_id(&pane_id)? != pinned_terminal_id
            || !pane_shell_is_initializing(&pane_id)?
        {
            return super::print_response(&response);
        }

        let deadline = *retry_deadline
            .get_or_insert_with(|| Instant::now() + PANE_SHELL_READINESS_RETRY_TIMEOUT);
        previous_busy_response = Some(response);
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            if let Some(previous_busy_response) = previous_busy_response.as_ref() {
                return super::print_response(previous_busy_response);
            }
        }
        std::thread::sleep(AGENT_START_POLL_INTERVAL.min(remaining));
    };

    let Some(expected_terminal_id) = response["result"]["agent"]["terminal_id"].as_str() else {
        return super::print_response(&cli_agent_error(
            "cli:agent:start",
            "agent_start_failed",
            "agent start response did not include terminal_id",
        ));
    };
    if pinned_terminal_id
        .as_deref()
        .is_some_and(|pinned| pinned != expected_terminal_id)
    {
        return super::print_response(&agent_name_lost_error("cli:agent:start", name));
    }
    let waited = wait_for_named_agent(
        name,
        &pane_id,
        timeout,
        &expected_kind,
        expected_terminal_id,
    );
    match waited {
        Ok(Ok(agent)) => {
            response["result"]["agent"] = agent;
            super::print_response(&response)
        }
        Ok(Err(error)) => super::print_response(&error),
        Err(err) => {
            print_agent_transport_error(err, "cli:agent:start", "agent_start_transport_failed")
        }
    }
}

fn agent_list(args: &[String]) -> std::io::Result<i32> {
    if !args.is_empty() {
        eprintln!("usage: kelpie agent list");
        return Ok(2);
    }

    super::print_response(&super::send_request(&Request {
        id: "cli:agent:list".into(),
        method: Method::AgentList(EmptyParams::default()),
    })?)
}

fn parse_agent_digest_args(args: &[String]) -> Result<AgentsDigestParams, String> {
    const USAGE: &str = "usage: kelpie agent digest [--page-cursor C] [--trace-id T]";
    let args = super::expand_equals_args(args, &["--page-cursor", "--trace-id"]);
    let mut page_cursor: Option<String> = None;
    let mut trace_id: Option<String> = None;
    let mut index = 0;
    while index < args.len() {
        let (flag, slot) = match args[index].as_str() {
            "--page-cursor" => ("--page-cursor", &mut page_cursor),
            "--trace-id" => ("--trace-id", &mut trace_id),
            _ => return Err(USAGE.to_string()),
        };
        if slot.is_some() {
            return Err(USAGE.to_string());
        }
        let Some(value) = args.get(index + 1) else {
            return Err(format!("missing value for {flag}"));
        };
        *slot = Some(value.clone());
        index += 2;
    }
    Ok(AgentsDigestParams {
        filter: None,
        page_cursor,
        trace_id,
    })
}

fn agent_digest(args: &[String]) -> std::io::Result<i32> {
    let params = match parse_agent_digest_args(args) {
        Ok(params) => params,
        Err(message) => {
            eprintln!("{message}");
            return Ok(2);
        }
    };
    super::print_response(&super::send_request(&Request {
        id: "cli:agent:digest".into(),
        method: Method::AgentsDigest(params),
    })?)
}

const WATCH_USAGE: &str =
    "kelpie agent watch [--pane ID] [--interval-ms N] [--max-changes N] [--timeout-ms N]";
const SUBMIT_USAGE: &str =
    "kelpie agent submit --target-ref <SERVER_EPOCH>:<PANE_ID>:<GENERATION> --submit-id ID <TEXT>";
const ANSWER_USAGE: &str = "kelpie agent answer --target-ref <SERVER_EPOCH>:<PANE_ID>:<GENERATION> --question-id Q --submit-id ID <TEXT>";
const INTERRUPT_USAGE: &str =
    "kelpie agent interrupt --target-ref <SERVER_EPOCH>:<PANE_ID>:<GENERATION> --submit-id ID";
const RESET_USAGE: &str =
    "kelpie agent reset --target-ref <SERVER_EPOCH>:<PANE_ID>:<GENERATION> --submit-id ID";
const CLEAR_DRAFT_USAGE: &str =
    "kelpie agent clear-draft --target-ref <SERVER_EPOCH>:<PANE_ID>:<GENERATION> --submit-id ID";
const RESUME_USAGE: &str =
    "kelpie agent resume --pane ID --session-id ID --submit-id ID [--agent LABEL]";
const SUBMIT_STATUS_USAGE: &str = "kelpie agent submit-status <SUBMIT_ID>";
const WATCH_DEFAULT_INTERVAL_MS: u64 = 500;
const WATCH_MIN_INTERVAL_MS: u64 = 100;
/// Bound on digest pages fetched per poll; a larger herd than this is truncated.
const WATCH_MAX_PAGES: usize = 256;
/// Bound on page-1 restarts per poll after a `stale page_cursor` error.
const WATCH_MAX_CURSOR_RESTARTS: usize = 3;
/// Bound on the `reason` text of a `stale` line.
const WATCH_MAX_REASON_CHARS: usize = 160;

#[derive(Debug, PartialEq)]
struct WatchArgs {
    pane: Option<String>,
    interval: Duration,
    max_changes: Option<u64>,
    timeout: Option<Duration>,
}

fn parse_agent_watch_args(args: &[String]) -> Result<WatchArgs, String> {
    let args = super::expand_equals_args(
        args,
        &["--pane", "--interval-ms", "--max-changes", "--timeout-ms"],
    );
    let usage = || format!("usage: {WATCH_USAGE}");
    let mut pane = None;
    let mut interval_ms = None;
    let mut max_changes = None;
    let mut timeout_ms = None;
    let mut index = 0;
    while index < args.len() {
        let flag = args[index].as_str();
        if !matches!(
            flag,
            "--pane" | "--interval-ms" | "--max-changes" | "--timeout-ms"
        ) {
            return Err(usage());
        }
        let Some(value) = args.get(index + 1) else {
            return Err(format!("missing value for {flag}"));
        };
        let number = || {
            value
                .parse::<u64>()
                .map_err(|_| format!("invalid value for {flag}: {value}"))
        };
        let repeated = match flag {
            "--pane" => pane.replace(super::normalize_pane_id(value)).is_some(),
            "--interval-ms" => interval_ms.replace(number()?).is_some(),
            "--max-changes" => max_changes.replace(number()?).is_some(),
            _ => timeout_ms.replace(number()?).is_some(),
        };
        if repeated {
            return Err(usage());
        }
        index += 2;
    }
    let interval_ms = interval_ms.unwrap_or(WATCH_DEFAULT_INTERVAL_MS);
    if interval_ms < WATCH_MIN_INTERVAL_MS {
        return Err(format!(
            "--interval-ms must be at least {WATCH_MIN_INTERVAL_MS}"
        ));
    }
    if max_changes == Some(0) {
        return Err("--max-changes must be at least 1".into());
    }
    if timeout_ms == Some(0) {
        return Err("--timeout-ms must be at least 1".into());
    }
    Ok(WatchArgs {
        pane,
        interval: Duration::from_millis(interval_ms),
        max_changes,
        timeout: timeout_ms.map(Duration::from_millis),
    })
}

fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

fn bounded_reason(reason: &str) -> String {
    reason.chars().take(WATCH_MAX_REASON_CHARS).collect()
}

/// The bounded field set `agent watch` compares between polls.
fn watch_view(agent: &serde_json::Value) -> serde_json::Value {
    let open_question_ids: Vec<serde_json::Value> = agent["pending_questions"]
        .as_array()
        .map(|questions| {
            questions
                .iter()
                .filter(|question| question.get("resolution").is_none_or(|r| r.is_null()))
                .map(|question| question["question_id"].clone())
                .collect()
        })
        .unwrap_or_default();
    serde_json::json!({
        "target_ref": agent["target_ref"],
        "agent_status": agent["agent_status"],
        "foreground": agent["foreground"]["value"],
        "background": agent["background"]["value"],
        "completion": agent["completion"],
        "blocking_reason": agent["blocking_reason"],
        "open_question_ids": open_question_ids,
        "draft": agent["owner_input"]["draft"],
        "handoff_armed": agent["owner_input"]["handoff_armed"].as_bool().unwrap_or(false),
        "delegation": watch_delegation(&agent["owner_input"]["delegation"]),
    })
}

/// Bounded delegation view: whether the owner delegated the pane, and until when.
fn watch_delegation(delegation: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "active": delegation["active"].as_bool().unwrap_or(false),
        "permanent": delegation["permanent"].as_bool().unwrap_or(false),
        "expires_at_ms": delegation["expires_at_ms"].as_u64(),
    })
}

/// True when a pane's bounded view differs from the last one printed.
fn watch_view_changed(last: Option<&serde_json::Value>, view: &serde_json::Value) -> bool {
    last != Some(view)
}

/// One read-only `agents.digest` poll across every page. Returns the highest
/// `as_of_seq` seen and the bounded view of each agent keyed by pane id, or a
/// bounded failure reason. A `stale page_cursor` restarts paging from page 1.
fn poll_watch_digest(
    pane: Option<&str>,
) -> Result<(u64, BTreeMap<String, serde_json::Value>), String> {
    let mut restarts = 0;
    'restart: loop {
        let mut views = BTreeMap::new();
        let mut as_of_seq = 0;
        let mut page_cursor = None;
        for _ in 0..WATCH_MAX_PAGES {
            let request = Request {
                id: "cli:agent:watch".into(),
                method: Method::AgentsDigest(AgentsDigestParams {
                    filter: pane.map(|pane| DigestFilter {
                        pane_ids: Some(vec![pane.to_string()]),
                    }),
                    page_cursor: page_cursor.take(),
                    trace_id: None,
                }),
            };
            let response = match super::send_request_unchecked(&request) {
                Ok(response) => response,
                Err(err) => {
                    return Err(match super::server_not_running_reported_response(&err) {
                        Some(reported) => reported.error.code.clone(),
                        None => bounded_reason(&format!("transport: {err}")),
                    });
                }
            };
            if let Some(error) = response.get("error") {
                let message = error["message"].as_str().unwrap_or_default();
                if message == "stale page_cursor" && restarts < WATCH_MAX_CURSOR_RESTARTS {
                    restarts += 1;
                    continue 'restart;
                }
                let code = error["code"].as_str().unwrap_or("error");
                return Err(bounded_reason(&format!("{code}: {message}")));
            }
            let result = &response["result"];
            as_of_seq = as_of_seq.max(result["as_of_seq"].as_u64().unwrap_or_default());
            for agent in result["agents"].as_array().into_iter().flatten() {
                let Some(pane_id) = agent["target_ref"]["pane_id"].as_str() else {
                    continue;
                };
                if pane.is_some_and(|pane| pane != pane_id) {
                    continue;
                }
                views.insert(pane_id.to_string(), watch_view(agent));
            }
            match result["next_page"].as_str() {
                Some(next) => page_cursor = Some(next.to_string()),
                None => return Ok((as_of_seq, views)),
            }
        }
        return Ok((as_of_seq, views));
    }
}

/// Prints one JSON line; false when stdout is gone (the reader quit).
fn emit_watch_line(line: &serde_json::Value) -> bool {
    let mut out = std::io::stdout().lock();
    writeln!(out, "{line}").and_then(|()| out.flush()).is_ok()
}

/// `kelpie agent watch`: read-only. Polls `agents.digest` and prints one JSON
/// line per agent whose bounded field set changed, `stale`/`recovered` lines
/// around request failures, and a final `timeout` line when `--timeout-ms`
/// elapses. Sends no writes of any kind.
fn agent_watch(args: &[String]) -> std::io::Result<i32> {
    let watch = match parse_agent_watch_args(args) {
        Ok(watch) => watch,
        Err(message) => {
            eprintln!("{message}");
            return Ok(2);
        }
    };
    let deadline = watch.timeout.map(|timeout| Instant::now() + timeout);
    let mut last: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    let mut changes: u64 = 0;
    let mut stale_since: Option<Instant> = None;
    let mut last_as_of_seq: Option<u64> = None;
    let mut last_ok_ms: Option<u64> = None;
    loop {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            emit_watch_line(&serde_json::json!({"type": "timeout", "changes": changes}));
            return Ok(0);
        }
        match poll_watch_digest(watch.pane.as_deref()) {
            Ok((as_of_seq, views)) => {
                if let Some(since) = stale_since.take() {
                    let line = serde_json::json!({
                        "type": "recovered",
                        "as_of_seq": as_of_seq,
                        "stale_ms": u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX),
                    });
                    if !emit_watch_line(&line) {
                        return Ok(0);
                    }
                }
                last_as_of_seq = Some(as_of_seq);
                last_ok_ms = Some(unix_now_ms());
                let mut lines = Vec::new();
                for (pane_id, view) in &views {
                    if watch_view_changed(last.get(pane_id), view) {
                        let mut line = view.clone();
                        line["type"] = "change".into();
                        line["pane_id"] = pane_id.as_str().into();
                        line["as_of_seq"] = as_of_seq.into();
                        lines.push(line);
                    }
                }
                for pane_id in last.keys().filter(|pane_id| !views.contains_key(*pane_id)) {
                    lines.push(serde_json::json!({
                        "type": "removed",
                        "pane_id": pane_id,
                        "as_of_seq": as_of_seq,
                    }));
                }
                for line in lines {
                    if !emit_watch_line(&line) {
                        return Ok(0);
                    }
                    changes += 1;
                    if watch.max_changes.is_some_and(|max| changes >= max) {
                        return Ok(0);
                    }
                }
                last = views;
            }
            Err(reason) => {
                if stale_since.is_none() {
                    stale_since = Some(Instant::now());
                    let line = serde_json::json!({
                        "type": "stale",
                        "reason": reason,
                        "last_as_of_seq": last_as_of_seq,
                        "last_ok_ms": last_ok_ms,
                    });
                    if !emit_watch_line(&line) {
                        return Ok(0);
                    }
                }
            }
        }
        let pause = match deadline {
            Some(deadline) => watch
                .interval
                .min(deadline.saturating_duration_since(Instant::now())),
            None => watch.interval,
        };
        std::thread::sleep(pause);
    }
}

/// Parses `<server_epoch>:<pane_id>:<generation>`. Pane ids contain `:`
/// (`w1:p1`), so the epoch is the first segment, the generation the last, and
/// the pane id everything between.
fn parse_target_ref(value: &str) -> Result<TargetRef, String> {
    let invalid = || {
        format!("invalid --target-ref: {value} (expected <server_epoch>:<pane_id>:<generation>)")
    };
    let (server_epoch, rest) = value.split_once(':').ok_or_else(invalid)?;
    let (pane_id, generation) = rest.rsplit_once(':').ok_or_else(invalid)?;
    if server_epoch.is_empty() || pane_id.is_empty() {
        return Err(invalid());
    }
    let generation = generation.parse::<u64>().map_err(|_| invalid())?;
    Ok(TargetRef {
        server_epoch: server_epoch.to_string(),
        pane_id: pane_id.to_string(),
        generation,
    })
}

/// Flags and positional text of one supervised write command.
#[derive(Debug, PartialEq)]
struct SupervisedArgs {
    target_ref: TargetRef,
    submit_id: String,
    question_id: Option<String>,
    text: Option<String>,
}

/// Shared parser for `agent submit|answer|interrupt|reset|clear-draft`: `--target-ref` (complete,
/// never refreshed) and `--submit-id` are required; `--question-id` only when
/// `question`; exactly one positional text only when `text` names the command.
fn parse_supervised_args(
    args: &[String],
    usage_text: &str,
    question: bool,
    text_command: Option<&str>,
) -> Result<SupervisedArgs, String> {
    let flags: &[&str] = if question {
        &["--target-ref", "--submit-id", "--question-id"]
    } else {
        &["--target-ref", "--submit-id"]
    };
    let args = super::expand_equals_args(args, flags);
    let usage = || format!("usage: {usage_text}");
    let mut target_ref = None;
    let mut submit_id: Option<String> = None;
    let mut question_id: Option<String> = None;
    let mut text: Option<String> = None;
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        match arg {
            "--target-ref" | "--submit-id" | "--question-id" if flags.contains(&arg) => {
                let Some(value) = args.get(index + 1) else {
                    return Err(format!("missing value for {arg}"));
                };
                let repeated = match arg {
                    "--target-ref" => target_ref.replace(parse_target_ref(value)?).is_some(),
                    "--submit-id" => submit_id.replace(value.clone()).is_some(),
                    _ => question_id.replace(value.clone()).is_some(),
                };
                if repeated {
                    return Err(usage());
                }
                index += 2;
            }
            "--" if text_command.is_some() => {
                let [value] = &args[index + 1..] else {
                    return Err(usage());
                };
                if text.replace(value.clone()).is_some() {
                    return Err(usage());
                }
                index = args.len();
            }
            value if value.starts_with('-') && value != "-" => return Err(usage()),
            value => {
                if text_command.is_none() || text.replace(value.to_string()).is_some() {
                    return Err(usage());
                }
                index += 1;
            }
        }
    }
    let Some(target_ref) = target_ref else {
        return Err("missing required --target-ref".into());
    };
    let Some(submit_id) = submit_id else {
        return Err("missing required --submit-id".into());
    };
    if question && question_id.is_none() {
        return Err("missing required --question-id".into());
    }
    if let Some(command) = text_command {
        if text.is_none() {
            return Err(format!("agent {command} requires text"));
        }
    }
    Ok(SupervisedArgs {
        target_ref,
        submit_id,
        question_id,
        text,
    })
}

fn parse_agent_submit_args(args: &[String]) -> Result<AgentSubmitParams, String> {
    let parsed = parse_supervised_args(args, SUBMIT_USAGE, false, Some("submit"))?;
    Ok(AgentSubmitParams {
        target_ref: parsed.target_ref,
        text: parsed.text.unwrap_or_default(),
        submit_id: parsed.submit_id,
    })
}

fn parse_agent_answer_args(args: &[String]) -> Result<AgentAnswerParams, String> {
    let parsed = parse_supervised_args(args, ANSWER_USAGE, true, Some("answer"))?;
    Ok(AgentAnswerParams {
        target_ref: parsed.target_ref,
        question_id: parsed.question_id.unwrap_or_default(),
        text: parsed.text.unwrap_or_default(),
        submit_id: parsed.submit_id,
    })
}

fn parse_agent_control_args(args: &[String], usage: &str) -> Result<AgentControlParams, String> {
    let parsed = parse_supervised_args(args, usage, false, None)?;
    Ok(AgentControlParams {
        target_ref: parsed.target_ref,
        submit_id: parsed.submit_id,
    })
}

/// `agent resume`: `--pane`, `--session-id` and `--submit-id`, each required once, and an
/// optional `--agent` at most once; no positional arguments.
fn parse_agent_resume_args(args: &[String]) -> Result<AgentResumeParams, String> {
    const FLAGS: [&str; 4] = ["--pane", "--session-id", "--submit-id", "--agent"];
    let args = super::expand_equals_args(args, &FLAGS);
    let usage = || format!("usage: {RESUME_USAGE}");
    let mut pane: Option<String> = None;
    let mut session_id: Option<String> = None;
    let mut submit_id: Option<String> = None;
    let mut agent: Option<String> = None;
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        if !FLAGS.contains(&arg) {
            return Err(usage());
        }
        let Some(value) = args.get(index + 1) else {
            return Err(format!("missing value for {arg}"));
        };
        let slot = match arg {
            "--pane" => &mut pane,
            "--session-id" => &mut session_id,
            "--agent" => &mut agent,
            _ => &mut submit_id,
        };
        if slot.replace(value.clone()).is_some() {
            return Err(usage());
        }
        index += 2;
    }
    let Some(pane) = pane else {
        return Err("missing required --pane".into());
    };
    let Some(session_id) = session_id else {
        return Err("missing required --session-id".into());
    };
    let Some(submit_id) = submit_id else {
        return Err("missing required --submit-id".into());
    };
    Ok(AgentResumeParams {
        pane_id: super::normalize_pane_id(&pane),
        session_id,
        submit_id,
        agent,
    })
}

fn send_supervised(id: &str, parsed: Result<Method, String>) -> std::io::Result<i32> {
    let method = match parsed {
        Ok(method) => method,
        Err(message) => {
            eprintln!("{message}");
            return Ok(2);
        }
    };
    super::print_response(&super::send_request(&Request {
        id: id.into(),
        method,
    })?)
}

/// `kelpie agent submit`: one submission to the exact, caller-supplied target.
/// The CLI never reads or refreshes the epoch or generation itself.
fn agent_submit(args: &[String]) -> std::io::Result<i32> {
    send_supervised(
        "cli:agent:submit",
        parse_agent_submit_args(args).map(Method::AgentSubmit),
    )
}

/// `kelpie agent answer`: answers the newest open prose question still on screen.
fn agent_answer(args: &[String]) -> std::io::Result<i32> {
    send_supervised(
        "cli:agent:answer",
        parse_agent_answer_args(args).map(Method::AgentAnswer),
    )
}

/// `kelpie agent interrupt`: one fenced Esc to a working agent.
fn agent_interrupt(args: &[String]) -> std::io::Result<i32> {
    send_supervised(
        "cli:agent:interrupt",
        parse_agent_control_args(args, INTERRUPT_USAGE).map(Method::AgentInterrupt),
    )
}

/// `kelpie agent reset`: submits `/clear` through the submit path.
fn agent_reset(args: &[String]) -> std::io::Result<i32> {
    send_supervised(
        "cli:agent:reset",
        parse_agent_control_args(args, RESET_USAGE).map(Method::AgentReset),
    )
}

/// `kelpie agent clear-draft`: clears the supervisor's own interrupted draft.
fn agent_clear_draft(args: &[String]) -> std::io::Result<i32> {
    send_supervised(
        "cli:agent:clear-draft",
        parse_agent_control_args(args, CLEAR_DRAFT_USAGE).map(Method::AgentClearDraft),
    )
}

/// `kelpie agent resume`: launches the agent's resume command for `<session-id>` (default
/// agent `claude`) in a pane hosting no agent.
fn agent_resume(args: &[String]) -> std::io::Result<i32> {
    send_supervised(
        "cli:agent:resume",
        parse_agent_resume_args(args).map(Method::AgentResume),
    )
}

fn agent_submit_status(args: &[String]) -> std::io::Result<i32> {
    let [submit_id] = args else {
        eprintln!("usage: {SUBMIT_STATUS_USAGE}");
        return Ok(2);
    };
    if submit_id.starts_with('-') {
        eprintln!("usage: {SUBMIT_STATUS_USAGE}");
        return Ok(2);
    }
    super::print_response(&super::send_request(&Request {
        id: "cli:agent:submit-status".into(),
        method: Method::AgentSubmitStatus(AgentSubmitStatusParams {
            submit_id: submit_id.clone(),
        }),
    })?)
}

const UNDELEGATE_USAGE: &str = "kelpie agent undelegate --pane <PANE_ID>";

/// Gives a delegated pane back to its owner. There is no command that delegates: only the
/// owner can, from an attached Kelpie window (ctrl+b shift+a).
fn agent_undelegate(args: &[String]) -> std::io::Result<i32> {
    let Some(pane_id) = parse_agent_undelegate_args(args) else {
        eprintln!("usage: {UNDELEGATE_USAGE}");
        return Ok(2);
    };
    super::print_response(&super::send_request(&Request {
        id: "cli:agent:undelegate".into(),
        method: Method::AgentUndelegate(AgentUndelegateParams { pane_id }),
    })?)
}

/// `agent undelegate`: exactly `--pane ID` (or `--pane=ID`); returns the pane ID.
fn parse_agent_undelegate_args(args: &[String]) -> Option<String> {
    let args = super::expand_equals_args(args, &["--pane"]);
    let [flag, pane_id] = args.as_slice() else {
        return None;
    };
    if flag != "--pane" || pane_id.starts_with('-') {
        return None;
    }
    Some(pane_id.clone())
}

fn agent_get(args: &[String]) -> std::io::Result<i32> {
    let Some(target) = args.first() else {
        eprintln!("usage: kelpie agent get <target>");
        return Ok(2);
    };
    if args.len() != 1 {
        eprintln!("usage: kelpie agent get <target>");
        return Ok(2);
    }

    super::print_response(&super::send_request(&Request {
        id: "cli:agent:get".into(),
        method: Method::AgentGet(AgentTarget {
            target: target.clone(),
        }),
    })?)
}

fn agent_focus(args: &[String]) -> std::io::Result<i32> {
    let Some(target) = args.first() else {
        eprintln!("usage: kelpie agent focus <target>");
        return Ok(2);
    };
    if args.len() != 1 {
        eprintln!("usage: kelpie agent focus <target>");
        return Ok(2);
    }

    super::print_response(&super::send_request(&Request {
        id: "cli:agent:focus".into(),
        method: Method::AgentFocus(AgentTarget {
            target: target.clone(),
        }),
    })?)
}

fn agent_attach(args: &[String]) -> std::io::Result<i32> {
    let (target, takeover) = match super::parse_attach_target(
        args,
        "usage: kelpie agent attach <target> [--takeover]",
    ) {
        Ok(parsed) => parsed,
        Err(code) => return Ok(code),
    };

    let response = resolve_agent_target(&target, "cli:agent:attach:resolve")?;
    if response.get("error").is_some() {
        eprintln!("{}", serde_json::to_string(&response).unwrap());
        return Ok(1);
    }
    let Some(terminal_id) = response["result"]["agent"]["terminal_id"].as_str() else {
        eprintln!("agent attach failed: response did not include terminal_id");
        return Ok(1);
    };
    crate::client::run_terminal_attach(terminal_id.to_owned(), takeover)?;
    Ok(0)
}

fn agent_wait(args: &[String]) -> std::io::Result<i32> {
    let Some(target) = args.first() else {
        eprintln!("usage: kelpie agent wait <target> [--until STATUS]... [--timeout MS]");
        return Ok(2);
    };
    let mut until = Vec::new();
    let mut timeout_ms = None;
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--until" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("--until requires at least one status");
                    return Ok(2);
                };
                let status = match super::parse_agent_status(value) {
                    Ok(status) => status,
                    Err(err) => {
                        eprintln!("{err}");
                        return Ok(2);
                    }
                };
                until.push(status);
                index += 2;
            }
            "--timeout" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --timeout");
                    return Ok(2);
                };
                timeout_ms = match parse_timeout(value) {
                    Ok(timeout_ms) => Some(timeout_ms),
                    Err(exit_code) => return Ok(exit_code),
                };
                index += 2;
            }
            "help" | "--help" | "-h" => {
                eprintln!("usage: kelpie agent wait <target> [--until STATUS]... [--timeout MS]");
                return Ok(0);
            }
            other => {
                eprintln!("unknown option: {other}");
                return Ok(2);
            }
        }
    }
    super::print_response(&super::send_request(&Request {
        id: "cli:agent:wait".into(),
        method: Method::AgentWait(AgentWaitParams {
            target: target.clone(),
            until,
            timeout_ms,
        }),
    })?)
}

fn wait_for_named_agent(
    name: &str,
    fallback_pane_id: &str,
    timeout: Duration,
    expected_kind: &str,
    expected_terminal_id: &str,
) -> std::io::Result<Result<serde_json::Value, serde_json::Value>> {
    let deadline = Instant::now().checked_add(timeout);
    let mut first_poll = true;
    loop {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            // Let the server reconcile its matching startup deadline before
            // returning so the pending name is immediately reusable.
            let _ = resolve_agent_target_unchecked(name, "cli:agent:start:timeout");
            return Ok(Err(agent_wait_timeout()));
        }
        let poll_id = "cli:agent:start";
        let mut response = if first_poll {
            first_poll = false;
            resolve_agent_target(name, poll_id)?
        } else {
            resolve_agent_target_unchecked(name, poll_id)?
        };
        if response.get("error").is_some() {
            response = resolve_agent_target_unchecked(fallback_pane_id, poll_id)?;
            if response.get("error").is_some() {
                std::thread::sleep(AGENT_START_POLL_INTERVAL);
                continue;
            }
        }
        let agent = &response["result"]["agent"];
        let outcome = if agent["terminal_id"].as_str() != Some(expected_terminal_id) {
            Some(Err(agent_name_lost_error("cli:agent:start", name)))
        } else if let Some(actual) = agent["agent"]
            .as_str()
            .filter(|actual| *actual != expected_kind)
        {
            Some(Err(cli_agent_error(
                "cli:agent:start",
                "agent_kind_mismatch",
                format!("expected {expected_kind}, detected {actual}"),
            )))
        } else if agent["name"].as_str() != Some(name) {
            Some(Err(agent_name_lost_error("cli:agent:start", name)))
        } else {
            match agent["agent_status"].as_str() {
                Some("blocked") => Some(Err(cli_agent_error(
                    "cli:agent:start",
                    "agent_not_ready",
                    format!("agent {name} is blocked during startup and is not ready for prompts"),
                ))),
                Some("unknown")
                    if expected_kind == "codex"
                        && agent["interactive_ready"].as_bool() == Some(true) =>
                {
                    Some(Ok(agent.clone()))
                }
                Some("working" | "unknown") => None,
                Some("idle" | "done") if agent["interactive_ready"].as_bool() == Some(true) => {
                    Some(Ok(agent.clone()))
                }
                Some("idle" | "done") if !agent["launch_pending"].as_bool().unwrap_or(false) => {
                    Some(Err(cli_agent_error(
                        "cli:agent:start",
                        "agent_start_failed",
                        "agent process exited before becoming interactive",
                    )))
                }
                _ => None,
            }
        };
        if let Some(outcome) = outcome {
            return Ok(outcome);
        }
        std::thread::sleep(AGENT_START_POLL_INTERVAL);
    }
}

fn pane_terminal_id(pane_id: &str) -> std::io::Result<Option<String>> {
    let response = super::send_request(&Request {
        id: "cli:agent:start:pane".into(),
        method: Method::PaneGet(PaneTarget {
            pane_id: pane_id.to_owned(),
        }),
    })?;
    Ok(response["result"]["pane"]["terminal_id"]
        .as_str()
        .map(str::to_owned))
}

fn pane_shell_is_initializing(pane_id: &str) -> std::io::Result<bool> {
    let response = super::send_request(&Request {
        id: "cli:agent:start:process_info".into(),
        method: Method::PaneProcessInfo(PaneProcessInfoParams {
            pane_id: Some(pane_id.to_owned()),
        }),
    })?;
    Ok(process_info_shows_shell_initialization(
        &response["result"]["process_info"],
    ))
}

#[cfg(unix)]
fn process_info_shows_shell_initialization(process_info: &serde_json::Value) -> bool {
    let Some(shell_pid) = process_info["shell_pid"].as_u64() else {
        return false;
    };
    if process_info["foreground_process_group_id"].as_u64() != Some(shell_pid) {
        return false;
    }
    process_info["foreground_processes"]
        .as_array()
        .is_some_and(|processes| {
            processes.iter().any(|process| {
                process["pid"].as_u64() == Some(shell_pid)
                    && (process["name"]
                        .as_str()
                        .is_some_and(crate::platform::is_pane_shell_process_name)
                        || process["argv"]
                            .as_array()
                            .and_then(|argv| argv.first())
                            .and_then(serde_json::Value::as_str)
                            .is_some_and(crate::platform::is_pane_shell_process_name))
            })
        })
}

// Windows exposes no foreground process group, so shell initialization is not
// observable and a busy `agent.start` is not retried there.
#[cfg(not(unix))]
fn process_info_shows_shell_initialization(_process_info: &serde_json::Value) -> bool {
    false
}

fn agent_name_lost_error(request_id: &str, expected_name: &str) -> serde_json::Value {
    cli_agent_error(
        request_id,
        "agent_name_not_found",
        format!("named agent {expected_name} no longer owns the target terminal"),
    )
}

fn print_agent_transport_error(
    err: std::io::Error,
    request_id: &str,
    code: &str,
) -> std::io::Result<i32> {
    if super::protocol_mismatch_was_reported(&err) {
        return Ok(1);
    }
    // A dead-server marker reaches here from `send_request` in the agent
    // startup path; surface its deferred response exactly once instead of
    // printing a second, generic transport-error line.
    if let Some(response) = super::server_not_running_reported_response(&err) {
        let value = serde_json::to_value(response).map_err(std::io::Error::other)?;
        return super::print_response(&value);
    }
    super::print_response(&cli_agent_error(request_id, code, err.to_string()))
}

fn agent_wait_timeout() -> serde_json::Value {
    cli_agent_error(
        "cli:agent:start",
        "timeout",
        "timed out waiting for agent startup",
    )
}

fn cli_agent_error(id: &str, code: &str, message: impl Into<String>) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "error": { "code": code, "message": message.into() }
    })
}

fn resolve_agent_target(target: &str, request_id: &str) -> std::io::Result<serde_json::Value> {
    super::send_request(&agent_get_request(target, request_id))
}

fn resolve_agent_target_unchecked(
    target: &str,
    request_id: &str,
) -> std::io::Result<serde_json::Value> {
    super::send_request_unchecked(&agent_get_request(target, request_id))
}

fn agent_get_request(target: &str, request_id: &str) -> Request {
    Request {
        id: request_id.into(),
        method: Method::AgentGet(AgentTarget {
            target: target.to_owned(),
        }),
    }
}

fn agent_rename(args: &[String]) -> std::io::Result<i32> {
    let [target, value] = args else {
        eprintln!("usage: kelpie agent rename <target> <name>|--clear");
        return Ok(2);
    };
    let name = if value == "--clear" {
        None
    } else {
        Some(value.clone())
    };

    super::print_response(&super::send_request(&Request {
        id: "cli:agent:rename".into(),
        method: Method::AgentRename(AgentRenameParams {
            target: target.clone(),
            name,
        }),
    })?)
}

fn agent_read(args: &[String]) -> std::io::Result<i32> {
    let Some(target) = args.first() else {
        eprintln!("usage: kelpie agent read <target> [--source visible|recent|recent-unwrapped] [--lines N] [--format text|ansi] [--ansi]");
        return Ok(2);
    };

    let mut source = ReadSource::Recent;
    let mut lines = None;
    let mut format = ReadFormat::Text;
    let mut strip_ansi = true;

    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--source" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --source");
                    return Ok(2);
                };
                source = super::parse_read_source(value)?;
                index += 2;
            }
            "--lines" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --lines");
                    return Ok(2);
                };
                lines = Some(super::parse_u32_flag("--lines", value)?);
                index += 2;
            }
            "--format" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --format");
                    return Ok(2);
                };
                format = super::parse_read_format(value)?;
                strip_ansi = !matches!(format, ReadFormat::Ansi);
                index += 2;
            }
            "--ansi" => {
                format = ReadFormat::Ansi;
                strip_ansi = false;
                index += 1;
            }
            other => {
                eprintln!("unknown option: {other}");
                return Ok(2);
            }
        }
    }

    let response = super::send_request(&Request {
        id: "cli:agent:read".into(),
        method: Method::AgentRead(AgentReadParams {
            target: target.clone(),
            source,
            lines,
            format,
            strip_ansi,
        }),
    })?;
    super::print_read_response(&response)
}

fn print_agent_help() {
    eprintln!("kelpie agent commands:");
    eprintln!("  kelpie agent list");
    eprintln!("  kelpie agent digest [--page-cursor C] [--trace-id T]");
    eprintln!("  {WATCH_USAGE}");
    eprintln!("  {SUBMIT_USAGE}");
    eprintln!("  {ANSWER_USAGE}");
    eprintln!("  {INTERRUPT_USAGE}");
    eprintln!("  {RESET_USAGE}");
    eprintln!("  {CLEAR_DRAFT_USAGE}");
    eprintln!("  {RESUME_USAGE}");
    eprintln!("  {SUBMIT_STATUS_USAGE}");
    eprintln!("  {UNDELEGATE_USAGE}");
    eprintln!("  kelpie agent get <target>");
    eprintln!("  kelpie agent read <target> [--source visible|recent|recent-unwrapped|detection] [--lines N] [--format text|ansi] [--ansi]");
    eprintln!("  kelpie agent rename <target> <name>|--clear");
    eprintln!("  kelpie agent focus <target>");
    eprintln!("  kelpie agent wait <target> [--until STATUS]... [--timeout MS]");
    eprintln!("  kelpie agent attach <target> [--takeover]");
    eprintln!(
        "  kelpie agent start <name> --kind KIND --pane ID [--timeout MS] [-- <agent-args...>]"
    );
    eprintln!("  kelpie agent explain <target> [--json|--format text|json] [--verbose]");
    eprintln!(
        "  kelpie agent explain --file PATH --agent LABEL [--json|--format text|json] [--verbose]"
    );
    eprintln!("  targets accept unique agent names and pane ids that currently host agents");
    eprintln!("  kinds: {}", super::spec::agent_kind_values().join("|"));
}

fn parse_timeout(value: &str) -> Result<u64, i32> {
    super::parse_u64_flag("--timeout", value).map_err(|err| {
        eprintln!("{err}");
        2
    })
}

#[cfg(test)]
mod tests {
    use super::{
        parse_agent_answer_args, parse_agent_control_args, parse_agent_digest_args,
        parse_agent_resume_args, parse_agent_submit_args, parse_agent_undelegate_args,
        parse_agent_watch_args, parse_target_ref, watch_view, watch_view_changed,
        CLEAR_DRAFT_USAGE, INTERRUPT_USAGE, RESET_USAGE,
    };
    use std::time::Duration;

    const USAGE: &str = "usage: kelpie agent digest [--page-cursor C] [--trace-id T]";

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| item.to_string()).collect()
    }

    #[test]
    fn agent_digest_args_empty_is_all_none() {
        let params = parse_agent_digest_args(&[]).unwrap();
        assert!(params.filter.is_none());
        assert!(params.page_cursor.is_none());
        assert!(params.trace_id.is_none());
    }

    #[test]
    fn agent_digest_args_parse_both_flags() {
        let params =
            parse_agent_digest_args(&args(&["--page-cursor", "o:2", "--trace-id", "t1"])).unwrap();
        assert_eq!(params.page_cursor.as_deref(), Some("o:2"));
        assert_eq!(params.trace_id.as_deref(), Some("t1"));
    }

    #[test]
    fn agent_digest_args_missing_value_errors() {
        assert_eq!(
            parse_agent_digest_args(&args(&["--page-cursor"])).unwrap_err(),
            "missing value for --page-cursor"
        );
        assert_eq!(
            parse_agent_digest_args(&args(&["--trace-id"])).unwrap_err(),
            "missing value for --trace-id"
        );
    }

    #[test]
    fn agent_digest_args_reject_json_positional_and_repeats() {
        for bad in [
            args(&["--json"]),
            args(&["x"]),
            args(&["--trace-id", "a", "--trace-id", "b"]),
            args(&["--page-cursor", "a", "--page-cursor", "b"]),
        ] {
            assert_eq!(parse_agent_digest_args(&bad).unwrap_err(), USAGE);
        }
    }

    #[test]
    fn agent_digest_args_accept_equals_form() {
        let spaced =
            parse_agent_digest_args(&args(&["--page-cursor", "abc", "--trace-id", "t1"])).unwrap();
        let equals =
            parse_agent_digest_args(&args(&["--page-cursor=abc", "--trace-id=t1"])).unwrap();
        assert_eq!(equals.page_cursor, spaced.page_cursor);
        assert_eq!(equals.trace_id, spaced.trace_id);
        assert_eq!(equals.page_cursor.as_deref(), Some("abc"));
        assert_eq!(equals.trace_id.as_deref(), Some("t1"));

        // Everything after the first `=` is the value.
        let params = parse_agent_digest_args(&args(&["--trace-id=a=b"])).unwrap();
        assert_eq!(params.trace_id.as_deref(), Some("a=b"));

        // A flag repeated across the two forms is still rejected.
        assert_eq!(
            parse_agent_digest_args(&args(&["--trace-id=a", "--trace-id", "b"])).unwrap_err(),
            USAGE
        );
    }

    #[test]
    fn agent_undelegate_args_accept_space_and_equals_forms() {
        assert_eq!(
            parse_agent_undelegate_args(&args(&["--pane", "w1:p1"])).as_deref(),
            Some("w1:p1")
        );
        assert_eq!(
            parse_agent_undelegate_args(&args(&["--pane=w1:p1"])).as_deref(),
            Some("w1:p1")
        );
        for bad in [
            args(&[]),
            args(&["--pane"]),
            args(&["--pane", "-x"]),
            args(&["--pane=w1:p1", "--pane", "w1:p2"]),
            args(&["--target", "w1:p1"]),
        ] {
            assert!(parse_agent_undelegate_args(&bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn target_ref_splits_epoch_first_and_generation_last() {
        let target = parse_target_ref("abc123:w1:p1:7").unwrap();
        assert_eq!(target.server_epoch, "abc123");
        assert_eq!(target.pane_id, "w1:p1");
        assert_eq!(target.generation, 7);
        let plain = parse_target_ref("e:p:0").unwrap();
        assert_eq!(plain.pane_id, "p");
    }

    #[test]
    fn target_ref_rejects_incomplete_values() {
        for bad in [
            "",
            "e",
            "e:7",
            ":w1:p1:1",
            "e::1",
            "e:w1:p1:",
            "e:w1:p1:x",
            "e:w1:p1:-1",
        ] {
            assert!(parse_target_ref(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn submit_args_require_full_target_ref_id_and_text() {
        let params = parse_agent_submit_args(&args(&[
            "--target-ref",
            "e:w1:p1:2",
            "--submit-id",
            "s-1",
            "hello there",
        ]))
        .unwrap();
        assert_eq!(params.target_ref.pane_id, "w1:p1");
        assert_eq!(params.target_ref.generation, 2);
        assert_eq!(params.submit_id, "s-1");
        assert_eq!(params.text, "hello there");

        let dashed = parse_agent_submit_args(&args(&[
            "--submit-id=s-2",
            "--target-ref=e:w1:p1:2",
            "--",
            "--not-a-flag",
        ]))
        .unwrap();
        assert_eq!(dashed.text, "--not-a-flag");
        assert_eq!(dashed.submit_id, "s-2");

        for bad in [
            args(&["--submit-id", "s", "text"]),
            args(&["--target-ref", "e:w1:p1:2", "text"]),
            args(&["--target-ref", "e:w1:p1:2", "--submit-id", "s"]),
            args(&["--target-ref", "w1:p1", "--submit-id", "s", "text"]),
            args(&["--target-ref", "e:w1:p1:2", "--submit-id", "s", "a", "b"]),
            args(&[
                "--target-ref",
                "e:w1:p1:2",
                "--submit-id",
                "s",
                "--json",
                "a",
            ]),
            args(&[
                "--target-ref",
                "e:p:1",
                "--target-ref",
                "e:p:1",
                "--submit-id",
                "s",
                "a",
            ]),
        ] {
            assert!(parse_agent_submit_args(&bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn answer_args_require_question_id_and_text() {
        let params = parse_agent_answer_args(&args(&[
            "--target-ref",
            "e:w1:p1:2",
            "--question-id",
            "q-1",
            "--submit-id",
            "a-1",
            "yes please",
        ]))
        .unwrap();
        assert_eq!(params.target_ref.pane_id, "w1:p1");
        assert_eq!(params.question_id, "q-1");
        assert_eq!(params.submit_id, "a-1");
        assert_eq!(params.text, "yes please");
        let eq = parse_agent_answer_args(&args(&[
            "--question-id=q-2",
            "--submit-id=a-2",
            "--target-ref=e:p:1",
            "--",
            "-x",
        ]))
        .unwrap();
        assert_eq!((eq.question_id.as_str(), eq.text.as_str()), ("q-2", "-x"));
        assert_eq!(
            parse_agent_answer_args(&args(&[
                "--target-ref",
                "e:p:1",
                "--submit-id",
                "a",
                "text"
            ]))
            .unwrap_err(),
            "missing required --question-id"
        );
        for bad in [
            args(&[
                "--target-ref",
                "e:p:1",
                "--question-id",
                "q",
                "--submit-id",
                "a",
            ]),
            args(&["--question-id", "q", "--submit-id", "a", "text"]),
            args(&[
                "--target-ref",
                "e:p:1",
                "--question-id",
                "q",
                "--question-id",
                "q",
                "--submit-id",
                "a",
                "text",
            ]),
        ] {
            assert!(parse_agent_answer_args(&bad).is_err(), "{bad:?}");
        }
        // `agent submit` does not take a question id.
        assert!(parse_agent_submit_args(&args(&[
            "--target-ref",
            "e:p:1",
            "--submit-id",
            "s",
            "--question-id",
            "q",
            "text",
        ]))
        .is_err());
    }

    #[test]
    fn resume_args_require_pane_session_and_submit_id() {
        let params = parse_agent_resume_args(&args(&[
            "--pane",
            "w1:p2",
            "--session-id",
            "sess-1",
            "--submit-id=r-1",
        ]))
        .unwrap();
        assert_eq!(params.pane_id, "w1:p2");
        assert_eq!(params.session_id, "sess-1");
        assert_eq!(params.submit_id, "r-1");
        assert_eq!(params.agent, None);
        let params = parse_agent_resume_args(&args(&[
            "--pane",
            "w1:p2",
            "--session-id",
            "sess-1",
            "--submit-id",
            "r-1",
            "--agent",
            "codex",
        ]))
        .unwrap();
        assert_eq!(params.agent.as_deref(), Some("codex"));
        for bad in [
            args(&[
                "--pane",
                "p",
                "--session-id",
                "s",
                "--submit-id",
                "r",
                "--agent",
                "codex",
                "--agent",
                "cursor",
            ]),
            args(&[
                "--pane",
                "p",
                "--session-id",
                "s",
                "--submit-id",
                "r",
                "--agent",
            ]),
            args(&["--session-id", "s", "--submit-id", "r"]),
            args(&["--pane", "p", "--submit-id", "r"]),
            args(&["--pane", "p", "--session-id", "s"]),
            args(&["--pane", "p", "--session-id", "s", "--submit-id"]),
            args(&[
                "--pane",
                "p",
                "--pane",
                "q",
                "--session-id",
                "s",
                "--submit-id",
                "r",
            ]),
            args(&[
                "--pane",
                "p",
                "--session-id",
                "s",
                "--submit-id",
                "r",
                "extra",
            ]),
            args(&[
                "--pane",
                "p",
                "--session-id",
                "s",
                "--submit-id",
                "r",
                "--target-ref",
                "e:p:1",
            ]),
        ] {
            assert!(parse_agent_resume_args(&bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn interrupt_reset_and_clear_draft_args_take_no_text() {
        for usage in [INTERRUPT_USAGE, RESET_USAGE, CLEAR_DRAFT_USAGE] {
            let params = parse_agent_control_args(
                &args(&["--target-ref", "e:w1:p1:4", "--submit-id", "c-1"]),
                usage,
            )
            .unwrap();
            assert_eq!(params.target_ref.server_epoch, "e");
            assert_eq!(params.target_ref.pane_id, "w1:p1");
            assert_eq!(params.target_ref.generation, 4);
            assert_eq!(params.submit_id, "c-1");
            for bad in [
                args(&[]),
                args(&["--target-ref", "e:w1:p1:4", "--submit-id", "c", "text"]),
                args(&["--target-ref", "e:w1:p1:4"]),
                args(&["--submit-id", "c"]),
                args(&["--target-ref", "w1:p1", "--submit-id", "c"]),
                args(&["--target-ref", "e:p:1", "--submit-id", "c", "--", "x"]),
                args(&["--target-ref"]),
                args(&["--target-ref", "e:p:1", "--submit-id"]),
                args(&[
                    "--target-ref",
                    "e:p:1",
                    "--target-ref",
                    "e:p:2",
                    "--submit-id",
                    "c",
                ]),
                args(&[
                    "--target-ref",
                    "e:p:1",
                    "--submit-id",
                    "c",
                    "--submit-id",
                    "d",
                ]),
                args(&[
                    "--target-ref",
                    "e:p:1",
                    "--submit-id",
                    "c",
                    "--unknown",
                    "x",
                ]),
                args(&[
                    "--target-ref",
                    "e:p:1",
                    "--submit-id",
                    "c",
                    "--question-id",
                    "q",
                ]),
            ] {
                assert!(parse_agent_control_args(&bad, usage).is_err(), "{bad:?}");
            }
        }
    }

    #[test]
    fn clear_draft_dispatch_rejects_invalid_args_before_sending() {
        for bad in [
            args(&["clear-draft"]),
            args(&["clear-draft", "--target-ref", "e:p:1", "--submit-id"]),
            args(&[
                "clear-draft",
                "--target-ref",
                "e:p:1",
                "--submit-id",
                "c",
                "text",
            ]),
        ] {
            assert_eq!(super::run_agent_command(&bad).unwrap(), 2, "{bad:?}");
        }
        let error = parse_agent_control_args(
            &args(&["--target-ref", "e:p:1", "--submit-id", "c", "text"]),
            CLEAR_DRAFT_USAGE,
        )
        .unwrap_err();
        assert_eq!(error, format!("usage: {CLEAR_DRAFT_USAGE}"));
    }

    #[test]
    fn watch_args_defaults_and_bounds() {
        let watch = parse_agent_watch_args(&[]).unwrap();
        assert_eq!(watch.interval, Duration::from_millis(500));
        assert!(watch.pane.is_none() && watch.max_changes.is_none() && watch.timeout.is_none());

        let watch = parse_agent_watch_args(&args(&[
            "--pane",
            "w1:p1",
            "--interval-ms=100",
            "--max-changes",
            "3",
            "--timeout-ms",
            "9000",
        ]))
        .unwrap();
        assert_eq!(watch.pane.as_deref(), Some("w1:p1"));
        assert_eq!(watch.interval, Duration::from_millis(100));
        assert_eq!(watch.max_changes, Some(3));
        assert_eq!(watch.timeout, Some(Duration::from_millis(9000)));

        for bad in [
            args(&["--interval-ms", "99"]),
            args(&["--max-changes", "0"]),
            args(&["--timeout-ms", "0"]),
            args(&["--timeout-ms", "x"]),
            args(&["--pane"]),
            args(&["--pane", "a", "--pane", "b"]),
            args(&["positional"]),
        ] {
            assert!(parse_agent_watch_args(&bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn watch_view_keeps_only_the_bounded_fields_and_open_question_ids() {
        let agent = serde_json::json!({
            "target_ref": {"server_epoch": "e", "pane_id": "w1:p1", "generation": 1},
            "agent_status": "idle",
            "foreground": {"value": "awaiting_human", "source": "screen_heuristic"},
            "background": {"value": "unknown"},
            "completion": "incomplete",
            "blocking_reason": "awaiting_human",
            "pending_questions": [
                {"question_id": "q-open"},
                {"question_id": "q-done", "resolution": {"at_ms": 1, "by": "owner"}}
            ],
            "owner_input": {"draft": "clean", "last_owner_input_at_ms": 5},
            "last_output_at_ms": 9,
            "state_change_seq": 4
        });
        let view = watch_view(&agent);
        assert_eq!(view["foreground"], "awaiting_human");
        assert_eq!(view["open_question_ids"], serde_json::json!(["q-open"]));
        assert_eq!(view["draft"], "clean");
        assert_eq!(view["handoff_armed"], false);
        assert!(view.get("last_output_at_ms").is_none());
        assert!(view.get("state_change_seq").is_none());
    }

    #[test]
    fn watch_reports_handoff_armed_changes_while_draft_stays_unknown() {
        let agent = |armed: bool| {
            serde_json::json!({
                "target_ref": {"server_epoch": "e", "pane_id": "w1:p1", "generation": 1},
                "agent_status": "idle",
                "owner_input": {"draft": "unknown", "handoff_armed": armed}
            })
        };
        let unarmed = watch_view(&agent(false));
        let armed = watch_view(&agent(true));
        let consumed = watch_view(&agent(false));
        assert_eq!(armed["handoff_armed"], true);
        assert_eq!(armed["draft"], "unknown");
        assert!(watch_view_changed(None, &unarmed));
        assert!(!watch_view_changed(Some(&unarmed), &unarmed));
        assert!(
            watch_view_changed(Some(&unarmed), &armed),
            "arming is a change"
        );
        assert!(
            watch_view_changed(Some(&armed), &consumed),
            "consumption is a change"
        );
    }
}
