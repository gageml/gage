## General rules

- Do NOT every commit to this Git repository. Each human contributor is
  responsible for their own commits. This is an important part of the
  contribution policy and must not be violated.

## Rust Rules

These rules apply to ALL Rust code in this workspace. They are non-negotiable.

- **IMPORTANT: Do not ignore errors!** - Return errors to callers, or handle
  them, or panic. Do not allow errors to be silently dropped.

  `.ok()` on a `Result` is banned. It drops the error. The only correct things
  to do with a `Result` are: return it, handle both arms, or panic. If you
  reached for `.ok()` to silence an unused-`Result` warning, use `.unwrap()`
  instead.

  Other examples of ignoring errors (what you must avoid):
  - Matching `Ok(...)` and ignoring `Err`
  - Returning `bool` in lieu of `Result`. If a function can fail, return
    `Result`, not `bool`.
  - Using `.is_ok()` / `.is_err()` to discard a `Result` (tail expression of a
    fn, or `if x.is_ok() {}` with no error-handling branch).

- **Handle errors at the call site.** When you call a function that returns
  `Result`, you must address the error --- propagate it with `?`, handle both
  arms, or panic with `.unwrap()` / `.expect()`. Don't accept a well-designed
  fallible API and then drop the error on the floor.

- `unwrap()` vs `expect()`
  - Use `.unwrap()` when a panic's location alone is enough to diagnose the
    failure. "This works or the world ends." No non-obvious precondition
    required.

  - Use `.expect("<should happen>")` ONLY when an internal assumption made at
    the call site is non-obvious and a bare panic wouldn't reveal it. The
    argument states what should happen. It does NOT state the error. It does
    NOT state the obvious, "this should work". If there is no non-obvious
    argument, use `.unwrap()`.

  - WRONG: `.expect("failed to open file")` - restating the error
  - WRONG: `.expect("should work")` / `.expect("unwrap failed")` - stating the
    obvious
  - RIGHT: `.expect("config should be loaded before server start")` - names a
    precondition that, if violated, tells you the root cause
  - RIGHT: `fs::create_dir_all(p).unwrap()` - if the filesystem is read-only
    the panic location alone is sufficient - no `expect` message would add
    value

  Keep `expect` messages as short as possible while still naming the
  precondition.
  - Good: `"splitn should yield >= one substr"`
  - Bad:
    `"splitn(2, ' ') always yields at least one element per the API contract"`

  Banned patterns:
  - `.filter_map(|r| r.ok())` - silently drops errors
  - `if let Ok(x) = ...` without handling the `Err` case
  - `match` on Result that discards the `Err` arm
  - `fn ...() -> bool { ....is_ok() }` - use `Result`
  - `.is_ok()` / `.is_err()` used to discard a `Result`

- `cargo clippy` must pass with no warnings before the change is done. Clippy
  runs the compiler too, so it covers what `cargo build` would catch. Do not
  proceed until clippy is satisfied.

- Run `cargo fmt` only after clippy passes.

- All tests must pass.

- Prefer imports over fully qualified references in the interest of keeping
  symbol use short, therefore improving readability.

- Use the _Stepdown Rule_ when ordering functions. Define functions below where
  they are first called, so code reads top-down like a narrative. This takes
  priority over grouping by visibility.

- Constructor naming - Match the prefix to what the function does:
  - `new(...)` - primary constructor; minimal/required arguments only
  - `with_xxx(...)` - alternate constructor that _configures_ the new value
    (`Vec::with_capacity`, `BufReader::with_capacity`) - use for
    scoping/parameterizing constructors where the inputs are not the data of
    `Self` in another form
  - `from_xxx(...)` - _conversion_ constructor per [Rust API Guidelines
    C-CONV]. The inputs _are_ `Self` in another representation (parse, decode,
    reinterpret). `Target::from_columns(row) -> Target` qualifies;
    `MessageTable::from_session(id, path)` does not.
  - Avoid `for_xxx` - prefer `with_xxx` for the same role

- **No banner/section-header comments.** Do not cordon off regions of a file
  with comments like `// -- TableProvider --`, `// === helpers ===`,
  `// --- User content ---`, or `// -------- Phase N --------`. They impose an
  arbitrary taxonomy on the file that fights the Stepdown Rule (order of
  use/call). If a group of items needs a label to be findable, the right fix is
  ordering, naming, or splitting the file - not a banner. The reader infers
  structure from the code itself.

- Do not end single-line comments (`//`) with periods. These are sentence
  fragments, not prose. Doc comments (`///`) are exempt.

- **Use `Path` / `PathBuf` for filesystem paths, not `&str` / `String`.**
  Function parameters, struct fields, and return values that represent a
  filesystem path take `&Path` (borrow) or return/store `PathBuf` (owned).
  Don't `to_string_lossy().into_owned()` a path just to fit it into a `String`
  field - store the `PathBuf` directly and let the display happen at the
  rendering boundary.
  - RIGHT: `fn find_claude() -> Result<PathBuf, _>`,
    `pub session_path: PathBuf`, `fn run(claude_bin: &Path, ...)`
  - WRONG: `fn find_claude() -> Result<String, _>` followed by
    `Command::new(&claude_bin)` - `Command::new` already accepts
    `AsRef<OsStr>`, so the `String` round-trip is pure loss
  - WRONG: `pub session_path: String` populated via
    `path.to_string_lossy().into_owned()` - silently drops non-UTF-8 components
    and forces every reader to re-parse

  Exceptions (these are NOT filesystem paths and `&str` / `String` is correct):
  - `rust-embed` asset keys (forced forward-slash regardless of host OS)
  - HTTP URI paths (use `&str` / `String`, not `Path`)
  - JSON field paths like `"message.content"`
  - Display strings explicitly intended for human output (e.g. a
    `~`-substituted config path)

- When writing clap command or arg text, do not include trailing periods in the
  first line. Clap strips these to enforce their style convention and we should
  never include them.

[Rust API Guidelines C-CONV]:
  https://rust-lang.github.io/api-guidelines/naming.html#ad-hoc-conversions-follow-as_-to_-into_-conventions-c-conv
