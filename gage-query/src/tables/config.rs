use std::any::Any;
use std::collections::HashSet;
use std::fmt::{self, Formatter};
use std::io;
use std::sync::Arc;

use async_trait::async_trait;
use datafusion::arrow::array::{Int64Builder, StringBuilder, TimestampMillisecondBuilder};
use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef, TimeUnit};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::catalog::Session;
use datafusion::datasource::{TableProvider, TableType};
use datafusion::error::{DataFusionError, Result};
use datafusion::execution::context::TaskContext;
use datafusion::logical_expr::{Operator, TableProviderFilterPushDown};
use datafusion::physical_expr::EquivalenceProperties;
use datafusion::physical_plan::execution_plan::{Boundedness, EmissionType};
use datafusion::physical_plan::memory::MemoryStream;
use datafusion::physical_plan::metrics::{
    Count, ExecutionPlanMetricsSet, MetricBuilder, MetricsSet,
};
use datafusion::physical_plan::{
    DisplayAs, DisplayFormatType, ExecutionPlan, Partitioning, PlanProperties,
    SendableRecordBatchStream,
};
use datafusion::prelude::*;
use gage_claude::config::{ConfigFile, ConfigFiles};
use gage_claude::home::ClaudeHome;

fn config_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("scope", DataType::Utf8, false),
        Field::new("project", DataType::Utf8, false),
        Field::new("type", DataType::Utf8, false),
        Field::new("name", DataType::Utf8, false),
        Field::new("path", DataType::Utf8, false),
        Field::new("text", DataType::Utf8, true),
        Field::new("size", DataType::Int64, false),
        Field::new(
            "mtime",
            DataType::Timestamp(TimeUnit::Millisecond, Some("UTC".into())),
            false,
        ),
    ]))
}

const COL_TEXT: usize = 5;

#[derive(Debug, Clone)]
pub struct ConfigTable {
    home: ClaudeHome,
    schema: SchemaRef,
}

impl ConfigTable {
    pub fn new(home: ClaudeHome) -> Self {
        Self {
            home,
            schema: config_schema(),
        }
    }
}

#[async_trait]
impl TableProvider for ConfigTable {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }

    fn table_type(&self) -> TableType {
        TableType::Base
    }

    fn supports_filters_pushdown(
        &self,
        filters: &[&Expr],
    ) -> Result<Vec<TableProviderFilterPushDown>> {
        let support = filters
            .iter()
            .map(|f| {
                if is_pushdown_eligible(f, "scope")
                    || is_pushdown_eligible(f, "project")
                    || is_pushdown_eligible(f, "type")
                {
                    TableProviderFilterPushDown::Exact
                } else {
                    TableProviderFilterPushDown::Inexact
                }
            })
            .collect();
        Ok(support)
    }

    async fn scan(
        &self,
        _state: &dyn Session,
        projection: Option<&Vec<usize>>,
        filters: &[Expr],
        _limit: Option<usize>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        let scope_filter = extract_string_set(filters, "scope");
        let project_filter = extract_string_set(filters, "project");
        let type_filter = extract_string_set(filters, "type");

        let want_text = projection.as_ref().is_none_or(|p| p.contains(&COL_TEXT));

        let projected_schema = match projection {
            Some(indices) => Arc::new(self.schema.project(indices)?),
            None => self.schema.clone(),
        };

        Ok(Arc::new(ConfigExec::new(
            self.home.clone(),
            self.schema.clone(),
            projected_schema,
            projection.cloned(),
            scope_filter,
            project_filter,
            type_filter,
            want_text,
        )))
    }
}

/// Whether a filter is one we recognize for pushdown on `col`: equality
/// against a string literal or `IN (...)` of string literals.
fn is_pushdown_eligible(expr: &Expr, col_name: &str) -> bool {
    if let Expr::BinaryExpr(binary) = expr
        && binary.op == Operator::Eq
        && let Expr::Column(ref col) = *binary.left
        && col.name == col_name
        && let Expr::Literal(ref scalar, _) = *binary.right
        && scalar.try_as_str().flatten().is_some()
    {
        return true;
    }
    if let Expr::InList(in_list) = expr
        && let Expr::Column(ref col) = *in_list.expr
        && col.name == col_name
        && !in_list.negated
        && in_list
            .list
            .iter()
            .all(|i| matches!(i, Expr::Literal(s, _) if s.try_as_str().flatten().is_some()))
    {
        return true;
    }
    false
}

/// Collect the literal string set for `WHERE <col> = '...'` and
/// `WHERE <col> IN (...)` filters. Returns `None` if no such filter is
/// present (caller treats that as "no constraint").
fn extract_string_set(filters: &[Expr], col_name: &str) -> Option<HashSet<String>> {
    let mut set: Option<HashSet<String>> = None;
    for expr in filters {
        if let Expr::BinaryExpr(binary) = expr
            && binary.op == Operator::Eq
            && let Expr::Column(ref col) = *binary.left
            && col.name == col_name
            && let Expr::Literal(ref scalar, _) = *binary.right
            && let Some(s) = scalar.try_as_str().flatten()
        {
            set.get_or_insert_with(HashSet::new).insert(s.to_string());
            continue;
        }
        if let Expr::InList(in_list) = expr
            && let Expr::Column(ref col) = *in_list.expr
            && col.name == col_name
            && !in_list.negated
        {
            for item in &in_list.list {
                if let Expr::Literal(scalar, _) = item
                    && let Some(s) = scalar.try_as_str().flatten()
                {
                    set.get_or_insert_with(HashSet::new).insert(s.to_string());
                }
            }
        }
    }
    set
}

#[derive(Debug, Clone)]
struct ConfigExec {
    home: ClaudeHome,
    full_schema: SchemaRef,
    projected_schema: SchemaRef,
    projection: Option<Vec<usize>>,
    scope_filter: Option<HashSet<String>>,
    project_filter: Option<HashSet<String>>,
    type_filter: Option<HashSet<String>>,
    want_text: bool,
    properties: PlanProperties,
    metrics: ExecutionPlanMetricsSet,
}

impl ConfigExec {
    #[allow(clippy::too_many_arguments)]
    fn new(
        home: ClaudeHome,
        full_schema: SchemaRef,
        projected_schema: SchemaRef,
        projection: Option<Vec<usize>>,
        scope_filter: Option<HashSet<String>>,
        project_filter: Option<HashSet<String>>,
        type_filter: Option<HashSet<String>>,
        want_text: bool,
    ) -> Self {
        let properties = PlanProperties::new(
            EquivalenceProperties::new(projected_schema.clone()),
            Partitioning::UnknownPartitioning(1),
            EmissionType::Incremental,
            Boundedness::Bounded,
        );
        Self {
            home,
            full_schema,
            projected_schema,
            projection,
            scope_filter,
            project_filter,
            type_filter,
            want_text,
            properties,
            metrics: ExecutionPlanMetricsSet::new(),
        }
    }
}

impl DisplayAs for ConfigExec {
    fn fmt_as(&self, _t: DisplayFormatType, f: &mut Formatter) -> fmt::Result {
        write!(f, "ConfigExec")
    }
}

impl ExecutionPlan for ConfigExec {
    fn name(&self) -> &'static str {
        "ConfigExec"
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn properties(&self) -> &PlanProperties {
        &self.properties
    }

    fn children(&self) -> Vec<&Arc<dyn ExecutionPlan>> {
        vec![]
    }

    fn with_new_children(
        self: Arc<Self>,
        _children: Vec<Arc<dyn ExecutionPlan>>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        Ok(self)
    }

    fn metrics(&self) -> Option<MetricsSet> {
        Some(self.metrics.clone_inner())
    }

    fn execute(
        &self,
        partition: usize,
        _context: Arc<TaskContext>,
    ) -> Result<SendableRecordBatchStream> {
        // `files_read` and `dirs_listed` aggregate the gage-claude walk
        // metrics plus reads we trigger directly from this exec:
        // `~/.claude.json` in `home.projects()` and per-row
        // `read_to_string` for the `text` column. Stats (`metadata`)
        // are not counted on either side.
        let files_read = MetricBuilder::new(&self.metrics).counter("files_read", partition);
        let dirs_listed = MetricBuilder::new(&self.metrics).counter("dirs_listed", partition);

        let rows = collect_rows(
            self.home.clone(),
            self.scope_filter.as_ref(),
            self.project_filter.as_ref(),
            self.type_filter.as_ref(),
            self.want_text,
            &files_read,
            &dirs_listed,
        )
        .map_err(|e| DataFusionError::External(Box::new(e)))?;

        let batch = build_batch(&self.full_schema, &rows)?;
        Ok(Box::pin(MemoryStream::try_new(
            vec![batch],
            self.projected_schema.clone(),
            self.projection.clone(),
        )?))
    }
}

/// One SQL row's worth of values.
#[derive(Debug)]
struct ConfigRow {
    scope: String,
    project: String,
    type_code: String,
    name: String,
    path: String,
    text: Option<String>,
    size: i64,
    mtime: i64,
}

#[allow(clippy::too_many_arguments)]
fn collect_rows(
    home: ClaudeHome,
    scope_filter: Option<&HashSet<String>>,
    project_filter: Option<&HashSet<String>>,
    type_filter: Option<&HashSet<String>>,
    want_text: bool,
    files_read: &Count,
    dirs_listed: &Count,
) -> io::Result<Vec<ConfigRow>> {
    let mut rows = Vec::new();

    let (want_user, want_project_scope) = decide_scopes(scope_filter, project_filter, type_filter);

    if want_user {
        let finder = home
            .config()
            .settings(type_filter.is_none_or(|f| f.contains("settings")))
            .memory(type_filter.is_none_or(|f| f.contains("memory")))
            .skills(type_filter.is_none_or(|f| f.contains("skill") || f.contains("skill-rule")))
            .commands(type_filter.is_none_or(|f| f.contains("command")))
            .agents(type_filter.is_none_or(|f| f.contains("agent")))
            .plugins(type_filter.is_none_or(|f| f.iter().any(|t| is_user_only_type(t))));
        push_files(
            &mut rows,
            finder.find(),
            "user",
            "",
            scope_filter,
            type_filter,
            want_text,
            files_read,
            dirs_listed,
        )?;
    }

    if want_project_scope {
        // `~/.claude.json` missing means no recorded projects — skip the
        // whole scope rather than failing the query.
        let projects = match home.projects() {
            Ok(p) => {
                files_read.add(1);
                p
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e),
        };
        // Settings and memory each split into a project-scope and a
        // local-scope variant (`settings.json` / `settings.local.json`,
        // `CLAUDE.md` / `CLAUDE.local.md`), so their finder toggles
        // consult both the type and scope filters.
        let want_project_settings = type_filter.is_none_or(|f| f.contains("settings"))
            && scope_filter.is_none_or(|s| s.contains("project"));
        let want_local_settings = type_filter.is_none_or(|f| f.contains("settings"))
            && scope_filter.is_none_or(|s| s.contains("local"));
        let want_project_memory = type_filter.is_none_or(|f| f.contains("memory"))
            && scope_filter.is_none_or(|s| s.contains("project"));
        let want_local_memory = type_filter.is_none_or(|f| f.contains("memory"))
            && scope_filter.is_none_or(|s| s.contains("local"));
        // The remaining variants all yield scope="project", so they
        // require the scope filter to permit "project".
        let want_project_other = scope_filter.is_none_or(|s| s.contains("project"));
        for project in projects {
            let display = project.path.to_string_lossy().into_owned();
            if let Some(filter) = project_filter
                && !filter.contains(&display)
            {
                continue;
            }
            let finder = project
                .config()
                .settings(want_project_settings)
                .local_settings(want_local_settings)
                .memory(want_project_memory)
                .local_memory(want_local_memory)
                .manifest(want_project_other && type_filter.is_none_or(|f| f.contains("manifest")))
                .skills(
                    want_project_other
                        && type_filter
                            .is_none_or(|f| f.contains("skill") || f.contains("skill-rule")),
                )
                .commands(want_project_other && type_filter.is_none_or(|f| f.contains("command")))
                .agents(want_project_other && type_filter.is_none_or(|f| f.contains("agent")));
            push_files(
                &mut rows,
                finder.find(),
                "project",
                &display,
                scope_filter,
                type_filter,
                want_text,
                files_read,
                dirs_listed,
            )?;
        }
    }

    Ok(rows)
}

#[allow(clippy::too_many_arguments)]
fn push_files(
    rows: &mut Vec<ConfigRow>,
    files: ConfigFiles,
    source_scope: &str,
    project_label: &str,
    scope_filter: Option<&HashSet<String>>,
    type_filter: Option<&HashSet<String>>,
    want_text: bool,
    files_read: &Count,
    dirs_listed: &Count,
) -> io::Result<()> {
    let mut iter = files;
    for file in iter.by_ref() {
        let file = file?;
        // Scope and type filters are checked against the variant first
        // — pure pattern match, no I/O — so files that don't match
        // never get their text read.
        if let Some(filter) = scope_filter
            && !filter.contains(effective_scope(&file, source_scope))
        {
            continue;
        }
        if let Some(filter) = type_filter
            && !filter.contains(type_code_of(&file))
        {
            continue;
        }
        rows.push(into_row(
            file,
            source_scope,
            project_label,
            want_text,
            files_read,
        )?);
    }
    // Fold the walk's own counters in once the iterator is drained
    let walk = iter.metrics();
    files_read.add(walk.files_read as usize);
    dirs_listed.add(walk.dirs_listed as usize);
    Ok(())
}

/// The scope a row will carry, given the variant and the walk it came
/// from. `LocalSettings` and `LocalMemory` always report `"local"`;
/// every other variant inherits the walk's scope (`"user"` or
/// `"project"`).
fn effective_scope<'a>(file: &ConfigFile, source_scope: &'a str) -> &'a str {
    match file {
        ConfigFile::LocalSettings(_) | ConfigFile::LocalMemory(_) => "local",
        _ => source_scope,
    }
}

/// Tier 1 scope selection: decide which top-level walks to run from
/// the SQL filter sets alone, before any I/O. Returns
/// `(want_user, want_project_scope)`.
fn decide_scopes(
    scope_filter: Option<&HashSet<String>>,
    project_filter: Option<&HashSet<String>>,
    type_filter: Option<&HashSet<String>>,
) -> (bool, bool) {
    let want_user = scope_filter.is_none_or(|s| s.contains("user"))
        // User-scope rows always carry `project=""`, so a `project = ...`
        // / `project IN (...)` filter that doesn't include `""` rules
        // them out entirely. Skip the user walk in that case — also
        // makes the `project` pushdown safe to report as Exact.
        && project_filter.is_none_or(|p| p.contains(""));
    let want_project_scope = scope_filter
        .is_none_or(|s| s.contains("project") || s.contains("local"))
        // If every type in the filter is user-only there's nothing
        // for the project walk to produce — skip the `.claude.json`
        // read and the per-project directory walks entirely.
        && type_filter.is_none_or(|f| f.iter().any(|t| !is_user_only_type(t)));
    (want_user, want_project_scope)
}

/// Types that only ever come out of the user-scope walk. Used to short-
/// circuit the project scope when a query's `type` filter is a subset
/// of these.
fn is_user_only_type(type_code: &str) -> bool {
    matches!(
        type_code,
        "installed-plugins"
            | "plugin-skill"
            | "plugin-skill-rule"
            | "plugin-command"
            | "plugin-agent"
            | "plugin-hook"
            | "plugin-mcp"
            | "plugin-lsp"
            | "plugin-monitor"
    )
}

fn type_code_of(file: &ConfigFile) -> &'static str {
    match file {
        ConfigFile::Settings(_) | ConfigFile::LocalSettings(_) => "settings",
        ConfigFile::Memory(_) | ConfigFile::LocalMemory(_) => "memory",
        ConfigFile::Manifest(_) => "manifest",
        ConfigFile::Skill { .. } => "skill",
        ConfigFile::SkillRule { .. } => "skill-rule",
        ConfigFile::Command { .. } => "command",
        ConfigFile::Agent { .. } => "agent",
        ConfigFile::InstalledPlugins(_) => "installed-plugins",
        ConfigFile::PluginSkill { .. } => "plugin-skill",
        ConfigFile::PluginSkillRule { .. } => "plugin-skill-rule",
        ConfigFile::PluginCommand { .. } => "plugin-command",
        ConfigFile::PluginAgent { .. } => "plugin-agent",
        ConfigFile::PluginHook { .. } => "plugin-hook",
        ConfigFile::PluginMcp { .. } => "plugin-mcp",
        ConfigFile::PluginLsp { .. } => "plugin-lsp",
        ConfigFile::PluginMonitor { .. } => "plugin-monitor",
    }
}

/// Build a row from a `ConfigFile`, consuming it so its `String` /
/// `PathBuf` fields move into the row without cloning. `source_scope`
/// is the bare source scope (`"user"` or `"project"`); the helper
/// promotes it to `"local"` for `LocalSettings`. Performs `len`,
/// `modified`, and (when projected) `read_to_string` IO up front.
fn into_row(
    file: ConfigFile,
    source_scope: &str,
    project_label: &str,
    want_text: bool,
    files_read: &Count,
) -> io::Result<ConfigRow> {
    let size = file.len()? as i64;
    let mtime = file.modified()?;
    let text = if want_text {
        let t = file.read_to_string()?;
        files_read.add(1);
        Some(t)
    } else {
        None
    };
    let path = file.path().to_string_lossy().into_owned();
    let type_code = type_code_of(&file);

    let scope = effective_scope(&file, source_scope).to_string();

    let name: String = match file {
        ConfigFile::Settings(_)
        | ConfigFile::LocalSettings(_)
        | ConfigFile::Memory(_)
        | ConfigFile::LocalMemory(_)
        | ConfigFile::InstalledPlugins(_) => String::new(),
        // The file name distinguishes manifests within one project
        ConfigFile::Manifest(p) => p
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        ConfigFile::Skill { name, .. }
        | ConfigFile::Command { name, .. }
        | ConfigFile::Agent { name, .. } => name,
        ConfigFile::SkillRule { skill, name, .. } => format!("{skill}::{name}"),
        ConfigFile::PluginSkill { plugin, name, .. }
        | ConfigFile::PluginCommand { plugin, name, .. }
        | ConfigFile::PluginAgent { plugin, name, .. }
        | ConfigFile::PluginHook { plugin, name, .. }
        | ConfigFile::PluginMcp { plugin, name, .. }
        | ConfigFile::PluginLsp { plugin, name, .. }
        | ConfigFile::PluginMonitor { plugin, name, .. } => format!("{plugin}::{name}"),
        ConfigFile::PluginSkillRule {
            plugin,
            skill,
            name,
            ..
        } => format!("{plugin}::{skill}::{name}"),
    };

    Ok(ConfigRow {
        scope,
        project: project_label.to_string(),
        type_code: type_code.to_string(),
        name,
        path,
        text,
        size,
        mtime,
    })
}

fn build_batch(schema: &SchemaRef, rows: &[ConfigRow]) -> Result<RecordBatch> {
    let len = rows.len();
    let mut scopes = StringBuilder::with_capacity(len, len * 8);
    let mut projects = StringBuilder::with_capacity(len, len * 16);
    let mut types = StringBuilder::with_capacity(len, len * 16);
    let mut names = StringBuilder::with_capacity(len, len * 32);
    let mut paths = StringBuilder::with_capacity(len, len * 96);
    let mut texts = StringBuilder::with_capacity(len, len * 256);
    let mut sizes = Int64Builder::with_capacity(len);
    let mut mtimes = TimestampMillisecondBuilder::with_capacity(len);

    for r in rows {
        scopes.append_value(&r.scope);
        projects.append_value(&r.project);
        types.append_value(&r.type_code);
        names.append_value(&r.name);
        paths.append_value(&r.path);
        match &r.text {
            Some(t) => texts.append_value(t),
            None => texts.append_null(),
        }
        sizes.append_value(r.size);
        mtimes.append_value(r.mtime);
    }

    Ok(RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(scopes.finish()),
            Arc::new(projects.finish()),
            Arc::new(types.finish()),
            Arc::new(names.finish()),
            Arc::new(paths.finish()),
            Arc::new(texts.finish()),
            Arc::new(sizes.finish()),
            Arc::new(mtimes.finish().with_timezone("UTC")),
        ],
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(items: &[&str]) -> HashSet<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn no_filters_walks_both_scopes() {
        assert_eq!(decide_scopes(None, None, None), (true, true));
    }

    #[test]
    fn scope_user_only_skips_project() {
        let s = set(&["user"]);
        assert_eq!(decide_scopes(Some(&s), None, None), (true, false));
    }

    #[test]
    fn scope_project_only_skips_user() {
        let s = set(&["project"]);
        assert_eq!(decide_scopes(Some(&s), None, None), (false, true));
    }

    #[test]
    fn scope_local_walks_project_scope() {
        // `local` rows live under the project walk
        let s = set(&["local"]);
        assert_eq!(decide_scopes(Some(&s), None, None), (false, true));
    }

    #[test]
    fn type_user_only_skips_project_scope() {
        let t = set(&["installed-plugins"]);
        assert_eq!(decide_scopes(None, None, Some(&t)), (true, false));
    }

    #[test]
    fn type_all_user_only_mix_skips_project_scope() {
        let t = set(&["plugin-mcp", "plugin-skill", "plugin-hook"]);
        assert_eq!(decide_scopes(None, None, Some(&t)), (true, false));
    }

    #[test]
    fn type_with_any_non_user_only_walks_project_scope() {
        let t = set(&["installed-plugins", "memory"]);
        assert_eq!(decide_scopes(None, None, Some(&t)), (true, true));
    }

    #[test]
    fn type_memory_walks_both() {
        let t = set(&["memory"]);
        assert_eq!(decide_scopes(None, None, Some(&t)), (true, true));
    }

    #[test]
    fn scope_user_with_user_only_type_walks_only_user() {
        let s = set(&["user"]);
        let t = set(&["plugin-mcp"]);
        assert_eq!(decide_scopes(Some(&s), None, Some(&t)), (true, false));
    }

    #[test]
    fn scope_local_with_user_only_type_walks_nothing() {
        // `local` permits the project walk, but every type in the
        // filter is user-only — so the project walk produces nothing
        // and is skipped. `user` is excluded by the scope filter.
        let s = set(&["local"]);
        let t = set(&["installed-plugins"]);
        assert_eq!(decide_scopes(Some(&s), None, Some(&t)), (false, false));
    }

    #[test]
    fn scope_in_user_project_walks_both() {
        let s = set(&["user", "project"]);
        assert_eq!(decide_scopes(Some(&s), None, None), (true, true));
    }

    #[test]
    fn project_filter_skips_user_walk() {
        // A non-empty `project = ...` predicate can never match a
        // user-scope row (project=""), so the user walk is skipped.
        let p = set(&["/some/project"]);
        assert_eq!(decide_scopes(None, Some(&p), None), (false, true));
    }

    #[test]
    fn project_filter_including_empty_keeps_user_walk() {
        // `project IN ('', '/some/project')` does match user-scope rows.
        let p = set(&["", "/some/project"]);
        assert_eq!(decide_scopes(None, Some(&p), None), (true, true));
    }
}
