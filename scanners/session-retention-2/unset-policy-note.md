Flags when session retention policy is unset (i.e. default is used)

Set `cleanupPeriodDays` in config to squelch this note.

The default value is 30 days, after which sessions are silently and permanently
deleted. If you're relying on the session record for scans, you should be aware
of this policy and set an explicit retention period.

**value** - always `true` - note is only written when default policy is unset

NOTE: The scanner opens an issue whenever this note is written. It's usually not
necessary to open additional issues.
