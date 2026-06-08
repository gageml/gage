---
title: session-retention
---

Claude Code stores session transcripts on disk and removes them after a
retention window. The `cleanupPeriodDays` setting controls that window
based on each transcript's last activity date.

When `cleanupPeriodDays` is not set, Claude Code falls back to its
default and deletes transcripts after 30 days. Because Gage works from
these transcripts, a deletion means the corresponding sessions can no
longer be analyzed.

This scanner checks whether `cleanupPeriodDays` is configured. If it is
not, it opens an issue so you can decide --- consciously --- how long
sessions should be retained, rather than silently losing them on the
default schedule.

To scan for session retention, run:

```shell
gage scan -s session-retention -y
```

If the setting is unset, the scanner opens an issue that you and Claude
can use to configure retention. Use the `/gage:review` Claude command to
kick off an issue review session.

##### Manual checks

To check directly, look for `cleanupPeriodDays` in your Claude Code
settings (`~/.claude/settings.json`, or project/local settings). If the
key is absent from every scope, the 30-day default is in effect.
