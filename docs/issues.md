---
title: Issues
---

If a scanner finds something worth reporting, it creates an _issue_. An issue
provides a detailed description of the finding along with supporting evidence.

List unresolved issues:

```cli
gage issue list
```

To view issue details, run:

```cli
gage issue show <ISSUE_ID>
```

Each issue provides a description of the problem, any applicable evidence it
found, and instructions or guidance for resolving the issue.

Issues are written for both humans and Claude Code. You can resolve the issue
yourself and close it by running:

```cli
gage issue close --completed <ISSUE_ID>
```

If you want to dismiss the issue as a non-issue, use the `--wontfix` option.

```cli
gage issue close --wontfix <ISSUE_ID>
```

For more issue related commands, run `gage issue -h`.

It's handy to use Claude to resolve issues. To instruct Claude to review open
issues and help you resolve them, use the `/gage:resolve` command, which is
installed with the Gage plugin, or run `gage resolve`.

Claude has several Gage tools for resolving issues, which will require your
permission to use.

## Status

An issue is _pending_, _open_, or _closed_. Scanners that infer issues with an
agent write them as pending. The first phase of `gage resolve` reconciles
pending issues against existing ones: a pending issue that re-reports a known
condition is closed as a duplicate, and the rest are opened. An issue you add
with `gage issue add` is open unless you pass `--pending`.

A closed issue records a reason: _completed_, _wontfix_, or _duplicate_ of
another issue.

## Authors and keys

Like notes, every issue records an `author`, and a plain write always inserts.
The runtime sets `task:<scanner>:<task>` on issues a scanner task writes and
`user:<username>` on issues a person writes. See [Notes](/docs/notes#authors).

A scanner that wants a single issue to represent a condition across scans
gives the write a _key_. A later write under the same key replaces the live
issue instead of adding a second one. A write marked _once_ happens only when
no issue under the key exists in any status, so closing the issue keeps it
closed on later scans.
