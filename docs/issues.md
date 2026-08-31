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
gage issue close <ISSUE_ID>
```

This marks the issue _completed_. If you want to skip the issue (i.e. mark it as
a non-issue) use the `--skipped` option.

```cli
gage issue close --skipped <ISSUE_ID>
```

For more issue related commands, run `gage issue -h`.

It's handy to use Claude to resolve issues. To instruct Claude to review open
issues and help you resolve them, use the `/gage:resolve` command, which is
installed with the Gage plugin.

Claude has several Gage tools for resolving issues, which will require your
permission to use.

## Authors and duplicate policies

Like notes, every issue records an `author`, and a plain write always inserts —
nothing constrains `(name, author)`. A writer that wants to fold a re-scan into
its prior issue states a merge policy on the write (`keep_status`,
`open_on_new_evidence`, `open_on_changed_evidence`), which acts on the most
recent issue it wrote with the same name: new evidence is added, and the reopen
policies reopen a closed issue when the evidence warrants it. Author values
follow the scheme described in [Notes](/docs/notes): `scanner:<name>` for
deterministic writers, `agent:...?call=<toolUseId>` values tying model writers
to the authoring transcript entry, and `user:<username>` for people.
