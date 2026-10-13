---
title: Scans
---

A _scan_ (or scan _run_) is the process of scanning sessions.

To start a scan, run:

```cli
gage scan
```

Gage prompts you to select scanners to run along with what you'd like to scan.

Gage runs each scanner over each of the selected sessions to find issues.

A scan reads from a _dataset_, a set of stored sessions and attachments. When
you select sessions, Gage adds them to its store, creates a dataset holding
them, and scans it. To scan the same sessions again, or a set you assembled
yourself, pass the dataset to `gage scan --dataset`. See
[`gage dataset`](/docs/commands/dataset).

Gage maintains a record of each scan. To list scans, run:

```cli
gage scan list
```

To open a scan's tasks, sessions, notes, and issues, run:

```cli
gage scan view
```

For a full list of scan related commands run `gage scan -h`.
