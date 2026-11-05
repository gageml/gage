//! `publish()`: the per-task publish definition a filter task fills
//! in, and the selector and reason types it takes.
//!
//! A filter task runs under a [`PublishContext`], scoped by the
//! publish engine through [`PUBLISH_CTX`]; `publish()` outside one is
//! a VM error. `publish()` returns a [`TaskPublishDef`], a handle on
//! the task's own [`Contribution`]: every call in one task writes the
//! same contribution, and the engine takes it when the task ends. A
//! contribution is declarative. It names what to remove or redact,
//! and the engine applies it to the session; a task performs no
//! operation on the session itself.
//!
//! The selectors `Entry`, `Value`, `Literal`, and `Span` name a
//! [`TypedSet`] the session's driver knows: the entries, whole values,
//! literal strings, and text spans whose type says what they hold.
//! `Reason` is the code a removal or redaction carries.

use std::fmt;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use gage_session::TypedSet;
use rune::alloc::fmt::TryWrite;
use rune::runtime::{Formatter, Object, Ref, Value as RuneValue, VmError};
use rune::{Any, ContextError, Module};
use serde_json as json;

use crate::patterns::{Pattern, Patterns};

tokio::task_local! {
    /// The running filter task's context, read by [`publish`] and
    /// `params()`
    pub static PUBLISH_CTX: PublishContext;
}

/// The filter task a `publish()` call runs under: the filter and task
/// names, the filter's resolved params, and the contribution the
/// task fills in.
#[derive(Debug, Clone)]
pub struct PublishContext {
    pub filter: String,
    pub task: String,
    pub params: Option<json::Value>,
    contribution: Arc<Mutex<Contribution>>,
}

impl PublishContext {
    pub fn new(filter: &str, task: &str, params: Option<json::Value>) -> Self {
        Self {
            filter: filter.to_string(),
            task: task.to_string(),
            params,
            contribution: Arc::new(Mutex::new(Contribution::default())),
        }
    }

    /// What the task contributed. Called once the task has ended; a
    /// handle the task left behind reads an empty contribution.
    pub fn take_contribution(&self) -> Contribution {
        std::mem::take(&mut *self.contribution.lock().unwrap())
    }
}

/// What one filter task asks the publish to do, in call order within
/// each list.
#[derive(Debug, Default, Clone)]
pub struct Contribution {
    /// Typed sets whose entries are left out
    pub entries: Vec<(TypedSet, Reason)>,
    /// Typed sets whose values are left out of their entries
    pub values: Vec<(TypedSet, Reason)>,
    /// Typed sets whose literal strings are text redactions
    pub literals: Vec<(TypedSet, Reason)>,
    /// Typed sets whose spans are removed from text values
    pub spans: Vec<(TypedSet, Reason)>,
    /// Attributes of `attrs.json` left out
    pub attrs: Vec<(String, Reason)>,
    /// Pattern sets whose matches are text redactions
    pub patterns: Vec<(Patterns, Reason)>,
    /// Instruction text for the redaction agent
    pub instructions: Vec<String>,
}

impl Contribution {
    /// Whether the task asked for nothing.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
            && self.values.is_empty()
            && self.literals.is_empty()
            && self.spans.is_empty()
            && self.attrs.is_empty()
            && self.patterns.is_empty()
            && self.instructions.is_empty()
    }
}

pub(crate) fn module() -> Result<Module, ContextError> {
    let mut m = Module::with_crate("gage")?;
    m.function("publish", publish).build()?;
    Ok(m)
}

pub(crate) fn types_module() -> Result<Module, ContextError> {
    let mut m = Module::new();
    m.ty::<TaskPublishDef>()?;
    m.function_meta(TaskPublishDef::remove_entries)?;
    m.function_meta(TaskPublishDef::remove_values)?;
    m.function_meta(TaskPublishDef::remove_spans)?;
    m.function_meta(TaskPublishDef::remove_attrs)?;
    m.function_meta(TaskPublishDef::literal_redact)?;
    m.function_meta(TaskPublishDef::pattern_redact)?;
    m.function_meta(TaskPublishDef::agent_redact)?;
    m.function_meta(TaskPublishDef::debug)?;
    Ok(m)
}

pub(crate) fn filter_module() -> Result<Module, ContextError> {
    let mut m = Module::with_crate_item("gage", ["filter"])?;
    m.ty::<EntrySet>()?;
    m.function_meta(EntrySet::harness_context)?;
    m.function_meta(EntrySet::harness_record)?;
    m.ty::<ValueSet>()?;
    m.function_meta(ValueSet::harness_record)?;
    m.function_meta(ValueSet::pii)?;
    m.function_meta(ValueSet::system)?;
    m.ty::<LiteralSet>()?;
    m.function_meta(LiteralSet::pii)?;
    m.function_meta(LiteralSet::system)?;
    m.ty::<SpanSet>()?;
    m.function_meta(SpanSet::harness_record)?;
    m.ty::<Reason>()?;
    m.function_meta(Reason::code__meta)?;
    m.function_meta(Reason::debug)?;
    Ok(m)
}

/// The running task's publish definition.
fn publish() -> Result<TaskPublishDef, VmError> {
    PUBLISH_CTX
        .try_with(|ctx| TaskPublishDef {
            inner: Arc::clone(&ctx.contribution),
        })
        .map_err(|_outside_scope| {
            VmError::panic("publish() is available only inside a publish filter task")
        })
}

/// The running task's params, when the caller runs under a publish
/// context.
pub(crate) fn current_params() -> Option<Option<json::Value>> {
    PUBLISH_CTX.try_with(|ctx| ctx.params.clone()).ok()
}

/// The handle `publish()` returns: the task's contribution, written
/// by each call. Every method returns the handle, so calls chain.
#[derive(Any, Clone)]
#[rune(item = ::gage)]
pub struct TaskPublishDef {
    #[rune(skip)]
    inner: Arc<Mutex<Contribution>>,
}

impl TaskPublishDef {
    fn with(&self, f: impl FnOnce(&mut Contribution)) -> Self {
        f(&mut self.inner.lock().unwrap());
        self.clone()
    }

    /// Leave out every entry of the set, under `reason`.
    #[rune::function(instance)]
    fn remove_entries(&self, set: Ref<EntrySet>, reason: Ref<Reason>) -> Self {
        self.with(|c| c.entries.push((set.0, *reason)))
    }

    /// Leave the set's values out of their entries, under `reason`.
    #[rune::function(instance)]
    fn remove_values(&self, set: Ref<ValueSet>, reason: Ref<Reason>) -> Self {
        self.with(|c| c.values.push((set.0, *reason)))
    }

    /// Remove the set's spans from the text values they ride in,
    /// under `reason`.
    #[rune::function(instance)]
    fn remove_spans(&self, set: Ref<SpanSet>, reason: Ref<Reason>) -> Self {
        self.with(|c| c.spans.push((set.0, *reason)))
    }

    /// Leave the named attributes out of the published `attrs.json`:
    /// one name, or a list.
    #[rune::function(instance)]
    fn remove_attrs(&self, names: RuneValue, reason: Ref<Reason>) -> Result<Self, VmError> {
        let names = string_list(&names)?;
        Ok(self.with(|c| c.attrs.extend(names.into_iter().map(|n| (n, *reason)))))
    }

    /// Redact the set's literal strings wherever they appear in text,
    /// under `reason`.
    #[rune::function(instance)]
    fn literal_redact(&self, set: Ref<LiteralSet>, reason: Ref<Reason>) -> Self {
        self.with(|c| c.literals.push((set.0, *reason)))
    }

    /// Redact what `patterns` match wherever it appears in text, under
    /// `reason`. `patterns` is a `Patterns` value or an object of
    /// pattern name to regex. A regex that does not compile is a
    /// defect in the filter's source and raises a VM fault.
    #[rune::function(instance)]
    fn pattern_redact(&self, patterns: RuneValue, reason: Ref<Reason>) -> Result<Self, VmError> {
        let patterns = patterns_of(&patterns)?;
        Ok(self.with(|c| c.patterns.push((patterns, *reason))))
    }

    /// Add `prompt` to the instructions the redaction agent applies to
    /// the session's text.
    #[rune::function(instance)]
    fn agent_redact(&self, prompt: Ref<str>) -> Self {
        let prompt = prompt.trim_end().to_string();
        self.with(|c| c.instructions.push(prompt))
    }

    #[rune::function(protocol = DEBUG_FMT)]
    fn debug(&self, f: &mut Formatter) -> Result<(), VmError> {
        write!(f, "{:?}", *self.inner.lock().unwrap())?;
        Ok(())
    }
}

/// A `Patterns` value as given, or one built from an object of name to
/// regex. A value of another type, a non-string regex, or a regex that
/// does not compile is a VM fault.
fn patterns_of(value: &RuneValue) -> Result<Patterns, VmError> {
    if let Ok(patterns) = value.borrow_ref::<Patterns>() {
        return Ok(patterns.clone());
    }
    let Ok(object) = value.borrow_ref::<Object>() else {
        return Err(VmError::panic(format!(
            "patterns is a Patterns value or an object of name to regex, not {}",
            value.type_info()
        )));
    };
    let mut patterns = Vec::with_capacity(object.len());
    for (name, regex) in object.iter() {
        let Ok(regex) = regex.borrow_string_ref() else {
            return Err(VmError::panic(format!("pattern {name}: regex is a string")));
        };
        let pattern = Pattern::new(name.as_str(), &regex)
            .map_err(|e| VmError::panic(format!("pattern {name}: {e}")))?;
        patterns.push(pattern);
    }
    Ok(Patterns::new(patterns))
}

/// One string as a one-element list, or the strings of a Rune list,
/// borrowed.
fn string_list(v: &RuneValue) -> Result<Vec<String>, VmError> {
    if let Ok(s) = v.borrow_string_ref() {
        return Ok(vec![s.to_string()]);
    }
    let list = v.borrow_ref::<rune::runtime::Vec>()?;
    let mut out = Vec::with_capacity(list.len());
    for item in list.iter() {
        out.push(item.borrow_string_ref()?.to_string());
    }
    Ok(out)
}

/// The entries of a typed set, for `remove_entries`.
#[derive(Any, Clone, Copy, Debug, PartialEq, Eq)]
#[rune(item = ::gage::filter, name = Entry)]
pub struct EntrySet(pub TypedSet);

impl EntrySet {
    /// What the harness wrote into the model's context.
    #[rune::function(path = Self::harness_context)]
    fn harness_context() -> Self {
        Self(TypedSet::HarnessContext)
    }

    /// The harness's own bookkeeping entries.
    #[rune::function(path = Self::harness_record)]
    fn harness_record() -> Self {
        Self(TypedSet::HarnessRecord)
    }
}

/// The whole values of a typed set, for `remove_values`.
#[derive(Any, Clone, Copy, Debug, PartialEq, Eq)]
#[rune(item = ::gage::filter, name = Value)]
pub struct ValueSet(pub TypedSet);

impl ValueSet {
    /// The harness's descriptive fields on a conversation entry.
    #[rune::function(path = Self::harness_record)]
    fn harness_record() -> Self {
        Self(TypedSet::HarnessRecord)
    }

    /// Fields that identify the person.
    #[rune::function(path = Self::pii)]
    fn pii() -> Self {
        Self(TypedSet::Pii)
    }

    /// Fields that describe the machine.
    #[rune::function(path = Self::system)]
    fn system() -> Self {
        Self(TypedSet::System)
    }
}

/// The literal strings of a typed set, for `literal_redact`.
#[derive(Any, Clone, Copy, Debug, PartialEq, Eq)]
#[rune(item = ::gage::filter, name = Literal)]
pub struct LiteralSet(pub TypedSet);

impl LiteralSet {
    /// Strings that identify the person: the username.
    #[rune::function(path = Self::pii)]
    fn pii() -> Self {
        Self(TypedSet::Pii)
    }

    /// Strings that describe the machine: paths and the OS version.
    #[rune::function(path = Self::system)]
    fn system() -> Self {
        Self(TypedSet::System)
    }
}

/// The text spans of a typed set, for `remove_spans`.
#[derive(Any, Clone, Copy, Debug, PartialEq, Eq)]
#[rune(item = ::gage::filter, name = Span)]
pub struct SpanSet(pub TypedSet);

impl SpanSet {
    /// The tags the harness rides inside a user turn.
    #[rune::function(path = Self::harness_record)]
    fn harness_record() -> Self {
        Self(TypedSet::HarnessRecord)
    }
}

/// The reason a removal or redaction carries, in listing order.
#[derive(Any, Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[rune(item = ::gage::filter)]
pub enum Reason {
    /// Trade secrets and commercial, financial, or privileged content
    #[rune(constructor)]
    Confidential,
    /// Content the publish's own instructions remove
    #[rune(constructor)]
    Custom,
    /// Profanity and other language unfit for a public record
    #[rune(constructor)]
    Language,
    /// Content identifying a person (name, email address, phone number)
    #[rune(constructor)]
    Pii,
    /// Internal personnel rules and practices of an organization
    #[rune(constructor)]
    Policy,
    /// Private conversational content (candid tone directed at an agent)
    #[rune(constructor)]
    Privacy,
    /// Content whose disclosure could endanger a person's life or safety
    #[rune(constructor)]
    Safety,
    /// Content not applicable to the publish scope
    #[rune(constructor)]
    Scope,
    /// A credential or key (password, API key, token)
    #[rune(constructor)]
    Secret,
    /// Content a statute exempts from disclosure
    #[rune(constructor)]
    Statute,
    /// Information about the machine or network the session ran on
    /// (path, host, OS version)
    #[rune(constructor)]
    System,
}

impl Reason {
    pub const ALL: [Reason; 11] = [
        Reason::Confidential,
        Reason::Custom,
        Reason::Language,
        Reason::Pii,
        Reason::Policy,
        Reason::Privacy,
        Reason::Safety,
        Reason::Scope,
        Reason::Secret,
        Reason::Statute,
        Reason::System,
    ];

    /// The code as a redaction records it.
    #[rune::function(keep)]
    pub fn code(self) -> &'static str {
        match self {
            Reason::Confidential => "confidential",
            Reason::Custom => "custom",
            Reason::Language => "language",
            Reason::Pii => "pii",
            Reason::Policy => "policy",
            Reason::Privacy => "privacy",
            Reason::Safety => "safety",
            Reason::Scope => "scope",
            Reason::Secret => "secret",
            Reason::Statute => "statute",
            Reason::System => "system",
        }
    }

    /// One line on what the code covers, for a listing.
    pub fn description(self) -> &'static str {
        match self {
            Reason::Confidential => {
                "Trade secrets and commercial, financial, or privileged content"
            }
            Reason::Custom => "Content the publish's own instructions remove",
            Reason::Language => "Profanity and other language unfit for a public record",
            Reason::Pii => "Information that can be used to identify a person",
            Reason::Policy => "Internal personnel rules and practices of an organization",
            Reason::Privacy => "Private conversational content",
            Reason::Safety => "Content whose disclosure could endanger a person's safety",
            Reason::Scope => "Content not applicable to the publish scope",
            Reason::Secret => "A private credential or key",
            Reason::Statute => "Content a statute exempts from disclosure",
            Reason::System => "Information about the machine or network the session ran on",
        }
    }

    /// The reason with `code`, when there is one.
    pub fn find(code: &str) -> Option<Reason> {
        Reason::ALL.into_iter().find(|r| r.code() == code)
    }

    /// The codes joined by `, `, for a message that lists them.
    pub fn codes() -> String {
        Reason::ALL
            .iter()
            .map(|r| r.code())
            .collect::<Vec<_>>()
            .join(", ")
    }

    #[rune::function(protocol = DEBUG_FMT)]
    fn debug(&self, f: &mut Formatter) -> Result<(), VmError> {
        write!(f, "Reason::{self:?}")?;
        Ok(())
    }
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

impl FromStr for Reason {
    type Err = String;

    fn from_str(code: &str) -> Result<Self, Self::Err> {
        Reason::find(code)
            .ok_or_else(|| format!("unknown reason {code:?}; reasons are {}", Reason::codes()))
    }
}

impl rune::alloc::prelude::TryClone for Reason {
    fn try_clone(&self) -> rune::alloc::Result<Self> {
        Ok(*self)
    }
}

#[cfg(test)]
mod tests {
    use rune::sync::Arc as RuneArc;
    use rune::{Diagnostics, Source, Sources, Vm};
    use tokio::sync::mpsc;

    use super::*;
    use crate::{OUTPUT_SINK, OutputSink, SCAN_CTX};

    /// Run `main` of `script` under a publish context for the filter
    /// `f` and task `main`, and return the contribution and the
    /// script's return value.
    async fn run(script: &str, params: Option<json::Value>) -> (Contribution, RuneValue) {
        let context = crate::context().unwrap();
        let rt = RuneArc::try_new(context.runtime().unwrap()).unwrap();
        let mut sources = Sources::new();
        sources.insert(Source::memory(script).unwrap()).unwrap();
        let mut diagnostics = Diagnostics::new();
        let unit = rune::prepare(&mut sources)
            .with_context(&context)
            .with_diagnostics(&mut diagnostics)
            .build();
        if !diagnostics.is_empty() {
            let mut buf = rune::termcolor::Buffer::no_color();
            diagnostics.emit(&mut buf, &sources).unwrap();
            panic!("{}", String::from_utf8(buf.into_inner()).unwrap());
        }
        let unit = RuneArc::try_new(unit.unwrap()).unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        let sink = OutputSink {
            scanner: "f".into(),
            task: "main".into(),
            tx,
        };
        let ctx = PublishContext::new("f", "main", params);
        let value = PUBLISH_CTX
            .scope(
                ctx.clone(),
                OUTPUT_SINK.scope(sink, async move {
                    let vm = Vm::new(rt, unit);
                    vm.send_execute(["main"], ())
                        .unwrap()
                        .complete()
                        .await
                        .unwrap()
                }),
            )
            .await;
        (ctx.take_contribution(), value)
    }

    #[tokio::test]
    async fn each_method_records_in_the_tasks_contribution_and_chains() {
        let (c, _) = run(
            r#"
            use gage::{filter::{Entry, Literal, Reason, Span, Value}, publish};
            pub fn main() {
                publish()
                    .remove_entries(Entry::harness_context(), Reason::Scope)
                    .remove_values(Value::harness_record(), Reason::Scope)
                    .remove_spans(Span::harness_record(), Reason::Scope)
                    .remove_attrs(["project"], Reason::System)
                    .remove_attrs("size", Reason::System)
                    .literal_redact(Literal::pii(), Reason::Pii)
                    .pattern_redact(#{ email: "[a-z]+@[a-z]+" }, Reason::Pii)
                    .agent_redact("Remove every duck.\n\n");
                publish().agent_redact("Then the geese.");
            }
            "#,
            None,
        )
        .await;
        assert_eq!(c.entries, [(TypedSet::HarnessContext, Reason::Scope)]);
        assert_eq!(c.values, [(TypedSet::HarnessRecord, Reason::Scope)]);
        assert_eq!(c.spans, [(TypedSet::HarnessRecord, Reason::Scope)]);
        assert_eq!(
            c.attrs,
            [
                ("project".to_string(), Reason::System),
                ("size".to_string(), Reason::System)
            ]
        );
        assert_eq!(c.literals, [(TypedSet::Pii, Reason::Pii)]);
        assert_eq!(c.patterns.len(), 1);
        assert_eq!(c.patterns[0].1, Reason::Pii);
        assert_eq!(
            c.patterns[0].0.matches("to ann@example"),
            [("email", "ann@example")]
        );
        assert_eq!(c.instructions, ["Remove every duck.", "Then the geese."]);
    }

    #[tokio::test]
    async fn pattern_redact_takes_a_patterns_value_and_leaves_the_callers_values_readable() {
        let (c, value) = run(
            r#"
            use gage::{Patterns, filter::Reason, publish};
            pub fn main() {
                let patterns = Patterns::from_gitleaks("[[rules]]\nid = \"k\"\nregex = '''key-[a-z]{4}'''\n");
                let plain = #{ n: "[0-9]+" };
                let prompt = "Keep this";
                publish().pattern_redact(patterns, Reason::Secret);
                publish().pattern_redact(plain, Reason::Pii);
                publish().agent_redact(prompt);
                publish().pattern_redact(patterns, Reason::Secret);
                format!("{} {}", plain.n, prompt)
            }
            "#,
            None,
        )
        .await;
        let text = value.borrow_string_ref().unwrap().to_string();
        assert_eq!(text, "[0-9]+ Keep this");
        assert_eq!(c.patterns.len(), 3);
        assert_eq!(c.patterns[0].0.matches("a key-abcd"), [("k", "key-abcd")]);
        assert_eq!(c.patterns[1].0.matches("n 42"), [("n", "42")]);
        assert_eq!(c.patterns[2].0.matches("a key-abcd"), [("k", "key-abcd")]);
    }

    #[tokio::test]
    async fn a_bad_pattern_is_a_vm_fault_naming_the_pattern() {
        for (script, message) in [
            (
                r#"publish().pattern_redact(#{ bad: "(" }, Reason::Pii);"#,
                "pattern bad: ",
            ),
            (
                r#"publish().pattern_redact(#{ bad: 3 }, Reason::Pii);"#,
                "pattern bad: regex is a string",
            ),
            (
                r#"publish().pattern_redact(7, Reason::Pii);"#,
                "patterns is a Patterns value",
            ),
            (
                r#"Patterns::from_gitleaks("rules = 3");"#,
                "gitleaks rule <file>",
            ),
        ] {
            let err = run_err(&format!(
                "use gage::{{Patterns, filter::Reason, publish}};\npub fn main() {{ {script} }}"
            ))
            .await;
            assert!(err.contains(message), "{script}: {err}");
        }
    }

    /// Run `main` of `script` under a publish context and return the
    /// VM error it ends with.
    async fn run_err(script: &str) -> String {
        let context = crate::context().unwrap();
        let rt = RuneArc::try_new(context.runtime().unwrap()).unwrap();
        let mut sources = Sources::new();
        sources.insert(Source::memory(script).unwrap()).unwrap();
        let mut diagnostics = Diagnostics::new();
        let unit = rune::prepare(&mut sources)
            .with_context(&context)
            .with_diagnostics(&mut diagnostics)
            .build()
            .unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        let sink = OutputSink {
            scanner: "f".into(),
            task: "main".into(),
            tx,
        };
        let ctx = PublishContext::new("f", "main", None);
        let err = PUBLISH_CTX
            .scope(
                ctx,
                OUTPUT_SINK.scope(sink, async move {
                    let vm = Vm::new(rt, RuneArc::try_new(unit).unwrap());
                    vm.send_execute(["main"], ())
                        .unwrap()
                        .complete()
                        .await
                        .unwrap_err()
                }),
            )
            .await;
        err.to_string()
    }

    #[tokio::test]
    async fn params_come_from_the_publish_context() {
        let (c, _) = run(
            r#"
            use gage::{params, publish};
            pub fn main() {
                publish().agent_redact(format!("Also: {}", params().prompt));
            }
            "#,
            Some(json::json!({ "prompt": "ducks" })),
        )
        .await;
        assert_eq!(c.instructions, ["Also: ducks"]);
    }

    #[tokio::test]
    async fn publish_outside_a_filter_task_is_a_vm_error() {
        let context = crate::context().unwrap();
        let rt = RuneArc::try_new(context.runtime().unwrap()).unwrap();
        let mut sources = Sources::new();
        sources
            .insert(Source::memory("pub fn main() { gage::publish(); }").unwrap())
            .unwrap();
        let unit = rune::prepare(&mut sources)
            .with_context(&context)
            .build()
            .unwrap();
        let vm = Vm::new(rt, RuneArc::try_new(unit).unwrap());
        let err = vm
            .send_execute(["main"], ())
            .unwrap()
            .complete()
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("publish() is available only"),
            "{err}"
        );
        assert!(SCAN_CTX.try_with(|_| ()).is_err());
    }

    #[test]
    fn reasons_round_trip_through_their_codes() {
        for reason in Reason::ALL {
            assert_eq!(reason.code().parse::<Reason>().unwrap(), reason);
            assert!(!reason.description().is_empty());
        }
        let err = "nope".parse::<Reason>().unwrap_err();
        assert!(err.contains("confidential, custom"), "{err}");
        assert_eq!(Reason::find("scope"), Some(Reason::Scope));
        assert_eq!(Reason::find("opaque"), None);
    }
}
