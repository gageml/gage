## General rules

- Never push to `main`, `build`, or `release` without the user's approval.

- Do not push without the user's approval. Approval covers the push it was
  given for and does not carry over to later pushes.

## Branch and pull request rules

Work reaches `main` through a feature branch and a pull request. The repository
is managed with `jj`, where a branch is a bookmark.

- **Start each piece of work from current `main`.** Run `jj git fetch`, then
  `jj new main`.

- **One feature branch per change.** Name the branch for the change in
  lowercase words joined by hyphens: `codec-number-text`, `docs-refresh`. Create
  it with `jj bookmark create <name> -r <commit>`.

- **Push the feature branch, then open a pull request against `main`.** Push
  with `jj git push --bookmark <name>`. The pull request check must
  pass before the merge.

- **Bring a branch up to date by rebasing it.** When `main` has moved, run
  `jj rebase -b <name> -d main` and push the branch again. Do not merge `main`
  into a feature branch.

- **Merge through the pull request.** After the merge, run `jj git fetch` to
  update local `main`. Do not move `main` locally and push it in place of a
  pull request unless the user asks for that.

- **Delete a merged branch.** Run `jj bookmark delete <name>`, then
  `jj git push --deleted`. Deleting a remote branch is a push and needs the
  user's approval.

## Design rules

- If there are design inconsistencies in the codebase, report them. Stop for
  resolution only when the inconsistency affects the design of the current task.
  Otherwise follow the dominant pattern and continue.

- If the user appears to misunderstand something a design decision depends on,
  raise it before proceeding.

## Rust Rules

These rules apply to ALL Rust code in this workspace. They are non-negotiable.

- **Do not ignore errors.** Discarding a `Result`'s `Err` asserts that the
  failure carries no signal worth surfacing. Name the failure mode discarded and
  why it does not matter. If you cannot name it, propagate or handle the error.
  Clippy rejects `let _ = result;` and a bare `.ok();`. The same rule covers the
  forms clippy does not catch: `.filter_map(|r| r.ok())`, `.is_ok()` or
  `.is_err()` used to collapse a fallible operation into a `bool`, and a match
  arm that drops `Err(e)`.

  Discarding is permitted when the `Err` is structurally meaningless at the call
  site: env var probes, dynamic-type downcasts (Rune, serde `Value`),
  best-effort enrichment whose absence adjacent output already covers, and
  `result.ok()?` in a function whose documented contract is `Option`. Prefer the
  `Option`-shaped API when one exists (`env::var_os` over `env::var`).

- **`unwrap()` vs `expect()`.** Use `.unwrap()` when the panic location alone
  diagnoses the failure. Use `.expect("...")` only when the call site relies on
  a non-obvious precondition. The message names that precondition as what should
  hold, in as few words as possible. It does not restate the error or state the
  obvious.
  - WRONG: `.expect("failed to open file")`
  - WRONG: `.expect("should work")`
  - RIGHT: `.expect("config should be loaded before server start")`
  - RIGHT: `.expect("splitn should yield >= one substr")`
  - RIGHT: `fs::create_dir_all(p).unwrap()`

- Run `just check` before declaring any change complete. It runs the full gate —
  clippy (no warnings), fmt, tests — as one command; partial verification is not
  an option.

- **New or changed behavior gets a test.** A change that adds or alters behavior
  adds a test that covers the new path. `just check` and manual runs are not a
  substitute. When a change cannot be tested, say so and why.

- **Do not delete or weaken a failing assertion to pass the gate.** Fix the code
  or correct the expectation. When the right expectation is unclear, raise it
  with the user.

- **Prefer imports over fully qualified references.** Before writing `a::b::C`
  inline, check the file's `use` block. Use the short form if the item is
  imported, and add the import if it is not. For a std item whose name collides
  with the prelude, import the module: `use std::io;` then `io::Result<T>`,
  never `use std::io::Result;`. A trait import scoped to a function body and an
  aliased import for disambiguation are not violations. Fix existing fully
  qualified references only when editing the surrounding code.

- Use the _Stepdown Rule_ when ordering functions. Define functions below where
  they are first called, so code reads top-down like a narrative. This takes
  priority over grouping by visibility.

- **Constructor naming** follows the Rust API Guidelines (C-CTOR, C-CASE). These
  points are the ones most often missed:
  - `from_xxx(...)` takes the source data the value is built from.
    `Target::from_columns(row)` qualifies. `MessageTable::from_session(id, path)`
    does not, because the arguments locate the data rather than being the data.
  - `with_xxx(...)` takes options or parameters that configure the new value,
    not source data.
  - A domain verb (`open`, `connect`, `bind`) names the constructor of a
    resource type when the verb is clearer than `new`.
  - Do not use `for_xxx`. Use `with_xxx` for the same role.

- **No banner comments.** Do not divide a file with comments like
  `// --- helpers ---`. If a group of items needs a label to be findable, fix
  the ordering, the naming, or the file split.

- **Comments do not cite documents.** A comment never directs the reader to a
  markdown file, a design document, a plan, a section title, or a footnote,
  because those are not kept in sync with the code. State the content in place.
  - WRONG: `// see watermarks.md`
  - RIGHT:
    `/// The whole diff is one index transaction. One commit per object took 11.6 s on the bench population; one transaction takes 251 ms.`

  Functional references are unaffected: `include_str!` paths, config-file
  discovery (`CLAUDE.md`, `SKILL.md`), and test fixtures.

- **Comment only what the code cannot say.** A comment is warranted for a
  performance optimization and its measured effect, a non-trivial algorithm, an
  invariant the type system does not enforce, or a workaround for an external
  behavior. Do not restate readable code. Make unclear code clearer instead of
  describing it.

- **Comment punctuation.** A doc comment (`///`, `//!`) ends with a period. A
  line comment (`//`) ends with a period when it contains a verb or spans more
  than one line. A line comment that is a bare label with no verb takes no
  period.
  - RIGHT: `/// The view options in effect.`
  - RIGHT: `// The log's content; bytes that are not UTF-8 are replaced.`
  - RIGHT: `// One of "out", "err", "records"`
  - WRONG: `/// A host whose session backend reads the fixture store`

  This deviates from the [Rust Style Guide comments] chapter, which requires a
  period on every comment except an inline block comment. The deviation is
  intentional.

- **Filesystem paths are `&Path` / `PathBuf`, not `&str` / `String`.** Store the
  `PathBuf` and convert for display at the rendering boundary. `rust-embed`
  asset keys, HTTP URI paths, JSON field paths, and strings built for human
  display are not filesystem paths.

- **Rune boundary: borrow, don't take.** `rune::from_value::<T>()`,
  `FromValue::from_value`, and `Value::downcast` move the contents out of the
  value's storage cell when `T` is an `Any` type, which in Rune includes
  `String`, `Vec`, and `Object`. Cloning the `Value` first does not help,
  because a clone is a second handle to the same cell. The script's next read of
  that value fails at runtime.
  - Registered functions take `Ref<T>` / `Mut<T>` parameters (or `&Value`),
    never an `Any` type by value, and never accept a container whose elements
    they will downcast.
  - Read through `Value::borrow_ref::<T>()` / `borrow_string_ref()` and clone
    what you need. Scalars are stored inline: use `as_bool()`,
    `as_integer::<T>()`, `as_float()`.
  - `clippy.toml` disallows the take-shaped methods. A deliberate take needs
    `#[expect(clippy::disallowed_methods, reason = "...")]` where the reason
    names why the take is safe.
  - A Rust function newly exposed to Rune that accepts values gets a test
    asserting the caller's value is still readable afterwards (see
    `functions_leave_the_caller_values_readable` in gage-runtime).

- **Clap help text.** `gage-cli/src/help_lint.rs` enforces the mechanical rules,
  and `cargo test -p gage-cli help_text` reports every violation. The lint
  checks: a rendered line of 79 columns or fewer; long help that opens with the
  short line and a blank line; no trailing period, backticks, single quotes, or
  `the`; qualifiers in one trailing parenthetical, separated by `; `, drawn from
  the lint's `TOPIC_ORDER` list in that order; `repeatable` on a repeating
  option; and a default stated in the text and never rendered by clap. The rules
  below are not checked:
  - A literal value takes double quotes: `Close issue as "wontfix"`. `a`/`an`
    stays when the noun is new: `Read message from a file`.
  - A positional that names the command's object reads `Xxx to yyy`:
    `Issues to close`, `Dataset to show`.
  - Set `hide_default_value` and `hide_possible_values`, and list enum values in
    long help.
  - No implementation vocabulary. A Git term appears only as the public spec,
    spelled out: `Git ref naming rules`.
  - A paragraph used by more than one arg is one `const` referenced through
    `long_help`, prefixed with that arg's own short line. Options shared by
    several commands are one flattened `Args` struct; a positional that names
    the command's object is declared per command.
  - A text value is `<TEXT>` and accepts `@PATH` to read a file.
  - `short` and `long` take no value unless the default letter is wrong.
  - Options are declared in the order they render, most central first: source,
    then destination (the container, then what the object refers to), then the
    object's own properties, then how the work runs, then output, then utility
    flags, with `--yes` last. A modifier follows the option it modifies: `--tag`
    then `--force-tag`.
  - Shared blocks keep their place: scope options (`--native`, `--stored`, `--stored-remote`) lead;
    the session filter block orders which-filters before how-many limiters; a
    list command puts its own filters first, then `--more`, `--all`, `--limit`,
    and `--full-id` last.

[Rust Style Guide comments]: https://doc.rust-lang.org/style-guide/index.html#comments

## Store schema rules

These rules keep a store readable by every Gage build that touches it. A store
is shared: `gage pull` brings in objects other builds wrote, so an older build
reading a newer object is a normal condition, not an upgrade-window edge case.

Confirm with the user before a `type` version bump, a generation bump, an
`INDEX_SCHEMA_VERSION` bump, a `HOOK_VERSION` bump, or any departure from a rule
below. Cite the rule and explain the reason. Apply the other rules without
asking, and name each rule applied in the summary of the change.

- **An attribute added to an existing type is optional and defaulted forever.**
  The field carries `#[serde(default, skip_serializing_if = ..)]` and every
  reader handles its absence. A required attribute, a rename, a removal, or a
  change of meaning is a `type` version bump, never an in-place change.

- **An enum stored in `attrs.json` tolerates unknown values.** It carries an
  `#[serde(other)] Unknown` variant. A reader degrades the one field; a writer
  rejects `Unknown` as input. One newer value never fails a listing.

- **A `type` version bump ships a reader for every prior version.** Decode is
  dispatched on `header.version` for versions at or below the type's
  `OBJECT_VERSION`. `require_type` refuses a version above it with
  `StoreError::ObjectVersion`, and `Store::select` leaves such objects out of
  listings with a warning.

- **An edit stamps the writer's `OBJECT_VERSION` into the `type` blob.** The
  blob names the shape of the content beside it. Tombstones and resurrections
  copy the entry from the commit they supersede; the `update` hook's identity
  check depends on that.

- **A change to any `INDEXED_ATTRS` bumps `INDEX_SCHEMA_VERSION` and updates
  `INDEXED_ATTRS_SNAPSHOT`.** Attribute rows are extracted at write time, so a
  list change reaches existing objects only through a rebuild. The test
  `indexed_attrs_match_the_snapshot_beside_the_index_version` enforces it.

- **A driver refuses a `content_format` it does not write.** `read_stored`
  returns an error naming the stored format and the upgrade.

- **A hook change bumps `HOOK_VERSION`.** `Store::open` rewrites the hooks of a
  store whose recorded version is behind.

- **A generation bump is the last resort.** It is for a change to the ref
  layout, the marker set, the link convention, or the tombstone rule only. It
  ships with a step in `MIGRATIONS`, a rewrite of every remote's fetch refspec
  (pinned to the generation by `RemoteStore::add`), and a hook update on every
  Gage store used as a remote.

- **Every type has a `layout_snapshot` test, and the generation has a fixture.**
  The snapshot pins the exact tree and `attrs.json` text a type writes;
  `gage-store/tests/fixture.rs` reads a bundle an earlier build wrote. A change
  to either is a format change and is reviewed as one.

## Data access rules

- **SQL is the read path for selection-shaped data.** CLI listings, TUI
  listings, scanner reads, MCP tool reads, and external agent reads go through
  `gage-query` (DataFusion). Any multi-row read with filter, sort, project,
  limit, or page uses the SQL interface. A second filtering or sorting
  implementation in a caller is a defect; DataFusion pushdown is the single
  shared implementation of these semantics.

- **Direct `gage-store` calls are permitted only for these five shapes.** Any
  direct read outside this list is a gap in the SQL surface. If data is required
  that is not already available via the datafusion schema (tables, TVFs, and
  UDFs) the schema should be modified rather than worked-around using the Rust
  interface to the store. With that said, see **Data schema stability** below
  for a counteracting goal. Work with the user to resolve the tension between
  making required data available and maintaining a stable data schema.

  1. Writes: create, edit, tombstone.
  2. Reads of one object at a known commit SHA.
  3. Prefix and alias resolution.
  4. Commit-graph traversal (`first_parent` chain, link parents).
  5. Opaque content reads where no column represents the bytes (e.g. the raw
     attachment bytes that sit behind the `attachment_file.text` projection).

- **Data schema stability.** The datafusion structure is stable and must not be
  changed unless all other options have been discussed and rejected in favor of
  a schema change. Look for ways to use the existing structure before
  considering or proposing changes to the schema. If a change to a schema is
  compelling enough, propose it with the understanding that such changes are
  rare and require a strong argument.

- **System-tier columns are tagged on the field, not by name.** A schema's
  system columns carry `gage_session::system(..)` on their `Field`. The
  `SkipSystemCols` wrapper filters by `gage_session::is_system(field)`. A
  name-based system list is a defect.

## CLI rules

- Every command that writes the store or config confirms through the shared
  dialog; `--yes` skips it. Exceptions: `gage store` subcommands are low level
  and exempt; `gage config edit` is confirmed by the editor save.
- Progress and spinners come from the dialog theme. No library defaults.
- CLI dialog work does not touch `gage-tui`.
- Use `CliError::Exit` to exit rather than `process::exit`. A command returns
  `Result<(), CliError>`, and `main` owns the single handler that prints the
  error and sets the exit code. `CliError::Exit` carries the exit code and an
  optional message. `process::exit` appears only in `main`.

## TUI rules

- Key-hint vocabulary for the key that exits an overlay. The term names the
  effect on the pending action or viewed item, not the navigation direction
  ("back" is never used):
  - `cancel` - action-related dialogs: key-option menus (actions menu, view
    options), pickers, and editor prompts. The key abandons a pending choice or
    discards in-progress input (`q cancel`, `Esc cancel`).
  - `close` - item-view overlays showing content with footer help (note, issue,
    session, log, test dialogs): `q close`.
  - `dismiss` - message dialogs (notices, announcements) with no pending action
    or item: `Enter dismiss`.
  - App level names its object where bare `cancel` would be ambiguous: `q quit`,
    `q cancel scan`.

- Key chars in key help use `Dialog::key()` when shown in a dialog and
  `Panel::footer_key()` when shown in a footer. Labels are unstyled.

## Other rules

- Keep the files under /docs current. A change to user-facing behavior, such
  as a command, an option, a table, a column, or a tool, updates the docs that
  describe it in the same piece of work. Match the surrounding page: its
  structure, its terms, and its plain declarative sentences.
