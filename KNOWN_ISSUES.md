# Known issues

This file lists problems that are known to exist in Gage and that there is
currently no plan to address. An entry is removed in the change that fixes it.

Each entry states what happens, when it happens, and the workaround if one
exists.

To report a problem that is not listed here, open an issue in the
[issue tracker](https://github.com/gageml/gage/issues).

## Pi

### `gage resolve` cannot assign an issue

A `gage resolve` session in Pi stops when it takes up an issue. The resolve
skill assigns an issue to its session with the `issue_assign` tool before it
works on the issue, and the tool returns an error in Pi. Pi does not tell an MCP
server which session a tool call comes from, so the tool has no session to
record as the holder.

This happens in every `gage resolve` session that runs in Pi.

Workaround: resolve issues with Claude Code or Codex as the default driver.

### Scan agents and skill sessions are not sandboxed

Pi has no sandbox and asks for no permission before a tool call. A scan agent
given `bash`, `write`, or `edit` acts as the user, and so does a `gage resolve`
or `gage publish` session.

This happens when a scanner gives its agent one of those tools and the default
driver is Pi, and in every skill session that runs in Pi. A scan agent given no
built-in tools can only call the tools its scanner serves.

Workaround: run a scanner whose agent has those tools with Claude Code or Codex
as the default driver.

### A scan agent cannot run with an empty system prompt

Pi adds the working directory to every system prompt. A scan agent whose scanner
asks for no system prompt gets one that states the working directory and nothing
else.

This happens when a scanner asks for no system prompt and the default driver is
Pi.

There is no workaround in Pi.
