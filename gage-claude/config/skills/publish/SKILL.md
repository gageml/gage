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

`init` takes the audience as `--audience`:

`self`
: Private use only. No filter is selected for it.

`trusted`
: An audience the user knows and trusts. A trusted audience includes only those
  who the user grants access to, such as the readers of a private Git
  repository.

`public`
: Anyone. A public audience represents all users, including those actively
  seeking to do harm to the user, such as the readers of a public Git
  repository. Publishing to a public audience requires the highest level of
  scrutiny.

The values are independent: each filter lists the audiences it is selected for,
and one value implies nothing about another. It's imperative to know who will be
given access to the published session in all cases.

### Use

A published session is read as a narrative, kept as a record of work, or put
to another use. `init` takes the use as `--use`:

`reading`
: Reading for narrative content. The entries a reader does not want are left
  out.

`provenance`
: A record of work. Every entry stays; no filter is selected for it.

`other`
: Another use. No filter is selected for it.

### Filters

Filters determine what is removed from a session. Filters apply to different
data domains and generally correspond to risk categories.

A filter specifies:

- Entries to drop
- Entry values to drop
- Text patterns (regex) to redact
- Session attributes to drop
- Model instructions for redacting text

Filters are named. Each filter lists the audiences and the uses it is selected
for; `--audience` and `--use` on `init` select every filter that lists the
value, and `--filter` adds to that selection.

Gage supports these filters:

`pii` (audience: public)
: Remove personally identifiable information.

`secret` (audience: public, trusted)
: Remove passwords, API keys, and other secrets.

`language` (audience: public, trusted)
: Remove profanity and other language unfit for a shared record.

`system` (audience: public)
: Remove paths, hosts, and OS versions.

`harness-context` (use: reading)
: Leave out what the harness placed in the model's context: the system prompt
  and tool definitions, message content as sent with its reminders and
  instruction files, attachments (rendered reminders, instruction files, skill
  and agent listings, hook output), and injected meta entries. What the model
  saw is part of the conversation, so a reader of the narrative does not want
  it, and a scan of the exact run does.

`harness-records` (use: reading)
: Leave out records the harness kept about the run: API request records, file
  snapshots, queue operations, generated titles, mode and permission records,
  cost state, the system entries the conversation does not show, and the
  run-description fields (version, token usage, request IDs) on the entries that
  stay. No reader wants these; a scan keeps them.

`custom`
: Remove what a prompt of your own describes. Added in the definition file as
  `["custom", PROMPT]`, never with `init --filter`.

`gage publish init --list-filters` prints this list with each filter's audiences
and uses.

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
or _op_. An op is one pending publish of one session. Its definition is a file
you edit: `init` writes it, and `refresh` rebuilds the staged session from it.

1. Initialize the op using initial filters and segments if known
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

### The definition file

`gage publish show SESSION` prints the op's definition. It is JSON with
comments, at `publish/SESSION.publish.jsonc` under Gage home. The comments in
the file describe each key. In outline:

```jsonc
{
  "session": "...", // title: ...
  "init": {"audience": "public", "use": "reading", "notes": "..."},
  "filters": ["pii", ["custom", "PROMPT"]],
  "segments": [["100-125,130", "DESCRIPTION", "AUTHOR"]],
  "allow": [["TEXT", "AUTHOR"]],
  "redact": [["TEXT", "REASON", "AUTHOR", MARK]],
}
```

- `init` records what `init` was given. The publish does not read it.
- `filters` is a filter name, or `[name, prompt]` for a filter with a prompt.
  `custom` requires a prompt.
- `segments` rows are `[lines, description, author]`. Lines are 1-based
  inclusive ranges of the original session as `"12-20,31"`, or `null` for a
  segment that does not have its lines yet. Once any segment has lines, every
  line outside every segment is dropped.
- `redact` rows are `[text, reason, author, mark]`. The text is matched
  case-insensitively everywhere it occurs. `mark` is `true` for a `[REDACTED]`
  marker and `false` to remove the text with no marker. A row written as
  `[text, reason, mark]` takes the author of the refresh that reads it, which
  is your session when `CLAUDE_CODE_SESSION_ID` is set.
- `allow` rows are `[text, author]`: text a named filter found that must stay.
  A filter's row deleted from `redact` comes back on the next refresh unless its
  text is listed here.

Change the file with your file editing tools, then rebuild the staged session:

```shell
gage publish refresh -y SESSION
```

`refresh` validates the file and names the row that is wrong by position. It
re-runs every named filter, fills in missing authors, rewrites the file, and
warns about each redaction that matches nothing in the session. An invalid file
changes nothing and is left as written.

### Step 1. Init the op

A native Claude Code session is named by its ID or an ID prefix:

```shell
gage publish init -y --audience AUDIENCE --use USE --notes TEXT --filter FILTER --segment TEXT SESSION_TO_PUBLISH
```

A session already in the Gage store is named by tag, ID, or prefix with `-S`:

```shell
gage publish init -y -S --audience AUDIENCE --use USE --notes TEXT --filter FILTER --segment TEXT SESSION_TO_PUBLISH
```

Always pass `--audience` and `--use`, so the publish states who it is for and
what it is for. Without them `init` selects no filter by audience or use. Pass
`--notes` with what you know of the user's motivation; it is recorded in the
file for the record.

Without `-S`, `init` looks for a native session only and a stored session is "no
native session matches". `gage session list` lists native sessions and
`gage session list -S` stored ones. `init` also takes the session selection
options of `gage scan`, and stages one op per selected session.

Repeat `--filter` and `--segment` as needed. A filter given here is added to
those the audience and use select. A segment given here starts without lines
and drops nothing until step 2 gives it lines.

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

When you have identified the lines for a segment, set them in the file: a
segment row is `[lines, description, author]`, and `init` wrote each segment
with `null` lines. A segment can also be added with its lines. Specify lines as
a comma separated list of:

- `N` - line number
- `N-M` - line range from start `N` to end `M` inclusive

Once any segment has lines, every line outside every segment is dropped. Run
`gage publish refresh -y SESSION` after the edit.

Segmentation is applied _before_ checking text for redaction candidates.
Segmentation drops unrelated entries altogether, including all their text.

### Step 3. Identify text to redact

Filters and segments prune the pending session, leaving a subset of text to
check for redactions.

Write the candidate text to files:

```shell
gage publish candidate SESSION --split-files
```

The command prints one path per line. Each file holds a share of the distinct
text values of the pending session, one per line as a JSON string, bounded in
size so that one sub-agent can read it in full.

Start one `gage:redacted-text` sub-agent per path, all in the same message so
they run at once. Give each the pending session ID and its path. The agent has
instructions for getting the filter redaction instructions and reading its
file. Each returns a JSON encoded list of text to redact in the format:

```json
[
    [text_to_redact, reason_code, no_mark_flag],
    ...
]
```

Append each returned item to `redact` in the file as `[text, reason, mark]`,
where `mark` is the opposite of `no_mark_flag`. Leave the author out: the
refresh records your session as the author. Then run
`gage publish refresh -y SESSION`. A text two agents both return is two rows
that redact the same text; delete one.

Do not read the candidate files yourself. They are the whole text of the
pending session and are large. The agents read them so that you do not have to.

### Step 4. Review the pending op and iterate as needed

Review the op's definition, not the session text:

```shell
gage publish show SESSION
```

Check that every class of content the user named is covered, that nothing is
redacted that the user wants kept, and that the segments are the lines the user
chose. The refresh output reports the number of instances and warns about each
redaction that matches nothing.

If you identify an error, you must stop and fix it in the file and refresh:

- Redact additional text everywhere it occurs: add a `redact` row. The reason
  defaults to nothing; give one of the reason codes the file's comment lists,
  and `custom` when no other fits.
- Remove a redaction: delete its row. A row a named filter recorded comes back
  on refresh unless its text is added to `allow`.
- Change a redaction: edit its text, reason, or mark.
- Change a segment: edit its description or lines.
- Add a filter: add its name to `filters`. `custom` is `["custom", PROMPT]`.
- Remove a filter: delete it from `filters`. Its redactions go with it on
  refresh.

If you alter the filters or segments, re-run step 3. Filters and segments change
what text is dropped and what remains to check.

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
