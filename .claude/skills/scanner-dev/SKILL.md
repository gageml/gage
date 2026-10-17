---
name: scanner-dev
description:
  Reference for building Gage scanners (scanner.rn files): the SCANNER
  declaration, task structure, the runtime API for sessions, notes, issues,
  attachments, and watermarks, and the naming conventions. Use when writing,
  modifying, or reviewing a scanner under scanners/.
---

# Scanner development

A scanner is a Rune module in `scanners/<name>/scanner.rn`. It declares a
`SCANNER` constant and defines one `pub async fn` per task. Gage loads the
declaration to plan the run, then calls each task function.

Scanners are written in the Rune programming language. Gage maintains its own
fork of Rune core support and provides a custom runtime. The runtime provides
the builtin SDK for scanners.

Load the `rune` skill for the language itself: syntax, values, pattern matching,
async, iterators.

This skill ("scanner-dev") covers the scanner model and the Gage runtime API.
The runnable scanners under `scanners/` are the living examples. Read them
before writing a new one.

## The SCANNER declaration

```rune
pub const SCANNER = #{
    name: "session-retention",
    description: "Unset session retention policy",
    groups: ["default"],
    tasks: #{
        attach_config: #{ attaches: ["claude-config"] },
        check_config: #{
            attachments: #{ needs: ["claude-config"] },
            issues: #{
                writes: #{
                    "session-retention": "Session retention policy is unset",
                },
            },
        },
    },
};
```

Fields:

- `name` --- the scanner subject. Matches the directory name.

- `description` --- one line, or a longer block for scanners that need it.

- `groups` --- the run sets a bare `gage scan` selects. `["default"]` runs with
  a bare scan. Omit to exclude from default runs.

- `hidden: true` --- keep the scanner out of listings.

- `tasks` --- one entry per task. The key is the task name and matches a
  `pub async fn` of the same name.

Each task declares what it reads and writes. Gage uses this information for
validation and task scheduling.

- `notes: #{ writes: #{ "<note>": "<doc>" } }` --- notes this task writes, each
  with a documentation string. Use `include_str!("<file>.md")` for the doc.

- `notes: #{ wants: ["<note-or-glob>"] }` --- notes this task reads. A glob such
  as `"*[finding]"` selects a note kind.

- `issues: #{ writes: #{ "<issue>": "<title>" } }` --- issues this task writes.

- `attaches: ["<attachment>"]` --- attachments this task captures.

- `attachments: #{ needs: ["<name>"] }` --- a hard dependency; the task fails if
  the attachment is absent. `wants` is a soft dependency; the task runs either
  way.

Tasks run in dependency order. A task that `attaches` runs before a task whose
`attachments.needs`/`wants` names it; a task that writes a note runs before one
that wants it.

## Naming conventions

The scanner name and the task name appear together everywhere Gage records a
task. A note written by the `scan_sessions` task of the `code-review` scanner
has the author `task:code-review:scan_sessions`. Neither name carries the whole
meaning alone.

### Scanner name

A noun phrase naming the subject. Kebab-case. Equals the directory name.
Examples: `hidden-thinking`, `session-retention`, `code-review`.

### Task name

`verb_object`, lower snake_case. It is a Rune function name. The verb is the
action; the object is what the task reads or produces. The object never repeats
the scanner subject, because the scanner name already supplies it.

- RIGHT: `attach_config`, `check_config`, `scan_sessions`, `summarize_projects`,
  `review_findings`, `attach_project_files`

- WRONG: `note`, `issue` --- names the output kind, which the task declaration
  already states

- WRONG: `detect`, `report` --- a bare verb with no object; the reader cannot
  tell what it works on

A single-task scanner names its task `main`. There is nothing to disambiguate.

### Note name

`<what>[<kind>]`. The bracketed kind is the selector a downstream task matches
with `*[kind]`; the prefix says what the note is about. A note no later task
selects takes a bare name.

- With a selector: `general[finding]`, `code-review[finding]`,
  `rules[project-summary]`

- Bare: `open`, `empty-thinking`

### Issue name and key

Name the condition. When a scanner raises one primary issue, the issue name
equals the scanner name: `session-retention`, `hidden-thinking`. The issue key
passed to `.key(..)` equals the issue name.

### Attachment name

A noun naming the captured files, kebab-case. Names the content, not the task
that captured it: `claude-config`, `project-rule-files`, `project-stack-files`.

### Watermark key

A tuple `(scanner, task, version)`. The middle segment is the task name,
kebab-cased; the version is an integer bumped when the task's watermark meaning
changes.

```rune
const SCAN_SESSIONS_KEY = ("code-review", "scan-sessions", 1);
```

### Agent, tool, and input names

- Agent name (`call_agent(..).name(..)`): the role the agent performs,
  kebab-case. `general-finding`, `summarize-project`, `issue-writer`.

- Tool name (`Tool::new("..", ..)`): a PascalCase noun for the thing the tool
  produces, because the model reads it as a tool call. `Finding`,
  `ProjectSummary`.

- Tool input (`Input::string("..")`): snake_case, the field's plain meaning.
  `description`, `session_id`, `lines`.

### Case summary

Kebab-case: scanner, note prefix, attachment, group, agent, issue. Snake_case:
task, tool input, const, helper function. PascalCase: tool. The watermark key
middle segment is kebab to match the task name.

## Runtime API

Import from `gage`. Everything that reads or writes is async; await it.

### Sessions and messages

```rune
let sessions = scan().sessions().newest_first().unseen(KEY).await?;
for (s, (start, end)) in sessions {
    for msg in s
        .messages()
        .lines(start, end)
        .type(#{ "assistant": "thinking" })
        .latest_first()
        .await?
    {
        // msg.text, msg.timestamp, msg.line, msg.type, msg.subtype, msg.model()
    }
    for t in s.tool_uses().name(["Edit", "Write"]).await? {
        // t.name, t.line, t.files_changed (Some(paths), Some([]), or None)
    }
}
```

`scan().sessions()` reads the scan's sessions. `.newest_first()` orders by
modified time. `.unseen(KEY)` pairs each session with the inclusive line range
`(start, end)` above its watermark under `KEY`, and drops fully-seen sessions.
`.hwm(KEY)` returns every session with its mark instead.

`.messages()` reads a session's messages. `.type(spec)` filters by role, or by
role and block kind with `#{ "assistant": "thinking" }`. `.lines(start, end)`
restricts to a line range, `.latest_first()` reverses order, `.limit(n)` caps
the count.

### Notes

```rune
let note = write_note("empty-thinking", msg.text == "")
    .target(Target::session_line(session_id, msg.line))
    .metadata(#{ model: msg.model() })
    .carry_forward(KEY)
    .await?;

let notes = scan().notes().name("empty-thinking").await?;
let kinds = scan().notes().names(["*[finding]"]).unseen(KEY).await?;
```

`write_note(name, value)` writes a note. The value is any Rune value. Builder:
`.target(Target)`, `.metadata(object)`, `.carry_forward(KEY)`. A returned note
has `id`, `name`, `value`, `author`, `target`, `metadata`, `created`.

`scan().notes()` reads the scan's notes, narrowed by `.name(..)` or
`.names([..])` and `.unseen(KEY)`/`.hwm(KEY)`.

`carry_forward_notes(KEY)` / `carry_forward_notes_named("*[finding]")` link
prior scans' carried notes into this scan before it reads them.

### Issues

```rune
write_issue(
    "hidden-thinking",
    "Thinking blocks hidden in recent session",
    include_str!("issue-description.md"),
)
.key("hidden-thinking")
.evidence(note)
.await?;
```

`write_issue(name, title, description)` writes an issue. Builder:

- `.key(key)` --- identity across scans. A later write under the same key
  replaces the live issue, reopening a closed one. Default policy.

- `.once()` --- write only when no issue under the key exists in any status.
  Requires `.key()`. Use it when a condition reported once and closed must not
  reopen.

- `.evidence(notes)` --- cite a note, a note id, or a list. Accumulates.

- `.pending()` --- write with status `pending` for reconciliation instead of
  `open`.

### Attachments

```rune
const SETTINGS = ["settings.json", "settings.local.json"];

pub async fn attach_config() {
    attach(Files::include(SETTINGS).root("~/.claude"))
        .name("claude-config")
        .await?;
}

pub async fn check_config() {
    let a = scan().attachments().name("claude-config").await?.next();
    if let Some(att) = a {
        if let Some(f) = att.file("settings.json").await {
            let settings = f.json().expect("should be JSON");
        }
    }
}
```

- `attach(Files::include(list).root(dir))` captures files into the dataset.

- `.name(..)` names the attachment; `.targets(sessions)` ties it to sessions.

- `scan().attachments()` reads them, narrowed by `.name(..)`/`.names([..])` and
  `.unseen(KEY)`. An attachment has `.file(key)`, `.files()`, `.root`, `.id`,
  `.name`. A file has `.text()` and `.json()`.

### Watermarks

```rune
const KEY = ("code-review", "scan-sessions", 1);

watermark(Mark::session(s), KEY).await?;
watermark(Mark::attachment(a), KEY).await?;
watermark(Mark::note(n), KEY).await?;
```

A watermark records that a task has processed an object up to a point. The next
scan's `.unseen(KEY)` reads only what is above the mark.

### Agents

```rune
let agents = AgentRunner::new();
for (s, range) in sessions {
    let prompt = Template::new(include_str!("scan-session-prompt.md.j2"))?
        .render(#{ session_id: s.id });
    agents.add(
        call_agent(prompt)
            .name("code-review")
            .tool(finding_tool())
            .tool(Query::with_session_range(s, range))
            .model("medium"),
        #{ session: s },
    );
}
let results = agents.start();
while let Some(next) = results.next().await {
    match next {
        Ok((result, #{ session })) => watermark(Mark::session(session), KEY).await?,
        Err(err) => log::error!("agent error: {err:?}"),
    }
}
```

- `call_agent(prompt)` builds an agent. Builder: `.name(..)`, `.tool(..)`,
  `.tools([..])`, `.model("medium")`.

- A single agent awaits directly: `call_agent(prompt).await?.wait().await?`.

- Many agents run through `AgentRunner`: `.add(agent, meta)`, then `.start()`
  returns a stream whose `.next().await` yields `Ok((result, meta))` or `Err`.

- Tools: `Tool::new("Name", handler)` with `.description(..)` and
  `.input(Input::string("field").description(..).required())`.

- The built-in `tools::Query`, `tools::IssueWrite`, and `tools::NoteWrite` give
  an agent read and write access.

- `Template::new(include_str!("..md.j2"))?.render(#{ .. })` renders a prompt.
  `Progress::new(n)` with `.tick()` reports progress.

## Patterns

- **Attach then check** (session-retention): one `attach_*` task captures config
  into an attachment; one `check_*` task reads it and raises the issue.

- **Evidence note then issue** (hidden-thinking): a task scans sessions, writes
  one evidence note with the state and metadata, and raises a keyed issue citing
  the note when the condition holds.

- **Per-session agent findings** (code-review, general): a `scan_sessions` task
  runs one agent per unseen session, each writing `*[finding]` notes through a
  tool, watermarking the session on completion.

- **Finding aggregation** (findings): a scanner wants `*[finding]` notes from
  every finding scanner and raises issues from them, with no per-kind logic.

## Testing

Run `just check` before declaring a change complete: clippy, fmt, and tests as
one gate. A scanner that fails to compile fails the gate through the
enumerated-files test, which compiles every scanner in a sandbox.

New or changed behavior gets a test. A new Rust function exposed to Rune that
accepts values gets a test asserting the caller's value is still readable
afterward.
