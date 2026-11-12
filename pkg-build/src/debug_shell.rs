use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use pkg_core::{PkgError, PkgResult};
use pkg_sandbox::{run_sandboxed_interactive_with_binds, SandboxProfile};

use crate::recipe::{
    build_env, prepare_recipe_workspace, run_recipe_phase, BuildContext, Phase, PreparedRecipe,
    Recipe, PHASES,
};

const SESSION_FORMAT: u32 = 1;
const SESSION_FILE: &str = "session.json";
const EDITABLE_RECIPE: &str = ".pkg-debug-recipe";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct SessionDep {
    name: String,
    store_dir: PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
struct SessionState {
    format: u32,
    name: String,
    package_version: String,
    recipe_commit: String,
    selection: std::collections::BTreeMap<String, bool>,
    gate_deps: Vec<SessionDep>,
    build_deps: Vec<SessionDep>,
    work_dir: PathBuf,
    completed: Vec<Phase>,
}

pub struct DebugShellResult {
    pub workspace: PathBuf,
    pub exported: Vec<PathBuf>,
}

pub fn remove_debug_workspace(debug_root: &Path, name: &str) -> PkgResult<PathBuf> {
    validate_package_name(name)?;
    let workspace = debug_root.join(name);
    if !workspace.is_dir() {
        return Err(PkgError::NotFound(format!(
            "no debug workspace for {name}; expected {}",
            workspace.display()
        )));
    }
    fs::remove_dir_all(&workspace).map_err(PkgError::Io)?;
    Ok(workspace)
}

pub fn run_debug_shell(
    original_recipe: &Recipe,
    ctx: &BuildContext,
    debug_root: &Path,
    requested_phase: Option<Phase>,
    resume: bool,
    export_dir: Option<&Path>,
) -> PkgResult<DebugShellResult> {
    validate_package_name(&original_recipe.manifest.name)?;
    if requested_phase == Some(Phase::Fetch) {
        return Err(PkgError::Parse(
            "debug-shell phases are prepare, compile, check, or install".into(),
        ));
    }
    let workspace = debug_root.join(&original_recipe.manifest.name);
    let mut state = if resume {
        load_state(&workspace)?
    } else {
        if workspace.exists() {
            return Err(PkgError::Build(format!(
                "debug workspace {} already exists; use --resume or --remove-tree",
                workspace.display()
            )));
        }
        create_workspace(original_recipe, ctx, &workspace)?
    };
    validate_state(original_recipe, ctx, &state)?;

    let recipe_dir = ctx.scratch_dir.join(EDITABLE_RECIPE);
    let mut recipe = Recipe::load(&recipe_dir, original_recipe.recipe_commit.clone())?;
    recipe.local_manifest = original_recipe.local_manifest.clone();
    let prepared = PreparedRecipe {
        work_dir: ctx.scratch_dir.join(&state.work_dir),
        env: build_env(ctx, &recipe)?,
        dep_binds: dependency_binds(ctx),
    };
    if !prepared.work_dir.is_dir() {
        return Err(PkgError::Build(format!(
            "debug source directory {} is missing; remove and recreate the workspace",
            prepared.work_dir.display()
        )));
    }

    let target = requested_phase.unwrap_or(if resume {
        Phase::Install
    } else {
        Phase::Prepare
    });
    if resume && requested_phase.is_none() {
        // A plain resume enters the retained tree without advancing phases.
    } else {
        advance_phases(&recipe, ctx, &prepared, &mut state, target)?;
        save_state(&workspace, &state)?;
    }

    let mut env = prepared.env.clone();
    env.push(("PKG_DEBUG_RECIPE".into(), recipe_dir.display().to_string()));
    env.push((
        "PS1".into(),
        format!("(pkg-debug:{}) \\w \\$ ", original_recipe.manifest.name),
    ));
    if let Ok(term) = std::env::var("TERM") {
        env.push(("TERM".into(), term));
    }
    eprintln!(
        "Entering debug workspace for {} {}. Changes are not package artifacts.\nEditable recipe: {}\nThe workspace is retained on exit.",
        recipe.manifest.name, recipe.manifest.version, recipe_dir.display()
    );
    let outcome = run_sandboxed_interactive_with_binds(
        &SandboxProfile::Build,
        &prepared.dep_binds,
        &["/bin/sh".into()],
        &ctx.scratch_dir,
        &prepared.work_dir,
        &env,
    )?;
    if !outcome.success() {
        eprintln!("pkg: debug shell exited with {}", outcome.status);
    }
    let exported = match export_dir {
        Some(dir) => export_changes(&workspace, &state, dir)?,
        None => Vec::new(),
    };
    Ok(DebugShellResult {
        workspace: ctx.scratch_dir.clone(),
        exported,
    })
}

fn create_workspace(
    original_recipe: &Recipe,
    ctx: &BuildContext,
    workspace: &Path,
) -> PkgResult<SessionState> {
    preflight_dependencies(ctx)?;
    fs::create_dir_all(workspace).map_err(PkgError::Io)?;
    let result = (|| {
        let prepared = prepare_recipe_workspace(original_recipe, ctx)?;
        let recipe_copy = ctx.scratch_dir.join(EDITABLE_RECIPE);
        copy_recipe(&original_recipe.dir, &recipe_copy)?;
        let baseline = workspace.join("baseline");
        copy_tree(&prepared.work_dir, &baseline.join("source"))?;
        copy_tree(&recipe_copy, &baseline.join("recipe"))?;
        let relative = prepared
            .work_dir
            .strip_prefix(&ctx.scratch_dir)
            .map_err(|_| PkgError::Build("debug source directory escaped scratch".into()))?
            .to_path_buf();
        let state = SessionState {
            format: SESSION_FORMAT,
            name: original_recipe.manifest.name.clone(),
            package_version: original_recipe.manifest.version.to_string(),
            recipe_commit: original_recipe.recipe_commit.clone(),
            selection: ctx.selection.clone(),
            gate_deps: session_deps(&ctx.gate_deps),
            build_deps: session_deps(&ctx.build_deps),
            work_dir: relative,
            completed: Vec::new(),
        };
        save_state(workspace, &state)?;
        Ok(state)
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(workspace);
    }
    result
}

fn advance_phases(
    recipe: &Recipe,
    ctx: &BuildContext,
    prepared: &PreparedRecipe,
    state: &mut SessionState,
    target: Phase,
) -> PkgResult<()> {
    if state.completed.iter().any(|phase| *phase >= target) {
        return Err(PkgError::Build(format!(
            "debug workspace already reached {target}; use the retained state or recreate it"
        )));
    }
    for phase in PHASES.into_iter().filter(|phase| *phase < target) {
        if state.completed.contains(&phase) {
            continue;
        }
        eprintln!("pkg: debug-shell: running {phase} phase");
        let outcome = run_recipe_phase(recipe, ctx, prepared, phase);
        let workspace = ctx
            .scratch_dir
            .parent()
            .unwrap_or(ctx.scratch_dir.as_path());
        let logs = workspace.join("logs");
        fs::create_dir_all(&logs).map_err(PkgError::Io)?;
        let details = match &outcome {
            Ok(Some(tail)) => tail.clone(),
            Ok(None) => "phase had no script; completed without running a command\n".into(),
            Err(error) => format!("{error}\n"),
        };
        fs::write(logs.join(format!("{phase}.log")), details).map_err(PkgError::Io)?;
        outcome?;
        state.completed.push(phase);
        save_state(workspace, state)?;
    }
    Ok(())
}

fn validate_state(recipe: &Recipe, ctx: &BuildContext, state: &SessionState) -> PkgResult<()> {
    let matches = state.format == SESSION_FORMAT
        && state.name == recipe.manifest.name
        && state.package_version == recipe.manifest.version.to_string()
        && state.recipe_commit == recipe.recipe_commit
        && state.selection == ctx.selection
        && state.gate_deps == session_deps(&ctx.gate_deps)
        && state.build_deps == session_deps(&ctx.build_deps);
    if !matches {
        return Err(PkgError::Build(
            "debug workspace inputs changed; use --remove-tree and create a fresh workspace".into(),
        ));
    }
    if state.work_dir.is_absolute()
        || state
            .work_dir
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return Err(PkgError::Build(
            "debug workspace contains an unsafe source path".into(),
        ));
    }
    preflight_dependencies(ctx)
}

fn preflight_dependencies(ctx: &BuildContext) -> PkgResult<()> {
    let missing: Vec<String> = ctx
        .gate_deps
        .iter()
        .chain(ctx.build_deps.iter())
        .filter(|dep| !dep.store_dir.is_dir())
        .map(|dep| format!("{} ({})", dep.name, dep.store_dir.display()))
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(PkgError::NotFound(format!(
            "debug shell requires installed dependencies:\n  {}",
            missing.join("\n  ")
        )))
    }
}

fn dependency_binds(ctx: &BuildContext) -> Vec<(PathBuf, PathBuf)> {
    ctx.gate_deps
        .iter()
        .chain(ctx.build_deps.iter())
        .flat_map(|dep| {
            [
                (dep.store_dir.clone(), dep.store_dir.clone()),
                (
                    dep.store_dir.clone(),
                    PathBuf::from(format!("/pkg/deps/{}", dep.name)),
                ),
            ]
        })
        .collect()
}

fn session_deps(deps: &[pkg_scanner::DeclaredDep]) -> Vec<SessionDep> {
    deps.iter()
        .map(|dep| SessionDep {
            name: dep.name.clone(),
            store_dir: dep.store_dir.clone(),
        })
        .collect()
}

fn load_state(workspace: &Path) -> PkgResult<SessionState> {
    let path = workspace.join(SESSION_FILE);
    let text = fs::read_to_string(&path).map_err(|e| {
        PkgError::NotFound(format!(
            "cannot resume debug workspace {}: {e}",
            workspace.display()
        ))
    })?;
    serde_json::from_str(&text)
        .map_err(|e| PkgError::Parse(format!("invalid {}: {e}", path.display())))
}

fn save_state(workspace: &Path, state: &SessionState) -> PkgResult<()> {
    let data = serde_json::to_vec_pretty(state)
        .map_err(|e| PkgError::Build(format!("serializing debug session: {e}")))?;
    let temporary = workspace.join(".session.json.tmp");
    fs::write(&temporary, data).map_err(PkgError::Io)?;
    fs::rename(temporary, workspace.join(SESSION_FILE)).map_err(PkgError::Io)
}

fn export_changes(
    workspace: &Path,
    state: &SessionState,
    destination: &Path,
) -> PkgResult<Vec<PathBuf>> {
    fs::create_dir_all(destination).map_err(PkgError::Io)?;
    let source_relative = if state.work_dir.as_os_str().is_empty() {
        PathBuf::from("scratch")
    } else {
        PathBuf::from("scratch").join(&state.work_dir)
    };
    let mut written = Vec::new();
    for (kind, baseline, current) in [
        ("source", PathBuf::from("baseline/source"), source_relative),
        (
            "recipe",
            PathBuf::from("baseline/recipe"),
            PathBuf::from("scratch").join(EDITABLE_RECIPE),
        ),
    ] {
        let output = Command::new("diff")
            .current_dir(workspace)
            .args([
                "-ruN",
                "--exclude=.pkg-phase-*",
                "--exclude=.pkg-debug-recipe",
            ])
            .arg(&baseline)
            .arg(&current)
            .output()
            .map_err(PkgError::Io)?;
        if !output.status.success() && output.status.code() != Some(1) {
            return Err(PkgError::Build(format!(
                "diff failed while exporting {kind}: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }
        if output.stdout.is_empty() {
            continue;
        }
        let old_prefix = format!("{}/", baseline.display());
        let new_prefix = format!("{}/", current.display());
        let mut patch = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|line| {
                if let Some(rest) = line.strip_prefix("--- ") {
                    format!("--- {}", rest.replacen(&old_prefix, "a/", 1))
                } else if let Some(rest) = line.strip_prefix("+++ ") {
                    format!("+++ {}", rest.replacen(&new_prefix, "b/", 1))
                } else {
                    line.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        patch.push('\n');
        let path = destination.join(format!("{}-{kind}.patch", state.name));
        fs::write(&path, patch).map_err(PkgError::Io)?;
        written.push(path);
    }
    Ok(written)
}

fn copy_recipe(source: &Path, destination: &Path) -> PkgResult<()> {
    fs::create_dir_all(destination).map_err(PkgError::Io)?;
    for entry in fs::read_dir(source).map_err(PkgError::Io)? {
        let entry = entry.map_err(PkgError::Io)?;
        if entry.file_name() == ".git" {
            continue;
        }
        let target = destination.join(entry.file_name());
        if entry.file_type().map_err(PkgError::Io)?.is_dir() {
            copy_recipe(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target).map_err(PkgError::Io)?;
        }
    }
    Ok(())
}

fn copy_tree(source: &Path, destination: &Path) -> PkgResult<()> {
    fs::create_dir_all(destination).map_err(PkgError::Io)?;
    for entry in fs::read_dir(source).map_err(PkgError::Io)? {
        let entry = entry.map_err(PkgError::Io)?;
        let target = destination.join(entry.file_name());
        let file_type = entry.file_type().map_err(PkgError::Io)?;
        if file_type.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else if file_type.is_symlink() {
            let link = fs::read_link(entry.path()).map_err(PkgError::Io)?;
            std::os::unix::fs::symlink(link, target).map_err(PkgError::Io)?;
        } else {
            fs::copy(entry.path(), target).map_err(PkgError::Io)?;
        }
    }
    Ok(())
}

fn validate_package_name(name: &str) -> PkgResult<()> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '+' | '.'))
    {
        return Err(PkgError::Parse(format!(
            "unsafe package name for debug workspace: {name:?}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_names_cannot_escape_the_debug_root() {
        assert!(validate_package_name("gtk3").is_ok());
        assert!(validate_package_name("../root").is_err());
        assert!(validate_package_name("").is_err());
    }

    #[test]
    fn state_rejects_paths_outside_scratch() {
        let state: SessionState = serde_json::from_value(serde_json::json!({
            "format": 1,
            "name": "hello",
            "package_version": "1",
            "recipe_commit": "abc",
            "selection": {},
            "gate_deps": [],
            "build_deps": [],
            "work_dir": "../outside",
            "completed": []
        }))
        .unwrap();
        assert!(state
            .work_dir
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir)));
    }
}
