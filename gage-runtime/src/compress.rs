//! `session.compress(budget, rules)`: the session rendered as a
//! compressed session for an agent prompt, read through the scan's
//! query context.
//!
//! The builder captures the session, the budget, and the rules;
//! `.lines(start, end)` constrains it to a line range, the pair
//! `scan().sessions().unseen(key)` yields. Awaiting it reads the
//! session's `message` rows and its `tool_use` links and renders
//! them with the compressor in `view`, which owns the budget and
//! rule grammar. The await is a `Result`: a
//! budget or a rule the scanner wrote wrong is `Error::Args`. A query
//! failure is a VM error, since the SQL is the runtime's.

use crate::error::Error;
use crate::row::push_lines_clause;
use crate::view::{compress_rows, rows_from_batches, tool_links_from_batches};
use datafusion::common::ScalarValue;
use rune::runtime::{Protocol, Ref, Value, VmError};
use rune::{Any, ContextError, Module};

use crate::query::run;
use crate::scan::Session;

pub(crate) fn types_module() -> Result<Module, ContextError> {
    let mut m = Module::new();
    m.ty::<SessionCompress>()?;
    m.function_meta(compress)?;
    m.function_meta(SessionCompress::lines)?;
    m.associated_function(&Protocol::INTO_FUTURE, |q: SessionCompress| async move {
        do_compress(q).await
    })?;
    Ok(m)
}

/// The value of `session.compress(budget, rules)`. Its await is a
/// `Result`.
#[derive(Any)]
#[rune(item = ::gage)]
pub struct SessionCompress {
    #[rune(skip)]
    session_id: String,
    #[rune(skip)]
    lines: Option<(u64, u64)>,
    #[rune(skip)]
    budget: Value,
    #[rune(skip)]
    rules: Value,
}

/// The session as a compressed session: every message a rule matches
/// under its `[L<line> …]` header, its text cut per the rule, sized
/// to fit the token budget. `budget` is a token count or
/// `"unlimited"`; `rules` maps a selector such as `"user.text"` to
/// its sizing attributes.
#[rune::function(instance)]
fn compress(session: Ref<Session>, budget: Value, rules: Value) -> SessionCompress {
    SessionCompress {
        session_id: session.id.clone(),
        lines: None,
        budget,
        rules,
    }
}

impl SessionCompress {
    /// Restrict to messages on lines `start` through `end`, inclusive.
    #[rune::function(instance)]
    fn lines(mut self, start: u64, end: u64) -> Self {
        self.lines = Some((start, end));
        self
    }
}

async fn do_compress(q: SessionCompress) -> Result<Result<String, Error>, VmError> {
    let mut params = vec![ScalarValue::Utf8(Some(q.session_id.clone()))];
    let mut clauses = vec!["session_id = $1".to_string()];
    push_lines_clause(q.lines, &mut clauses, &mut params);
    let sql = format!(
        "SELECT * FROM message WHERE {} ORDER BY line",
        clauses.join(" AND ")
    );
    let batches = run(&sql, params.clone()).await?;
    // A result inside the range is linked even when its call is
    // before the range, so the link query admits either line
    let links_sql = match q.lines {
        Some(_) => {
            "SELECT line, result_line, name, is_error FROM tool_use \
             WHERE session_id = $1 AND ((line >= $2 AND line <= $3) \
             OR (result_line >= $2 AND result_line <= $3))"
        }
        None => "SELECT line, result_line, name, is_error FROM tool_use WHERE session_id = $1",
    };
    let link_batches = run(links_sql, params).await?;
    Ok(compress_rows(
        &q.session_id,
        rows_from_batches(&batches),
        &tool_links_from_batches(&link_batches),
        &q.budget,
        &q.rules,
    ))
}

#[cfg(test)]
mod tests {
    use rune::Vm;
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

    /// The builder carries the session, the budget, the rules, and
    /// the line range as given, and leaves the caller's rules object
    /// readable.
    #[test]
    fn builder_carries_its_arguments_and_borrows_the_rules() {
        let mut vm = vm(r#"
            pub fn check(s) {
                let rules = #{ "user.text": #{ min: 10 } };
                let q = s.compress(100, rules).lines(2, 5);
                [q, rules.len()]
            }
            "#);
        let session = Session {
            id: "s1".into(),
            line_count: 9,
            commit: "c".into(),
        };
        let output = vm.call(["check"], (session,)).unwrap();
        let pair = output.borrow_ref::<rune::runtime::Vec>().unwrap();
        let q = pair[0].borrow_ref::<SessionCompress>().unwrap();
        assert_eq!(q.session_id, "s1");
        assert_eq!(q.lines, Some((2, 5)));
        assert_eq!(q.budget.as_integer::<i64>().unwrap(), 100);
        assert_eq!(pair[1].as_integer::<i64>().unwrap(), 1);
    }
}
