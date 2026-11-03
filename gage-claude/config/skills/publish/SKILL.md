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
of four types of changes:

- Dropped session entries
- Dropped entry values
- Redacted text with optional `[REDACTED]` marker
- Dropped session attributes

Changes are limited to data removal only. Session content is not otherwise
changed.

A published session is structurally different from original sessions in two
ways:

- `origin` attribute is in the form `session:<ID>`
- `changes.json` file records changes from original

## Publish overview

1. Identify sessions to publish
2. Understand the audience and intended use of each published session
3. Run `gage publish init <SESSION>` for each session with an initial set of
   options
4. Redact text for each session
5. Assign a new title to each session if applicable
6. Apply the publish command to add each session to the store

### Sessions to publish

The user may want to publish more than one session.

If the user has not provided sessions, ask them what they want to publish. If
they don't know the IDs, use `mcp__plugin_gage_gage__Query` and in particular
the `message_text` TVF to find the sessions.

### Audience and use

There are two categories of target audience for a session:

- `public` - Anyone in the world
- `trusted` - Only designated users

## Publish interface

A session is published by first staging it using `gage publish init`.

Get help for this command:

```shell
gage publish init --help
```

This creates a staged session under `~/.gage/publish/<SESSION>`.

Common edits use `gage publish edit`: `--redact` adds text redactions and
`--title` sets the title. Any other change is a direct edit of the definition
file. `gage publish edit <SESSION> --path` prints the file path, and
`gage publish edit <SESSION> --check` validates the file and reports problems by
line.

Use `mcp__plugin_gage_gage__Query` with the `publish` flag set to `true` to
preview staged sessions.

Use `gage publish apply <SESSION> -y` to add the staged publish session to the
store. This finalized the publish operation and removed the session from the
pending list.

## Text redaction

Use `gage publish candidate <SESSION> -y` to split candidate text into files
that are provided to `redact` agents.

Each agent returns a list of rows to redact, each `[text, reason, mark]`.
Collect the lists from every agent into one JSON array and add them in one call:

```bash
gage publish edit <SESSION> --redact @FILE
```

`gage publish edit <SESSION> --redact -` reads the array from stdin. The command
fills the author of each row from the current session and drops rows that
exactly duplicate a row already present. Do not set the author yourself.

## Title

Set or change the published session's title:

```bash
gage publish edit <SESSION> --title "<TEXT>"
```
