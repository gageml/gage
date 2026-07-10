---
title: Issues
---

If a scanner finds something worth reporting, it creates an _issue_. An issue
provides a detailed description of the finding along supporting evidence.

List unresolved issues:

```shell
gage issue list
```

To view issue details, run:

```shell
gage issue show <ISSUE_ID>
```

Each issue provides a description of the problem, any applicable evidence it
found, and instructions or guidance for resolving the issue.

Issues are written for both humans and Claude Code. You can resolve the issue
yourself and close it by running:

```shell
gage issue close <ISSUE_ID>
```

This marks the issue _completed_. If you want to skip the issue (i.e. mark it
as a non-issue) use the `--skipped` option.

```shell
gage issue close <ISSUE_ID>
```

For more issue related commands, run `gage issue -h`

It's handy to use Claude to resolve issues. To instruct Claude to review open
issues and help you resolve them, use the `/gage:review` command, which is
installed with the Gage plugin.

Claude has several Gage tools for resolving issues, which will require your
permission to use.

## Authors and duplicate detection

Like notes, every issue records an `author`, and the issue duplicate key is
`(name, author)` — the same writer opening the same issue twice is detected
at write time. Author values follow the scheme described in
[Notes](notes.md): `scanner:<name>` for deterministic writers,
`agent:...` values carrying per-run instance identity for model writers,
and `user:<username>` for people.
