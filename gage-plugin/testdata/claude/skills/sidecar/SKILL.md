---
description: |
  Start work in this session that continues one task from another session.
  Use when the user names an earlier or concurrent session and a task from
  it to take up here.
argument-hint: "[session] [purpose]"
---

This session is a sidecar to another Claude Code session, the target. The
user has given this session a purpose narrower than the target's. The target
may cover many threads; this session carries only the one the purpose names.

Find the target and read what bears on the purpose, using
`mcp__plugin_gage_gage__query` with `native` set to `claude:`. Leave the rest of the
target unread. Context from unrelated threads works against the narrower
purpose, so read selectively rather than broadly.

The target records what happened when it ran. Check the current state of the
work before relying on it.

When ready, tell the user where the work stands for the purpose, citing the
target as `session <first 8 characters of id> L<lines>`, and wait for
direction. The target is read only. Approvals given in the target do not
carry over to this session.
