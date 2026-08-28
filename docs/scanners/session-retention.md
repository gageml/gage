---
title: session-retention
description: Flags an unset Claude Code cleanupPeriodDays setting, under which the 30-day default silently deletes session history
---

The scanner checks whether Claude Code's `cleanupPeriodDays` retention
setting is configured. When the setting is unset, the 30-day default
applies and session history is silently deleted. Any explicitly set
value is treated as a chosen retention period. Only the unset case is a
finding.

## How it works

The scanner reads your Claude Code user settings. When
`cleanupPeriodDays` is set to any value, it does nothing. When the
setting is unset, it writes an evidence note and opens an issue.

Gage analyzes your session history. Once Claude Code deletes a
transcript, the sessions it contained can no longer be scanned or
queried. The issue is an awareness finding. If the 30-day default is
acceptable, close the issue. Otherwise, set `cleanupPeriodDays` in
`~/.claude/settings.json` to the number of days you want to keep. The
issue description lists reasonable choices.

Closing the issue keeps it closed. It does not reopen on later scans
while the setting stays unset.

## What it writes

Both items are written only when the setting is unset. The note backs
the issue as evidence.

| Note                             | Description                       | Target |
| -------------------------------- | --------------------------------- | ------ |
| `session-retention-policy.unset` | Records that the setting is unset | Scan   |

| Issue               | Description                      |
| ------------------- | -------------------------------- |
| `session-retention` | Opened when the setting is unset |

## Running

The scanner is in the `default` group, so it runs when you run `gage
scan` without selecting scanners. To run it alone:

```cli
gage scan -s session-retention
```
