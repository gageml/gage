---
description: Publish a session
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

- What motivates the user to publish a session (motivation)
- Who will have access to the shared session (target audience)
- What content must not be shared (filters and segments)

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

To do you job effectively, you should have an understanding of the user's
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

`secret`
: Redact secrets.

`pii`
: Redact personally identifiable information.

`language`
: Redact inappropriate language.

`system`
: Omit and redact information that reveals information about a system.

`non-narrative`
: Omit information that is not related to a session narrative.

`custom`
: User customized filter.

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
4. Review the pending session and, if the session is not ready to publish,
   modify filters and segmentation descriptions as needed and go to step 2
5. Apply the publish op to finalize published session

If the session is interactive, work with the user to understand their
motivation, target audience, and what's important to keep out of the published
session.

## Publish commands

Use `gage publish` commands to perform the steps.

### Step 1. Init the op

```shell
gage publish init -y --filter FILTER_NAME --segment SEGMENT_DESCRIPTION
```

Repeat `--filter` and `--segment` as needed.

This returns the session ID of the pending session (`SESSION` below).

Use filters and segment instructions based on your understanding of the user's
motivation, target audience, and express filter and segmentation needs.

This is the starting point of the process. The in the next steps you review the
public op and decide how to adjust it to meet the user goals.

### Step 2. Apply segmentation

If the user wants to segment the session, use the `message` table to identify
each segments line ranges.

List each segment.

```shell
gage publish segment list SESSION
```

Note the ID for each segment.

When you have identified the lines for each segment, add them:

```shell
gage publish segment edit -y SESSION ID -l LINES
```

Specify lines as a comma separated list of:

- `N` - line number
- `N-M` - line range from start `N` to end `M` inclusive

Segmentation should be applied _before_ checking text for redaction candidates.
Segmentation drop unrelated entries altogether including all text.

### Step 3. Identify text to redact

Filters and segments prune the pending session leaving a subset of text to check
for redactions.

Start a `redact-text` sub-agent by passing the session ID. The agent has
instructions to for getting filter redaction rules and for getting candidate
text. It returns a JSON encoded list of text to redact in the format:

```json
[
    [text_to_redact, reason_code, filter_name, no_mark_flag],
    ...
]
```

Use the return value to add the redacted text to the publish op:

```shell
echo 'AGENT_OUTPUT' | gage publish redact import -y ID -
```

### Step 4. Review the pending session and iterate as needed

Perform a final review of the session by reading the candidate text with
redactions applied:

```shell
gage publish redact candidate --applied
```

Text is shown in session order and can be used to verify that content is limited
to user-defined segments.

If you identify an error, you must stop and fix it. Consider any changes needed
to the public op settings and use any of these commands to address remaining
issues.

- `gage publish redact add SESSION TEXT` - redact additional text
- `gage publish redact remove SESSION TEXT` - remove a redaction
- `gage publish segment edit SESSION ID --lines LINES` - change segmentation
  lines
- `gage publish filter add SESSION NAME` - add an additional filter
- `gage publish filer remove SESSION NAME` - remove a filter

If you alter the filters or segments, you must re-run the text redaction in
step 3. Filters and segments affect what text is dropped.

Calls to `redact import` are idempotent. To remove a redaction, use
`redact remove`.

### Step 6. Apply publish op

When you are confident the session is ready to publish, use the `apply` command:

```shell
gage publish apply ID -y
```

## Interactive vs automated sessions

If you are running in an interactive session, the user participates in this
process. Walk the user through each step. Get input from the user to strengthen
yur understanding of user motivation, target audience, and needed filters.

When segmenting, summarized selected segment ranges to let the user review and
confirm the selection.

Summarize the redacted text without quoting specific redacted strings.

If the user wants a preview of the pending session, generate an HTML file they
can view in a browser.

If you are not running in an interactive session, proceed with the steps above
without user input. The user will review the published session after the
operation is applied and decide whether or not to distribute it or to delete it
and re-run the publish routine.
