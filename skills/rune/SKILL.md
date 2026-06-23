---
name: rune
description:
  Reference for the Rune programming language (rune-rs), a dynamically typed
  scripting language embedded in Rust. Use when reading, writing, or reviewing
  Rune code (.rn files).
---

# Rune Language Reference

Rune is a dynamically typed scripting language whose syntax closely follows
Rust. It runs on a virtual machine embedded in a Rust host application. Source
files use the `.rn` extension. The upstream project is
https://github.com/rune-rs/rune with documentation at https://rune-rs.github.io.

A script may consist of top-level statements, or define functions. When run by
the Rune CLI, `pub fn main()` is the entry point; embedders may call any public
function directly.

## Differences from Rust

Rune looks like Rust but differs in ways that matter when writing it:

- No type annotations anywhere: not on variables, parameters, return types, or
  struct/enum fields.
- All variables are mutable. There is no `mut` keyword on bindings.
- No ownership or borrow checking at compile time. Nontrivial values are
  reference counted with shared ownership, like `Rc<RefCell<T>>`. Assignment
  copies a reference, not the value.
- No generics or type parameters. Turbofish appears only in a few stdlib calls
  such as `collect::<Vec>()`, with bare type names.
- Instance methods are looked up at runtime; calling a missing method is a
  runtime error, not a compile error.
- Anonymous objects (`#{ ... }`) and template literals (`` `${x}` ``) exist;
  these are borrowed from EcmaScript, not Rust.
- `is` and `is not` perform runtime type checks.
- Generators (`yield`) are a first-class language feature.

Items (function names, types, imports) are still checked at compile time:
calling an undeclared function or importing a missing module is a compile error.
This distinguishes Rune from Python or Lua.

## Variables and memory

```rune
let x = 5;
x = 6;              // all variables are mutable
let x = "shadowed"; // rebinding works like Rust shadowing
```

Primitives are `Copy` and are copied on assignment: unit `()`, booleans, bytes
(`b'\xff'`), chars (4-byte, `'今'`), 64-bit signed integers, 64-bit floats, and
static strings. Everything else (strings built at runtime, vectors, objects,
structs) is reference counted and shared:

```rune
let a = #{ field: 1 };
let b = a;          // b points to the same object
b.field = 2;
assert_eq!(a.field, 2);
```

`let` accepts patterns (`let (a, b) = pair;`), but plain assignment does not:
valid assignment targets are variables (`x = v`), fields (`x.f = v`), and index
expressions (`x[i] = v`). Destructuring assignment such as `(a, b) = (b, a)` is
a compile error.

A host function may take ownership of (move) its argument. Using a moved value
raises a runtime error. `drop(value)` frees a value explicitly;
`is_readable(value)` and `is_writable(value)` test accessibility.

## Functions

```rune
fn add(a, b) {
    a + b           // last expression is the implicit return
}

fn early(n) {
    if n < 1 {
        return "less than one";
    }
    "something else"
}

pub fn main() {
    println!("{}", add(1, 2));
}
```

A function name used without calling it is a function pointer and can be passed
as a value:

```rune
fn apply(op) { op(1, 2) }
apply(add);
```

## Closures

```rune
let n = 1;
let f = |a, b| n + a + b;     // captures n from the environment
let g = |x| { x * 2 };
let h = move |a| n + a;       // move forces capture by value
let (x, y) = (1, 2);
let unpack = |(a, b)| a + b;  // patterns work in closure parameters
```

Closures that capture nothing are identical to functions. `async ||` creates an
async closure (see Async section).

## Control flow

`if`/`else if`/`else` work as in Rust and are expressions:

```rune
let label = if n < 5 { "small" } else { "big" };
```

Loops: `loop`, `while`, `for`. `break` accepts a value for `loop` expressions,
and labels are supported:

```rune
let total = loop {
    counter += 1;
    if counter > 10 {
        break counter;
    }
};

'outer: for x in 0..10 {
    for y in 0..10 {
        if x * y > 25 { break 'outer; }
        if y == x { continue 'outer; }
    }
}

for item in [1, 2, 3] { }
for i in 0..limit { }       // exclusive range

while let Some(n) = it.next() { }
if let Some(v) = opt { }
```

## Pattern matching

```rune
match n {
    1 => "the number one",
    n if n is i64 => "another number",
    [1, 2, rest, ..] => "vector starting with 1, 2",
    ("test", n) => "tuple",
    #{ "model": "Ford", "make": year, .. } if year >= 2000 => "object",
    "one" => "a string",
    _ => "anything else",
}
```

Patterns include: literals (unit, bool, byte, char, integer, string), vectors
`[1, _, ..]`, tuples `(a, _, 42)`, objects `#{ key: pattern, .. }`, struct
patterns (`Foo`, `Foo(1, _)`, `Foo { bar: 1, .. }`), and enum variant patterns
(`Foo::Variant`, `Foo::Variant(1, _)`, `Foo::Variant { bar: 1, .. }`). Values
inside patterns are themselves patterns and nest arbitrarily.

`_` ignores a value; a bare identifier binds it. `..` (the rest pattern) matches
remaining elements or fields; without it, collection patterns must match
exactly:

```rune
match #{ a: 0, b: 1 } {
    #{ a } => "no match: requires exactly the key a",
    #{ a, .. } => "matches: requires a to be present",
}
```

## Vectors

Dynamic lists holding values of any type:

```rune
let v = [1, "two", 3.0];
v.push(4);
v[0];           // index access
v.0;            // field-style access also works
v.get(0);       // Option
v.len();
v.pop();        // Option
v.insert(1, "x");
v.remove(0);
v.sort();
v.sort_by(|a, b| a.cmp(b));
v.extend([5, 6]);
[ "a", "b" ].iter().rev();
for item in v { }
```

## Objects

Anonymous string-keyed maps, suited to JSON-like data:

```rune
let obj = #{ name: "test", count: 42, "key with spaces": true };
obj.name;               // field access
obj["name"];            // index access
obj["new_key"] = 1;     // insert via index
obj.get("missing");     // Option
obj.insert("k", v);
obj.remove("k");
obj.contains_key("k");
obj.len();
obj.keys();
obj.values();
for entry in obj { }    // iterates (key, value) tuples
```

## Tuples

Fixed-size, heterogeneous, fields assignable:

```rune
let t = (1, "test");
t.0;
t.1 = "changed";
let (a, b) = t;
```

## Structs and impl blocks

Fields are declared by name only. Instance functions take `self` (no `&`):

```rune
struct User {
    username,
    active,
}

struct Point(x, y);     // tuple struct
struct Marker;          // unit struct

impl User {
    fn new(username) {
        User { username, active: false }   // field shorthand works
    }

    fn set_active(self, active) {
        self.active = active;
    }
}

let user = User::new("setbac");
user.set_active(true);
user.username = "newt";     // fields are public and mutable
```

The compiler verifies struct field names: using an undefined field is a compile
error.

## Enums

```rune
enum Op {
    Unit,
    Tuple(a, b),
    Struct { field },
}

match op {
    Op::Unit => 0,
    Op::Tuple(a, b) => a + b,
    Op::Struct { field } => field,
}
```

## Option, Result, and the try operator

`Option` (`Some`/`None`) and `Result` (`Ok`/`Err`) are built in and in the
prelude. `?` returns early on `None` or `Err`, in any function:

```rune
fn checked_div_mod(a, b) {
    let div = a.checked_div(b)?;    // returns None on division by zero
    Some((div, a % b))
}
```

Common methods: `unwrap`, `unwrap_or`, `unwrap_or_else`, `expect`, `map`,
`and_then`, `is_some`/`is_none`, `is_ok`/`is_err`, `ok_or`, `ok` (Result to
Option), `transpose`, `take`.

Any value can serve as an error: `Err("message")` and custom marker structs like
`struct Timeout;` with `Err(Timeout)` are idiomatic.

## Strings, templates, and formatting

```rune
let s = "literal";
let s = String::from("built");
let msg = `Hello ${name}, you are ${age} years old!`;   // template literal
```

Template literals interpolate any expression with `${}` using the value's
display protocol. Types without a display implementation (e.g. `Vec`) fail at
runtime inside templates; use `{:?}` formatting or `dbg!` for those.

Format strings support positional `{}`, inline variables `{name}`, and debug
`{:?}` / `{name:?}`:

```rune
println!("{} and {other}", first);
println!("{v:?}");
let s = format!("{a}-{b}");
```

String methods: `len`, `is_empty`, `push`, `push_str`, `contains`,
`starts_with`, `ends_with`, `replace`, `split`, `split_once`, `trim`,
`trim_end`, `to_lowercase`, `to_uppercase`, `chars`, `char_at`, and `as_bytes`.
Parsing uses `s.parse::<i64>()`, `s.parse::<f64>()`, or `s.parse::<char>()`,
each returning a `Result`.

Concatenation: `a + b` works on strings, as does `+=`.

## Iterators

Anything iterable supports the standard combinators:

```rune
v.iter()
    .filter(|x| x > 0)
    .map(|x| x * 2)
    .filter_map(|x| if x > 1 { Some(x) } else { None })
    .flat_map(|x| [x, x])
    .enumerate()
    .take(5)
    .skip(1)
    .chain(other)
    .rev()
    .collect::<Vec>()
```

Terminal operations: `collect::<Vec>()`, `count`, `sum`, `product`, `fold`,
`reduce`, `find`, `any`, `all`, `nth`, `next`. `peekable()` provides `peek`.
Vectors of strings support `join(", ")`. `std::iter` also provides `once`,
`empty`, and `range`.

## Type checking

```rune
value is String
value is not Vec
```

Type names usable with `is`: `i64`, `u64`, `f64`, `bool`, `char`, `String`,
`Vec`, `Object`, `Tuple`, and any user-defined or host-registered type. Types
are identified by item path (e.g. `std::f64`). Match guards combine patterns
with type tests: `n if n is String => ...`.

## Modules, imports, visibility, constants

```rune
use std::iter::once;
use foo::{bar, baz};

mod inline_module {
    pub fn number() { 1 }
}

mod from_file;      // loads ./from_file.rn or ./from_file/mod.rn

pub fn public_fn() { }
fn private_fn() { }     // visible only within its module
pub(crate) fn crate_fn() { }

const MAX = 10_000;
```

Path keywords work as in Rust: `self::` (current module root), `crate::` (entry
point), `super::` (parent module). Common types (`Option`, `Result`, `String`,
`Vec`) are in the prelude and need no import.

## Async

Async is first class. `async fn` and `async` closures/blocks produce futures;
`.await` runs them and is permitted only inside async contexts:

```rune
async fn fetch(url) {
    Ok(http::get(url).await?.status())
}

let f = async || Ok(1);     // async closure
let blk = async { 42 };     // async block, captures like a closure

let results = std::future::join([fetch(a), fetch(b)]).await;
```

`select` waits on multiple futures and resumes with the first to complete:

```rune
let result = select {
    res = request => res,
    _ = time::sleep(time::Duration::from_secs(2)) => Err(Timeout),
}?;
```

Modules such as `http`, `time`, `json`, `process`, and `rand` are provided by
the host (the `rune-modules` crate); availability depends on what the embedding
application registers.

## Generators and streams

A function containing `yield` is a generator. `next()` returns `Option`;
`resume(value)` sends a value in (the `yield` expression evaluates to it) and
returns `GeneratorState::Yielded(v)` or `GeneratorState::Complete(v)`:

```rune
fn fib() {
    let a = 0;
    let b = 1;
    loop {
        yield a;
        let c = a + b;
        a = b;
        b = c;
    }
}

let g = fib();
while let Some(n) = g.next() {
    if n > 100 { break; }
}
```

A generator must be "warmed up" with one `resume(())` before sent values are
observed, and resuming a completed generator is a runtime error.

An `async fn` containing `yield` is a stream; iterate with
`stream.next().await`.

## Built-in macros

- `println!("fmt", args)` / `print!` - write to stdout
- `format!("fmt", args)` - build a string
- `dbg!(value)` - debug-print a value and return it
- `panic!("msg")` - abort the virtual machine with an error
- `assert!(cond, "msg")`, `assert_eq!(a, b)`, `assert_ne!(a, b)`
- `stringify!(expr)` - expression source as a string

New macros can only be defined natively by the Rust host, not in Rune source.

## Runtime errors

Many mistakes that Rust catches at compile time surface as virtual machine
errors at runtime in Rune: calling a missing instance function, using a moved
value, type mismatches in operators, formatting a value without a display
implementation, and resuming a completed generator. Compile-time checks cover
unknown items, unknown struct fields, and variable use after `move`.

## Gage API

Everything Gage exposes lives under the `gage::` crate. Import what you use:

```rune
use gage::{scan, write_note, write_issue, user, call_agent, call_llm,
           query, render_template, DateTime, Ignore};
```

Top-level functions (all async unless noted):

- `scan()` - current `Scan` context (sync). `.id`, `.sessions()` (iterator of
  `Session`).
- `user()` - current `User` (sync). `.config(key)` reads Claude config.
- `query(sql)` - run a SQL query against the Gage DB; returns rows.
- `write_note(#{ ... })` - insert a note. Builder methods: `.replace_prev()`,
  `.keep_prev()`. `await` to commit.
- `write_issue(#{ ... })` - insert/update an issue. Builder methods:
  `.keep_status()`, `.open_on_new_evidence()`, `.open_on_changed_evidence()`.
  `await` to commit.
- `session_notes(session_id)` / `cohort_notes()` - note queries; chain
  `.with_name(name)` then `.await?`.
- `call_agent(prompt, #{ ... })` - spawn an isolated `claude` judge process;
  returns an `AgentSession`. Methods: `.wait().await`, `.kill()`, `.id()`,
  `.output()`.
- `call_llm(prompt, #{ ... })` - direct LLM call; returns an `LlmSession`.
  Methods: `.active()`, `.tool_result(...)`, `poll(session).await`.
- `render_template(template, #{ ... })` - Tera-style template render.

Sessions and messages:

```rune
for s in scan().sessions() {
    for msg in s.messages().await? {                          // all messages
    }
    for msg in s.messages().with_type("assistant").await? { } // by role
    for msg in s.messages()
        .with_type(#{ assistant: "tool_use" }).await? { }     // by role+kind
    // msg.timestamp is a DateTime; msg.as_object(), msg.model(), msg.to_json()
}
```

`DateTime` - provides date/time support

- `DateTime::from_millis(ms)`
- `DateTime::now()`
- `from_rfc3339(s)`, `parse(s, fmt)`
- `to_rfc3339()`
- `to_rfc2822()`
- `millis()`
- `year()`, `month()`, `day()`
- `hour()`, `minute()`, `second()`
- `weekday()` (0–6) or `weekday_name()`
- `ordinal()` (day of year)
- `timestamp()` (seconds)
- `format(fmt)` - strftime-style
- `add(Duration)` / `sub(Duration)` (also `+` / `-` protocols)
- `duration_since(other)` → `Duration`
- Protocols: `PARTIAL_EQ`, `EQ`, `PARTIAL_CMP`, `CMP`, `DISPLAY_FMT`,
  `DEBUG_FMT`

`Duration` - provides signed time span support

- `Duration::milliseconds(n)`, `Duration::seconds(n)`, `Duration::minutes(n)`,
  `Duration::hours(n)`, `Duration::days(n)`, `Duration::weeks(n)`
- `as_millis()`, `as_seconds()`, `as_minutes()`, `as_hours()`, `as_days()`
- Protocols: `PARTIAL_EQ`, `EQ`, `PARTIAL_CMP`, `CMP`, `DISPLAY_FMT`,
  `DEBUG_FMT`

`Ignore` - unit struct returned from a scanner to signal "no result this run".

Errors are the `gage::Error` enum (`Args`, `Db`, `Config`, `Network`, `Http`,
`Decode`, `Template`, `Agent`, ...). Propagate with `?`.
