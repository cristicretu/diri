//! `dirijor sync` and `dirijor migrate`: portable profile sync and moving
//! every session to another Mac. The Engine does the work; this is its
//! command line.

use std::io::{IsTerminal, Read};
use std::time::Duration;

use diri_proto::Method;
use diri_proto::portable::{
    MigrateExportResult, MigrateImportPlan, PlannedAction, ProjectMatch, SyncStatus,
};
use serde_json::{Value, json};

use super::CliError;

/// Exports, imports and Git syncs touch every transcript or the network.
const TIMEOUT: Duration = Duration::from_secs(600);

pub(crate) const SYNC_HELP: &str = "dirijor sync status [--json]           where the profile syncs and how the last sync went\n  \
dirijor sync init --git URL|--folder PATH [--auto]\n                                        sync notes, App Skills, recipes, schedules, preferences and init.lua\n                                        with a private Git repository or a synced folder (never credentials)\n  \
dirijor sync now [--json]              sync once, both ways\n  \
dirijor sync off                       stop syncing (the synced copy is left as it is)";

pub(crate) const MIGRATE_HELP: &str = "dirijor migrate export PATH [--passphrase-file FILE|-] [--no-transcripts] [--json]\n                                        save the profile, every session and its Claude Code / Codex\n                                        conversation into one .diribundle (PATH may be a folder)\n  \
dirijor migrate import PATH [--dry-run] [--map OLD=NEW]... [--passphrase-file FILE|-] [--json]\n                                        bring a bundle onto this Mac: paths follow the new home, projects are\n                                        found by git origin, sessions already here are kept; --dry-run only plans\n  \
The passphrase can also come from DIRI_BUNDLE_PASSPHRASE. It is never accepted on the command line.";

struct Flags {
    positional: Vec<String>,
    json: bool,
    dry_run: bool,
    auto: bool,
    no_transcripts: bool,
    git: Option<String>,
    folder: Option<String>,
    passphrase_file: Option<String>,
    maps: Vec<(String, String)>,
}

fn parse(arguments: &[String]) -> Result<Flags, CliError> {
    let mut flags = Flags {
        positional: Vec::new(),
        json: false,
        dry_run: false,
        auto: false,
        no_transcripts: false,
        git: None,
        folder: None,
        passphrase_file: None,
        maps: Vec::new(),
    };
    let mut iter = arguments.iter();
    while let Some(argument) = iter.next() {
        let mut value = |name: &str| {
            iter.next()
                .cloned()
                .ok_or_else(|| CliError::failure(format!("{name} needs a value")))
        };
        match argument.as_str() {
            "--json" => flags.json = true,
            "--dry-run" | "-n" => flags.dry_run = true,
            "--auto" => flags.auto = true,
            "--no-transcripts" => flags.no_transcripts = true,
            "--git" => flags.git = Some(value("--git")?),
            "--folder" => flags.folder = Some(value("--folder")?),
            "--passphrase-file" => flags.passphrase_file = Some(value("--passphrase-file")?),
            "--passphrase" => {
                return Err(CliError::failure(
                    "a passphrase on the command line is visible to other processes; use --passphrase-file or DIRI_BUNDLE_PASSPHRASE",
                ));
            }
            "--map" => {
                let rule = value("--map")?;
                let (from, to) = rule
                    .split_once('=')
                    .filter(|(from, to)| !from.is_empty() && !to.is_empty())
                    .ok_or_else(|| CliError::failure("--map needs OLD=NEW"))?;
                flags.maps.push((from.to_owned(), to.to_owned()));
            }
            other if other.starts_with("--") => {
                return Err(CliError::failure(format!("unknown option {other}")));
            }
            other => flags.positional.push(other.to_owned()),
        }
    }
    Ok(flags)
}

fn passphrase(flags: &Flags) -> Result<Option<String>, CliError> {
    let read = match flags.passphrase_file.as_deref() {
        Some("-") => {
            let mut text = String::new();
            if std::io::stdin().is_terminal() {
                eprint!("Passphrase: ");
            }
            std::io::stdin()
                .read_to_string(&mut text)
                .map_err(|e| CliError::failure(format!("cannot read the passphrase: {e}")))?;
            Some(text)
        }
        Some(path) => Some(
            std::fs::read_to_string(path)
                .map_err(|e| CliError::failure(format!("cannot read {path}: {e}")))?,
        ),
        None => std::env::var("DIRI_BUNDLE_PASSPHRASE").ok(),
    };
    Ok(read
        .map(|text| text.trim_end_matches(['\n', '\r']).to_owned())
        .filter(|text| !text.is_empty()))
}

fn absolute(path: &str) -> String {
    if path.starts_with('/') || path.starts_with('~') {
        return path.to_owned();
    }
    std::env::current_dir()
        .map(|dir| dir.join(path).to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_owned())
}

fn print_json(value: &Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).unwrap_or_default()
    );
}

pub(crate) fn sync(arguments: &[String]) -> Result<(), CliError> {
    let Some(command) = arguments.first().map(String::as_str) else {
        println!("{SYNC_HELP}");
        return Ok(());
    };
    let flags = parse(&arguments[1..])?;
    let status = match command {
        "help" | "--help" | "-h" => {
            println!("{SYNC_HELP}");
            return Ok(());
        }
        "status" => super::request(Method::SYNC_STATUS, json!({}), TIMEOUT)?,
        "init" | "setup" => {
            let (backend, location) = match (&flags.git, &flags.folder) {
                (Some(url), None) => ("git", url.clone()),
                (None, Some(folder)) => ("folder", absolute(folder)),
                _ => {
                    return Err(CliError::failure(
                        "choose one of --git URL or --folder PATH",
                    ));
                }
            };
            super::request(
                Method::SYNC_CONFIGURE,
                json!({"config": {"backend": backend, "location": location, "auto": flags.auto}}),
                TIMEOUT,
            )?;
            super::request(Method::SYNC_NOW, json!({}), TIMEOUT)?
        }
        "now" => super::request(Method::SYNC_NOW, json!({}), TIMEOUT)?,
        "off" => super::request(Method::SYNC_CONFIGURE, json!({"config": null}), TIMEOUT)?,
        other => {
            return Err(CliError::failure(format!(
                "unknown sync command {other}\n{SYNC_HELP}"
            )));
        }
    };
    if flags.json {
        print_json(&status);
        return Ok(());
    }
    let status: SyncStatus = serde_json::from_value(status)
        .map_err(|e| CliError::failure(format!("unexpected answer: {e}")))?;
    print!("{}", describe_sync(&status));
    match status.last_error {
        Some(error) if command != "status" => Err(CliError::failure(error)),
        _ => Ok(()),
    }
}

fn describe_sync(status: &SyncStatus) -> String {
    let Some(config) = &status.config else {
        return "Sync is off.\n".to_owned();
    };
    let mut out = format!(
        "{} {}{}\n",
        match config.backend {
            diri_proto::portable::SyncBackendKind::Git => "Git",
            diri_proto::portable::SyncBackendKind::Folder => "Folder",
        },
        config.location,
        if config.auto { " (automatic)" } else { "" }
    );
    if let Some(summary) = &status.last_summary {
        out.push_str(&format!(
            "Last sync: {} in, {} out{}{}\n",
            summary.pulled,
            summary.pushed,
            if summary.deleted > 0 {
                format!(", {} removed", summary.deleted)
            } else {
                String::new()
            },
            summary
                .commit
                .as_deref()
                .map(|c| format!(", commit {c}"))
                .unwrap_or_default()
        ));
        for conflict in &summary.conflicts {
            out.push_str(&format!(
                "  kept this Mac's {conflict} (the other copy is in conflicts/)\n"
            ));
        }
        if summary.prefs_pending {
            out.push_str("Preferences arrived; Diri applies them when it is open.\n");
        }
    } else {
        out.push_str("Not synced yet.\n");
    }
    if let Some(error) = &status.last_error {
        out.push_str(&format!("Last sync failed: {error}\n"));
    }
    out
}

pub(crate) fn migrate(arguments: &[String]) -> Result<(), CliError> {
    let Some(command) = arguments.first().map(String::as_str) else {
        println!("{MIGRATE_HELP}");
        return Ok(());
    };
    let flags = parse(&arguments[1..])?;
    let path = || {
        flags
            .positional
            .first()
            .map(|p| absolute(p))
            .ok_or_else(|| CliError::failure(format!("which file?\n{MIGRATE_HELP}")))
    };
    match command {
        "help" | "--help" | "-h" => {
            println!("{MIGRATE_HELP}");
            Ok(())
        }
        "export" => {
            let mut params = json!({"path": path()?, "skipTranscripts": flags.no_transcripts});
            if let Some(passphrase) = passphrase(&flags)? {
                params["passphrase"] = json!(passphrase);
            }
            let result = super::request(Method::MIGRATE_EXPORT, params, TIMEOUT)?;
            if flags.json {
                print_json(&result);
                return Ok(());
            }
            let result: MigrateExportResult = serde_json::from_value(result)
                .map_err(|e| CliError::failure(format!("unexpected answer: {e}")))?;
            println!(
                "Saved {} sessions ({} conversations), {} notes, {} skills, {} recipes, {} schedules{}\n  {}",
                result.counts.sessions,
                result.counts.transcripts,
                result.counts.notes,
                result.counts.skills,
                result.counts.recipes,
                result.counts.schedules,
                if result.encrypted { ", encrypted" } else { "" },
                result.path
            );
            for item in &result.skipped {
                println!("  left out {}: {}", item.item, item.reason);
            }
            Ok(())
        }
        "import" => {
            let mut params = json!({
                "path": path()?,
                "dryRun": flags.dry_run,
                "maps": flags.maps.iter().map(|(from, to)| json!({"from": from, "to": to})).collect::<Vec<_>>(),
            });
            if let Some(passphrase) = passphrase(&flags)? {
                params["passphrase"] = json!(passphrase);
            }
            let plan = super::request(Method::MIGRATE_IMPORT, params, TIMEOUT)?;
            if flags.json {
                print_json(&plan);
                return Ok(());
            }
            let plan: MigrateImportPlan = serde_json::from_value(plan)
                .map_err(|e| CliError::failure(format!("unexpected answer: {e}")))?;
            print!("{}", describe_plan(&plan));
            Ok(())
        }
        other => Err(CliError::failure(format!(
            "unknown migrate command {other}\n{MIGRATE_HELP}"
        ))),
    }
}

fn describe_plan(plan: &MigrateImportPlan) -> String {
    let verb = if plan.dry_run { "Would add" } else { "Added" };
    let mut out = format!(
        "From {} ({})\n{verb} {} sessions, {} conversations, {} notes, {} skills, {} recipes, {} schedules\n",
        plan.source.device,
        plan.source.home,
        plan.added.sessions,
        plan.added.transcripts,
        plan.added.notes,
        plan.added.skills,
        plan.added.recipes,
        plan.added.schedules,
    );
    if plan.updated > 0 || plan.kept > 0 {
        out.push_str(&format!(
            "{} updated from newer copies, {} already here and kept\n",
            plan.updated, plan.kept
        ));
    }
    if !plan.projects.is_empty() {
        out.push_str("\nProjects\n");
        for project in &plan.projects {
            let how = match project.how {
                ProjectMatch::Same => "same path",
                ProjectMatch::Mapped => "mapped",
                ProjectMatch::Origin => "found by origin",
                ProjectMatch::Unresolved => "not found — pass --map",
            };
            out.push_str(&format!(
                "  {} -> {}  ({how}, {} sessions)\n",
                project.from, project.to, project.sessions
            ));
        }
    }
    let notes: Vec<_> = plan
        .sessions
        .iter()
        .filter(|s| s.action != PlannedAction::Keep && s.note.is_some())
        .collect();
    if !notes.is_empty() {
        out.push('\n');
        for session in notes {
            out.push_str(&format!(
                "  “{}”: {}\n",
                session.title,
                session.note.as_deref().unwrap_or_default()
            ));
        }
    }
    for conflict in &plan.conflicts {
        out.push_str(&format!("  kept this Mac's {conflict}\n"));
    }
    if plan.prefs_pending {
        out.push_str("Preferences arrived; Diri applies them when it is open.\n");
    }
    if plan.dry_run {
        out.push_str("\nNothing changed. Run again without --dry-run to import.\n");
    }
    out
}
