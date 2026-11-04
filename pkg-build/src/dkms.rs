use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use pkg_core::manifest::PackageManifest;
use pkg_core::paths;
use pkg_core::{Arch, Identity, PkgResult};
use pkg_db::queries;
use pkg_sandbox::{run_sandboxed_with_binds, SandboxProfile};
use rusqlite::Connection;

use crate::cache;
use crate::orchestrator::{Report, SchedConfig};
use crate::staging;

#[derive(Debug, Clone, PartialEq, Eq)]
struct DkmsPackage {
    package: String,
    module: String,
    version: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DkmsTarget {
    package: DkmsPackage,
    kernel_release: String,
}

pub fn installed_kernels(root: &Path) -> PkgResult<BTreeSet<String>> {
    let modules = root.join("usr/lib/modules");
    if !modules.is_dir() {
        return Ok(BTreeSet::new());
    }
    let mut kernels = BTreeSet::new();
    for entry in fs::read_dir(modules)? {
        let entry = entry?;
        let dir = entry.path();
        if !entry.file_type()?.is_dir() || !dir.join("build/Makefile").is_file() {
            continue;
        }
        let Some(release) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if valid_component(&release) {
            kernels.insert(release);
        }
    }
    Ok(kernels)
}

fn installed_dkms_packages(conn: &Connection, cfg: &SchedConfig) -> Vec<DkmsPackage> {
    let rows = match queries::list_installed_packages(conn) {
        Ok(rows) => rows,
        Err(e) => {
            eprintln!("warning: cannot enumerate installed DKMS packages: {e}");
            return Vec::new();
        }
    };
    let mut packages = Vec::new();
    for row in rows.into_iter().filter(|row| row.arch == Arch::Amd64) {
        let opt_tag = match row.opt_tag.parse() {
            Ok(tag) => tag,
            Err(e) => {
                eprintln!(
                    "warning: bad opt_tag in installed package {}: {e}",
                    row.name
                );
                continue;
            }
        };
        let identity = Identity {
            name: row.name.clone(),
            version: row.version,
            abi_tag: row.abi_tag,
            opt_tag,
            arch: row.arch,
        };
        let store_dir = match staging::store_root_for(row.arch, &cfg.store_roots) {
            Ok(root) => root.join(identity.store_dir_name()),
            Err(e) => {
                eprintln!("warning: cannot locate installed package {}: {e}", row.name);
                continue;
            }
        };
        let path = store_dir.join(cache::RECIPE_MANIFEST_NAME);
        let manifest = match fs::read_to_string(&path) {
            Ok(text) => match PackageManifest::from_toml_str(&text) {
                Ok(manifest) => manifest,
                Err(e) => {
                    eprintln!(
                        "warning: cannot parse installed recipe {}: {e}",
                        path.display()
                    );
                    continue;
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                eprintln!(
                    "warning: cannot read installed recipe {}: {e}",
                    path.display()
                );
                continue;
            }
        };
        if let Some(dkms) = manifest.dkms {
            if !valid_component(&dkms.dkms_name) || !valid_component(&dkms.dkms_version) {
                eprintln!(
                    "warning: ignoring unsafe DKMS name/version for installed package {}",
                    row.name
                );
                continue;
            }
            packages.push(DkmsPackage {
                package: row.name,
                module: dkms.dkms_name,
                version: dkms.dkms_version,
            });
        }
    }
    packages
}

fn plan_rebuilds(
    packages: &[DkmsPackage],
    activated: &BTreeSet<String>,
    before: &BTreeSet<String>,
    after: &BTreeSet<String>,
) -> Vec<DkmsTarget> {
    let new_kernels: BTreeSet<String> = after.difference(before).cloned().collect();
    let mut targets = Vec::new();
    for package in packages {
        let changed = activated.contains(&package.package);
        let kernels = if changed { after } else { &new_kernels };
        for kernel_release in kernels {
            targets.push(DkmsTarget {
                package: package.clone(),
                kernel_release: kernel_release.clone(),
            });
        }
    }
    targets.sort_by(|a, b| {
        (&a.package.package, &a.kernel_release).cmp(&(&b.package.package, &b.kernel_release))
    });
    targets
}

pub fn reconcile_after_plan(
    conn: &Connection,
    cfg: &SchedConfig,
    report: &mut Report,
    before: &BTreeSet<String>,
) {
    let root = paths::root();
    let after = match installed_kernels(root) {
        Ok(kernels) => kernels,
        Err(e) => {
            eprintln!("warning: cannot enumerate installed kernels for DKMS: {e}");
            return;
        }
    };
    let activated: BTreeSet<String> = report.activated.iter().cloned().collect();
    let packages = installed_dkms_packages(conn, cfg);
    let targets = plan_rebuilds(&packages, &activated, before, &after);
    for target in targets {
        let log = log_path(root, &target);
        match run_target(root, &target, &log, cfg.max_pkg_jobs) {
            Ok(()) => report.notices.push(format!(
                "DKMS: {} {} installed for {}",
                target.package.module, target.package.version, target.kernel_release
            )),
            Err(e) => eprintln!(
                "warning: DKMS rebuild failed for {} against kernel {}.\n         \
                 The package remains installed, but its kernel module may be unavailable.\n         \
                 See {}.\n         Details: {e}",
                target.package.package,
                target.kernel_release,
                log.display()
            ),
        }
    }
}

fn valid_component(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && !value
            .chars()
            .any(|c| c == '/' || c == '\0' || c.is_control())
}

fn safe_log_component(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._+-".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn log_path(root: &Path, target: &DkmsTarget) -> PathBuf {
    root.join("var/log/pkg").join(format!(
        "dkms-{}-{}.log",
        safe_log_component(&target.package.package),
        safe_log_component(&target.kernel_release)
    ))
}

const RUNNER: &str = r#"#!/bin/bash
set -u
log=$1 dkms=$2 state=$3 sources=$4 modules=$5 module=$6 version=$7 kernel=$8 usr=$9 jobs=${10}
: >"$log"
dkms_cmd=(bash "$dkms" --dkmstree "$state" --sourcetree "$sources" --installtree "$modules")
run() { "$@" >>"$log" 2>&1; }
if [ ! -L "$state/$module/$version/source" ]; then
    run "${dkms_cmd[@]}" add -m "$module" -v "$version" -k "$kernel" || exit
fi
run "${dkms_cmd[@]}" build -m "$module" -v "$version" -k "$kernel" -j "$jobs" --force || exit
run "${dkms_cmd[@]}" install -m "$module" -v "$version" -k "$kernel" --force --no-depmod || exit
run depmod -b "$usr" -a "$kernel"
"#;

fn run_target(root: &Path, target: &DkmsTarget, log: &Path, jobs: u32) -> Result<(), String> {
    let source = root.join("usr/src").join(format!(
        "{}-{}",
        target.package.module, target.package.version
    ));
    let dkms = root.join("usr/sbin/dkms");
    if !source.join("dkms.conf").is_file() {
        return Err(format!("missing {}", source.join("dkms.conf").display()));
    }
    if !dkms.is_file() {
        return Err(format!("missing {}", dkms.display()));
    }
    let physical_config = fs::canonicalize(source.join("dkms.conf"))
        .map_err(|e| format!("cannot resolve module source {}: {e}", source.display()))?;
    let physical_source = physical_config
        .parent()
        .ok_or_else(|| format!("module config {} has no parent", physical_config.display()))?;
    let source_tree = physical_source
        .parent()
        .ok_or_else(|| format!("module source {} has no parent", physical_source.display()))?
        .to_path_buf();

    let work_parent = root.join("var/lib/pkg");
    fs::create_dir_all(&work_parent).map_err(|e| e.to_string())?;
    let scratch = tempfile::Builder::new()
        .prefix("dkms-")
        .tempdir_in(work_parent)
        .map_err(|e| e.to_string())?;
    let state = root.join("var/lib/dkms");
    let modules = root.join("usr/lib/modules");
    let kernel_dir = modules.join(&target.kernel_release);
    fs::create_dir_all(&state).map_err(|e| e.to_string())?;
    fs::create_dir_all(&kernel_dir).map_err(|e| e.to_string())?;
    if let Some(parent) = log.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }

    let script = scratch.path().join("run.sh");
    let attempt_log = scratch.path().join("dkms.log");
    let os_release = scratch.path().join("os-release");
    fs::write(&script, RUNNER).map_err(|e| e.to_string())?;
    fs::write(&os_release, "ID=yakimo\nPRETTY_NAME=Yakimo\n").map_err(|e| e.to_string())?;

    let cmd: Vec<String> = vec![
        "bash".into(),
        script.display().to_string(),
        attempt_log.display().to_string(),
        dkms.display().to_string(),
        state.display().to_string(),
        source_tree.display().to_string(),
        modules.display().to_string(),
        target.package.module.clone(),
        target.package.version.clone(),
        target.kernel_release.clone(),
        root.join("usr").display().to_string(),
        jobs.max(1).to_string(),
    ];
    let env = vec![
        ("HOME".into(), scratch.path().display().to_string()),
        ("LC_ALL".into(), "C".into()),
        (
            "LD_LIBRARY_PATH".into(),
            root.join("usr/lib").display().to_string(),
        ),
        ("PATH".into(), "/usr/sbin:/usr/bin:/sbin:/bin".into()),
        ("TMPDIR".into(), "/tmp".into()),
    ];
    let profile = SandboxProfile::KernelModule {
        root: root.to_path_buf(),
        kernel_release: target.kernel_release.clone(),
    };
    let binds = vec![(os_release, PathBuf::from("/etc/os-release"))];
    let outcome = run_sandboxed_with_binds(&profile, &binds, &cmd, scratch.path(), &env)
        .map_err(|e| e.to_string())?;
    fs::copy(&attempt_log, log).map_err(|e| format!("cannot save {}: {e}", log.display()))?;
    if outcome.success() {
        Ok(())
    } else {
        Err(format!("{}: {}", outcome.status, outcome.stderr_tail))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package(name: &str) -> DkmsPackage {
        DkmsPackage {
            package: name.into(),
            module: "example".into(),
            version: "1.0".into(),
        }
    }

    #[test]
    fn kernels_require_a_usable_build_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let modules = tmp.path().join("usr/lib/modules");
        fs::create_dir_all(modules.join("6.19.14-yakimo/build")).unwrap();
        fs::write(modules.join("6.19.14-yakimo/build/Makefile"), "").unwrap();
        fs::create_dir_all(modules.join("empty")).unwrap();
        assert_eq!(
            installed_kernels(tmp.path()).unwrap(),
            BTreeSet::from(["6.19.14-yakimo".into()])
        );
    }

    #[test]
    fn changed_module_builds_for_every_kernel() {
        let packages = vec![package("nvidia-drivers")];
        let activated = BTreeSet::from(["nvidia-drivers".into()]);
        let before = BTreeSet::from(["6.18".into()]);
        let after = BTreeSet::from(["6.18".into(), "6.19".into()]);
        let targets = plan_rebuilds(&packages, &activated, &before, &after);
        assert_eq!(targets.len(), 2);
    }

    #[test]
    fn unchanged_module_builds_only_for_new_kernels() {
        let packages = vec![package("nvidia-drivers")];
        let targets = plan_rebuilds(
            &packages,
            &BTreeSet::new(),
            &BTreeSet::from(["6.18".into()]),
            &BTreeSet::from(["6.18".into(), "6.19".into()]),
        );
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].kernel_release, "6.19");
    }

    #[test]
    fn identifiers_are_opaque_but_cannot_escape_paths() {
        assert!(valid_component("6.19.14-yakimo+debug"));
        assert!(valid_component("1.0~rc1"));
        assert!(!valid_component("../escape"));
        assert!(!valid_component("with/slash"));
        assert!(!valid_component("with\nnewline"));
    }

    #[test]
    fn unrelated_transactions_do_not_rebuild_modules() {
        let packages = vec![package("nvidia-drivers")];
        let kernels = BTreeSet::from(["6.19".into()]);
        assert!(plan_rebuilds(&packages, &BTreeSet::new(), &kernels, &kernels).is_empty());
    }
}
