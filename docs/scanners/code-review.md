---
title: code-review
description:
  Reviews code written or changed in Claude sessions for rule violations,
  best-practice violations, and undetected bugs
---

The scanner reviews the code a session wrote or changed for defects. Defects
include project rule violations, best-practice violations, poor stack alignment,
and bugs the session did not detect. Each finding names the violated project
rule and describes the defect concretely enough to check against current project
content. Defects the session itself diagnosed and fixed are skipped.

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

The `scan_sessions` task reviews each session that edited files. Sessions with
no file edits are skipped. A review agent reads the session, informed by the
project's summary notes, and writes one finding note per defect. A session that
has grown since the last scan is reviewed only over its new lines.

## What it writes

The scanner writes notes that are in turn used by downstream issue writers. In
particular, [`findings`](/docs/scanners/findings) reads `code-review[finding]`
notes and reports issues from them.

| Note                     | Description                                 | Target     |
| ------------------------ | ------------------------------------------- | ---------- |
| `rules[project-summary]` | Summarizes project rules                    | Attachment |
| `stack[project-summary]` | Summarizes a project stack and architecture | Attachment |
| `code-review[finding]`   | Code quality finding                        | Session    |

## Running

The scanner is in the `default` group. It runs when you run `gage scan`
without selecting scanners. To run it explicitly:

```cli
gage scan -s code-review
```
