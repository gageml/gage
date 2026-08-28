---
title: gage mcp
description:
  Command reference for gage mcp, which starts the Gage MCP server over stdio
  or HTTP
---

:::note

Claude Code starts this server through the Gage plugin. Normal use does not
require running it yourself.

:::

The `gage mcp` command starts the MCP server. The server exposes Gage tools to
MCP clients. See [Tools](/docs/tools) for the tool list.

```cli
gage mcp [COMMAND]
```

## mcp stdio

Serve over stdio. This is the default when no subcommand is given.

```cli
gage mcp stdio
```

## mcp http

Serve over HTTP at the given bind address.

```cli
gage mcp http [OPTIONS]
```

| Option              | Description                                              |
| ------------------- | -------------------------------------------------------- |
| `-b, --bind <BIND>` | Address to bind, e.g. `127.0.0.1:8765` (default `127.0.0.1:0`) |
