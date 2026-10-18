#!/usr/bin/env python3
"""Port herdr upstream commits into Kelpie one at a time, in upstream order.

Kelpie is a fork of herdr. Upstream fixes are ported by hand, never merged:
each upstream commit becomes one Kelpie commit, checked for leftover herdr
names, and recorded in `distribution/upstream-ports.json`. That ledger is the
source of truth for what was ported, skipped, or is still pending.

Typical loop (see AGENTS.md "Porting from herdr"):

    python scripts/upstream_port.py fetch
    python scripts/upstream_port.py status
    python scripts/upstream_port.py apply <sha>      # stage the next commit
    # resolve conflicts, rename herdr -> kelpie, build, run targeted tests
    python scripts/upstream_port.py audit --staged
    python scripts/upstream_port.py commit <sha> --body "Why it matters."
    python scripts/upstream_port.py skip <sha> --reason "Release metadata."
"""
from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
LEDGER_PATH = ROOT / "distribution" / "upstream-ports.json"
STATUSES = {"ported", "skipped"}
PR_SUFFIX_RE = re.compile(r"\s*\(#(?P<pr>\d+)\)\s*$")
HERDR_RE = re.compile(r"herdr", re.IGNORECASE)
# Mentions that credit upstream and must stay herdr: issue/PR references,
# links to the upstream repository, and port notes.
ALLOWED_HERDR_RES = [
    re.compile(r"herdr\s?#\d+", re.IGNORECASE),
    re.compile(r"github\.com/herdrdev/herdr[\w./#-]*", re.IGNORECASE),
    re.compile(r"\bports?\s+herdr\b", re.IGNORECASE),
    re.compile(r"\bported\s+from\s+herdr\b", re.IGNORECASE),
    re.compile(r"\bfork\s+of\s+herdr\b", re.IGNORECASE),
    re.compile(r"\bbuilt\s+on\s+herdr\b", re.IGNORECASE),
]
# A line carrying this marker is deliberately herdr (for example code that
# detects an installed herdr); the reason belongs in the same comment.
ALLOW_MARKER = "upstream-port: keep-herdr"
TRAILER = "Upstream-Commit"


class PortError(Exception):
    pass


@dataclass(frozen=True)
class Hit:
    path: str
    line: int
    text: str


def git(*args: str, check: bool = True, input_text: str | None = None) -> str:
    result = subprocess.run(
        ["git", *args],
        cwd=ROOT,
        input=input_text,
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
    )
    if check and result.returncode != 0:
        raise PortError(f"git {' '.join(args)} failed:\n{result.stderr.strip()}")
    return result.stdout


# --- Ledger -------------------------------------------------------------------------------------


def load_ledger(path: Path = LEDGER_PATH) -> dict:
    ledger = json.loads(path.read_text(encoding="utf-8"))
    validate_ledger(ledger)
    return ledger


def validate_ledger(ledger: dict) -> None:
    upstream = ledger.get("upstream", {})
    for key in ("url", "branch", "ref", "base"):
        if not upstream.get(key):
            raise PortError(f"ledger upstream.{key} is missing")
    seen: set[str] = set()
    for entry in ledger.get("commits", []):
        sha = entry.get("sha", "")
        if not re.fullmatch(r"[0-9a-f]{40}", sha):
            raise PortError(f"ledger entry has a bad sha: {sha!r}")
        if sha in seen:
            raise PortError(f"ledger lists {sha} twice")
        seen.add(sha)
        if entry.get("status") not in STATUSES:
            raise PortError(f"ledger entry {sha} has status {entry.get('status')!r}")
        if entry["status"] == "skipped" and not entry.get("reason"):
            raise PortError(f"skipped entry {sha} needs a reason")


def save_ledger(ledger: dict, path: Path = LEDGER_PATH) -> None:
    validate_ledger(ledger)
    path.write_text(json.dumps(ledger, indent=2) + "\n", encoding="utf-8")


def record(ledger: dict, sha: str, subject: str, status: str, reason: str | None) -> dict:
    if any(entry["sha"] == sha for entry in ledger["commits"]):
        raise PortError(f"{sha} is already in the ledger")
    entry = {"sha": sha, "subject": subject, "status": status}
    pr = pr_number(subject)
    if pr is not None:
        entry["pr"] = pr
    if reason:
        entry["reason"] = reason
    ledger["commits"].append(entry)
    return entry


# --- Upstream commits ---------------------------------------------------------------------------


def pr_number(subject: str) -> int | None:
    match = PR_SUFFIX_RE.search(subject)
    return int(match.group("pr")) if match else None


def strip_pr(subject: str) -> str:
    return PR_SUFFIX_RE.sub("", subject).strip()


def upstream_commits(ledger: dict) -> list[tuple[str, str]]:
    """Every upstream commit after the base, oldest first, as (sha, subject)."""
    up = ledger["upstream"]
    out = git("log", "--reverse", "--no-merges", "--format=%H%x09%s", f"{up['base']}..{up['ref']}")
    return [tuple(line.split("\t", 1)) for line in out.splitlines() if line.strip()]


def pending(commits: list[tuple[str, str]], ledger: dict) -> list[tuple[str, str]]:
    done = {entry["sha"] for entry in ledger["commits"]}
    return [(sha, subject) for sha, subject in commits if sha not in done]


def resolve(sha: str) -> str:
    return git("rev-parse", "--verify", f"{sha}^{{commit}}").strip()


def patch_of(sha: str) -> str:
    return git("format-patch", "-1", "--stdout", sha)


def applies_cleanly(sha: str) -> bool:
    result = subprocess.run(
        ["git", "apply", "--check", "-"],
        cwd=ROOT,
        input=patch_of(sha),
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
    )
    return result.returncode == 0


# --- Audit --------------------------------------------------------------------------------------


def audit_diff(diff: str) -> list[Hit]:
    """Added lines that still name herdr outside the allowed credit forms."""
    hits: list[Hit] = []
    path = ""
    line_no = 0
    for raw in diff.splitlines():
        if raw.startswith("+++ "):
            target = raw[4:].strip()
            path = target[2:] if target.startswith("b/") else target
            continue
        if raw.startswith("@@"):
            match = re.search(r"\+(\d+)", raw)
            line_no = int(match.group(1)) if match else 0
            continue
        if raw.startswith("+") and not raw.startswith("+++"):
            text = raw[1:]
            if offending(text):
                hits.append(Hit(path, line_no, text.strip()))
            line_no += 1
        elif raw.startswith(" "):
            line_no += 1
    return hits


def offending(text: str) -> bool:
    if ALLOW_MARKER in text:
        return False
    stripped = text
    for allowed in ALLOWED_HERDR_RES:
        stripped = allowed.sub("", stripped)
    return bool(HERDR_RE.search(stripped))


def print_hits(hits: list[Hit]) -> None:
    for hit in hits:
        print(f"  {hit.path}:{hit.line}: {hit.text}")


# --- Commit message -----------------------------------------------------------------------------


def compose_message(
    upstream_subject: str, body: str, sha: str, subject_override: str | None = None
) -> str:
    subject = (subject_override or strip_pr(upstream_subject)).strip()
    if len(subject) > 72:
        raise PortError(f"subject is {len(subject)} chars; shorten it with --subject")
    body = body.strip()
    if not body:
        raise PortError("--body must say what the port changes and why")
    pr = pr_number(upstream_subject)
    if pr is not None and not re.search(rf"herdr\s?#{pr}\b", body):
        body = f"{body} Ports herdr #{pr}."
    return f"{subject}\n\n{body}\n\n{TRAILER}: {sha}\n"


# --- Commands -----------------------------------------------------------------------------------


def cmd_fetch(args: argparse.Namespace) -> int:
    up = load_ledger()["upstream"]
    git("fetch", "--no-tags", up["url"], f"+{up['branch']}:{up['ref']}")
    print(f"fetched {up['url']} {up['branch']} -> {up['ref']} at {resolve(up['ref'])[:12]}")
    return 0


def cmd_status(args: argparse.Namespace) -> int:
    ledger = load_ledger()
    todo = pending(upstream_commits(ledger), ledger)
    rows = [
        {"sha": sha, "subject": subject, "applies_cleanly": applies_cleanly(sha)}
        for sha, subject in todo
    ]
    if args.json:
        print(json.dumps(rows, indent=2))
        return 0
    if not rows:
        print("nothing pending: every upstream commit is ported or skipped")
        return 0
    print(f"{len(rows)} pending, oldest first (port in this order):")
    for row in rows:
        mark = "clean" if row["applies_cleanly"] else "3-way"
        print(f"  {row['sha'][:10]}  [{mark}]  {row['subject']}")
    return 0


def require_clean_tree() -> None:
    if git("status", "--porcelain", "--untracked-files=no").strip():
        raise PortError("working tree has changes; commit or set them aside first")


def require_next(ledger: dict, sha: str, allow_out_of_order: bool) -> str:
    todo = pending(upstream_commits(ledger), ledger)
    if not todo:
        raise PortError("nothing pending")
    shas = [item[0] for item in todo]
    if sha not in shas:
        raise PortError(f"{sha} is not pending (already recorded, or not after the base)")
    if sha != shas[0] and not allow_out_of_order:
        raise PortError(
            f"{sha[:10]} is not the oldest pending commit; port {shas[0][:10]} first "
            "or pass --out-of-order with a reason in the commit body"
        )
    return dict(todo)[sha]


def cmd_apply(args: argparse.Namespace) -> int:
    ledger = load_ledger()
    sha = resolve(args.sha)
    require_next(ledger, sha, args.out_of_order)
    require_clean_tree()
    print("upstream herdr names to rename (informational):")
    print_hits(audit_diff(git("show", "--format=", sha)))
    result = subprocess.run(
        ["git", "apply", "--3way", "--index", "-"],
        cwd=ROOT,
        input=patch_of(sha),
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
    )
    sys.stdout.write(result.stdout)
    sys.stderr.write(result.stderr)
    if result.returncode != 0:
        print("conflicts: resolve them, `git add` the files, then audit and commit")
        return 1
    print("staged cleanly; rename, build, test, then `audit --staged` and `commit`")
    return 0


def cmd_audit(args: argparse.Namespace) -> int:
    if args.staged:
        diff = git("diff", "--cached", "--unified=0")
    else:
        diff = git("show", "--format=", "--unified=0", args.rev)
    hits = audit_diff(diff)
    if not hits:
        print("audit clean: no herdr names in added lines")
        return 0
    print(f"{len(hits)} added line(s) still name herdr:")
    print_hits(hits)
    print(f"rename them, or mark a deliberate one with `{ALLOW_MARKER}` and a reason")
    return 1


def cmd_commit(args: argparse.Namespace) -> int:
    ledger = load_ledger()
    sha = resolve(args.sha)
    subject = require_next(ledger, sha, args.out_of_order)
    if not git("diff", "--cached", "--name-only").strip():
        raise PortError("nothing staged")
    if git("diff", "--name-only", "--diff-filter=U").strip():
        raise PortError("unresolved conflicts remain")
    hits = audit_diff(git("diff", "--cached", "--unified=0"))
    if hits:
        print_hits(hits)
        raise PortError("audit failed; fix the herdr names before committing")
    message = compose_message(subject, args.body, sha, args.subject)
    record(ledger, sha, subject, "ported", None)
    save_ledger(ledger)
    git("add", str(LEDGER_PATH.relative_to(ROOT)))
    git("commit", "-q", "-F", "-", input_text=message)
    print(git("log", "-1", "--format=%h %s").strip())
    return 0


def cmd_skip(args: argparse.Namespace) -> int:
    ledger = load_ledger()
    sha = resolve(args.sha)
    subject = dict(upstream_commits(ledger)).get(sha)
    if subject is None:
        raise PortError(f"{sha} is not an upstream commit after the base")
    record(ledger, sha, subject, "skipped", args.reason)
    save_ledger(ledger)
    print(f"recorded skip of {sha[:10]}; commit {LEDGER_PATH.relative_to(ROOT)} with the next port")
    return 0


def cmd_check(args: argparse.Namespace) -> int:
    """Ledger is valid and every `ported` entry has a Kelpie commit pointing at it."""
    ledger = load_ledger()
    linked = set(re.findall(rf"^{TRAILER}: ([0-9a-f]{{40}})$", git("log", "--format=%B"), re.M))
    missing = [
        entry["sha"]
        for entry in ledger["commits"]
        if entry["status"] == "ported" and entry["sha"] not in linked and not entry.get("kelpie")
    ]
    if missing:
        print("ported entries with no Kelpie commit (add an Upstream-Commit trailer or `kelpie`):")
        for sha in missing:
            print(f"  {sha}")
        return 1
    print(f"ledger ok: {len(ledger['commits'])} entries")
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    sub.add_parser("fetch", help="fetch upstream into the ledger's ref").set_defaults(func=cmd_fetch)
    status = sub.add_parser("status", help="list pending upstream commits, oldest first")
    status.add_argument("--json", action="store_true")
    status.set_defaults(func=cmd_status)
    apply = sub.add_parser("apply", help="stage one upstream commit (3-way)")
    apply.add_argument("sha")
    apply.add_argument("--out-of-order", action="store_true")
    apply.set_defaults(func=cmd_apply)
    audit = sub.add_parser("audit", help="fail on herdr names in added lines")
    target = audit.add_mutually_exclusive_group(required=True)
    target.add_argument("--staged", action="store_true")
    target.add_argument("--rev")
    audit.set_defaults(func=cmd_audit)
    commit = sub.add_parser("commit", help="commit the staged port and record it")
    commit.add_argument("sha")
    commit.add_argument("--body", required=True)
    commit.add_argument("--subject", help="override the upstream subject (<= 72 chars)")
    commit.add_argument("--out-of-order", action="store_true")
    commit.set_defaults(func=cmd_commit)
    skip = sub.add_parser("skip", help="record an upstream commit as deliberately not ported")
    skip.add_argument("sha")
    skip.add_argument("--reason", required=True)
    skip.set_defaults(func=cmd_skip)
    sub.add_parser("check", help="validate the ledger against git history").set_defaults(func=cmd_check)
    args = parser.parse_args(argv)
    try:
        return args.func(args)
    except PortError as error:
        print(f"error: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
