---
title: findings
description: Reports issues inferred from finding notes written by other scanners, independent of finding type
---

The scanner reports issues inferred from finding notes written by other
scanners, such as code review and general findings. The scanner is
issue-type agnostic. Any finding kind participates by declaring a
descriptive note doc, with no change to this scanner.

## How it works

The scanner runs one task, `review_findings`. It collects every note named
`*[finding]` in the current scan that an earlier run has not reported, along
with each finding kind's note doc. The doc describes what that kind's notes
contain and how they were produced. An agent reads the findings and decides
which show a real underlying problem worth reporting.

Not every finding implies an issue. The agent weighs evidence by
repetition across sessions, by verifying concrete claims against session
or project content, and by severity. An issue must give you something to
act on, such as a change to the project, its configuration, or the rules
and memory files that direct its agents. Similar findings across
sessions are reported as one issue, not one issue per instance.

Finding notes from earlier scans are carried forward so the agent sees the
history behind a session. Findings the task has reported are watermarked and
not reported again. When a scan produces no unreported findings, the task does
nothing.

## What it writes

The scanner writes issues, not notes. The supporting finding notes are
attached to each issue as evidence. Issues are written as _pending_; the
first phase of `gage resolve` reconciles them against existing issues. See
[Issues](/docs/issues#status).

| Issue      | Description                         |
| ---------- | ----------------------------------- |
| `findings` | Issue derived from finding notes    |

## Running

The scanner is in the `default` group. It runs when you run `gage scan`
without selecting scanners. It only reports when the same scan includes a
scanner that writes finding notes, such as
[`code-review`](/docs/scanners/code-review) and
[`general`](/docs/scanners/general). To run it with them explicitly:

```cli
gage scan -s general -s findings
```
