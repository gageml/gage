---
description: Author and run scanner tests from reviewed sessions.
disable-model-invocation: true
---

Your task is to help the user maintain and run scanner tests. This skill is
typically run after `gage:resolve`, while that session's judgments about
scanner output are fresh.

## Background

A scanner test runs the real scanner pipeline over captured sessions in a
sandbox and judges the written notes and issues against stated expectations.
Tests are defined in suite TOML files and run with `gage test run`, which reports
per-item match rates across samples.

Ad hoc tests live outside source control under `~/.gage/tmp/tests/`:

```
~/.gage/tmp/tests/
├── failure-modes.toml
└── fixtures/{fixture-name}/projects/{project}/{session-uuid}.jsonl
```

Test shape:

```toml
[[test]]
name = "rune-iter-error"
scanners = ["general"]
fixture = "rune-iter-error"     # fixtures/rune-iter-error/projects/
samples = 3                     # optional; default 1
expect.empty = true             # the scan should write nothing of substance
```

or, when specific output is expected:

```toml
[[test.expect.note]]
name = "finding"
expect = "identifies the .iter() misuse on the Sessions iterator; fix is .next()"

[[test.expect.issue]]
name = "general"
expect = "the Rune API's missing-method errors report opaque hashes"
```

Every scanner test states its expectation explicitly: `expect.empty = true` or
one or more note/issue entries — never neither. A comment above each test should
record the policy the label encodes and where the case came from.

## Authoring tests

Identify candidate sessions. The strongest candidates come from the resolve
session the user just completed: issues closed as bogus (scanner over-reported), issues
closed as legitimate (true positives worth preserving), and sessions where the
scanner missed something. Query recently closed issues and their evidence:

```sql
SELECT id, title, status, closed_reason FROM issue ORDER BY modified DESC
```

Join through `issue_evidence` and `session_note` to find the source sessions.

For each candidate, propose a test to the user: a short kebab-case name, the
session(s) to include, and the draft expectation. Expectations are the ground
truth the test measures against, and writing them is a policy decision, not a
transcription. A finding that faithfully reads the session may still be unwanted
output (for example, mining a user's stated intent as an issue). Present the
draft and what it commits the scanner to, and get the user's explicit
confirmation before writing anything. Keep expect prose short and specific: name
the concrete content the item must state, not the theme.

To materialize a confirmed test:

1. Get the session's source path: `SELECT path FROM session WHERE id = '<id>'`
2. Copy it: `mkdir -p ~/.gage/tmp/tests/fixtures/<name>/projects/<project-dir>/`
   and `cp <path>` there, where `<project-dir>` is the source file's parent
   directory name
3. Add the `[[test]]` entry to a TOML file under `~/.gage/tmp/tests/`

Promoting a test to source code means copying the `[[test]]` entry into a repo
suite file (`gage-test/tests/`) and the fixture dir into `gage-test/fixtures/`.
Whoever promotes is responsible for scrubbing the session content for
suitability as source, like any checked-in code.

## Running

```
gage test run -d ~/.gage/tmp/tests [SPEC...] [-j JOBS] [--judge-model MODEL] [--yes]
```

- The scanner pipeline is nondeterministic: `samples = 1` is a smoke signal; use
  `samples = 3` or more when the user is deciding whether a prompt change
  improved behavior
- Results print as match rates per expectation, plus unexpected-item and
  sample-completion rows; `gage test view <run>` renders the full report
- Per-sample artifacts persist under the run's
  `results/<suite>/<test>/sample{n}/`: the sandbox `gage-home/` (with its db),
  `dump.json` (what the scan wrote), `judge-prompt.md`, `judge-output.txt`,
  `verdict.json` (the judge's per-item reasons), and `ERROR` when a sample
  failed outright

When reporting results, lead with what changed or failed. For a failed item,
read the sample's `verdict.json` and `dump.json` and show the user the specific
mismatch: what was expected, what the scan actually wrote, and the judge's
reason. When a failure looks like a label problem rather than a scanner problem,
say so — the remedy is editing the test's expectation, not the scanner.

Interpret rates against the test's purpose: for a test capturing an
over-reporting failure (`expect.empty`), unexpected items are the signal to
watch; for a true-positive test, unmatched expectations are.
