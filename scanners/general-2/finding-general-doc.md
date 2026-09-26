Noteworthy finding in a session

An observation of agent behavior or user friction, written by an agent that
read one session. The value is a description of the observation. The target is
the session. `lines` metadata, when present, gives the session lines the
observation refers to. `mode` metadata records the scan variant that wrote
the note: `query` (the agent read the session through its Query view) or
`roadmap` (the agent read a compressed session and selectively retrieved
full message text).

The writer sees a single session and skips problems the session itself
diagnosed and fixed. It has no visibility across sessions.
