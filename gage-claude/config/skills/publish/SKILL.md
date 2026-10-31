---
description: |
  Publish a shareable copy of a session. Use whenever the user asks to publish,
  share, redact, scrub, sanitize, or export a session, to prepare a session for a
  shared or public repository, or to remove secrets, personal information, paths,
  or profanity from a session before sharing it. Drives the gage publish commands
  from staging through review to apply.
---

## Background

Sessions contain information that is not suited for sharing:

- Leaked secrets (passwords, API keys, tokens, etc.)
- Personal information
- Content that doesn't apply to what's being shared
- Inappropriate language

To share a session, a user _publishes_ a modified version. A modification is one
of four types of changes:

- Dropped session entries
- Dropped entry values
- Redacted text

Published sessions do not contain new content apart from redaction markers. They
are strictly subsets of the original.

Your task is to help the user publish one or more sessions.
