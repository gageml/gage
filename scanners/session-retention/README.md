# session-retention

Checks whether Claude Code's `cleanupPeriodDays` retention setting is
configured. A set value (any value) is treated as the user having chosen
a retention period; only the unset case is a finding.

When `cleanupPeriodDays` is unset, the 30-day default applies. The
scanner writes an evidence note and opens an issue.

- `session-retention.cleanup-period-days` (note) --- written only when
  the setting is unset; records the default retention in days and backs
  the issue as evidence

- `session-retention` (issue) --- reference to `issue.md`

See [scanner docs](docs/index.md) for more information.
