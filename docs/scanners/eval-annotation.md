---
title: eval-annotation
description:
  Annotates Claude sessions with open codes for error analysis, recording
  observations of wrong or surprising behavior
---

:::note

This scanner is for internal use and is not generally included in scan runs.

:::

The scanner annotates sessions with open codes for error analysis. An open code
is a brief, unstructured observation of something wrong or surprising in a
session. Open codes are the raw input to axial coding, which clusters
observations across sessions into recurring failure modes. A session with
nothing worth noting is still marked as annotated.

## How it works

The scanner submits each session to an annotator agent. The agent reads the
session and writes one open code note per observation. An open code is a short,
plain statement of what went wrong or what was surprising. It can anchor to the
session line where the observed failure first shows.

Sessions with less than 200 characters of text are skipped. A session with
nothing worth noting gets a note recording that it was reviewed. The note's
presence marks the session as annotated.

Every run annotates every target session again. Each run's notes carry a
scan-qualified author, so repeat runs are independent annotation passes. Later
runs add notes without replacing earlier ones.

## What it writes

The scanner writes notes only. It reports no issues. Clustering open codes into
failure modes is a separate analysis step.

| Note   | Description   | Target                     |
| ------ | ------------- | -------------------------- |
| `open` | One open code | Session, optionally a line |

## Running

This scanner is designed for Gage agent session analysis and is not typically
run on normal Claude sessions. It's hidden from scanner listings for this
reason.

To use the scanner to for Gage agent error analysis, run:

```cli
gage scan --scan SCAN_ID
```

To run it on a normal Claude session --- e.g. a Gage issue resolution session,
run:

```cli
gage scan RESOLVE_SESSION_ID -s eval-annotation
```
