# Commands

Gage command modules are defined in `gage._internal.commands`.

## Command Modules

Each command is defined in two separate modules:

- Interface module
- Implementation module

Refer to an existing command for the patterns used in each.

Generally:

- Define the command interface using
  [Typer](https://typer.tiangolo.com/) conventions
- In handling the command execution, import the implementation module
  and call the applicable function, passing along the parsed arguments
- **Avoid any expensive imports by the the interface module**

The last point is crucial to minimize Gage's startup time as **all of
the interface modules are loaded on every command, even commands that
request help**. Any expensive imports will instantly degrade user
perception of Gage's speed.

Unless your approach is vetted by the team, avoid deviating from the
existing conventions.

Topics to consider when copying existing conventions:

- Names
- Help style (e.g. general length/detail, capitalization, punctuation)
- Types
- Default values
- Command complexity

Consistency and command complexity are the two primary concerns here.

If you think a command requires sub-commands, vet your ideas with the
team. Some commands use options to process sub-commands (e.g. see
`comment`, which adds, edits, lists and deletes comments from a single
top-level command, rather than introduce sub-commands).

## Adding Commands to `gage`

Add the the command interface module to `gage._internal.main`.

- Import the interface module
- Add the module via `app.command()`
- Maintain the existing sort orders (imports and added commands)

Test the command by running `gage <command> -h`. This is generally the
first line of the test document (see below).

## Testing Commands

Commands must be accompanied by a command text file. The test file must
be named `cmd-xxx.md` where `xxx` is the command name. Use multiple test
commands for different command scenarios as needed to avoid test files
that cover multiple, detailed, independent test scenarios.

Each document test file must show command help.

## Command Check List

Each new command should come with the following:

1. Interface module that avoids expensive imports and follows existing
   conventions, or has otherwise been vetting by Gage team members

2. Implementation module

3. Command test file (`cmd-xxx.md`)

4. Additional test files as needed (e.g. `lib-xxx.md` for new libraries
   introduced in support of the command implementation, `topic-xxx.md` to
   cover higher level concepts or user scenarios that aren't part of the
   command test file, `example-xxx.md` files that exercise any examples
   created in support of the command)

Refer to [bf232af](https://github.com/gageml/gage/commit/bf232af2abd) for an
example of a minimal set of changes for a new command.
