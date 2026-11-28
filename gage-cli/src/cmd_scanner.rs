//! `gage scanner`: the scanners Gage finds, and the install, upgrade,
//! and uninstall of the scanners published in the scanner source.

use anyhow::Context;
use clap::{Args, Subcommand};
use cliclack as cli;
use console::style;
use gage_core::config::Config;
use gage_core::path::shorten_home;
use gage_registry::DefSource;
use gage_registry::catalog::{
    self, CatalogError, Entry, ScannerSet, Step, plan_install, plan_uninstall, plan_upgrade,
};
use gage_registry::scanner::{ScannerDef, ScannerRegistry, check_api, scanner_source};
use tabled::{
    Table,
    settings::{
        Color, Style, Width,
        object::{Columns, Object, Rows},
        peaker::Priority,
    },
};

use crate::dialog::{self, DialogError, DialogResult};
use crate::error::CliError;
use crate::human::count;
use crate::progress::busy;
use crate::style as s;

#[derive(Subcommand)]
pub enum ScannerCommand {
    /// List scanners
    ///
    /// Shows every scanner Gage finds: installed scanners, user
    /// scanners, and project scanners.
    List(ScannerListArgs),

    /// Search scanner source
    ///
    /// Shows the scanners that scanner source holds, with the version
    /// of each one that is installed.
    Search(ScannerSearchArgs),

    /// Show a scanner
    ///
    /// A scanner that is not installed is read from scanner source.
    Show(ScannerShowArgs),

    /// Install scanners
    ///
    /// Installs each scanner from scanner source, along with every
    /// scanner it requires. An installed scanner is replaced when
    /// scanner source holds another version of it.
    Install(ScannerInstallArgs),

    /// Uninstall scanners
    ///
    /// Notes and issues that an uninstalled scanner wrote stay in
    /// store.
    Uninstall(ScannerUninstallArgs),

    /// Upgrade installed scanners
    ///
    /// Replaces each installed scanner that has a newer version in
    /// scanner source, and installs every scanner a newer version
    /// requires.
    Upgrade(ScannerUpgradeArgs),
}

/// Shown under an empty listing of the scanners Gage finds
const SEARCH_HINT: &str = "Use 'gage scanner search' to show scanners to install";

#[derive(Args)]
pub struct ScannerListArgs {
    /// Show installed scanners that have a newer version
    ///
    /// Reads scanner source and shows each installed scanner it holds
    /// a newer version of.
    #[arg(short, long)]
    outdated: bool,
}

#[derive(Args)]
pub struct ScannerSearchArgs {
    /// Words to search for
    ///
    /// Shows the scanners whose name or description has every word.
    /// Without a word, shows every scanner.
    #[arg(value_name = "TERM")]
    terms: Vec<String>,
}

#[derive(Args)]
pub struct ScannerShowArgs {
    /// Scanner to show
    ///
    /// A scanner name, or NAMESPACE/NAME for a scanner of scanner
    /// source.
    scanner: String,
}

#[derive(Args)]
pub struct ScannerInstallArgs {
    /// Scanners to install
    ///
    /// Each is a scanner name, or NAMESPACE/NAME when more than one
    /// namespace has a scanner of that name.
    #[arg(required = true, value_name = "SCANNER")]
    scanners: Vec<String>,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,
}

#[derive(Args)]
pub struct ScannerUninstallArgs {
    /// Scanners to uninstall
    #[arg(required = true, value_name = "SCANNER")]
    scanners: Vec<String>,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,
}

#[derive(Args)]
pub struct ScannerUpgradeArgs {
    /// Scanners to upgrade
    ///
    /// Without a scanner, every installed scanner is upgraded.
    #[arg(value_name = "SCANNER")]
    scanners: Vec<String>,

    /// Skip confirmation prompt
    #[arg(short, long)]
    yes: bool,
}

pub fn run(command: ScannerCommand) -> Result<(), CliError> {
    match command {
        ScannerCommand::List(args) => list(args),
        ScannerCommand::Search(args) => search(args),
        ScannerCommand::Show(args) => show(args),
        ScannerCommand::Install(args) => install(args),
        ScannerCommand::Uninstall(args) => uninstall(args),
        ScannerCommand::Upgrade(args) => upgrade(args),
    }
}

fn list(args: ScannerListArgs) -> Result<(), CliError> {
    let config = Config::load_user().context("gage scanner list")?;
    if args.outdated {
        return list_outdated(&config);
    }
    let registry = ScannerRegistry::load();
    for (path, error) in registry.parse_errors() {
        eprintln!("gage scanner list: {}\n{error}", path.display());
    }
    let mut rows: Vec<Row> = registry
        .list_visible()
        .into_iter()
        .map(|def| Row::found(def, &config))
        .collect();
    if rows.is_empty() {
        println!("No scanners found");
        println!("{}", style(SEARCH_HINT).dim());
        return Ok(());
    }
    rows.sort_by(|a, b| a.order().cmp(&b.order()));
    println!("{}", scanner_table(&rows));
    Ok(())
}

/// One line of `gage scanner list`.
#[derive(Debug, PartialEq, Eq)]
struct Row {
    name: String,
    version: String,
    /// Where the scanner is: `installed`, `user`, or `project`
    source: String,
    description: String,
    state: RowState,
}

/// What a listed scanner is to a scan. The order is the order of the
/// listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum RowState {
    /// Found, and the config enables it
    Enabled,
    /// Found, and the config disables it
    Disabled,
}

impl Row {
    /// A scanner the registry found.
    fn found(def: &ScannerDef, config: &Config) -> Self {
        Row {
            name: def.name.clone(),
            version: def.version.clone(),
            source: scanner_source(&def.path).to_string(),
            description: def.description.clone(),
            state: if config.is_scanner_enabled(&def.name) {
                RowState::Enabled
            } else {
                RowState::Disabled
            },
        }
    }

    /// Scanners that can be run first, then the rest, each by name.
    fn order(&self) -> (RowState, &str) {
        (self.state, &self.name)
    }

    fn cells(&self) -> Vec<String> {
        let name = match self.state {
            RowState::Enabled => self.name.clone(),
            RowState::Disabled => style(format!("{} (disabled)", self.name)).dim().to_string(),
        };
        vec![
            name,
            self.version.clone(),
            style(&self.source).dim().to_string(),
            style(&self.description).dim().to_string(),
        ]
    }
}

fn scanner_table(rows: &[Row]) -> Table {
    let header = ["Scanner", "Version", "Source", "Description"].map(String::from);
    let mut table =
        Table::from_iter(std::iter::once(header.to_vec()).chain(rows.iter().map(Row::cells)));
    table.with(Style::rounded());
    if let Some(width) = s::stdout_width() {
        table.with(
            Width::wrap(width)
                .keep_words(true)
                .priority(Priority::max(true)),
        );
    }
    table.modify(Rows::first(), s::tty(Color::FG_BRIGHT_YELLOW));
    table
}

/// The scanners of scanner source that match every term, with the
/// installed version of each.
fn search(args: ScannerSearchArgs) -> Result<(), CliError> {
    let config = Config::load_user().context("gage scanner search")?;
    let catalog = read_source("gage scanner search", &config)?;
    let installed = ScannerSet::installed();
    let rows: Vec<[String; 4]> = catalog
        .iter()
        .filter(|entry| entry.visible() && matches_terms(entry, &args.terms))
        .map(|entry| {
            let installed_version = installed
                .get(&entry.qualified())
                .map(|found| found.def.version.clone())
                .unwrap_or_default();
            [
                entry.qualified(),
                entry.def.version.clone(),
                installed_version,
                entry.def.description.clone(),
            ]
        })
        .collect();
    if rows.is_empty() {
        println!("No scanners found");
        return Ok(());
    }
    println!("{}", search_table(rows));
    Ok(())
}

/// Whether the scanner's qualified name or description has every
/// term, whatever the case. No term matches every scanner.
fn matches_terms(entry: &Entry, terms: &[String]) -> bool {
    let text = format!("{} {}", entry.qualified(), entry.def.description).to_lowercase();
    terms.iter().all(|term| text.contains(&term.to_lowercase()))
}

fn search_table(rows: Vec<[String; 4]>) -> Table {
    let header = ["Scanner", "Version", "Installed", "Description"].map(String::from);
    let mut table = Table::from_iter(std::iter::once(header).chain(rows));
    table.with(Style::rounded());
    if let Some(width) = s::stdout_width() {
        table.with(
            Width::wrap(width)
                .keep_words(true)
                .priority(Priority::max(true)),
        );
    }
    table
        .modify(Rows::first(), s::tty(Color::FG_BRIGHT_YELLOW))
        .modify(Columns::one(3).not(Rows::first()), s::dim());
    table
}

fn list_outdated(config: &Config) -> Result<(), CliError> {
    let catalog = read_source("gage scanner list", config)?;
    let installed = ScannerSet::installed();
    let plan = plan_upgrade(&catalog, &installed, &[])
        .map_err(|e| CliError::message(format!("gage scanner list: {e}")))?;
    let upgrades = plan.steps.iter().filter_map(|step| {
        let from = step.replaces.as_ref()?;
        Some([
            step.entry.def.name.clone(),
            from.clone(),
            step.entry.def.version.clone(),
            String::new(),
        ])
    });
    let held = plan.held.iter().filter_map(|(qualified, mismatch)| {
        let entry = catalog.get(qualified)?;
        let from = installed.get(qualified)?;
        Some([
            entry.def.name.clone(),
            from.def.version.clone(),
            entry.def.version.clone(),
            mismatch.to_string(),
        ])
    });
    let rows: Vec<[String; 4]> = upgrades.chain(held).collect();
    if rows.is_empty() {
        println!("All installed scanners are up to date");
        return Ok(());
    }
    let header = ["Scanner", "Installed", "Available", "Note"].map(String::from);
    let table = Table::from_iter(std::iter::once(header).chain(rows))
        .with(Style::rounded())
        .modify(Rows::first(), s::tty(Color::FG_BRIGHT_YELLOW))
        .modify(Columns::one(3).not(Rows::first()), s::dim())
        .to_string();
    println!("{table}");
    Ok(())
}

/// Bring the copy of the scanner source up to date and read it, for a
/// command that prints to the terminal. A scanner the source holds
/// that cannot be offered is reported on stderr.
fn read_source(command: &str, config: &Config) -> Result<ScannerSet, CliError> {
    let failed = |e: CatalogError| CliError::message(format!("{command}: {e}"));
    let _lock = catalog::lock().map_err(failed)?;
    let catalog = busy("Reading scanner source", || {
        catalog::fetch_catalog(&config.scanners.source)
    })
    .map_err(failed)?;
    for (path, reason) in &catalog.problems {
        eprintln!("{command}: {}: {reason}", shorten_home(path));
    }
    Ok(catalog)
}

fn show(args: ScannerShowArgs) -> Result<(), CliError> {
    let config = Config::load_user().context("gage scanner show")?;
    let registry = ScannerRegistry::load();
    let installed = ScannerSet::installed();
    // A registry name first, so the scanner shown is the one a scan
    // of that name runs
    if let Some(def) = registry.get_def(&args.scanner) {
        let namespace = installed
            .iter()
            .find(|e| e.def.path == def.path)
            .map(|e| e.namespace.as_str());
        println!("{}", attrs_table(&attrs(def, namespace, Shown::Found)));
        return Ok(());
    }
    if let Ok(entry) = installed.resolve(&args.scanner) {
        let attrs = attrs(&entry.def, Some(&entry.namespace), Shown::Found);
        println!("{}", attrs_table(&attrs));
        return Ok(());
    }
    let catalog = read_source("gage scanner show", &config)?;
    let entry = catalog
        .resolve(&args.scanner)
        .map_err(|e| CliError::message(format!("gage scanner show: {e}")))?;
    let attrs = attrs(&entry.def, Some(&entry.namespace), Shown::Available);
    println!("{}", attrs_table(&attrs));
    Ok(())
}

/// Whether a shown scanner is one Gage found or one of the source.
#[derive(Clone, Copy)]
enum Shown {
    Found,
    Available,
}

/// The attributes `gage scanner show` prints, in order. An attribute
/// the scanner does not declare is left out.
fn attrs(def: &ScannerDef, namespace: Option<&str>, shown: Shown) -> Vec<(&'static str, String)> {
    let source = match shown {
        Shown::Found => scanner_source(&def.path).to_string(),
        Shown::Available => "available".to_string(),
    };
    let api = match def.api {
        Some(level) => match check_api(level) {
            Ok(()) => level.to_string(),
            Err(mismatch) => format!("{level} ({mismatch})"),
        },
        None => String::new(),
    };
    let notes: Vec<&str> = def
        .tasks
        .values()
        .flat_map(|task| task.notes.writes.keys())
        .map(String::as_str)
        .collect();
    let issues: Vec<&str> = def
        .tasks
        .values()
        .flat_map(|task| task.issues.writes.keys())
        .map(String::as_str)
        .collect();
    let params: Vec<String> = def
        .params_def()
        .iter()
        .map(|(name, _)| name.to_string())
        .collect();
    // An installed scanner is its directory; any other is its file
    let path = match (shown, scanner_source(&def.path)) {
        (Shown::Available, _) => String::new(),
        (Shown::Found, DefSource::Installed) => shorten_home(&def.module_dir()),
        (Shown::Found, _) => shorten_home(&def.path),
    };
    let all = [
        ("name", def.name.clone()),
        ("namespace", namespace.unwrap_or_default().to_string()),
        ("version", def.version.clone()),
        ("api", api),
        ("source", source),
        ("description", def.description.clone()),
        ("requires", def.requires.join("\n")),
        ("groups", def.groups.join("\n")),
        (
            "tasks",
            def.tasks
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        ("notes", notes.join("\n")),
        ("issues", issues.join("\n")),
        ("params", params.join("\n")),
        ("path", path),
    ];
    all.into_iter()
        .filter(|(_, value)| !value.is_empty())
        .collect()
}

fn attrs_table(attrs: &[(&str, String)]) -> String {
    Table::from_iter(attrs.iter().map(|(k, v)| [k.to_string(), v.clone()]))
        .with(Style::rounded())
        .modify(Columns::first(), s::tty(Color::FG_BRIGHT_YELLOW))
        .to_string()
}

fn install(args: ScannerInstallArgs) -> Result<(), CliError> {
    let config = Config::load_user().context("gage scanner install")?;
    dialog::run("Install scanners", || {
        let _lock = catalog::lock().map_err(failed)?;
        let catalog = fetch_in_dialog(&config)?;
        let installed = ScannerSet::installed();
        let plan = plan_install(&catalog, &installed, &args.scanners).map_err(failed)?;
        for name in &plan.current {
            cli::log::remark(format!("{name} is already installed"))?;
        }
        if plan.steps.is_empty() {
            return Ok("Nothing to install".into());
        }
        dialog::step_list("Scanners", plan.steps.iter().map(step_line))?;
        dialog::confirm(args.yes, true)?;
        apply(&plan.steps)?;
        Ok(format!("Installed {}", count(plan.steps.len(), "scanner")).into())
    })
}

fn upgrade(args: ScannerUpgradeArgs) -> Result<(), CliError> {
    let config = Config::load_user().context("gage scanner upgrade")?;
    dialog::run("Upgrade scanners", || {
        let _lock = catalog::lock().map_err(failed)?;
        let catalog = fetch_in_dialog(&config)?;
        let installed = ScannerSet::installed();
        let plan = plan_upgrade(&catalog, &installed, &args.scanners).map_err(failed)?;
        for (name, mismatch) in &plan.held {
            cli::log::warning(format!("{name} is not upgraded: {mismatch}"))?;
        }
        if plan.steps.is_empty() {
            return Ok(upgrade_outro(0, 0, plan.held.len()));
        }
        dialog::step_list("Scanners", plan.steps.iter().map(step_line))?;
        dialog::confirm(args.yes, true)?;
        apply(&plan.steps)?;
        let upgraded = plan.steps.iter().filter(|s| s.replaces.is_some()).count();
        Ok(upgrade_outro(
            upgraded,
            plan.steps.len() - upgraded,
            plan.held.len(),
        ))
    })
}

/// The last line of an upgrade: how many scanners were upgraded, how
/// many were installed because an upgraded scanner requires them, and
/// whether any were left because this build cannot run them.
fn upgrade_outro(upgraded: usize, required: usize, held: usize) -> DialogResult {
    let mut text = match (upgraded, held) {
        (0, 0) => "All scanners are up to date".to_string(),
        (0, _) => "No scanners upgraded".to_string(),
        _ => format!("Upgraded {}", count(upgraded, "scanner")),
    };
    if required > 0 {
        text.push_str(&format!("; installed {} required", required));
    }
    text.into()
}

fn uninstall(args: ScannerUninstallArgs) -> Result<(), CliError> {
    dialog::run("Uninstall scanners", || {
        let _lock = catalog::lock().map_err(failed)?;
        let installed = ScannerSet::installed();
        let targets = plan_uninstall(&installed, &args.scanners).map_err(failed)?;
        dialog::step_list(
            "Scanners",
            targets
                .iter()
                .map(|t| format!("{} {}", t.qualified(), t.def.version)),
        )?;
        cli::log::remark("Notes and issues these scanners wrote stay in the store")?;
        dialog::confirm(args.yes, false)?;
        for target in &targets {
            catalog::uninstall(target).map_err(failed)?;
        }
        Ok(format!("Uninstalled {}", count(targets.len(), "scanner")).into())
    })
}

/// A catalog failure as the outro of a dialog.
fn failed(e: CatalogError) -> DialogError {
    DialogError::Failed(e.to_string())
}

/// Bring the copy of the scanner source up to date and read it, under
/// a spinner. A scanner the source holds that cannot be offered is
/// logged as a warning.
fn fetch_in_dialog(config: &Config) -> Result<ScannerSet, DialogError> {
    let spinner = cli::spinner();
    spinner.start("Reading scanner source");
    let fetched = catalog::fetch_catalog(&config.scanners.source);
    spinner.clear();
    let catalog = fetched.map_err(failed)?;
    for (path, reason) in &catalog.problems {
        cli::log::warning(format!("{}: {reason}", shorten_home(path)))?;
    }
    Ok(catalog)
}

/// A plan step as the dialog lists it: the scanner and its version,
/// the version it replaces, and the scanner that requires it.
fn step_line(step: &Step<'_>) -> String {
    let mut line = match &step.replaces {
        Some(from) => format!(
            "{} {from} -> {}",
            step.entry.qualified(),
            step.entry.def.version
        ),
        None => format!("{} {}", step.entry.qualified(), step.entry.def.version),
    };
    if let Some(parent) = &step.required_by {
        line.push_str(&format!(" (required by {parent})"));
    }
    line
}

fn apply(steps: &[Step<'_>]) -> Result<(), DialogError> {
    for step in steps {
        catalog::install(step.entry).map_err(failed)?;
    }
    Ok(())
}

/// Whether `gage init` offers to install scanners: none is installed.
pub(crate) fn none_installed() -> bool {
    ScannerSet::installed().is_empty()
}

/// Install every scanner of the Gage namespace from the scanner
/// source, for `gage init`. Returns the line that records the install.
/// A scanner this build cannot run is logged and left out.
pub(crate) fn install_gage_scanners(config: &Config) -> Result<String, DialogError> {
    let _lock = catalog::lock().map_err(failed)?;
    let catalog = fetch_in_dialog(config)?;
    let done = catalog::install_namespace(&catalog, catalog::GAGE_NAMESPACE).map_err(failed)?;
    for (name, mismatch) in &done.held {
        cli::log::warning(format!("{name} is not installed: {mismatch}"))?;
    }
    Ok(format!(
        "Installed {}",
        count(done.installed.len(), "scanner")
    ))
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use console::strip_ansi_codes;

    use super::*;

    fn write_scanner(root: &Path, qualified: &str, fields: &str) -> PathBuf {
        let name = qualified.rsplit('/').next().unwrap();
        let dir = root.join(qualified);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("scanner.rn"),
            format!(
                "pub const SCANNER = #{{ name: {name:?}, description: \"About {name}\", \
                 {fields} tasks: #{{ main: #{{ notes: #{{ writes: [\"n\"] }} }} }} }};\n\
                 pub async fn main() {{}}"
            ),
        )
        .unwrap();
        dir
    }

    #[test]
    fn a_step_names_the_version_change_and_the_requiring_scanner() {
        let tmp = tempfile::tempdir().unwrap();
        write_scanner(tmp.path(), "ns/one", "version: \"1.1.0\", api: 1,");
        let set = ScannerSet::read_installed(tmp.path());
        let entry = set.get("ns/one").unwrap();

        let step = |replaces: Option<&str>, required_by: Option<&str>| Step {
            entry,
            replaces: replaces.map(String::from),
            required_by: required_by.map(String::from),
        };
        assert_eq!(step_line(&step(None, None)), "ns/one 1.1.0");
        assert_eq!(
            step_line(&step(Some("1.0.0"), None)),
            "ns/one 1.0.0 -> 1.1.0"
        );
        assert_eq!(
            step_line(&step(None, Some("ns/parent"))),
            "ns/one 1.1.0 (required by ns/parent)"
        );
    }

    #[test]
    fn listed_scanners_that_can_run_come_first_and_a_disabled_one_is_marked() {
        let tmp = tempfile::tempdir().unwrap();
        write_scanner(tmp.path(), "ns/beta", "version: \"1.0.0\",");
        write_scanner(tmp.path(), "ns/alpha", "version: \"2.0.0\",");
        write_scanner(tmp.path(), "ns/off", "");
        let set = ScannerSet::read_installed(tmp.path());
        let mut config = Config::default();
        config.scanners.disable.push("off".to_string());

        let mut rows: Vec<Row> = set.iter().map(|e| Row::found(&e.def, &config)).collect();
        rows.sort_by(|a, b| a.order().cmp(&b.order()));

        let cells: Vec<Vec<String>> = rows
            .iter()
            .map(|row| {
                row.cells()
                    .iter()
                    .map(|cell| strip_ansi_codes(cell).to_string())
                    .collect()
            })
            .collect();
        assert_eq!(
            cells,
            [
                ["alpha", "2.0.0", "project", "About alpha"],
                ["beta", "1.0.0", "project", "About beta"],
                ["off (disabled)", "", "project", "About off"],
            ]
        );
    }

    #[test]
    fn a_search_matches_every_term_in_the_name_or_description() {
        let tmp = tempfile::tempdir().unwrap();
        write_scanner(tmp.path(), "ns/code-review", "version: \"1.0.0\", api: 1,");
        let set = ScannerSet::read_installed(tmp.path());
        let entry = set.get("ns/code-review").unwrap();
        let terms = |words: &[&str]| words.iter().map(|w| w.to_string()).collect::<Vec<_>>();

        assert!(matches_terms(entry, &[]));
        // The description is `About code-review`
        assert!(matches_terms(entry, &terms(&["REVIEW"])));
        assert!(matches_terms(entry, &terms(&["ns/code", "about"])));
        assert!(!matches_terms(entry, &terms(&["review", "missing"])));
    }

    #[test]
    fn show_leaves_out_what_a_scanner_does_not_declare() {
        let tmp = tempfile::tempdir().unwrap();
        write_scanner(
            tmp.path(),
            "ns/full",
            "version: \"1.0.0\", api: 1, requires: [\"ns/dep\"],",
        );
        write_scanner(tmp.path(), "ns/bare", "");
        let set = ScannerSet::read_installed(tmp.path());

        let full = attrs(
            &set.get("ns/full").unwrap().def,
            Some("ns"),
            Shown::Available,
        );
        let labels: Vec<&str> = full.iter().map(|(label, _)| *label).collect();
        assert_eq!(
            labels,
            [
                "name",
                "namespace",
                "version",
                "api",
                "source",
                "description",
                "requires",
                "tasks",
                "notes"
            ]
        );
        assert!(full.contains(&("source", "available".to_string())));
        assert!(full.contains(&("requires", "ns/dep".to_string())));

        let bare = attrs(&set.get("ns/bare").unwrap().def, None, Shown::Found);
        let labels: Vec<&str> = bare.iter().map(|(label, _)| *label).collect();
        assert_eq!(
            labels,
            ["name", "source", "description", "tasks", "notes", "path"]
        );
    }

    #[test]
    fn an_upgrade_reports_what_it_did_and_what_it_left() {
        let text = |result: DialogResult| match result {
            DialogResult::Message(text) => text,
        };
        assert_eq!(text(upgrade_outro(0, 0, 0)), "All scanners are up to date");
        assert_eq!(text(upgrade_outro(0, 0, 1)), "No scanners upgraded");
        assert_eq!(text(upgrade_outro(2, 0, 1)), "Upgraded 2 scanners");
        assert_eq!(
            text(upgrade_outro(1, 2, 0)),
            "Upgraded 1 scanner; installed 2 required"
        );
    }
}
