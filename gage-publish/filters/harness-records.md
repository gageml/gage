Leave out the records the harness kept about the run. Every entry that is
neither conversation nor context is left out whole: API request records,
file snapshots and deltas, queue operations, the last prompt, the generated
title, mode and permission records, cost state, tool-use tallies, and the
system entries the conversation does not show. On the entries that stay,
the fields that describe the run rather than what happened are left out:
the Claude Code version and entry point, the user type, the slug, the
permission mode, the effort and requested model, the thinking duration, the
API request id, the block index, and the token usage. Entries and fields are
selected by type, so there is nothing for a model to judge.
