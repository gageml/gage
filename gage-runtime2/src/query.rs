//! `session.messages()` and `session.entries()`: builders over the
//! scan's query context.
//!
//! Each builder captures the session and its constraints and runs
//! when awaited: one parameterized `SELECT` over the `message` or
//! `entry` table of the context [`ScanContext::query_context`] serves,
//! scoped to the dataset's members at the commit the scan links. The
//! row values, `Message` and `Entry`, the `gage::Error` values, and
//! the SQL fragments for `.type(spec)` are the first generation's,
//! reached by calling. The await returns `Result`, as legacy does: a
//! `.type(spec)` the caller wrote wrong is `Error::Args`, and a query
//! failure is `Error::Db`. A failure to reach the store is a VM
//! error, since the scanner has no recourse.

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
    m.function_meta(MessageQuery::latest_first)?;
    m.associated_function(&Protocol::INTO_FUTURE, |q: MessageQuery| async move {
        fetch_messages(q).await
    })?;

    m.ty::<EntryQuery>()?;
    m.function_meta(entries)?;
    m.associated_function("type", EntryQuery::type_)?;
    m.associated_function(&Protocol::INTO_FUTURE, |q: EntryQuery| async move {
        fetch_entries(q).await
    })?;
    Ok(m)
}

/// The value of `session.messages()`.
#[derive(Any)]
#[rune(item = ::gage)]
pub struct MessageQuery {
    #[rune(skip)]
    session_id: String,
    #[rune(skip)]
    type_: Option<Value>,
    #[rune(skip)]
    reverse: bool,
}

/// The session's messages, read when awaited.
#[rune::function(instance)]
fn messages(session: Ref<Session>) -> MessageQuery {
    MessageQuery {
        session_id: session.id.clone(),
        type_: None,
        reverse: false,
    }
}

impl MessageQuery {
    fn type_(mut self, t: Value) -> Self {
        self.type_ = Some(t);
        self
    }

    /// Return messages in descending `line` order instead of the
    /// default ascending order.
    #[rune::function(instance)]
    fn latest_first(mut self) -> Self {
        self.reverse = true;
        self
    }
}

/// The value of `session.entries()`.
#[derive(Any)]
#[rune(item = ::gage)]
pub struct EntryQuery {
    #[rune(skip)]
    session_id: String,
    #[rune(skip)]
    type_: Option<Value>,
}

/// The session's entries, read when awaited.
#[rune::function(instance)]
fn entries(session: Ref<Session>) -> EntryQuery {
    EntryQuery {
        session_id: session.id.clone(),
        type_: None,
    }
}

impl EntryQuery {
    fn type_(mut self, t: Value) -> Self {
        self.type_ = Some(t);
        self
    }
}

/// The outer error is a VM error; the inner is the scanner's
/// `Result`.
type Fetched<T> = Result<Result<T, Error>, VmError>;

async fn fetch_messages(q: MessageQuery) -> Fetched<Vec<Message>> {
    let (where_clause, params) = match where_clause(&q.session_id, q.type_.as_ref()) {
        Ok(built) => built,
        Err(e) => return Ok(Err(e)),
    };
    let order = if q.reverse { " DESC" } else { "" };
    let sql = format!("SELECT * FROM message{where_clause} ORDER BY line{order}");
    Ok(run(&sql, params).await?.map(messages_from_batches))
}

async fn fetch_entries(q: EntryQuery) -> Fetched<Vec<Entry>> {
    let (where_clause, params) = match where_clause(&q.session_id, q.type_.as_ref()) {
        Ok(built) => built,
        Err(e) => return Ok(Err(e)),
    };
    let sql = format!("SELECT * FROM entry{where_clause} ORDER BY line");
    Ok(run(&sql, params).await?.map(entries_from_batches))
}

/// The `WHERE` clause and its parameters for one session and an
/// optional `.type(spec)`. A malformed spec is `Error::Args`.
fn where_clause(
    session_id: &str,
    type_: Option<&Value>,
) -> Result<(String, Vec<ScalarValue>), Error> {
    let mut clauses: Vec<String> = Vec::new();
    let mut params: Vec<ScalarValue> = Vec::new();
    params.push(ScalarValue::Utf8(Some(session_id.to_string())));
    clauses.push(format!("session_id = ${}", params.len()));
    push_lines_clause(None, &mut clauses, &mut params);
    if let Some(t) = type_ {
        let spec = serde_json::to_value(t)
            .map_err(|e| Error::Args(format!("`.type()` value could not be read: {e}")))?;
        clauses.push(type_clause(&spec, &mut params)?);
    }
    Ok((format!(" WHERE {}", clauses.join(" AND ")), params))
}

/// Run `sql` on the scan's query context. Reaching the context is
/// the VM's concern; the query itself failing is the scanner's, as
/// `Error::Db`.
async fn run(
    sql: &str,
    params: Vec<ScalarValue>,
) -> Fetched<Vec<datafusion::arrow::record_batch::RecordBatch>> {
    let ctx = current()?;
    let df_ctx: &SessionContext = ctx.query_context().await?;
    let db = |e: datafusion::error::DataFusionError| Error::Db(e.to_string());
    let df = match df_ctx.sql(sql).await {
        Ok(df) => df,
        Err(e) => return Ok(Err(db(e))),
    };
    let df = match df.with_param_values(params) {
        Ok(df) => df,
        Err(e) => return Ok(Err(db(e))),
    };
    Ok(df.collect().await.map_err(db))
}
