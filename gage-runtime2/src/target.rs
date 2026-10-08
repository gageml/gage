//! `gage::Target`: what a note is about. A target names a session,
//! lines of a session, or an attachment, and encodes the stored
//! target URL. It validates nothing and looks nothing up: a write
//! resolves the URL through the store and rejects a bad line
//! selection there.

use rune::alloc::fmt::TryWrite;
use rune::runtime::{Formatter, Value, Vec as RuneVec, VmError};
use rune::{Any, ContextError, Module};

use crate::attachment::{Attachment, attachment_id};
use crate::scan::{Session, session_id};

pub(crate) fn types_module() -> Result<Module, ContextError> {
    let mut m = Module::new();
    m.ty::<Target>()?;
    m.function_meta(Target::session)?;
    m.function_meta(Target::session_line)?;
    m.function_meta(Target::session_range)?;
    m.function_meta(Target::session_lines)?;
    m.function_meta(Target::attachment)?;
    m.function_meta(Target::id__meta)?;
    m.function_meta(Target::to_url__meta)?;
    m.function_meta(Target::debug)?;
    Ok(m)
}

#[derive(Any, Clone, Debug, PartialEq, Eq)]
#[rune(item = ::gage)]
pub enum Target {
    /// A whole session
    Session(String),
    /// Lines of a session: the id and the selection, in the URL
    /// fragment grammar
    SessionLines(String, String),
    /// A whole attachment
    Attachment(String),
}

impl Target {
    /// A whole session. `session` is a `Session` or an id string.
    #[rune::function(path = Self::session)]
    fn session(session: Value) -> Result<Target, VmError> {
        Ok(Target::Session(session_id(&session)?))
    }

    /// One line of a session.
    #[rune::function(path = Self::session_line)]
    fn session_line(session: Value, line: u64) -> Result<Target, VmError> {
        Ok(Target::SessionLines(
            session_id(&session)?,
            line.to_string(),
        ))
    }

    /// An inclusive line range of a session.
    #[rune::function(path = Self::session_range)]
    fn session_range(session: Value, start: u64, end: u64) -> Result<Target, VmError> {
        Ok(Target::SessionLines(
            session_id(&session)?,
            format!("{start}-{end}"),
        ))
    }

    /// Lines of a session: a selection string in the URL fragment
    /// grammar, kept as given, or a list of line numbers joined with
    /// `,`. An empty string or list is the whole session.
    #[rune::function(path = Self::session_lines)]
    fn session_lines(session: Value, lines: Value) -> Result<Target, VmError> {
        let id = session_id(&session)?;
        Ok(match selection(&lines)? {
            Some(lines) => Target::SessionLines(id, lines),
            None => Target::Session(id),
        })
    }

    /// A whole attachment. `attachment` is an `Attachment` or an id
    /// string.
    #[rune::function(path = Self::attachment)]
    fn attachment(attachment: Value) -> Result<Target, VmError> {
        Ok(Target::Attachment(attachment_id(&attachment)?))
    }

    /// The id of the object the target names.
    #[rune::function(instance, keep)]
    pub(crate) fn id(&self) -> String {
        match self {
            Target::Session(id) | Target::SessionLines(id, _) | Target::Attachment(id) => {
                id.clone()
            }
        }
    }

    /// The stored target URL: `session:<id>`, `session:<id>#<lines>`,
    /// or `attachment:<id>`.
    #[rune::function(instance, keep)]
    pub(crate) fn to_url(&self) -> String {
        match self {
            Target::Session(id) => format!("session:{id}"),
            Target::SessionLines(id, lines) => format!("session:{id}#{lines}"),
            Target::Attachment(id) => format!("attachment:{id}"),
        }
    }

    #[rune::function(protocol = DEBUG_FMT)]
    fn debug(&self, f: &mut Formatter) -> Result<(), VmError> {
        write!(f, "{self:?}")?;
        Ok(())
    }
}

/// The target in a scanner's argument: a [`Target`] as given, or a
/// `Session` or an `Attachment` as a whole-object target. The value
/// is borrowed, not taken.
pub(crate) fn target_of(v: &Value) -> Result<Target, VmError> {
    if let Ok(t) = v.borrow_ref::<Target>() {
        return Ok(t.clone());
    }
    if let Ok(s) = v.borrow_ref::<Session>() {
        return Ok(Target::Session(s.id.clone()));
    }
    if let Ok(a) = v.borrow_ref::<Attachment>() {
        return Ok(Target::Attachment(a.id.clone()));
    }
    Err(VmError::panic(format!(
        "expected a Target, a Session, or an Attachment, got {}",
        v.type_info()
    )))
}

/// The fragment of a `session_lines` argument: a string trimmed, or
/// a list of integers joined with `,`; `None` when empty.
fn selection(lines: &Value) -> Result<Option<String>, VmError> {
    if let Ok(s) = lines.borrow_string_ref() {
        let s = s.trim();
        return Ok((!s.is_empty()).then(|| s.to_string()));
    }
    if let Ok(list) = lines.borrow_ref::<RuneVec>() {
        let mut parts = Vec::with_capacity(list.len());
        for item in list.iter() {
            let n = item
                .as_integer::<u64>()
                .map_err(|e| VmError::panic(format!("lines: expected a list of integers: {e}")))?;
            parts.push(n.to_string());
        }
        return Ok((!parts.is_empty()).then(|| parts.join(",")));
    }
    Err(VmError::panic(format!(
        "lines: expected a selection string or a list of integers, got {}",
        lines.type_info()
    )))
}

#[cfg(test)]
mod tests {
    use rune::runtime::Vm;
    use rune::sync::Arc as RuneArc;
    use rune::{Diagnostics, Source, Sources};

    use super::*;

    fn vm(script: &str) -> Vm {
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
        Vm::new(rt, RuneArc::try_new(unit).unwrap())
    }

    fn session(id: &str) -> Session {
        Session {
            id: id.to_string(),
            line_count: 3,
            commit: "c".to_string(),
        }
    }

    fn attachment(id: &str) -> Attachment {
        Attachment {
            id: id.to_string(),
            name: None,
            key: None,
            targets: rune::to_value(Vec::<String>::new()).unwrap(),
            root: "/r".to_string(),
            digest: None,
            commit: "c".to_string(),
        }
    }

    /// Every constructor encodes its URL and exposes its id, and each
    /// borrows its object, so the caller's values stay readable.
    #[test]
    fn constructors_encode_urls_and_leave_caller_values_readable() {
        let mut vm = vm(r#"
            use gage::Target;

            pub fn check(s, a) {
                let lines = [2, 5];
                let urls = [
                    Target::session(s).to_url(),
                    Target::session(s.id).to_url(),
                    Target::session_line(s, 2).to_url(),
                    Target::session_range(s.id, 1, 3).to_url(),
                    Target::session_lines(s, "61-72, 80").to_url(),
                    Target::session_lines(s, lines).to_url(),
                    Target::session_lines(s, "").to_url(),
                    Target::session_lines(s, []).to_url(),
                    Target::attachment(a).to_url(),
                    Target::attachment("att-2").to_url(),
                ];
                let ids = [Target::session_line(s, 2).id(), Target::attachment(a).id()];
                (urls, ids, s.id, a.id, lines.len())
            }
            "#);
        let output = vm
            .call(["check"], (session("s1"), attachment("att-1")))
            .unwrap();
        #[expect(
            clippy::disallowed_methods,
            reason = "takes the VM execution's return value; the test holds the only live handle"
        )]
        let (urls, ids, sid, aid, len): (Vec<String>, Vec<String>, String, String, i64) =
            rune::from_value(output).unwrap();
        assert_eq!(
            urls,
            [
                "session:s1",
                "session:s1",
                "session:s1#2",
                "session:s1#1-3",
                "session:s1#61-72, 80",
                "session:s1#2,5",
                "session:s1",
                "session:s1",
                "attachment:att-1",
                "attachment:att-2",
            ]
        );
        assert_eq!(ids, ["s1", "att-1"]);
        assert_eq!((sid.as_str(), aid.as_str(), len), ("s1", "att-1", 2));
    }

    #[test]
    fn target_of_accepts_target_session_and_attachment() {
        let t = rune::to_value(Target::Attachment("x".into())).unwrap();
        assert_eq!(target_of(&t).unwrap(), Target::Attachment("x".into()));
        let s = rune::to_value(session("s1")).unwrap();
        assert_eq!(target_of(&s).unwrap(), Target::Session("s1".into()));
        let a = rune::to_value(attachment("att-1")).unwrap();
        assert_eq!(target_of(&a).unwrap(), Target::Attachment("att-1".into()));
        let bad = rune::to_value(7i64).unwrap();
        assert!(target_of(&bad).is_err());
    }
}
