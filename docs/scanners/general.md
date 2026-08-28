---
title: general
description: Scans Claude sessions for noteworthy findings such as agent misbehavior and user friction
---

The scanner scans sessions for noteworthy findings. A finding is an
observation of agent behavior or user friction made by an agent that
read the session. Problems the session itself diagnosed and fixed are
skipped.

## How it works

The scanner submits each session to an agent that reads the session and
writes one finding note per observation. The agent sees a single session
and has no visibility across sessions.

Sessions with less than 200 characters of text are skipped. A session
already scanned is not scanned again. A session that has grown since the
last scan is scanned only over its new lines.

## What it writes

The scanner writes notes that are in turn used by downstream issue
writers. In particular, [`finding-issues`](/docs/scanners/finding-issues)
reads `finding.general` notes and reports issues from them.

| Note              | Description                         | Target  |
| ----------------- | ----------------------------------- | ------- |
| `finding.general` | Noteworthy observation in a session | Session |

When known, the note also records the session lines the observation
refers to.

## Running

The scanner is in the `default` group, so it runs when you run `gage
scan` without selecting scanners. To run it alone:

```cli
gage scan -s general
```
