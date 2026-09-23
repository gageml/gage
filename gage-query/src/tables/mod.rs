pub mod config;
pub mod issue_report;
pub mod message_text;
pub mod note_doc;
pub mod note_message_context;
pub mod related_issue;

pub use config::ConfigTable;
pub use gage_claude::index::entry_text;
pub use gage_claude::tables::{EntryTable, MessageTable, SessionTable};
pub use issue_report::IssueReportFn;
pub use message_text::MessageTextFn;
pub use note_message_context::NoteMessageContextFn;
pub use related_issue::RelatedIssueFn;

use datafusion::arrow::datatypes::SchemaRef;

/// Static descriptor for a table-valued function surfaced in the repl
/// by `\df`. `schema` is the function's fixed result columns, or `None`
/// when the columns depend on the arguments (as `native_session`'s do
/// on the source's driver).
pub struct TvfInfo {
    pub name: &'static str,
    pub args: &'static str,
    pub schema: Option<SchemaRef>,
}

/// All TVFs registered by `create_context`. The repl reads this to
/// implement `\df` since DataFusion does not expose argument
/// signatures or output schemas through the `TableFunction` trait.
pub fn registered_tvfs() -> Vec<TvfInfo> {
    vec![
        TvfInfo {
            name: "message_text",
            args: message_text::MESSAGE_TEXT_ARGS,
            schema: Some(message_text::message_text_schema()),
        },
        TvfInfo {
            name: "note_message_context",
            args: note_message_context::NOTE_MESSAGE_CONTEXT_ARGS,
            schema: Some(note_message_context::note_message_context_schema()),
        },
        TvfInfo {
            name: "issue_report",
            args: issue_report::ISSUE_REPORT_ARGS,
            schema: Some(issue_report::issue_report_schema()),
        },
        TvfInfo {
            name: "related_issue",
            args: related_issue::RELATED_ISSUE_ARGS,
            schema: Some(related_issue::related_issue_schema()),
        },
    ]
}
