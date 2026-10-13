//! Path normalisation and coarse size buckets for host facts.
//!
//! Paths leave the process only after well-known roots are replaced by
//! environment variable tokens and any path segment equal to the user or host
//! name is replaced by its token [CWE-200].

use std::path::Path;

use super::probe::HostProbes;

/// Roots replaced by tokens, longest match first. `%LOCALAPPDATA%` and
/// `%APPDATA%` sit under `%USERPROFILE%`, so length ordering picks them first.
const WINDOWS_ROOTS: &[&str] = &[
    "LOCALAPPDATA",
    "APPDATA",
    "USERPROFILE",
    "ProgramFiles(x86)",
    "ProgramFiles",
    "ProgramData",
    "SystemRoot",
];
const UNIX_ROOTS: &[&str] = &["HOME"];
const WINDOWS_SECRETS: &[&str] = &["USERNAME", "COMPUTERNAME", "USERDOMAIN"];
const UNIX_SECRETS: &[&str] = &["USER", "LOGNAME", "HOSTNAME"];

#[derive(Debug, Clone)]
pub(crate) struct PathNormalizer {
    /// (token, root) sorted by root length, longest first.
    roots: Vec<(String, String)>,
    /// (token, value) for whole-segment redaction.
    secrets: Vec<(String, String)>,
    case_insensitive: bool,
}

impl PathNormalizer {
    pub(crate) fn new(
        roots: Vec<(String, String)>,
        secrets: Vec<(String, String)>,
        case_insensitive: bool,
    ) -> Self {
        let mut roots: Vec<(String, String)> = roots
            .into_iter()
            .map(|(token, root)| (token, trim_trailing_separators(&root).to_string()))
            .filter(|(_, root)| root.len() > 1)
            .collect();
        roots.sort_by_key(|(_, root)| std::cmp::Reverse(root.len()));
        let secrets = secrets
            .into_iter()
            .filter(|(_, value)| value.chars().count() >= 2)
            .collect();
        Self {
            roots,
            secrets,
            case_insensitive,
        }
    }

    pub(crate) fn from_probes(probes: &dyn HostProbes) -> Self {
        let windows = probes.windows();
        let token = |name: &str| {
            if windows {
                format!("%{name}%")
            } else {
                format!("${name}")
            }
        };
        let (root_names, secret_names) = if windows {
            (WINDOWS_ROOTS, WINDOWS_SECRETS)
        } else {
            (UNIX_ROOTS, UNIX_SECRETS)
        };
        let lookup = |names: &[&str]| -> Vec<(String, String)> {
            names
                .iter()
                .filter_map(|name| {
                    let value = probes.env_var(name)?;
                    Some((token(name), value.to_string_lossy().into_owned()))
                })
                .collect()
        };
        Self::new(lookup(root_names), lookup(secret_names), windows)
    }

    pub(crate) fn normalize(&self, path: &Path) -> String {
        self.normalize_str(&path.to_string_lossy())
    }

    pub(crate) fn normalize_str(&self, raw: &str) -> String {
        let raw = raw
            .strip_prefix(r"\\?\UNC\")
            .map(|rest| format!(r"\\{rest}"))
            .unwrap_or_else(|| raw.strip_prefix(r"\\?\").unwrap_or(raw).to_string());
        let mut text = raw.clone();
        for (token, root) in &self.roots {
            if let Some(rest) = self.strip_root(&raw, root) {
                text = format!("{token}{rest}");
                break;
            }
        }
        self.redact_segments(&text)
    }

    /// The remainder after `root` when `raw` starts with it at a segment boundary.
    fn strip_root<'a>(&self, raw: &'a str, root: &str) -> Option<&'a str> {
        let head = raw.get(..root.len())?;
        let matches = if self.case_insensitive {
            head.to_lowercase() == root.to_lowercase()
        } else {
            head == root
        };
        let rest = &raw[root.len()..];
        (matches && (rest.is_empty() || rest.starts_with(['\\', '/']))).then_some(rest)
    }

    fn redact_segments(&self, text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut segment = String::new();
        let flush = |segment: &mut String, out: &mut String| {
            let replacement = self.secrets.iter().find(|(_, value)| {
                if self.case_insensitive {
                    segment.to_lowercase() == value.to_lowercase()
                } else {
                    segment == value
                }
            });
            match replacement {
                Some((token, _)) => out.push_str(token),
                None => out.push_str(segment),
            }
            segment.clear();
        };
        for ch in text.chars() {
            if matches!(ch, '\\' | '/') {
                flush(&mut segment, &mut out);
                out.push(ch);
            } else {
                segment.push(ch);
            }
        }
        flush(&mut segment, &mut out);
        out
    }

    /// Redacts user/host names anywhere in free text such as a `--version`
    /// line or a WSL distribution name. Names shorter than three characters
    /// are left alone in free text to avoid shredding ordinary words.
    pub(crate) fn redact_text(&self, text: &str) -> String {
        let mut out = text.to_string();
        for (token, value) in &self.secrets {
            if value.chars().count() >= 3 {
                out = replace_ascii_case_insensitive(&out, value, token);
            }
        }
        out
    }
}

/// Replaces every ASCII-case-insensitive occurrence of `needle`. ASCII
/// lowercasing keeps byte offsets, so indices map back onto `text`.
fn replace_ascii_case_insensitive(text: &str, needle: &str, replacement: &str) -> String {
    let haystack = text.to_ascii_lowercase();
    let needle_lower = needle.to_ascii_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    while let Some(found) = haystack[index..].find(&needle_lower) {
        out.push_str(&text[index..index + found]);
        out.push_str(replacement);
        index += found + needle_lower.len();
    }
    out.push_str(&text[index..]);
    out
}

fn trim_trailing_separators(root: &str) -> &str {
    let trimmed = root.trim_end_matches(['\\', '/']);
    if trimmed.is_empty() {
        root
    } else {
        trimmed
    }
}

const GIB: u64 = 1024 * 1024 * 1024;

/// Total physical memory bucket. Rounded to whole GiB first, so a 16 GB machine
/// that reports 15.8 GiB lands in `8-16 GB`; upper bounds are inclusive.
pub(crate) fn memory_bucket(bytes: u64) -> &'static str {
    match bytes.saturating_add(GIB / 2) / GIB {
        0..=8 => "0-8 GB",
        9..=16 => "8-16 GB",
        17..=32 => "16-32 GB",
        33..=64 => "32-64 GB",
        _ => ">64 GB",
    }
}

/// Free disk bucket of the state directory volume, in whole GiB rounded down.
pub(crate) fn disk_bucket(bytes: u64) -> &'static str {
    match bytes / GIB {
        0..=4 => "0-5 GB",
        5..=19 => "5-20 GB",
        20..=99 => "20-100 GB",
        100..=499 => "100-500 GB",
        _ => ">500 GB",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn windows_normalizer() -> PathNormalizer {
        PathNormalizer::new(
            vec![
                ("%USERPROFILE%".into(), r"C:\Users\alice.smith".into()),
                (
                    "%LOCALAPPDATA%".into(),
                    r"C:\Users\alice.smith\AppData\Local".into(),
                ),
                ("%ProgramFiles%".into(), r"C:\Program Files\".into()),
                ("%SystemRoot%".into(), r"C:\WINDOWS".into()),
            ],
            vec![
                ("%USERNAME%".into(), "alice.smith".into()),
                ("%COMPUTERNAME%".into(), "DESKTOP-42".into()),
            ],
            true,
        )
    }

    #[test]
    fn normalisation_hides_user_profile() {
        let n = windows_normalizer();
        assert_eq!(
            n.normalize_str(r"C:\Users\alice.smith\scoop\shims\rg.exe"),
            r"%USERPROFILE%\scoop\shims\rg.exe"
        );
        // The longer root wins.
        assert_eq!(
            n.normalize_str(r"c:\users\Alice.Smith\AppData\Local\Microsoft\WinGet\Links\jq.exe"),
            r"%LOCALAPPDATA%\Microsoft\WinGet\Links\jq.exe"
        );
        assert_eq!(
            n.normalize_str(r"C:\Program Files\Git\usr\bin\sed.exe"),
            r"%ProgramFiles%\Git\usr\bin\sed.exe"
        );
        assert_eq!(
            n.normalize_str(r"\\?\C:\Windows\System32\find.exe"),
            r"%SystemRoot%\System32\find.exe"
        );
    }

    #[test]
    fn normalisation_redacts_names_outside_known_roots() {
        let n = windows_normalizer();
        let out = n.normalize_str(r"D:\Users\alice.smith\tools\DESKTOP-42\fd.exe");
        assert_eq!(out, r"D:\Users\%USERNAME%\tools\%COMPUTERNAME%\fd.exe");
        assert!(!out.to_lowercase().contains("alice"));
        // Partial matches stay: only whole segments are names.
        assert_eq!(
            n.normalize_str(r"D:\alice.smith-tools\x.exe"),
            r"D:\alice.smith-tools\x.exe"
        );
        // A root must end at a segment boundary.
        assert_eq!(
            n.normalize_str(r"C:\Users\alice.smithers\x.exe"),
            r"C:\Users\alice.smithers\x.exe"
        );
    }

    #[test]
    fn normalisation_on_unix_uses_home() {
        let n = PathNormalizer::new(
            vec![("$HOME".into(), "/home/bob".into())],
            vec![("$USER".into(), "bob".into())],
            false,
        );
        assert_eq!(
            n.normalize_str("/home/bob/.cargo/bin/rg"),
            "$HOME/.cargo/bin/rg"
        );
        assert_eq!(n.normalize_str("/srv/bob/bin/fd"), "/srv/$USER/bin/fd");
        assert_eq!(n.normalize_str("/home/Bob/x"), "/home/Bob/x");
    }

    #[test]
    fn redact_text_replaces_names_anywhere() {
        let n = windows_normalizer();
        assert_eq!(
            n.redact_text("built by Alice.Smith on (DESKTOP-42)"),
            "built by %USERNAME% on (%COMPUTERNAME%)"
        );
    }

    #[test]
    fn memory_bucket_boundaries() {
        assert_eq!(memory_bucket(4 * GIB), "0-8 GB");
        assert_eq!(memory_bucket(8 * GIB), "0-8 GB");
        assert_eq!(memory_bucket(15 * GIB + 800 * 1024 * 1024), "8-16 GB");
        assert_eq!(memory_bucket(16 * GIB), "8-16 GB");
        assert_eq!(memory_bucket(32 * GIB), "16-32 GB");
        assert_eq!(memory_bucket(64 * GIB), "32-64 GB");
        assert_eq!(memory_bucket(128 * GIB), ">64 GB");
        assert_eq!(memory_bucket(0), "0-8 GB");
        assert_eq!(memory_bucket(u64::MAX - GIB), ">64 GB");
    }

    #[test]
    fn disk_bucket_boundaries() {
        assert_eq!(disk_bucket(0), "0-5 GB");
        assert_eq!(disk_bucket(5 * GIB - 1), "0-5 GB");
        assert_eq!(disk_bucket(5 * GIB), "5-20 GB");
        assert_eq!(disk_bucket(20 * GIB), "20-100 GB");
        assert_eq!(disk_bucket(100 * GIB), "100-500 GB");
        assert_eq!(disk_bucket(500 * GIB), ">500 GB");
    }
}
