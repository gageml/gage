---
title: hidden-thinking
description: Detects sessions where model thinking text is hidden, leaving empty thinking blocks in the transcript
---

The scanner detects sessions where model thinking is hidden. Claude
Code's default settings replace thinking text with summaries, leaving
empty thinking blocks in the transcript. The scanner records whether the
newest thinking block is empty and opens an issue when it is.

## How it works

The `note` task finds the single newest thinking block across all
scanned sessions and records whether its text is empty. The note pins
the session line of the block. It also freezes the observation context,
including the block's timestamp, the model, and the
`showThinkingSummaries` setting at observation time. The snapshot
survives after the session file is deleted or the setting is changed.

The `issue` task reads the newest observation. When the block was empty,
it opens an issue. The issue description explains the cause and the fix,
which is setting `showThinkingSummaries` to `true` in
`~/.claude/settings.json`. It also lists known cases the setting does
not cover.

A closed issue reopens when a later scan observes a new empty thinking
block. This covers the case where the setting regresses after you fix
it.

## What it writes

| Note             | Description                                       | Target       |
| ---------------- | ------------------------------------------------- | ------------ |
| `thinking.empty` | Whether the newest thinking block's text is empty | Session line |

| Issue             | Description                                  |
| ----------------- | -------------------------------------------- |
| `hidden-thinking` | Opened when the newest observation is `true` |

## Running

The scanner is in the `default` group, so it runs when you run `gage
scan` without selecting scanners. To run it alone:

```cli
gage scan -s hidden-thinking
```
