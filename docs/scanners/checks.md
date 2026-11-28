---
title: checks
description:
  Applies several checks to each Claude session in one agent call, as an
  experimental alternative to running one scanner per check
---

:::note

This scanner is experimental.

:::

The scanner applies a set of checks to each session in one agent call. A check
describes one kind of issue to look for. The agent reads the session once and
writes findings for every check, where separate scanners would each read the
session again.

The scanner has two checks. They carry the instructions of the scanners they
are named for and write the same notes.

| Check         | Looks for                                    | Applies to                                 |
| ------------- | -------------------------------------------- | ------------------------------------------ |
| `general`     | Agent misbehavior and user friction          | Sessions with at least 200 characters of text |
| `code-review` | Defects in code the session wrote or changed | Sessions that edited files                 |

## How it works

The scanner runs three tasks.

The `attach_project_files` task adds two attachments per project that appears
in the scan: `project-rule-files` holds the project's rule files, such as
memory files, settings, skills, commands, and agent definitions, and
`project-stack-files` holds its manifest files. Each attachment targets the
sessions from that project.

The `summarize_projects` task summarizes each project. One agent call per
project reads the attached files and writes summary notes. When a project's
files have not changed since the last run, the existing summary carries forward
without a new agent call. A project with no rule or manifest files is skipped.

The `scan_sessions` task submits each session to one agent. The agent's prompt
holds the instructions of every selected check that applies to the session. The
`code-review` check's instructions include the project's summary notes. A
session that no selected check applies to is skipped.

Every run scans every session in full. A session scanned before is scanned
again, and each run's notes belong to the scan that wrote them. Repeat runs
over the same sessions are independent passes, which lets you compare runs
with different checks selected.

## Selecting checks

The `checks` param lists the checks to apply. It defaults to every check. To
apply one check, override the param:

```cli
gage scan -s 'checks#{checks: ["code-review"]}'
```

To set it in the config file:

```toml
[scanners.checks.params]
checks = ["general"]
```

## What it writes

The scanner writes notes that are in turn used by downstream issue writers. In
particular, [`findings`](/scanners/findings) reads `*[finding]` notes and
reports issues from them.

| Note                     | Description                                 | Target     |
| ------------------------ | ------------------------------------------- | ---------- |
| `rules[project-summary]` | Summarizes project rules                    | Attachment |
| `stack[project-summary]` | Summarizes a project stack and architecture | Attachment |
| `general[finding]`       | Noteworthy observation in a session         | Session    |
| `code-review[finding]`   | Code quality finding                        | Session    |

When known, a finding note also records the session lines the finding refers
to.

The notes have the same names as those the [`general`](/scanners/general)
and [`code-review`](/scanners/code-review) scanners write. Run `checks` in
a scan of its own, without those scanners, so that each finding is written
once.

## Running

The scanner is in no group. To run it:

```cli
gage scan -s checks
```
