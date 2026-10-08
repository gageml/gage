//! `session.messages()` and `session.entries()`: builders over the
//! scan's query context.
//!
//! Each builder captures the session and its constraints and runs
//! when awaited: one parameterized `SELECT` over the `message` or
//! `entry` table of the context [`ScanContext::query_context`] serves,
//! scoped to the dataset's members at the commit the scan links. The
//! row values, `Message` and `Entry`, the `gage::Error` values, and
//! the SQL fragments for `.type(spec)` are the first generation's,
//! reached by calling.
//!
//! The base builders take no syntaxed argument, so their await is
//! the list itself. `.type(spec)` moves the chain to a filtered
//! builder whose await is a `Result`: a spec the caller wrote wrong
//! is `Error::Args`. A query failure or a failure to reach the store
//! is a VM error, since the SQL is the runtime's and the scanner has
//! no recourse.

use datafusion::common::ScalarValue;
use datafusion::prelude::SessionContext;
use gage_runtime::error::{self, Error};
use gage_runtime::query::{
    Entry, Message, entries_from_batches, messages_from_batches, push_lines_clause,
    register_row_types, type_clause,
};
use rune::runtime::{Protocol, Ref, Value, VmError};
use rune::{Any, ContextError, Module};

use crate::scan::{Session, current};

pub(crate) fn types_module() -> Result<Module, ContextError> {
    let mut m = Module::new();
    register_row_types(&mut m)?;
    error::register_types(&mut m)?;

    m.ty::<MessageQuery>()?;
    m.function_meta(messages)?;
    m.associated_function("type", MessageQuery::type_)?;
    m.function_meta(MessageQuery::lines)?;
    m.function_meta(MessageQuery::latest_first)?;
    m.function_meta(MessageQuery::limit)?;
    m.associated_function(&Protocol::INTO_FUTURE, |q: MessageQuery| async move {
        fetch_messages(q).await
    })?;
    m.ty::<FilteredMessageQuery>()?;
    m.function_meta(FilteredMessageQuery::lines)?;
    m.function_meta(FilteredMessageQuery::latest_first)?;
    m.function_meta(FilteredMessageQuery::limit)?;
    m.associated_function(
        &Protocol::INTO_FUTURE,
        |q: FilteredMessageQuery| async move { fetch_filtered_messages(q).await },
    )?;

    m.ty::<EntryQuery>()?;
    m.function_meta(entries)?;
    m.associated_function("type", EntryQuery::type_)?;
    m.function_meta(EntryQuery::limit)?;
    m.associated_function(&Protocol::INTO_FUTURE, |q: EntryQuery| async move {
        fetch_entries(q).await
    })?;
    m.ty::<FilteredEntryQuery>()?;
    m.function_meta(FilteredEntryQuery::limit)?;
    m.associated_function(&Protocol::INTO_FUTURE, |q: FilteredEntryQuery| async move {
        fetch_filtered_entries(q).await
    })?;
    Ok(m)
}

/// The value of `session.messages()`. Its await is the list itself.
#[derive(Any)]
#[rune(item = ::gage)]
pub struct MessageQuery {
    #[rune(skip)]
    session_id: String,
    #[rune(skip)]
    lines: Option<(u64, u64)>,
    #[rune(skip)]
    reverse: bool,
    #[rune(skip)]
    limit: Option<u64>,
}

/// `session.messages()` narrowed by `.type(spec)`. Its await is a
/// `Result`: a spec is a syntax.
#[derive(Any)]
#[rune(item = ::gage)]
pub struct FilteredMessageQuery {
    #[rune(skip)]
    query: MessageQuery,
    #[rune(skip)]
    type_: Value,
}

/// The session's messages, read when awaited.
#[rune::function(instance)]
fn messages(session: Ref<Session>) -> MessageQuery {
    MessageQuery {
        session_id: session.id.clone(),
        lines: None,
        reverse: false,
        limit: None,
    }
}

impl MessageQuery {
    fn type_(self, t: Value) -> FilteredMessageQuery {
        FilteredMessageQuery {
            query: self,
            type_: t,
        }
    }

    /// Restrict to messages on lines `start` through `end`, inclusive.
    #[rune::function(instance)]
    fn lines(mut self, start: u64, end: u64) -> Self {
        self.lines = Some((start, end));
        self
    }

    /// Return messages in descending `line` order instead of the
    /// default ascending order.
    #[rune::function(instance)]
    fn latest_first(mut self) -> Self {
        self.reverse = true;
        self
    }

    /// Return at most `n` messages, taken in the query's order.
    #[rune::function(instance)]
    fn limit(mut self, n: u64) -> Self {
        self.limit = Some(n);
        self
    }
}

impl FilteredMessageQuery {
    /// As [`MessageQuery::lines`].
    #[rune::function(instance)]
    fn lines(mut self, start: u64, end: u64) -> Self {
        self.query.lines = Some((start, end));
        self
    }

    /// As [`MessageQuery::latest_first`].
    #[rune::function(instance)]
    fn latest_first(mut self) -> Self {
        self.query.reverse = true;
        self
    }

    /// As [`MessageQuery::limit`].
    #[rune::function(instance)]
    fn limit(mut self, n: u64) -> Self {
        self.query.limit = Some(n);
        self
    }
}

/// The value of `session.entries()`. Its await is the list itself.
#[derive(Any)]
#[rune(item = ::gage)]
pub struct EntryQuery {
    #[rune(skip)]
    session_id: String,
    #[rune(skip)]
    limit: Option<u64>,
}

/// `session.entries()` narrowed by `.type(spec)`. Its await is a
/// `Result`: a spec is a syntax.
#[derive(Any)]
#[rune(item = ::gage)]
pub struct FilteredEntryQuery {
    #[rune(skip)]
    query: EntryQuery,
    #[rune(skip)]
    type_: Value,
}

/// The session's entries, read when awaited.
#[rune::function(instance)]
fn entries(session: Ref<Session>) -> EntryQuery {
    EntryQuery {
        session_id: session.id.clone(),
        limit: None,
    }
}

impl EntryQuery {
    fn type_(self, t: Value) -> FilteredEntryQuery {
        FilteredEntryQuery {
            query: self,
            type_: t,
        }
    }

    /// Return at most `n` entries, in `line` order.
    #[rune::function(instance)]
    fn limit(mut self, n: u64) -> Self {
        self.limit = Some(n);
        self
    }
}

impl FilteredEntryQuery {
    /// As [`EntryQuery::limit`].
    #[rune::function(instance)]
    fn limit(mut self, n: u64) -> Self {
        self.query.limit = Some(n);
        self
    }
}

/// The outer error is a VM error; the inner is the scanner's
/// `Result`.
type Fetched<T> = Result<Result<T, Error>, VmError>;

async fn fetch_messages(q: MessageQuery) -> Result<Vec<Message>, VmError> {
    let (clauses, params) = session_clauses(&q.session_id, q.lines);
    Ok(messages_from_batches(
        run(&message_sql(&q, &clauses), params).await?,
    ))
}

async fn fetch_filtered_messages(q: FilteredMessageQuery) -> Fetched<Vec<Message>> {
    let (mut clauses, mut params) = session_clauses(&q.query.session_id, q.query.lines);
    if let Err(e) = push_type_clause(&q.type_, &mut clauses, &mut params) {
        return Ok(Err(e));
    }
    let batches = run(&message_sql(&q.query, &clauses), params).await?;
    Ok(Ok(messages_from_batches(batches)))
}

fn message_sql(q: &MessageQuery, clauses: &[String]) -> String {
    let order = if q.reverse { " DESC" } else { "" };
    let limit = limit_clause(q.limit);
    format!(
        "SELECT * FROM message{} ORDER BY line{order}{limit}",
        where_clause(clauses)
    )
}

async fn fetch_entries(q: EntryQuery) -> Result<Vec<Entry>, VmError> {
    let (clauses, params) = session_clauses(&q.session_id, None);
    Ok(entries_from_batches(
        run(&entry_sql(&q, &clauses), params).await?,
    ))
}

async fn fetch_filtered_entries(q: FilteredEntryQuery) -> Fetched<Vec<Entry>> {
    let (mut clauses, mut params) = session_clauses(&q.query.session_id, None);
    if let Err(e) = push_type_clause(&q.type_, &mut clauses, &mut params) {
        return Ok(Err(e));
    }
    let batches = run(&entry_sql(&q.query, &clauses), params).await?;
    Ok(Ok(entries_from_batches(batches)))
}

fn entry_sql(q: &EntryQuery, clauses: &[String]) -> String {
    let limit = limit_clause(q.limit);
    format!(
        "SELECT * FROM entry{} ORDER BY line{limit}",
        where_clause(clauses)
    )
}

fn limit_clause(limit: Option<u64>) -> String {
    limit.map(|n| format!(" LIMIT {n}")).unwrap_or_default()
}

/// The `WHERE` clauses and their parameters for one session and an
/// optional line range.
fn session_clauses(session_id: &str, lines: Option<(u64, u64)>) -> (Vec<String>, Vec<ScalarValue>) {
    let mut clauses: Vec<String> = Vec::new();
    let mut params: Vec<ScalarValue> = Vec::new();
    params.push(ScalarValue::Utf8(Some(session_id.to_string())));
    clauses.push(format!("session_id = ${}", params.len()));
    push_lines_clause(lines, &mut clauses, &mut params);
    (clauses, params)
}

/// Add the clause for a `.type(spec)`. A malformed spec is
/// `Error::Args`.
fn push_type_clause(
    type_: &Value,
    clauses: &mut Vec<String>,
    params: &mut Vec<ScalarValue>,
) -> Result<(), Error> {
    let spec = serde_json::to_value(type_)
        .map_err(|e| Error::Args(format!("`.type()` value could not be read: {e}")))?;
    clauses.push(type_clause(&spec, params)?);
    Ok(())
}

fn where_clause(clauses: &[String]) -> String {
    format!(" WHERE {}", clauses.join(" AND "))
}

/// Run `sql` on the scan's query context. The SQL is the runtime's,
/// so a failure is a VM error.
async fn run(
    sql: &str,
    params: Vec<ScalarValue>,
) -> Result<Vec<datafusion::arrow::record_batch::RecordBatch>, VmError> {
    let ctx = current()?;
    let df_ctx: &SessionContext = ctx.scan_context().await?;
    let db = |e: datafusion::error::DataFusionError| VmError::panic(format!("query: {e}"));
    let df = df_ctx.sql(sql).await.map_err(db)?;
    let df = df.with_param_values(params).map_err(db)?;
    df.collect().await.map_err(db)
}
