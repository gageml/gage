# Gage language server

To install the Gage LSP (language support for Gage scanners), install the
`gage-lsp` crate using cargo.

```shell
cargo install --path gage-lsp
```

To setup Gage with your IDE, refer to the section below.

## VS Code

Install the [Rune extension for VS Code].

[Rune extension for VS Code]:
  https://marketplace.visualstudio.com/items?itemName=udoprog.rune-vscode

Set the `rune.server.path` VS Code config to the full path to `gage-lsp`
(installed under `~/.car/bin` by default).

```json
{
  "rune.server.path": "/home/garrett/.cargo/bin/gage-lsp"
}
```

Reload VS Code extensions. Verify that the language server works by opening a
Rune file (`.rn` extension) and confirming that there are no reported problems
for missing Gage functions.
