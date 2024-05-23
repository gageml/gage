# Commands

Gage command modules are defined in `gage._internal.commands`.

## Command Modules

Each command is defined in two separate modules:

- Interface module
- Implementation module

By convention, the interface module uses the same name as the command
and the implementation module uses the same name as the interface module
plus `_impl`.

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

## Implementation Guidance

The interface module should not contain any implementation code. As
stated previously, it must not directly import anything that is
expensive. This means limiting imports to types and CLI related support.

Expensive imports *may* be included in the implementation class.
However, if an expensive import is only used in limited cases, it should
be imported in the function that requires it. It's okay to import the
same module from multiple functions in the implementation module, but up
to a point. More than three times, e.g. as a rule of thumb, might
suggest the import belongs at the top level. Use your best judgement.

Calls to `cli` for user facing behavior *must* be limited to
implementation modules or helper modules in `gage._internal.commands`.
Unless you're enhancing `cli` this behavior must not be defined outside
the `commands` package.

Try to implement the entire command feature set in the implementation
module before considering new modules.

Only modify other modules if it's absolutely clear that the modification
(new functions, changed/enhanced behavior) belongs there.

If there is a clear set of unified behavior implemented for the command,
consider a new library module. The triggers for this decision:

- The set of functionality is narrowly scoped, coherent, and well
  understood.

- Other areas of the Gage code base would benefit from such
  functionality defined in a standalone module (the obvious case is that
  another implementation uses the function).

- If the functions in question implement user interface behavior,
  chances are good they belong in `impl_support` or remain in the
  implementation module to be exported for use by other modules.

Non-triggers:

- Code reuse. It's okay for a command implementation module to import
  and reuse functions from another command implementation. The shared
  functions, however, must be limited to command implementation.

As a rule of thumb, start by implementing all of the command
functionality in a single implementation module. If you feel a set of
functions should be moved to a library module (i.e. moved outside the
`commands` package to `gage._internal`), propose the move to the team to
get feedback.

Any new library modules *must* be accompanied by a complementary
`lib-xxx.md` test file. This test file should document and exercise the
module API. These tests are in addition to any tests in `cmd-xxx.md`.
