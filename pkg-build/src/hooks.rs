use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use pkg_core::{HookOperation, PackageManifest, PkgError, PkgResult};
use pkg_db::queries;
use pkg_sandbox::run_target_root_command;
use rusqlite::Connection;

use crate::cache::RECIPE_MANIFEST_NAME;

pub fn hooks_in_store(store_dir: &Path) -> PkgResult<BTreeSet<HookOperation>> {
    let path = store_dir.join(RECIPE_MANIFEST_NAME);
    if !path.is_file() {
        return Ok(BTreeSet::new());
    }
    let text = fs::read_to_string(&path).map_err(PkgError::Io)?;
    let manifest = PackageManifest::from_toml_str(&text)?;
    Ok(manifest.hooks.into_iter().collect())
}

/// Runs each requested operation once, records current failures, and returns
/// the failed operations. Package activation has already committed.
pub fn run_hooks(
    conn: &Connection,
    operations: &BTreeSet<HookOperation>,
) -> PkgResult<Vec<HookOperation>> {
    let mut failed = Vec::new();
    for operation in HookOperation::ALL
        .into_iter()
        .filter(|operation| operations.contains(operation))
    {
        if let Err(error) = run_one(conn, operation) {
            return Err(error);
        }
        if queries::list_hook_failures(conn)?
            .iter()
            .any(|row| row.operation == operation)
        {
            failed.push(operation);
        }
    }
    Ok(failed)
}

fn run_one(conn: &Connection, operation: HookOperation) -> PkgResult<()> {
    let root = pkg_core::paths::root();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    let relative_log = PathBuf::from(format!("var/log/pkg/hook-{operation}.log"));
    let log_path = root.join(&relative_log);
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent).map_err(PkgError::Io)?;
    }
    let scratch = root.join("var/lib/pkg/hooks/scratch");
    fs::create_dir_all(&scratch).map_err(PkgError::Io)?;
    for mount_point in ["proc", "dev", "tmp"] {
        fs::create_dir_all(root.join(mount_point)).map_err(PkgError::Io)?;
    }

    let (commands, writable) = operation_plan(root, operation)?;
    for path in &writable {
        fs::create_dir_all(root.join(path)).map_err(PkgError::Io)?;
    }
    let mut output = format!("operation: {operation}\nstarted-at: {now}\n");
    let mut failure = None;
    if commands.is_empty() {
        output.push_str("no applicable input directories; nothing to do\n");
    }
    for command in commands {
        if !root.join(command[0].trim_start_matches('/')).is_file() {
            failure = Some(format!(
                "required executable {} is not installed",
                command[0]
            ));
            output.push_str(&format!(
                "error: {}\n",
                failure.as_deref().unwrap_or_default()
            ));
            break;
        }
        match run_target_root_command(root, &scratch, &writable, &command) {
            Ok(outcome) => {
                output.push_str(&format!("command: {}\n", command.join(" ")));
                output.push_str(&outcome.stdout);
                output.push_str(&outcome.stderr);
                if !outcome.status.success() {
                    failure = Some(format!(
                        "{} exited with {}; {}",
                        command[0], outcome.status, outcome.stderr
                    ));
                    break;
                }
            }
            Err(error) => {
                failure = Some(error.to_string());
                output.push_str(&format!("error: {error}\n"));
                break;
            }
        }
    }
    fs::write(&log_path, output).map_err(PkgError::Io)?;
    let root_relative_log = format!("/{}", relative_log.display());
    match failure {
        None => {
            queries::clear_hook_failure(conn, operation)?;
            eprintln!("pkg: hook {operation}: done");
        }
        Some(error) => {
            let summary = bounded_summary(&error, 4000);
            queries::upsert_hook_failure(conn, operation, now, &root_relative_log, &summary)?;
            eprintln!(
                "pkg: warning: hook {operation} failed after activation: {summary}\n\
                 The package state remains committed. Log: {root_relative_log}; retry: pkg hooks retry"
            );
        }
    }
    Ok(())
}

fn operation_plan(
    root: &Path,
    operation: HookOperation,
) -> PkgResult<(Vec<Vec<String>>, Vec<PathBuf>)> {
    match operation {
        HookOperation::Ldconfig => Ok((
            vec![vec!["/sbin/ldconfig".into()]],
            vec![PathBuf::from("etc")],
        )),
        HookOperation::FontCache => Ok((
            vec![vec!["/usr/bin/fc-cache".into(), "-f".into()]],
            vec![PathBuf::from("var/cache/fontconfig")],
        )),
        HookOperation::MimeCache => Ok((
            vec![vec![
                "/usr/bin/update-mime-database".into(),
                "/usr/share/mime".into(),
            ]],
            vec![PathBuf::from("usr/share/mime")],
        )),
        HookOperation::DesktopDatabase => Ok((
            vec![vec![
                "/usr/bin/update-desktop-database".into(),
                "/usr/share/applications".into(),
            ]],
            vec![PathBuf::from("usr/share/applications")],
        )),
        HookOperation::GlibSchemas => Ok((
            vec![vec![
                "/usr/bin/glib-compile-schemas".into(),
                "/usr/share/glib-2.0/schemas".into(),
            ]],
            vec![PathBuf::from("usr/share/glib-2.0/schemas")],
        )),
        HookOperation::IconCache => {
            let icons = root.join("usr/share/icons");
            let mut themes = Vec::new();
            if icons.is_dir() {
                for entry in fs::read_dir(icons).map_err(PkgError::Io)? {
                    let entry = entry.map_err(PkgError::Io)?;
                    if entry.path().join("index.theme").is_file() {
                        themes.push(entry.file_name().to_string_lossy().into_owned());
                    }
                }
            }
            themes.sort();
            let commands = themes
                .iter()
                .map(|theme| {
                    vec![
                        "/usr/bin/gtk-update-icon-cache".into(),
                        "-f".into(),
                        "-t".into(),
                        format!("/usr/share/icons/{theme}"),
                    ]
                })
                .collect();
            let writable = themes
                .into_iter()
                .map(|theme| PathBuf::from("usr/share/icons").join(theme))
                .collect();
            Ok((commands, writable))
        }
    }
}

fn bounded_summary(message: &str, cap: usize) -> String {
    if message.len() <= cap {
        return message.to_string();
    }
    let start = message.len() - cap;
    let boundary = message
        .char_indices()
        .map(|(index, _)| index)
        .find(|index| *index >= start)
        .unwrap_or(message.len());
    message[boundary..].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_order_is_fixed() {
        let requested = [
            HookOperation::GlibSchemas,
            HookOperation::IconCache,
            HookOperation::DesktopDatabase,
            HookOperation::FontCache,
        ]
        .into_iter()
        .collect::<BTreeSet<_>>();
        let ordered: Vec<_> = HookOperation::ALL
            .into_iter()
            .filter(|operation| requested.contains(operation))
            .collect();
        assert_eq!(
            ordered,
            [
                HookOperation::FontCache,
                HookOperation::IconCache,
                HookOperation::DesktopDatabase,
                HookOperation::GlibSchemas,
            ]
        );
    }

    #[test]
    fn desktop_and_schema_hooks_have_fixed_target_root_plans() {
        let (commands, writable) =
            operation_plan(Path::new("/target"), HookOperation::DesktopDatabase).unwrap();
        assert_eq!(
            commands,
            [vec![
                "/usr/bin/update-desktop-database".to_string(),
                "/usr/share/applications".to_string(),
            ]]
        );
        assert_eq!(writable, [PathBuf::from("usr/share/applications")]);

        let (commands, writable) =
            operation_plan(Path::new("/target"), HookOperation::GlibSchemas).unwrap();
        assert_eq!(
            commands,
            [vec![
                "/usr/bin/glib-compile-schemas".to_string(),
                "/usr/share/glib-2.0/schemas".to_string(),
            ]]
        );
        assert_eq!(writable, [PathBuf::from("usr/share/glib-2.0/schemas")]);
    }

    #[test]
    fn summaries_stay_bounded_at_utf8_boundaries() {
        assert_eq!(bounded_summary("short", 10), "short");
        let long = format!("{}tail", "é".repeat(2000));
        let summary = bounded_summary(&long, 100);
        assert!(summary.len() <= 102);
        assert!(summary.ends_with("tail"));
    }

    #[test]
    fn missing_icon_themes_are_a_noop() {
        let root = tempfile::tempdir().unwrap();
        let (commands, writable) = operation_plan(root.path(), HookOperation::IconCache).unwrap();
        assert!(commands.is_empty());
        assert!(writable.is_empty());
    }
}
