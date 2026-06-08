## Session retention left at the default

Claude Code keeps session transcripts on disk only for a limited window.
The `cleanupPeriodDays` setting controls how many days a transcript is
retained based on its last activity date. When the setting is **absent**,
Claude Code applies its default and deletes transcripts after **30 days**.

Gage analyzes your session history. Once a transcript is deleted, the
sessions it contained can no longer be scanned, queried, or reviewed ---
the underlying data is gone. The evidence below shows that
`cleanupPeriodDays` is not set, so the 30-day default is in effect.

This is an awareness finding: if 30 days is acceptable to you, you can
close this issue. Otherwise, set an explicit retention period below.

## Fix

Set `cleanupPeriodDays` in `~/.claude/settings.json` to the number of
days you want Claude Code to retain transcripts. Choose a value that fits
how far back you want Gage to be able to look: a larger number keeps
history longer at the cost of disk, a smaller one frees disk sooner but
shortens Gage's analysis window.

Reasonable choices:

| Days   | Effect                                                   |
| ------ | -------------------------------------------------------- |
| `90`   | A quarter of history; modest disk use                    |
| `365`  | A year of history; a good default for ongoing analysis   |
| `3650` | About ten years; effectively keep everything             |

```json
{
  "cleanupPeriodDays": 365
}
```

The setting can also be placed in project (`.claude/settings.json`) or
local (`.claude/settings.local.json`) scope if you only want to change
retention for a single project.

## Verify

After editing settings, confirm the value is picked up:

```shell
gage scan -s session-retention -y
```

If `cleanupPeriodDays` is now set, the scanner records the configured
value and does not reopen the issue.

## LLM notes

The evidence note's metadata has `configured: false`, confirming
`cleanupPeriodDays` is unset and the 30-day default applies.

Do not pick a retention period unilaterally. Present the user with the
reasonable choices from the Fix section above --- 90, 365, or 3650 days,
or a custom value --- and explain the disk-vs-history tradeoff. Then
write their choice to `cleanupPeriodDays` in `~/.claude/settings.json`
(or a project/local settings file if they prefer per-project scope).

To keep history effectively forever, use a large day count. Do not assume
a value like `0` or `-1` disables cleanup unless you have verified it;
treat that as unknown.

After applying the change, re-run `gage scan -s session-retention -y` to
confirm the issue does not reopen, and confirm with the user before
closing it as completed.
