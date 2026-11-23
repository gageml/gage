---
title: Recall a decision
description:
  How to ask your coding agent why code is the way it is and get an answer
  from the sessions that wrote it
---

Your code records what was decided. It does not record why. The reasons are in
your sessions: the discussion before each change, the alternatives that were
turned down, and the reasoning your agent followed. Without that record, you and
your agent guess at the intent of old code, repeat ideas that already failed,
and undo choices that were made on purpose. With Gage, your agent reads the
sessions behind the code and answers from what was said there.

## Before you start

Install Gage and run `gage init`. See [Quick start](/start). `gage init`
installs the Gage plugin in each coding agent you have, and the plugin is what
lets your agent read your sessions.

## Steps

1. Start your coding agent in the project you want to ask about.

2. Ask your question in plain words. Name the file, function, or decision.

   ```text
   Why does the session parser skip compressed entries?
   ```

   ```text
   What did we consider before we settled on the store layout?
   ```

   ```text
   Why is src/index.rs lines 40-72 written this way?
   ```

   Your agent finds the sessions that wrote the code, reads the discussion that
   led to it, and answers.

3. Check the sources. The answer cites each session it read, in the form
   `session 1a2b3c4d L120-180`. To view those entries from the terminal, run
   `gage session view SESSION:LINES`:

   ```cli
   gage session view 1a2b3c4d:120-180
   ```

   The answer also says where the sessions are silent. A line that no session
   wrote has no record, and your agent then relies on Git history and comments.

## Ask about specific lines

To go straight to the record for a file or a range of lines, invoke the `blame`
skill yourself. The question is optional.

| Agent       | Command                                     |
| ----------- | ------------------------------------------- |
| Claude Code | `/gage:blame FILE[:LINES] [question]`       |
| Codex       | `$gage:blame FILE[:LINES] [question]`       |
| Pi          | `/skill:gage-blame FILE[:LINES] [question]` |

## Keep the record

Your agent reads the sessions that your coding agent keeps on disk. Coding
agents delete old sessions, and the reasons go with them. Gage keeps a copy of
each session in its store, and your agent reads the stored copy when the
original is gone. See [Stored sessions](/sessions#stored-sessions).
