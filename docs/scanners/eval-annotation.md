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

The scanner runs one task, `annotate`. It submits each session to an
annotator agent. The agent reads the session and writes one open code note
per observation. An open code is a short, plain statement of what went wrong
or what was surprising. It can anchor to the session line where the observed
failure first shows.

Sessions with less than 200 characters of text are skipped. A session with
nothing worth noting gets a note recording that it was reviewed. The note's
presence marks the session as annotated.

Every run annotates every target session again. Each run's notes belong to
the scan that wrote them, so repeat runs are independent annotation passes.
Later runs add notes without replacing earlier ones.

## What it writes

The scanner writes notes only. It reports no issues. Clustering open codes into
failure modes is a separate analysis step.

| Note   | Description   | Target                     |
| ------ | ------------- | -------------------------- |
| `open` | One open code | Session, optionally a line |

## Running

The scanner is in the `eval` group and is hidden from scanner listings. It is
designed for analysis of Claude sessions that ran Gage work, such as a
`gage resolve` session. To run it on such a session:

```cli
gage scan RESOLVE_SESSION_ID -s eval-annotation
```

Agent sessions that a scan spawned are not dataset members and cannot be
selected for a scan.
