## General rules

- Do NOT every commit to this Git repository. Each human contributor is
  responsible for their own commits. This is an important part of the
  contribution policy and must not be violated.

## Rust Rules

These rules apply to ALL Rust code in this workspace. They are non-negotiable.

- **IMPORTANT: Do not ignore errors!** - The principle: when you discard a
  `Result`'s `Err`, you are asserting "no signal here worth surfacing." That
  assertion has to be defensible. The test: name the failure mode you are
  discarding and why it doesn't matter. If you can't name it, you don't get to
  discard it.

  Always wrong (no defensible reading):
  - `.filter_map(|r| r.ok())` - collection-shaped burial; items vanish with no
    diagnostic
  - `let _ = result;` or bare `.ok();` as a statement - discards a `Result` you
    should have examined
  - `fn foo() -> bool { result.is_ok() }` - collapses a fallible operation into
    yes/no; return `Result`
  - Matching `Ok(x)` and explicitly dropping `Err(e)` to silence a real failure

  Permitted when the `Err` variant is structurally meaningless at this call
  site - i.e. the API exposes `Result` for type-system reasons, not because
  there is information to act on:
  - `if let Ok(x) = result { ... }` - env var probes, dynamic-type downcasts
    (Rune, serde Value), best-effort enrichment whose absence is already covered
    by adjacent output
  - `result.ok()?` inside a function whose own signature returns `Option` and
    where "absent" is the documented contract
  - Prefer the `Option`-shaped API when one exists (`env::var_os` over
    `env::var`)

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
    argument states what should happen. It does NOT state the error. It does NOT
    state the obvious, "this should work". If there is no non-obvious argument,
    use `.unwrap()`.

  - WRONG: `.expect("failed to open file")` - restating the error
  - WRONG: `.expect("should work")` / `.expect("unwrap failed")` - stating the
    obvious
  - RIGHT: `.expect("config should be loaded before server start")` - names a
    precondition that, if violated, tells you the root cause
  - RIGHT: `fs::create_dir_all(p).unwrap()` - if the filesystem is read-only the
    panic location alone is sufficient - no `expect` message would add value

  Keep `expect` messages as short as possible while still naming the
  precondition.
  - Good: `"splitn should yield >= one substr"`
  - Bad:
    `"splitn(2, ' ') always yields at least one element per the API contract"`

  Other patterns to avoid:
  - `.is_ok()` / `.is_err()` to discard a `Result` - same rule as above; if you
    actually need the boolean (rare), name the discarded variant

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
  - `from_xxx(...)` - _conversion_ constructor per [Rust API Guidelines C-CONV].
    The inputs _are_ `Self` in another representation (parse, decode,
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

- Comment puntuation rules:
  - Sentences that provide background or details not obvious in the code should
    end with periods
  - Short fragments that demark sections or provide quick clarity to code
    without filling in detail should not use periods

  Rule of thumb: If it reads like a description, use periods. If it reads like a
  label or title, omit the period.

  Doc comments (`///`) are prose and follow normal conventions.

- **Use `Path` / `PathBuf` for filesystem paths, not `&str` / `String`.**
  Function parameters, struct fields, and return values that represent a
  filesystem path take `&Path` (borrow) or return/store `PathBuf` (owned). Don't
  `to_string_lossy().into_owned()` a path just to fit it into a `String` field -
  store the `PathBuf` directly and let the display happen at the rendering
  boundary.
  - RIGHT: `fn find_claude() -> Result<PathBuf, _>`,
    `pub session_path: PathBuf`, `fn run(claude_bin: &Path, ...)`
  - WRONG: `fn find_claude() -> Result<String, _>` followed by
    `Command::new(&claude_bin)` - `Command::new` already accepts `AsRef<OsStr>`,
    so the `String` round-trip is pure loss
  - WRONG: `pub session_path: String` populated via
    `path.to_string_lossy().into_owned()` - silently drops non-UTF-8 components
    and forces every reader to re-parse

  Exceptions (these are NOT filesystem paths and `&str` / `String` is correct):
  - `rust-embed` asset keys (forced forward-slash regardless of host OS)
  - HTTP URI paths (use `&str` / `String`, not `Path`)
  - JSON field paths like `"message.content"`
  - Display strings explicitly intended for human output (e.g. a `~`-substituted
    config path)

- **Rune boundary: borrow, don't take.** At the Rust/Rune boundary,
  `rune::from_value::<T>()`, `FromValue::from_value`, and `Value::downcast` are
  _takes_ when `T` is an `Any` type - and in Rune that includes `String`, `Vec`,
  and `Object`, not just `#[derive(Any)]` externals. A take moves the contents
  out of the value's storage cell. Cloning the `Value` first does not help: a
  clone is a second handle to the same cell. If the script still holds a
  variable referring to that cell, its next read fails at runtime with "Cannot
  read, value has snapshot M-000000 and is not available for reading" - Rune's
  use-after-move.

  - Registered functions take `Ref<T>` / `Mut<T>` parameters (or `&Value`),
    never an `Any` type by value, and never accept a container whose elements
    they will downcast.
  - Read through `Value::borrow_ref::<T>()` / `borrow_string_ref()` and clone
    what you need. Scalars (bool/int/float/char) are stored inline, not in a
    cell: use `as_bool()`, `as_integer::<T>()`, `as_float()`.
  - `clippy.toml` disallows the take-shaped methods. A deliberate take needs
    `#[expect(clippy::disallowed_methods, reason = "...")]` where the reason
    names why the take is safe - e.g. the function holds the only live handle,
    as with a VM execution's return value.
  - A Rust function newly exposed to Rune that accepts values gets a test
    asserting the caller's value is still readable afterwards (see
    `parse_sessions_leaves_caller_values_readable` in gage-runtime).

- When writing clap command or arg text:
  - Look for and follow existing conventions --- this is a user interface and
    should be consistent throughout the CLI
  - Do not include trailing periods in the first line. Clap strips these to
    enforce their style convention and we should never include them.
  - Always use a short and to-the-point first time - never a full contiguous
    paragraph --- this is important as it makes the short help much easier to
    read
  - To provide additional detail, separate the first line with an empty line and
    provide detail as needed.
  - Do not specify the default short or long value: use `short` and `long` when
    possible and only specify a value when the default is incorrect (e.g. for
    option `prompt` DO NOT use `short = 'p'` just use `short`, assuming user
    wants the short form)
  - When describing content DO NOT describe implementation details. This is user
    facing help text, NOT code docs.

- When

[Rust API Guidelines C-CONV]:
  https://rust-lang.github.io/api-guidelines/naming.html#ad-hoc-conversions-follow-as_-to_-into_-conventions-c-conv

## Other rules

- Do NOT edit files under /docs in this project unless instructed to do so. User
  facing docs are written exclusively by competent humans who can write without
  sounding like clowns.
