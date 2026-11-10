---
description: |
    Publish a copy of a session for sharing. Use whenever the user asks to publish,
    share, redact, scrub, sanitize, or export a session, to prepare a session for a
    shared or public repository, or to remove secrets, personal information, paths,
    or profanity from a session before sharing it. Drives the gage publish commands
    from staging through review to apply.
---

## Background

Sessions contain information that is not suited for sharing:

- Leaked secrets (passwords, API keys, tokens, etc.)
- Personal information
- Content that doesn't apply to what's being shared
- Inappropriate language

To share a session, a user _publishes_ a modified version. A modification is one
of three types of changes:

- Remove session entries
- Remove entry values
- Redact text with optional `[REDACTED]` marker

Changes are limited to data removal only. Session content is not otherwise
changed.

A published session is structurally different from original sessions in two
ways:

- `origin` attribute is in the form `session:<ID>`
- `project` attribute is absent
- `publish` attribute records the filters applied, each with its version
- `changes.json` file records changes from original

## Publish overview

1. Identify sessions to publish
2. Understand the audience and intended use of each published session
3. Run `gage publish init -y <SESSION>` for each session with an initial set
   of options
4. Redact text for each session
5. Assign a new title to each session if applicable
6. Review the session before publishing to confirm the user goals are met
7. Apply the publish command to add each session to the store

### Skill arguments

The skill arguments may include `--description <TEXT>`: free text that states
what to publish, for whom, and for what use. Read the description as the user's
request and use it to decide the sessions, groups, filters, and segments that
the arguments do not already name. `--description` is not an option of any
`gage publish` subcommand; do not pass it on.

The other arguments are `gage publish init` options, `--share` options, and
session IDs.

### Sessions to publish

The user may want to publish more than one session.

If the user has not provided sessions, ask them what they want to publish. If
they don't know the IDs, use `mcp__plugin_gage_gage__Query` and in particular
the `message_text` TVF to find the sessions.

Consult `session` or `native_session('')` tables for recent session titles.

```sql
select id, title from session limit 20
```

```sql
select id, title, from native_session('') limit 20
```

### Filter groups

Filters remove content from a published session. Each filter lists the groups it
belongs to, and `gage publish init --group <GROUP>` runs every filter in a
group. Groups describe the audience or the use of a published session:

- `public` - Anyone in the world
- `trusted` - Only designated users
- `reading` - A narrative for reading; harness bookkeeping is left out

`gage publish init --list-filters` lists every filter with its groups and
description. `--filter <NAME>` adds one filter to those the groups select. A
filter's params are overridden in place, as
`--filter 'pii#{ prompt: "Watch for street addresses." }'`.

There is no default group. Ask the user which groups or filters apply when they
have not said.

### Shares

A share is a set of published sessions to push to others. It names the filters
every member must have been published with, each with an optional version
requirement. `gage share list` lists shares; `gage share show <SHARE>` shows a
share's description, filters, and members.

When the user wants published sessions in a share, or the skill arguments
include `--share <SHARE>`:

- Run `gage publish init -y --for-share <SHARE> <SESSION>` so the share's
  filters are selected. `--filter` and `--group` add to them.
- Pass `--share <SHARE>` to `gage publish apply` for each session. Apply checks
  the definition against the share's filters before writing, then adds the
  published session to the share. `--share` is repeatable.

`--share` is not an option of `gage publish init`. A share that refuses a
session names the filter it requires; add that filter and apply again.

## Publish interface

A session is published by first staging it using `gage publish init`.

Get help for this command:

```bash
gage publish init --help
```

This creates a staged session under `~/.gage/publish/<SESSION>`.

## Text redaction

Use `gage publish candidate <SESSION> --split-files` to split candidate text
into files that are provided to `gage:redact` agents. The command prints one
file PATH per line. Each file contains candidate text to redact. Start one agent
pre file, passing it `ID` and `PATH`.

Each agent returns a list of rows to redact, each `[text, reason, mark]`.
Collect the lists from every agent into one JSON array and add them in one call:

```bash
gage publish edit -y <SESSION> --redact @FILE
```

The command fills the author of each row from the current session and drops rows
that exactly duplicate a row already present. Do not set the author yourself.

To add additional redacted text without creating a file, use stdin:

```bash
gage publish edit -y <SESSION> --redact -
```

## Title

Set or change the published session's title:

```bash
gage publish edit -y <SESSION> --title "<TEXT>"
```

## Review the session

Use `mcp__plugin_gage_gage__Query` with the `publish` flag set to `true` to
review staged sessions. The `session_change` table records what the filters
and redactions removed:

```sql
select type, reason, author, count(*) as n from session_change
group by type, reason, author order by type, n desc
```

Report the staged title and message count as the `session` table returns them:

```sql
select title, summary->>'message_count' as messages from session
```

If the session is interactive, advise the user to review the session by giving
them the command:

```bash
gage publish view <SESSION>
```

This is an interactive TUI application that the user must run.

Get the user's input after reviewing the session. Make changes accordingly by
editing the file directly or using `--redact` or `--title` as per above.

After editing the file, check it:

```bash
gage publish edit <SESSION> --check
```

When the user approves the session apply it.

## Apply the staged session

Use `gage publish apply -y <SESSION>` to add the staged publish session to the
store. This finalizes the publish operation and removes the session from the
pending list. With `--share <SHARE>`, the published session is also added to the
share.
