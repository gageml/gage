---
description: Publish a shareable copy of a session. Use whenever the user asks to publish, share, redact, scrub, sanitize, or export a session, to prepare a session for a shared or public repository, or to remove secrets, personal information, paths, or profanity from a session before sharing it. Drives the gage publish commands from staging through review to apply.
---

## Background

Gage stores agent sessions as commits in a Git repo. This makes sharing sessions
easier as they are pushed and fetched like any commit in Git.

However, sessions contain information that is not suited for sharing:

- Leaked secrets (passwords, API keys, tokens, etc.)
- Personal information
- Mixed content, some of which doesn't apply to what's being shared
- Language inappropriate for sharing

To facilitate sharing, Gage supports a _publish_ function that creates a derived
session with filtered content. These are referred to as _published sessions_. A
published session can differ from its original in four ways:

- Dropped session entries (source content)
- Dropped entry values (source content)
- Redacted text (source content)
- Dropped session attributes (metadata)

Published sessions do not contain new content apart from redaction markers. They
are strictly subsets of the original.

Your task is to help the user publish one or more sessions.

## Process

It's hard to filter a session to ensure that only intended information is
shared. The criteria for a correctly filtered session is user intention. It's
critical to understand why a user wants to publish a session.

You need to understand this information:

- What motivates the user to publish a session (**motivation**)
- Who will have access to the shared session (**target audience**)
- What content must not be shared (**filters and segments**)

The user may not initially have this information. Due to the sensitivity of
sharing sessions, the user must form a clear understanding of the implications
of sharing a session.

Implications of sharing the wrong information:

- Leaked secret leads to compromised systems
- Sharing of personal information causes reputational harm
- Sharing of confidential information such as trade secrets is a contractual
  breach

### Motivation

Common reasons to publish sessions:

- **Submit work history to a shared repository.** This may be required by an
  organization or may be at the user's discretion. Work history is useful for
  capturing design thinking, decisions, and agent coding history. The session
  can be used with tools like Gage blame (match session records to source code
  lines), Gage query (full text search, structured analysis using SQL), and Gage
  scan (scan sessions for issues).

- **Share an anecdote.** A user may want to share an experience or insight by
  publishing a session segment. This is closely related to blog publishing. The
  intent is to share specific human readable material rather than provide a
  general record.

- **Submit task work history for review, audit, or general retention.** A user
  may want to provide a session record of how work on a specific task was done.
  This is a narrow form of general session sharing. The motivation is not to add
  to a general repository but to provide a supporting record for a completed
  task. The record may be subject to review or audit before the work product is
  approved.

A user may have any number of motivations for sharing a session.

To do your job effectively, you should have an understanding of the user's
motivation.

### Target audience

A published session is meant to be shared. A user shares the session by pushing
it to a repository that other users (or agents) have access to. To determine
what information can be shared, you must understand who will eventually see the
information.

There are two classes of audience:

- Private access (limited audience)
- Public access (general audience)

These classes correspond to private and public Git repositories respectively.

Publishing shared sessions to a general audience requires the highest level of
scrutiny. A public audience represents all users, including those actively
seeking to do harm to the user.

A limited audience includes only those who the user grants access to.

A limited audience may be highly trusted or not trusted. It's imperative to know
who will be given access to the published session in all cases.

### Filters

Filters determine what is removed from a session. Filters apply to different
data domains and generally correspond to risk categories.

A filter specifies:

- Entries to drop
- Entry values to drop
- Text patterns (regex) to redact
- Session attributes to drop
- Model instructions for redacting text

Filters are named.

Gage supports these filters:

`pii`
: Remove personally identifiable information.

`secret`
: Remove passwords, API keys, and other secrets.

`language`
: Remove profanity and other language unfit for a public record.

`system`
: Remove paths, hosts, and OS versions.

`harness-context`
: Leave out what the harness placed in the model's context: the system prompt
  and tool definitions, message content as sent with its reminders and
  instruction files, attachments (rendered reminders, instruction files, skill
  and agent listings, hook output), and injected meta entries. What the model
  saw is part of the conversation, so this filter is a motivation and audience
  decision: on for a conversation record, off for an exact record of the run.

`harness-records`
: Leave out records the harness kept about the run: API request records, file
  snapshots, queue operations, generated titles, mode and permission records,
  cost state, the system entries the conversation does not show, and the
  run-description fields (version, token usage, request IDs) on the entries that
  stay. No reader wants these, so this filter is on for every publish unless the
  user says otherwise.

`custom`
: Remove what a prompt of your own describes. Added with
  `gage publish filter add` and `--prompt`, never with `init --filter`.

`gage publish init --list-filters` prints this list.

### Segments

A segment is a subset of session entries. Segments limit what is shared at the
entry level.

Segments should always be applied in these cases:

- **User wants to share a specific session anecdote.** Only anecdote related
  entries should be shared. The rest must be dropped.

- **Session is submitted for task work review.** Only entries related to the
  specified task should be shared. The rest must be dropped.

In cases where the full session history is shared, segments may not be
applicable.

### Publish steps

Publishing a session is done in a series of steps involving a publish operation,
or _op_.

1. Initialize the publish op using initial filters and segments if known
2. Apply segmentation
3. Identify text to redact
4. Review the pending op and, if the session is not ready to publish, modify
   filters, segments, and redactions as needed and go to step 2
5. Apply the publish op to finalize the published session

If the session is interactive, work with the user to understand their
motivation, target audience, and what's important to keep out of the published
session.

## Publish commands

Use `gage publish` commands to perform the steps. Every command that changes the
op takes `-y` to skip its confirmation prompt. The ID an op is known by is the
pending session ID that `init` prints (`SESSION` below). Any prefix of it that
is unambiguous works.

### Step 1. Init the op

A native Claude Code session is named by its ID or an ID prefix:

```shell
gage publish init -y --filter FILTER --segment TEXT SESSION_TO_PUBLISH
```

A session already in the Gage store is named by tag, ID, or prefix with `-S`:

```shell
gage publish init -y -S --filter FILTER --segment TEXT SESSION_TO_PUBLISH
```

Without `-S`, `init` looks for a native session only and a stored session is "no
native session matches". `gage session list` lists native sessions and
`gage session list -S` stored ones.

Repeat `--filter` and `--segment` as needed. A segment given here starts without
lines and drops nothing until step 2 gives it lines.

The command prints a dialog ending in `Staged publish SESSION`. It also reports,
per filter, the redactions the filter's deterministic patterns staged at once.

Use filters and segment descriptions based on your understanding of the user's
motivation, target audience, and express filter and segmentation needs.

This is the starting point of the process. In the next steps you review the
pending op and decide how to adjust it to meet the user's goals.

### Step 2. Apply segmentation

If the user wants to segment the session, use the `message` table of the Gage
Query tool to identify each segment's line ranges. A stored session is read with
its stored ID; a native session with `native_message('')` and its native ID.

List each segment and note its ID:

```shell
gage publish segment list SESSION
```

When you have identified the lines for a segment, set them. The description is
optional and keeps its current value when left out:

```shell
gage publish segment edit -y SESSION ID --lines LINES
```

A segment can also be added with its lines in one command:

```shell
gage publish segment add -y SESSION TEXT --lines LINES
```

Specify lines as a comma separated list of:

- `N` - line number
- `N-M` - line range from start `N` to end `M` inclusive

Once any segment has lines, every line outside every segment is dropped.

Segmentation is applied _before_ checking text for redaction candidates.
Segmentation drops unrelated entries altogether, including all their text.

### Step 3. Identify text to redact

Filters and segments prune the pending session, leaving a subset of text to
check for redactions.

Start a `gage:redacted-text` sub-agent with the pending session ID. The agent
has instructions for getting the filter redaction instructions and the candidate
text. It returns a JSON encoded list of text to redact in the format:

```json
[
    [text_to_redact, reason_code, no_mark_flag],
    ...
]
```

Write the returned JSON to a file in your scratchpad directory and import it:

```shell
gage publish redact import -y SESSION PATH
```

The author of the imported redactions is the current Claude Code session, which
the command reads from `CLAUDE_CODE_SESSION_ID`. Do not pass `--author`.

Do not read the candidate text yourself. It is the whole text of the pending
session and is large. The agent reads it so that you do not have to.

### Step 4. Review the pending op and iterate as needed

Review the op's definition, not the session text:

```shell
gage publish status SESSION
gage publish redact list SESSION
```

`redact list` shows one row per redaction: its ID, the text, the reason, the
author, and the number of instances. Check that every class of content the user
named is covered, that nothing is redacted that the user wants kept, and that
the segments are the lines the user chose.

If you identify an error, you must stop and fix it. Use any of these commands:

- `gage publish redact add -y SESSION TEXT --reason REASON` - redact additional
  text everywhere it occurs; `--no-mark` removes it without a marker; `TEXT` is
  matched case-insensitively and must occur in the session; `--reason` defaults
  to `custom`
- `gage publish redact remove -y SESSION ID...` - remove redactions by the IDs
  `redact list` shows
- `gage publish redact edit -y SESSION ID [TEXT] [--reason REASON] [--mark | --no-mark]` -
  change a redaction
- `gage publish segment edit -y SESSION ID [TEXT] [--lines LINES]` - change a
  segment's description, its lines, or both
- `gage publish filter add -y SESSION NAME` - add a filter; `custom` requires
  `--prompt TEXT`
- `gage publish filter remove -y SESSION NAME` - remove a filter and its
  redactions

If you alter the filters or segments, re-run step 3. Filters and segments change
what text is dropped and what remains to check.

`redact import` is idempotent: a redaction already present is reported as such
and not duplicated. To remove a redaction, use `redact remove`.

`gage publish abort -y SESSION` deletes a pending op without publishing.

### Step 5. Apply the publish op

When you are confident the session is ready to publish, use the `apply` command:

```shell
gage publish apply -y SESSION
```

`apply` verifies that the staged session is the original with removals only,
adds it to the store as a new session, and deletes the pending op. It is the
only publish command that writes the store. `--tag NAME` tags the published
session.

## Interactive vs automated sessions

If you are running in an interactive session, the user participates in this
process. Walk the user through each step. Get input from the user to strengthen
your understanding of user motivation, target audience, and needed filters.

When segmenting, summarize selected segment ranges to let the user review and
confirm the selection.

Summarize the redacted text by class and count without quoting specific redacted
strings.

If you are not running in an interactive session, proceed with the steps above
without user input. The user will review the published session after the
operation is applied and decide whether or not to distribute it or to delete it
and re-run the publish routine.
