---
title: finding-issues
description: Reports issues inferred from finding notes written by other scanners, independent of finding type
---

The scanner reports issues inferred from finding notes written by other
scanners, such as code review and general findings. The scanner is
issue-type agnostic. Any finding kind participates by declaring a
descriptive note doc, with no change to this scanner.

## How it works

An agent reads the finding notes written during the current scan, along
with each finding kind's note doc. The doc describes what that kind's
notes contain and how they were produced. The agent decides which
findings show a real underlying problem worth reporting.

Not every finding implies an issue. The agent weighs evidence by
repetition across sessions, by verifying concrete claims against session
or project content, and by severity. An issue must give you something to
act on, such as a change to the project, its configuration, or the rules
and memory files that direct its agents. Similar findings across
sessions are reported as one issue, not one issue per instance.

When a scan produces no new findings, the task does nothing.

## What it writes

The scanner writes issues, not notes. The supporting finding notes are
attached to each issue as evidence.

| Issue      | Description                         |
| ---------- | ----------------------------------- |
| `findings` | Issue derived from session findings |

## Running

This is a library scanner. It is not listed and cannot be selected
directly. It runs automatically in any scan whose selected scanners
write finding notes, such as [`code-review`](/docs/scanners/code-review)
and [`general`](/docs/scanners/general).
