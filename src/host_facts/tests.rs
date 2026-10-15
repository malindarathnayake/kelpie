use super::*;
use crate::api::schema::host::{
    HostBucketFact, HostDockerFacts, HostDockerStatus, HostGitFacts, HostGpuFacts, HostOsFacts,
    HostProbeStatus, HostShellFacts, HostShellInfo, HostToolFact, HostToolSource, HostToolStatus,
    HostWslFacts, HOST_FACTS_SCHEMA_VERSION,
};

fn sample_facts() -> HostFacts {
    HostFacts {
        schema_version: HOST_FACTS_SCHEMA_VERSION,
        refreshed_at_ms: 1_760_000_000_000,
        interval_secs: 60,
        os: HostOsFacts {
            name: "Windows 11".into(),
            version: Some("24H2".into()),
            build: Some("26100.4061".into()),
            arch: "x86_64".into(),
            process_arch: "x86_64".into(),
            checked_at_ms: 1_760_000_000_000,
        },
        memory: HostBucketFact {
            bucket: Some("16-32 GB".into()),
            checked_at_ms: 1_760_000_000_000,
        },
        disk: HostBucketFact {
            bucket: None,
            checked_at_ms: 1_760_000_000_000,
        },
        shells: HostShellFacts {
            preferred: Some("pwsh".into()),
            pwsh: Some(HostShellInfo {
                path: r"%ProgramFiles%\PowerShell\7\pwsh.exe".into(),
                version: Some("7.5.1".into()),
                language_mode: Some("FullLanguage".into()),
                psreadline_version: Some("2.3.6".into()),
                status: HostProbeStatus::Ok,
            }),
            windows_powershell: None,
            psreadline_version: Some("2.3.6".into()),
            constrained_language_mode: Some(false),
            checked_at_ms: 1_760_000_000_000,
        },
        git: HostGitFacts {
            git_path: Some(r"%ProgramFiles%\Git\cmd\git.exe".into()),
            bash_path: Some(r"%ProgramFiles%\Git\bin\bash.exe".into()),
            checked_at_ms: 1_760_000_000_000,
        },
        gpu: HostGpuFacts {
            nvidia_smi: HostProbeStatus::Missing,
            names: Vec::new(),
            driver_version: None,
            checked_at_ms: 1_759_999_800_000,
        },
        docker: HostDockerFacts {
            status: HostDockerStatus::NotRunning,
            checked_at_ms: 1_760_000_000_000,
        },
        wsl: HostWslFacts {
            present: true,
            distros: vec!["Ubuntu-24.04".into()],
            checked_at_ms: 1_760_000_000_000,
        },
        tools: vec![
            HostToolFact {
                name: "rg".into(),
                path: Some(r"%USERPROFILE%\scoop\shims\rg.exe".into()),
                version: Some("ripgrep 14.1.1".into()),
                source: Some(HostToolSource::Scoop),
                status: HostToolStatus::Ok,
                shadowed_by: None,
            },
            HostToolFact {
                name: "find".into(),
                path: Some(r"%ProgramFiles%\Git\usr\bin\find.exe".into()),
                version: None,
                source: Some(HostToolSource::GitForWindows),
                status: HostToolStatus::Missing,
                shadowed_by: Some(r"%SystemRoot%\System32\find.exe".into()),
            },
        ],
        tools_checked_at_ms: 1_760_000_000_000,
    }
}

fn assert_plain_json(value: &serde_json::Value) {
    match value {
        serde_json::Value::Number(number) => assert!(
            number.is_u64() || number.is_i64(),
            "non-integer number {number}"
        ),
        serde_json::Value::Array(items) => items.iter().for_each(assert_plain_json),
        serde_json::Value::Object(map) => map.values().for_each(assert_plain_json),
        _ => {}
    }
}

#[test]
fn json_round_trips_with_plain_integer_values() {
    let facts = sample_facts();
    let text = serde_json::to_string_pretty(&facts).expect("serialize");
    assert!(!text.contains("NaN") && !text.contains("Infinity"));
    let value: serde_json::Value = serde_json::from_str(&text).expect("json");
    assert!(value.is_object());
    assert_plain_json(&value);
    assert_eq!(value["docker"]["status"], "not_running");
    assert_eq!(value["tools"][0]["source"], "scoop");
    assert_eq!(value["tools"][1]["source"], "git-for-windows");
    assert_eq!(value["tools"][1]["status"], "missing");
    // Absent optionals are omitted rather than null.
    assert!(value["disk"].get("bucket").is_none());
    let back: HostFacts = serde_json::from_str(&text).expect("deserialize");
    assert_eq!(back, facts);
}

#[test]
fn api_success_shape_is_tagged() {
    let json = serde_json::to_value(SuccessResponse {
        id: "r".into(),
        result: ResponseResult::HostFacts(Box::new(sample_facts())),
    })
    .expect("serialize");
    assert_eq!(json["result"]["type"], "host_facts");
    assert_eq!(json["result"]["schema_version"], 1);
}

fn unique_dir(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "kelpie-host-facts-{name}-{}-{}",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
}

#[test]
fn atomic_write_leaves_only_the_facts_file() {
    let dir = unique_dir("write");
    let facts = sample_facts();
    write_facts_file(&dir, &facts).expect("first write");
    let mut updated = facts.clone();
    updated.refreshed_at_ms += 60_000;
    write_facts_file(&dir, &updated).expect("replace");

    let names: Vec<String> = std::fs::read_dir(&dir)
        .expect("dir")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert_eq!(names, vec![HOST_FACTS_FILE_NAME.to_string()]);
    let read = read_facts_file(&dir.join(HOST_FACTS_FILE_NAME)).expect("read");
    assert_eq!(read, updated);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn interval_has_a_floor() {
    assert_eq!(effective_interval_secs(5), MIN_INTERVAL_SECS);
    assert_eq!(effective_interval_secs(60), 60);
    assert_eq!(effective_interval_secs(600), 600);
    assert_eq!(effective_interval_secs(u64::MAX), MAX_INTERVAL_SECS);
}

#[test]
fn refresh_wait_returns_new_generation() {
    let shared = Arc::new(Shared::default());
    shared.lock().snapshot = Some(Arc::new(sample_facts()));
    let worker = Arc::clone(&shared);
    let handle = std::thread::spawn(move || {
        // Stand-in worker: answer the refresh request with a new snapshot.
        let mut state = worker.lock();
        while !state.refresh_requested {
            state = worker
                .changed
                .wait_timeout(state, Duration::from_secs(5))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        state.refresh_requested = false;
        let mut facts = sample_facts();
        facts.refreshed_at_ms += 1;
        state.snapshot = Some(Arc::new(facts));
        state.generation += 1;
        worker.changed.notify_all();
    });
    let facts = wait_for_snapshot(&shared, true, Duration::from_secs(10)).expect("snapshot");
    assert_eq!(facts.refreshed_at_ms, sample_facts().refreshed_at_ms + 1);
    handle.join().expect("worker");
}

#[test]
fn wait_without_snapshot_times_out_bounded() {
    let shared = Shared::default();
    let started = Instant::now();
    assert!(wait_for_snapshot(&shared, false, Duration::from_millis(50)).is_none());
    assert!(started.elapsed() < Duration::from_secs(5));
}
