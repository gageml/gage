use std::future::Future;
use std::pin::Pin;

use gage_core::datetime::ms_to_iso8601;
use gage_core::text_resolve::TextResolver;
use gage_db::db::open_db;
use gage_db::issue::{self, Issue, LoggedEvent};
use gage_db::note::Note;
use gage_scan::scanner::ScannerRegistry;
use gage_scan::scanner_scheme::{ErrorScheme, ScannerScheme};
use rmcp::{
    ErrorData as McpError, RoleServer, handler::server::router::tool::ToolRoute, model::JsonObject,
    service::RequestContext,
};

use crate::server::GageServer;
use crate::tool::{ToolDef, build_tool_meta};

pub const TOOL: ToolDef = route;

const MD: &str = include_str!("../../config/tools/GetIssue.md");

fn route() -> ToolRoute<GageServer> {
    ToolRoute::new(build_tool_meta(MD), call)
}

fn call(
    _server: &GageServer,
    _ctx: RequestContext<RoleServer>,
    params: JsonObject,
) -> Pin<Box<dyn Future<Output = Result<String, McpError>> + Send + '_>> {
    Box::pin(handle(params))
}

async fn handle(params: JsonObject) -> Result<String, McpError> {
    let issue_id = params
        .get("issue_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| McpError::invalid_params("missing or non-string `issue_id`", None))?;

    let conn = open_db().unwrap();
    let issue = issue::get(&conn, issue_id).map_err(|e| match e {
        issue::IssueError::NotFound(_) | issue::IssueError::Ambiguous(_, _) => {
            McpError::invalid_params(e.to_string(), None)
        }
        issue::IssueError::Db(_) | issue::IssueError::Duplicate(_) => {
            McpError::internal_error(e.to_string(), None)
        }
    })?;

    let scanner_name = issue.author.strip_prefix("scanner:");
    let description = resolved_description(&issue);
    let related = issue::related_notes(&conn, &issue.id)
        .map_err(|e| McpError::internal_error(format!("load related notes: {e}"), None))?;
    let events = issue::issue_events_for(&conn, &issue.id)
        .map_err(|e| McpError::internal_error(format!("load issue events: {e}"), None))?;

    Ok(render(
        &issue,
        scanner_name,
        &description,
        &related,
        &events,
    ))
}

fn render(
    issue: &Issue,
    scanner_name: Option<&str>,
    description: &str,
    related: &[Note],
    events: &[LoggedEvent],
) -> String {
    let mut out = String::new();
    out.push_str(&format!("**{}**\n\n", issue.title));
    out.push_str(&format!("- status: {}\n", issue.status.as_str()));
    out.push_str(&format!("- id: {}\n", issue.id));
    out.push_str(&format!("- name: {}\n", issue.name));
    if !issue.target.is_empty() {
        out.push_str(&format!("- target: {}\n", issue.target));
    }
    if let Some(r) = issue.closed_reason {
        out.push_str(&format!("- closed reason: {}\n", r.as_str()));
    }
    if let Some(s) = scanner_name {
        out.push_str(&format!("- scanner: {s}\n"));
    }
    out.push_str(&format!("- created: {}\n", ms_to_iso8601(issue.created)));
    if let Some(m) = issue.modified {
        out.push_str(&format!("- modified: {}\n", ms_to_iso8601(m)));
    }
    if !description.is_empty() {
        out.push('\n');
        out.push_str(description.trim_end());
        out.push('\n');
    }

    if !related.is_empty() {
        out.push_str("\n## Evidence\n\n");
        for n in related {
            render_note(&mut out, n);
        }
    }

    if !events.is_empty() {
        out.push_str("\n## History\n\n");
        for ev in events {
            render_event(&mut out, ev);
        }
    }

    out
}

fn render_event(out: &mut String, ev: &LoggedEvent) {
    out.push_str(&format!(
        "- {} · {} · {}\n",
        ev.event.type_str(),
        ev.author,
        ms_to_iso8601(ev.timestamp),
    ));
    if let Some(m) = ev.event.message() {
        for line in m.lines() {
            out.push_str(&format!("  > {line}\n"));
        }
    }
}

fn render_note(out: &mut String, note: &Note) {
    out.push_str(&format!("### {} (note ID {})\n\n", note.name, note.id));
    out.push_str(&format!("- value: {}\n", note.value.to_json()));
    out.push_str(&format!("- target: {}\n", note.target.to_uri()));
    out.push_str(&format!("- author: {}\n", note.author));
    out.push_str(&format!("- created: {}\n", ms_to_iso8601(note.created)));
    if let Some(e) = note.explanation.as_deref().filter(|s| !s.is_empty()) {
        out.push_str(&format!("- explanation: {e}\n"));
    }
    if let Some(m) = note.metadata.as_deref().filter(|s| !s.is_empty()) {
        out.push_str(&format!("- metadata: {m}\n"));
    }
    out.push('\n');
}

fn resolved_description(issue: &Issue) -> String {
    let Some(raw) = issue.description.as_deref() else {
        return String::new();
    };
    let registry = ScannerRegistry::load();
    let r = TextResolver::new();
    let resolver = match issue.author.strip_prefix("scanner:") {
        Some(name) => match ScannerScheme::with_scanner_name(&registry, name) {
            Ok(s) => r.with_scheme("scanner", s),
            Err(e) => r.with_scheme("scanner", ErrorScheme::new(e.to_string())),
        },
        None => r.with_scheme("scanner", ScannerScheme::absolute_only()),
    };
    match resolver.resolve(raw.to_string()) {
        Ok(text) => text,
        Err(e) => format!("(unresolved {raw}: {e})"),
    }
}
