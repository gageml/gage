---
title: Resolve
description:
  How the Gage resolve skill works through issues with you in a coding agent
  session
---

<!-- test -->

Resolve is a skill that the Gage plugin installs in your coding agent. It works
through your issues with you. The agent explains each issue, checks whether it
still applies, and makes the fixes you approve.

## Start a session

Run `gage resolve` to start a session from the terminal. Gage starts your coding
agent and runs the skill.

```cli
gage resolve
```

You can also run the skill from a session that is already open.

| Coding agent | Command               |
| ------------ | --------------------- |
| Claude Code  | `/gage:resolve`       |
| Codex        | `$gage:resolve`       |
| Pi           | `/skill:gage-resolve` |

To work on specific issues, name them by ID.

```cli
gage resolve 3f2a91c4
```

For the command options, see [`gage resolve`](/commands/resolve).

## What the session does

The session does three things in order.

1. It reconciles pending issues. A scanner writes an issue as pending when the
   issue may repeat one that already exists. The agent compares each pending
   issue with similar issues. It closes the duplicates and opens the rest. It
   asks you only when a pending issue matches an issue that you already closed.

2. It frees abandoned issues. An issue that was assigned to a session that has
   ended becomes open again.

3. It resolves open issues. The agent lists the open issues and asks where to
   start. You then work through them one at a time.

For the meaning of each status, see [Issues](/issues#status).

## Resolve an issue

The agent investigates an issue before it presents it. An issue can be stale or
wrong, so the agent checks the claim against your project as it is now.

The agent then presents the issue with these parts.

- The summary says what the issue is.
- The status says whether the agent verified the issue. An issue can also be
  unverified, no longer applicable, or invalid.
- The implication says what the issue means for you.
- The recommendation says how to proceed.

You decide what happens next. The agent makes no change until you approve it.

- To fix the issue, approve the fix. The agent makes the change and confirms
  that it worked. It then closes the issue as completed.
- To dismiss the issue, tell the agent. It closes the issue as wontfix and
  records your reason.
- To put the issue off, tell the agent. It returns the issue to open so that you
  can take it up later.

## Work in several sessions

You can run several resolve sessions at once. A session assigns an issue to
itself before it works on it, and other sessions skip assigned issues. Two
sessions do not work on the same issue.

A session in Pi cannot assign an issue. See
[Known issues](https://github.com/gageml/gage/blob/main/KNOWN_ISSUES.md#pi).

## Why resolve runs in your coding agent

Resolving an issue often changes a project. Resolve runs as a session in your
coding agent so that this work is part of the session record. Gage can scan a
resolve session like any other session, and you can trace a fix to the
discussion behind it.

A session also gives you a place to review each change before it is made.
Resolve works in any interface that your coding agent supports.
