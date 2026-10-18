// Port herdr upstream commits into Kelpie (fork). Two runs, owner in between:
//
//   1. { mode: 'plan' }  read-only. Triage every pending upstream commit: is it needed in
//      Kelpie, how should it be ported, what does it depend on. A second agent argues the
//      opposite of each verdict. Returns a table plus suggested args for the port run.
//   2. { mode: 'port', approve: ['<sha prefix>', ...], skip: { '<sha prefix>': 'reason' } }
//      Ports approved commits one at a time in upstream order through
//      scripts/upstream_port.py, then reviews each port through three lenses (fidelity,
//      regression, rename/install safety) with two refuters per finding, fixes confirmed
//      findings (max two rounds), runs the suite, optional Mac validation, CHANGELOG, and a
//      completeness critic. Stops at the first pending commit that is neither approved nor
//      skipped, because later commits may depend on it.
//
// Optional: limit (number of commits), mac (validate on the owner's Mac), council (also send
// each port to the Foreman council's external seats; the diff leaves the machine).
//
// Every agent runs on the host model: this is perspective, not independent review. The
// deterministic parts (order, ledger, rename audit, commit message) live in the script.
export const meta = {
  name: 'port-herdr',
  description: 'Triage, port and adversarially review herdr upstream commits into Kelpie, one commit at a time',
  whenToUse: "Bringing new herdr upstream commits into the Kelpie fork. Run {mode:'plan'} first, then {mode:'port', approve, skip}.",
  phases: [
    { title: 'Discover', detail: 'fetch upstream, list pending commits from the ledger' },
    { title: 'Triage', detail: 'plan mode: is each commit needed, how to port it, then argue the opposite' },
    { title: 'Port', detail: 'port mode: apply, rename, build, test, commit one commit at a time' },
    { title: 'Review', detail: 'fidelity, regression and rename lenses; two refuters per finding' },
    { title: 'Fix', detail: 'fix confirmed findings, then re-review (max two rounds)' },
    { title: 'Suite', detail: 'lint and full tests against the pre-run baseline' },
    { title: 'Mac', detail: 'optional: clippy and tests on the owner Mac' },
    { title: 'Wrap-up', detail: 'CHANGELOG entries and a completeness critic' },
  ],
}

const opts = args || {}
const mode = opts.mode || 'plan'
if (!['plan', 'port'].includes(mode)) throw new Error("port-herdr: mode must be 'plan' or 'port'")
if (mode === 'port' && !(Array.isArray(opts.approve) && opts.approve.length) && !(opts.skip && Object.keys(opts.skip).length)) {
  throw new Error("port-herdr: port mode needs approve: [sha...] and/or skip: {sha: reason}; run mode 'plan' first")
}

const REPO = 'C:\\Coding_Workspace\\Github\\herdr'
const KELPIE = `Repo ${REPO}: Kelpie, a fork of herdr. Binary/crate 'kelpie', env KELPIE_*, config/state %APPDATA%\\kelpie and ~/.config/kelpie, own sockets and launchd labels, so it runs beside an installed herdr. Fork-only areas: the supervisor (src/supervisor, src/app/api/submit.rs, delegation), agent profiles, sidebar/settings/host facts UI, themes, release pipeline. The upstream ledger is distribution/upstream-ports.json; scripts/upstream_port.py drives ports (see AGENTS.md "Porting from herdr").`
const WRITE_RULES = `Hard rules:
- Stage, audit and commit ports ONLY with scripts/upstream_port.py (apply / audit / commit / skip). Never git cherry-pick, merge, rebase, reset, stash, or checkout to discard. Never push.
- Never touch the installed herdr or installed Kelpie; never run 'kelpie server stop'.
- Rename every herdr identifier the port brings in to Kelpie's equivalent, matching how surrounding Kelpie code names it (binary, env vars, sockets, config dirs, bundle ids, launchd labels, temp files). Upstream credits (herdr#1234, github.com/herdrdev/herdr links) stay. A line that must stay herdr carries 'upstream-port: keep-herdr: <reason>'.
- Keep Kelpie's fork behaviour when resolving conflicts; port the upstream intent, not its exact text. Carry upstream tests over.
- Build with 'cargo build --bin kelpie'; run targeted tests ('cargo test --bin kelpie <module>' and any tests/*.rs the change touches), not the full suite. cfg(target_os = "macos") code does not compile here: say so.
- Commit bodies: what changed for the user and why, at most 8 lines, no AI attribution trailers.`
const READ_ONLY = 'Read-only: do not edit files, stage, or commit. Read files selectively (grep, then ranges); never paste whole files.'

const PENDING = {
  type: 'object',
  properties: {
    head: { type: 'string' },
    clean_tree: { type: 'boolean' },
    pending: { type: 'array', items: { type: 'object', properties: {
      sha: { type: 'string' }, subject: { type: 'string' }, applies_cleanly: { type: 'boolean' } },
      required: ['sha', 'subject'] } },
  },
  required: ['head', 'clean_tree', 'pending'],
}
const TRIAGE = {
  type: 'object',
  properties: {
    verdict: { type: 'string', enum: ['port', 'port-adapted', 'skip', 'owner-decision'] },
    skip_class: { type: 'string', enum: ['none', 'release-metadata', 'website-publishing', 'not-applicable', 'already-in-kelpie', 'superseded-by-fork'] },
    need: { type: 'string', description: 'what breaks for a Kelpie user without it, on which platform, how likely' },
    kelpie_evidence: { type: 'array', items: { type: 'string' }, description: 'file:line in Kelpie showing the affected code exists, or is absent/already fixed' },
    approach: { type: 'string', description: 'how to port: conflicts expected, renames needed, fork areas it touches' },
    depends_on: { type: 'array', items: { type: 'string' }, description: 'earlier pending upstream shas this needs' },
    platforms: { type: 'array', items: { type: 'string', enum: ['windows', 'macos', 'linux', 'remote', 'all', 'none'] } },
    risk: { type: 'string', enum: ['low', 'medium', 'high'] },
  },
  required: ['verdict', 'skip_class', 'need', 'kelpie_evidence', 'approach', 'depends_on', 'platforms', 'risk'],
}
const COUNTER = {
  type: 'object',
  properties: {
    agrees: { type: 'boolean' },
    revised_verdict: { type: 'string', enum: ['port', 'port-adapted', 'skip', 'owner-decision'] },
    argument: { type: 'string', description: 'strongest case against the verdict, with file:line evidence' },
  },
  required: ['agrees', 'revised_verdict', 'argument'],
}
const PORTED = {
  type: 'object',
  properties: {
    outcome: { type: 'string', enum: ['ported', 'blocked'] },
    kelpie_commit: { type: 'string' },
    conflicts: { type: 'string' },
    renames: { type: 'string' },
    tests: { type: 'string', description: 'commands run and verbatim result lines' },
    touches_macos: { type: 'boolean' },
    blocked_reason: { type: 'string' },
  },
  required: ['outcome', 'tests', 'touches_macos'],
}
const FINDINGS = {
  type: 'object',
  properties: {
    checked: { type: 'array', items: { type: 'string' } },
    findings: { type: 'array', items: { type: 'object', properties: {
      severity: { type: 'string', enum: ['critical', 'high', 'medium', 'low'] },
      file_line: { type: 'string' }, problem: { type: 'string' }, fix: { type: 'string' } },
      required: ['severity', 'file_line', 'problem', 'fix'] } },
  },
  required: ['checked', 'findings'],
}
const VERDICT = {
  type: 'object',
  properties: {
    refuted: { type: 'boolean' },
    severity: { type: 'string', enum: ['critical', 'high', 'medium', 'low'] },
    evidence: { type: 'string' },
  },
  required: ['refuted', 'severity', 'evidence'],
}

const LENSES = {
  fidelity: (sha) => `Lens: FIDELITY (how it was done). Compare upstream 'git show ${sha}' with the Kelpie port. Is every upstream behaviour change carried over, including edge cases and error paths? Were upstream tests carried over and do they still test the same thing? Did the conflict resolution change the upstream logic? Is there any hunk the port added that upstream did not need (scope creep)?`,
  regression: (sha) => `Lens: REGRESSION. Does the port break Kelpie behaviour that herdr does not have? Trace callers of every changed function into fork areas (supervisor, delegation, agent profiles, settings/host UI, themes, Windows-specific code). Check Windows paths (named pipes, ConPTY, PowerShell) and macOS paths separately. Re-run the targeted tests for the touched modules and for fork code that calls them; report verbatim results.`,
  rename: (sha) => `Lens: RENAME AND INSTALL SAFETY. Run 'python scripts/upstream_port.py audit --rev <commit>'. Then look for what the audit cannot see: a herdr name assembled from parts, a path or socket that resolves to herdr's dirs, env vars read as HERDR_*, launchd labels or cleanup that could match herdr's jobs, remote scripts that invoke 'herdr', anything that could stop, delete or rewrite an installed herdr's state.`,
}

// --- Discover -----------------------------------------------------------------------------------

phase('Discover')
const found = await agent(`${KELPIE}
${READ_ONLY} (fetching upstream refs is allowed.)
Run 'python scripts/upstream_port.py fetch', then 'python scripts/upstream_port.py status --json', 'git rev-parse HEAD' and 'git status --porcelain --untracked-files=no'. Return HEAD, whether the tracked tree is clean, and the pending list exactly as printed, oldest first.`,
  { label: 'discover', phase: 'Discover', schema: PENDING, effort: 'low' })
if (!found) throw new Error('port-herdr: discovery failed')
let pending = found.pending
if (opts.limit && pending.length > opts.limit) {
  log(`limit ${opts.limit}: ${pending.length - opts.limit} later commit(s) left for another run`)
  pending = pending.slice(0, opts.limit)
}
log(`${pending.length} pending upstream commit(s) at HEAD ${found.head.slice(0, 10)}`)
if (!pending.length) return { mode, pending: [] }

// --- Plan mode: triage --------------------------------------------------------------------------

if (mode === 'plan') {
  const list = pending.map((c) => `${c.sha.slice(0, 10)} ${c.subject}`).join('\n')
  const triaged = await pipeline(
    pending,
    (c) => agent(`${KELPIE}
${READ_ONLY}
Triage upstream herdr commit ${c.sha} "${c.subject}" for Kelpie. Read 'git show ${c.sha}' and the matching Kelpie code.
Decide whether Kelpie needs it: does the affected code exist in Kelpie (cite file:line), is the bug reachable for a Kelpie user (Windows, macOS, remote machines over SSH), is it already fixed or superseded by fork changes, or is it herdr release metadata / website publishing? 'owner-decision' is for design conflicts with fork behaviour or very large refactors. Describe the porting approach and name earlier pending commits it depends on.
All pending commits, oldest first:
${list}`,
      { label: `triage:${c.sha.slice(0, 10)}`, phase: 'Triage', schema: TRIAGE }),
    (t, c) => t ? agent(`${KELPIE}
${READ_ONLY}
Another reviewer triaged upstream herdr commit ${c.sha} "${c.subject}" as '${t.verdict}' (${t.skip_class}).
Need: ${t.need}
Evidence: ${t.kelpie_evidence.join('; ')}
Approach: ${t.approach}
Argue the opposite as strongly as the code allows: if they said port, show Kelpie does not need it or it would harm fork behaviour; if they said skip, show a Kelpie user hits the bug. Open the code and cite file:line. Agree only if the counter-case fails.`,
      { label: `counter:${c.sha.slice(0, 10)}`, phase: 'Triage', schema: COUNTER }).then((k) => ({ c, t, k })) : { c, t: null, k: null },
  )
  const rows = triaged.filter(Boolean).map(({ c, t, k }) => ({
    sha: c.sha, subject: c.subject, applies_cleanly: c.applies_cleanly,
    verdict: t ? t.verdict : 'triage-failed',
    contested: !!(t && k && !k.agrees),
    counter_verdict: k ? k.revised_verdict : null,
    skip_class: t ? t.skip_class : null, need: t ? t.need : null, approach: t ? t.approach : null,
    depends_on: t ? t.depends_on : [], platforms: t ? t.platforms : [], risk: t ? t.risk : null,
    evidence: t ? t.kelpie_evidence : [], counter_argument: k ? k.argument : null,
  }))
  const settled = (r) => !r.contested && r.verdict !== 'owner-decision' && r.verdict !== 'triage-failed'
  return {
    mode, head: found.head,
    rows,
    suggested_port_args: {
      mode: 'port',
      approve: rows.filter((r) => settled(r) && r.verdict.startsWith('port')).map((r) => r.sha.slice(0, 10)),
      skip: Object.fromEntries(rows.filter((r) => settled(r) && r.verdict === 'skip').map((r) => [r.sha.slice(0, 10), `${r.skip_class}: ${r.need}`])),
    },
    needs_owner: rows.filter((r) => !settled(r)).map((r) => `${r.sha.slice(0, 10)} ${r.subject}: ${r.verdict}${r.contested ? ` (counter says ${r.counter_verdict})` : ''}`),
    note: 'Same-model triage: perspective, not independent review. The owner approves the port args.',
  }
}

// --- Port mode ----------------------------------------------------------------------------------

if (!found.clean_tree) throw new Error('port-herdr: tracked tree is dirty; commit or set changes aside first')
const match = (map, sha) => Object.keys(map).find((k) => k.length >= 7 && sha.startsWith(k))
const approve = Object.fromEntries((opts.approve || []).map((s) => [s, true]))
const skip = opts.skip || {}
const baseHead = found.head
const results = []
let touchedMac = false

const review = (sha, commit, round) => parallel(Object.entries(LENSES).map(([lens, prompt]) => () => agent(`${KELPIE}
${READ_ONLY}
Review the Kelpie port of upstream ${sha} at commit ${commit} ('git show ${commit}'; earlier fix commits for this port may follow it up to HEAD, include them). Round ${round}.
${prompt(sha)}
Only real defects with file:line; severity is blast radius. Zero findings is valid but list what you examined in checked[].`,
  { label: `review:${lens}:${sha.slice(0, 7)}:r${round}`, phase: 'Review', schema: FINDINGS })))

const verify = (sha, findings) => parallel(findings.map((f) => () =>
  parallel(['correctness', 'reproduce'].map((angle) => () => agent(`${KELPIE}
${READ_ONLY}
Try to REFUTE this review finding on the Kelpie port of upstream ${sha} from the ${angle} angle. ${angle === 'reproduce' ? 'Give the concrete input or call sequence that triggers it, or show it cannot happen.' : 'Open the code and show whether it behaves as claimed.'} Default to refuted=true when uncertain. Re-rate severity by blast radius.
FINDING: [${f.severity}] ${f.file_line}: ${f.problem}`,
    { label: `verify:${sha.slice(0, 7)}:${angle}`, phase: 'Review', schema: VERDICT })))
    .then((vs) => ({ f, votes: vs.filter(Boolean) }))))
// A finding stays open unless both refuters ran and at least one refuted it; a missing vote
// never closes a finding.
const confirmed = (checked) => checked
  .filter(({ votes }) => votes.length < 2 || votes.every((v) => !v.refuted))
  .map(({ f, votes }) => ({
    ...f,
    severity: votes.length ? votes[0].severity : f.severity,
    status: votes.length < 2 ? 'unverified' : 'confirmed',
    evidence: votes.map((v) => v.evidence).join(' | '),
  }))

for (const c of pending) {
  const short = c.sha.slice(0, 10)
  const skipKey = match(skip, c.sha)
  if (skipKey) {
    const s = await agent(`${KELPIE}
${WRITE_RULES}
Run 'python scripts/upstream_port.py skip ${c.sha} --reason ${JSON.stringify(skip[skipKey])}', then commit distribution/upstream-ports.json alone as 'chore: skip herdr ${c.subject.replace(/\s*\(#\d+\)$/, '').slice(0, 40)}' with a one-line body. Return the commit hash only.`,
      { label: `skip:${short}`, phase: 'Port', effort: 'low' })
    results.push({ sha: c.sha, subject: c.subject, outcome: 'skipped', reason: skip[skipKey], commit: s })
    continue
  }
  if (!match(approve, c.sha)) {
    results.push({ sha: c.sha, subject: c.subject, outcome: 'not-approved' })
    log(`stopped at ${short}: not approved or skipped; later commits wait for it`)
    break
  }

  const port = await agent(`${KELPIE}
${WRITE_RULES}
Port upstream commit ${c.sha} "${c.subject}".
1. 'git show ${c.sha}' to understand the intent.
2. 'python scripts/upstream_port.py apply ${c.sha}'. Resolve conflicts by hand, then 'git add' the files. Skip upstream locale docs Kelpie no longer has.
3. Rename herdr identifiers (apply lists the upstream ones).
4. Build and run the targeted tests.
5. 'python scripts/upstream_port.py audit --staged' must be clean.
6. 'python scripts/upstream_port.py commit ${c.sha} --body "<why>"' (add --subject when the upstream subject is over 72 chars).
If the port cannot be done without changing fork behaviour in a way the owner should decide, stop: leave the tree as staged, return outcome blocked with the reason.`,
    { label: `port:${short}`, phase: 'Port', schema: PORTED })
  if (!port || port.outcome !== 'ported') {
    results.push({ sha: c.sha, subject: c.subject, outcome: 'blocked', port })
    log(`stopped at ${short}: ${port ? port.blocked_reason : 'port agent failed'}`)
    break
  }
  if (port.touches_macos) touchedMac = true

  if (opts.council) {
    // External seats see the diff (egress); recorded alongside the fan, never instead of it.
    port.council = await agent(`${KELPIE}
${READ_ONLY}
Load mcp__foreman__invoke_council with ToolSearch and call it: phase 'port-herdr', objective 'Review the Kelpie port of herdr ${short} for fidelity to upstream and regressions in Kelpie', lenses ['contract','state'], evidence = the output of 'git show ${c.sha} --stat', the upstream diff and 'git show ${port.kelpie_commit}' (trim to what matters). Then verify each council finding against the code yourself and return them as AGREE / DISAGREE / NUANCE with file:line evidence.`,
      { label: `council:${short}`, phase: 'Review' })
  }

  let open = []
  const rounds = []
  for (let round = 1; round <= 2; round++) {
    const reports = (await review(c.sha, port.kelpie_commit, round)).filter(Boolean)
    const seen = new Map()
    for (const r of reports) for (const f of r.findings) {
      const key = `${f.file_line}:${f.problem.slice(0, 50).toLowerCase()}`
      if (!seen.has(key)) seen.set(key, f)
    }
    open = confirmed(await verify(c.sha, [...seen.values()].slice(0, 20)))
    rounds.push({ round, lenses_ok: reports.length, raw: seen.size, confirmed: open })
    log(`${short} review round ${round}: ${seen.size} raw, ${open.length} confirmed`)
    if (!open.length) break
    if (round === 2) break
    const fix = await agent(`${KELPIE}
${WRITE_RULES}
Confirmed review findings on the port of upstream ${c.sha} (Kelpie commit ${port.kelpie_commit}):
${JSON.stringify(open, null, 1)}
Fix each in a new commit 'fix: <what>' (normal git commit; body says it follows the port of herdr ${short}). Do not amend. Re-run the targeted tests and 'python scripts/upstream_port.py audit --rev HEAD'. Return what you changed and the verbatim test lines.`,
      { label: `fix:${short}`, phase: 'Fix' })
    rounds[rounds.length - 1].fix = fix
  }
  results.push({ sha: c.sha, subject: c.subject, outcome: 'ported', port, rounds, unresolved: open })
  if (open.length) {
    log(`stopped after ${short}: ${open.length} finding(s) still confirmed after two rounds`)
    break
  }
}

const ported = results.filter((r) => r.outcome === 'ported')

phase('Suite')
const suite = ported.length ? await agent(`${KELPIE}
${READ_ONLY} (running builds and tests is allowed; a temporary 'git worktree' for the baseline is allowed and must be removed.)
Run 'cargo fmt --check', 'cargo clippy --all-targets --locked -- -D warnings', 'cargo test --locked --no-fail-fast' (nextest if installed), the python maintenance tests from the justfile's maintenance-test recipe, and 'python scripts/upstream_port.py check'. For every failure, check whether it also fails at the pre-run baseline ${baseHead} (temporary worktree, run only the failing tests). Report verbatim counts, each failure's first error line, and NEW vs PRE-EXISTING.`,
  { label: 'suite', phase: 'Suite' }) : null

phase('Mac')
let mac = null
if (ported.length && (opts.mac || touchedMac)) {
  if (opts.mac) {
    mac = await agent(`${KELPIE}
${READ_ONLY}
Validate HEAD on the owner's Mac. Access, build dir and tooling are in the Claude memory note 'fork-progress-handoff' (plink/pscp with -pwfile from a temp file you delete afterwards; pinned host key). Sync HEAD into the build dir, run 'cargo clippy --all-targets -- -D warnings' and 'cargo test' (supervisor_fixture with --test-threads=2). Do not touch Homebrew herdr or any running herdr/kelpie server; record 'launchctl list | grep -i herdr' before and after and confirm it is unchanged. Report verbatim results.`,
      { label: 'mac', phase: 'Mac' })
  } else {
    log('ports touch macOS code; rerun with {mac: true} or validate on the Mac before release')
  }
}

phase('Wrap-up')
const changelog = ported.length ? await agent(`${KELPIE}
${WRITE_RULES}
Add CHANGELOG.md 'Unreleased' entries for these ports in the existing style (one user-facing sentence, then '(herdr #NNNN)'); test-only commits get none. Commit as 'docs: list the fixes ported from herdr' with a short body.
Ports: ${JSON.stringify(ported.map((r) => ({ sha: r.sha, subject: r.subject, kelpie: r.port.kelpie_commit })))}`,
  { label: 'changelog', phase: 'Wrap-up', effort: 'low' }) : null
const critic = ported.length ? await agent(`${KELPIE}
${READ_ONLY}
Completeness critic for this port run (base ${baseHead}, now HEAD). What is missing? Upstream tests not carried over, docs or config-reference entries the upstream commits changed that Kelpie did not, ledger entries without a commit ('python scripts/upstream_port.py check'), CHANGELOG gaps, macOS code not validated, review findings left open. Cite evidence; list nothing you cannot point to.
Run summary: ${JSON.stringify(results.map((r) => ({ sha: r.sha.slice(0, 10), outcome: r.outcome, unresolved: (r.unresolved || []).length })))}`,
  { label: 'critic', phase: 'Wrap-up' }) : null

return {
  mode, base: baseHead, results, suite, mac, changelog, critic,
  note: 'Same-model review fan: perspective, not independence. Owner reviews the run before pushing or releasing.',
}
