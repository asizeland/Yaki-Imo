//! pkg-cli/src/options.rs — persisted build-option configuration and
//! per-transaction option requests, merged into per-package effective
//! selections (§4.3 / Issue 1 §5).
//!
//! Precedence, strongest first (mirroring pins' "explicit wins"):
//!
//! 1. per-command `--opt name=0|1` (transaction-wide; not persisted)
//!    and `name[opt:key=val]` spec brackets (per package);
//! 2. per-package `/etc/pkg/options.toml` `[package.<name>]` entries;
//! 3. global `/etc/pkg/options.toml` `[defaults]`.
//!
//! ## Includes (v2)
//!
//! A heavy option setup need not live in one file: the main file may
//! name sibling files to pull in —
//!
//! ```toml
//! include = ["sound.toml", "desktop.toml"]
//! ```
//!
//! Paths are relative to the directory holding `options.toml`;
//! absolute paths and `..` escapes are hard errors (an include that
//! reaches outside the config directory is a surprise, and surprises
//! here are config bugs). Included files use the same `[defaults]` /
//! `[package.<name>]` tables but may NOT themselves include (one level
//! keeps the merge order something a user can hold in their head).
//! Merge order is the include list's order, then the main file last —
//! the main file always wins. Every entry records its source file so
//! error messages and `pkg options` can say where a value came from.
//!
//! Two rules make this safe rather than merely convenient:
//!
//! - **Loud, never silent.** Unknown option names, non-boolean values,
//!   or options on packages that declare none are hard errors — a
//!   silently ignored `x11 = false` is the config-file twin of silent
//!   divergence (§4.3).
//! - **Widen, else conflict.** Two *per-command* requests for the same
//!   package and key with different values fail naming both
//!   requesters; config layers never conflict because strict precedence
//!   resolves them.

use std::collections::{BTreeMap, HashMap};
use std::path::{Component, Path};

use pkg_core::manifest::PackageManifest;
use pkg_core::{PkgError, PkgResult};

/// The persisted option-selection file (§4.3).
pub const OPTIONS_PATH: &str = "/etc/pkg/options.toml";

/// Parsed `/etc/pkg/options.toml` (plus any included siblings).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct OptionConfig {
    /// `[defaults]` — applies to every package that declares the key.
    pub defaults: BTreeMap<String, bool>,
    /// `[package.<name>]` — applies to that package only.
    pub packages: BTreeMap<String, BTreeMap<String, bool>>,
    /// Where each entry came from: `defaults.<key>` or
    /// `package.<pkg>.<key>` → the file that set it (the main file is
    /// shown by its bare file name). Display/audit only — merging
    /// already applied precedence.
    pub origins: BTreeMap<String, String>,
}

/// Parses options.toml text. `source` names the file in error
/// messages. A missing `[defaults]`/`[package]` table is simply
/// absent; a non-boolean value or an unknown top-level table is a hard
/// error. Returns the config and the file's `include` list (unresolved
/// — the caller owns path policy).
fn parse_option_config_named(text: &str, source: &str) -> PkgResult<(OptionConfig, Vec<String>)> {
    let value: toml::Value =
        toml::from_str(text).map_err(|e| PkgError::Parse(format!("{source}: {e}")))?;
    let Some(table) = value.as_table() else {
        return Err(PkgError::Parse(format!("{source}: not a TOML table")));
    };
    let bool_map = |t: &toml::value::Table, where_: &str| -> PkgResult<BTreeMap<String, bool>> {
        t.iter()
            .map(|(k, v)| {
                v.as_bool()
                    .map(|b| (k.clone(), b))
                    .ok_or_else(|| {
                        PkgError::Parse(format!("{source}: {where_}.{k} must be true or false"))
                    })
            })
            .collect()
    };
    let mut out = OptionConfig::default();
    let mut includes = Vec::new();
    for (key, val) in table {
        match key.as_str() {
            "include" => {
                let list = val.as_array().ok_or_else(|| {
                    PkgError::Parse(format!("{source}: include must be a list of file names"))
                })?;
                for entry in list {
                    includes.push(
                        entry
                            .as_str()
                            .ok_or_else(|| {
                                PkgError::Parse(format!(
                                    "{source}: include entries must be strings"
                                ))
                            })?
                            .to_string(),
                    );
                }
            }
            "defaults" => {
                let t = val.as_table().ok_or_else(|| {
                    PkgError::Parse(format!("{source}: [defaults] must be a table"))
                })?;
                for (k, v) in bool_map(t, "defaults")? {
                    out.origins.insert(format!("defaults.{k}"), source.to_string());
                    out.defaults.insert(k, v);
                }
            }
            "package" => {
                let t = val.as_table().ok_or_else(|| {
                    PkgError::Parse(format!("{source}: [package] must be a table"))
                })?;
                for (pkg, entries) in t {
                    let entries = entries.as_table().ok_or_else(|| {
                        PkgError::Parse(format!("{source}: [package.{pkg}] must be a table"))
                    })?;
                    let entries = bool_map(entries, &format!("package.{pkg}"))?;
                    for (k, _) in &entries {
                        out.origins
                            .insert(format!("package.{pkg}.{k}"), source.to_string());
                    }
                    out.packages.entry(pkg.clone()).or_default().extend(entries);
                }
            }
            other => {
                return Err(PkgError::Parse(format!(
                    "{source}: unknown table [{other}] (expected [defaults], [package.<name>], or `include`)"
                )))
            }
        }
    }
    Ok((out, includes))
}

/// Parses options.toml text (tests only — production callers go through
/// [`option_config_from`], which resolves includes against the file's
/// directory). Includes are a file-level concept; parsing text alone
/// yields none.
#[cfg(test)]
pub fn parse_option_config(text: &str) -> PkgResult<OptionConfig> {
    Ok(parse_option_config_named(text, OPTIONS_PATH)?.0)
}

/// Validates one include entry and resolves it against the config
/// directory: the name must be relative, must not escape via `..`, and
/// the file must exist and parse. Included files may not include
/// further (one level, deliberately — the merge order must stay
/// holdable in one's head).
fn load_include(dir: &Path, name: &str, main: &str) -> PkgResult<OptionConfig> {
    let rel = Path::new(name);
    let mut components = rel.components();
    let plain_name = matches!(components.next(), Some(Component::Normal(_)))
        && components.next().is_none();
    if rel.is_absolute() || !plain_name {
        return Err(PkgError::Parse(format!(
            "{main}: include entry '{name}' must be a plain file name \
             in the same directory (no absolute paths, no `..`, no subdirectories)"
        )));
    }
    let path = dir.join(rel);
    let text = std::fs::read_to_string(&path).map_err(|e| {
        PkgError::Parse(format!("{main}: cannot read included file {}: {e}", path.display()))
    })?;
    let (cfg, nested) = parse_option_config_named(&text, name)?;
    if !nested.is_empty() {
        return Err(PkgError::Parse(format!(
            "{name}: included files may not themselves `include` (one level only)"
        )));
    }
    Ok(cfg)
}

/// Reads and parses the config file at `path`, resolving its `include`
/// list against the file's own directory. Merge order: includes in
/// listed order, the main file last (it always wins). A missing main
/// file is an empty config; an unreadable or invalid one is a hard
/// error.
pub fn option_config_from(path: &Path) -> PkgResult<OptionConfig> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(OptionConfig::default()),
        Err(e) => return Err(PkgError::Io(e)),
    };
    let main_name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| OPTIONS_PATH.to_string());
    let dir = path.parent().unwrap_or(Path::new("."));
    let (main, includes) = parse_option_config_named(&text, &main_name)?;
    // Includes apply in listed order (earlier = weaker); the main file
    // is strongest and merges last.
    let mut merged = OptionConfig::default();
    for name in &includes {
        let included = load_include(dir, name, &main_name)?;
        merged = merge_configs(merged, included);
    }
    Ok(merge_configs(merged, main))
}

/// `weaker` merged under `stronger`: per-key, the stronger file's value
/// wins; origins follow the winning write.
fn merge_configs(weaker: OptionConfig, stronger: OptionConfig) -> OptionConfig {
    let mut out = weaker;
    for (k, v) in stronger.defaults {
        out.defaults.insert(k, v);
    }
    for (pkg, entries) in stronger.packages {
        out.packages.entry(pkg).or_default().extend(entries);
    }
    out.origins.extend(stronger.origins);
    out
}

/// One per-command option request from a `name[opt:key=val]` spec
/// bracket (per-package) or a `--opt` flag (transaction-wide).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptionRequest {
    /// Target package for spec brackets; `None` for `--opt`.
    pub package: Option<String>,
    pub key: String,
    pub value: bool,
    /// Who asked, for conflict reports ("--opt", a spec string).
    pub requester: String,
}

/// Splits an install/upgrade spec into its plain `name[@version]`
/// part and any `[opt:k=v,...]` option requirements (§4.3
/// pseudo-dependency surface on the CLI).
///
/// ```text
/// greeter                    → ("greeter", [])
/// greeter@2[opt:loud=1]      → ("greeter@2", [("loud", true)])
/// ```
///
/// Malformed brackets, empty keys, and non-boolean values are hard
/// errors — this syntax is a request, and a request the user did not
/// actually make must never be silently built.
pub fn split_opt_bracket(spec: &str) -> PkgResult<(String, Vec<(String, bool)>)> {
    let Some(open) = spec.find('[') else {
        return Ok((spec.to_string(), Vec::new()));
    };
    let plain = &spec[..open];
    let bracket = &spec[open..];
    let inner = bracket
        .strip_prefix("[opt:")
        .and_then(|s| s.strip_suffix(']'))
        .ok_or_else(|| {
            PkgError::Parse(format!(
                "malformed option bracket in {spec:?} (expected name[opt:key=val,...])"
            ))
        })?;
    let mut reqs = Vec::new();
    for pair in inner.split(',') {
        let (key, val) = pair.split_once('=').ok_or_else(|| {
            PkgError::Parse(format!("option bracket in {spec:?}: expected key=val, got {pair:?}"))
        })?;
        if key.is_empty() {
            return Err(PkgError::Parse(format!(
                "option bracket in {spec:?}: empty option name"
            )));
        }
        let value = match val {
            "1" | "true" | "on" => true,
            "0" | "false" | "off" => false,
            _ => {
                return Err(PkgError::Parse(format!(
                    "option bracket in {spec:?}: {key} must be 0/1 (true/false), got {val:?}"
                )))
            }
        };
        reqs.push((key.to_string(), value));
    }
    Ok((plain.to_string(), reqs))
}

/// Assembles the effective per-package option selection for a
/// transaction (spec §4.3 precedence + loudness rules).
///
/// - `selected`: every package the transaction will consider;
/// - `manifest_of`: recipe lookup, used both to filter and to validate
///   (returns `None` for a package with no local recipe);
/// - `cli_opts`: the transaction-wide `--opt` map;
/// - `requests`: per-package requests from spec brackets.
///
/// The result maps each selected package to its effective selection —
/// only keys that package declares. A `--opt` key no selected package
/// declares is a hard error (typo protection); two per-command
/// requests assigning the same package+key differently is a hard error
/// naming both requesters (widen-else-conflict).
pub fn effective_selections(
    selected: &[String],
    manifest_of: &dyn Fn(&str) -> Option<PackageManifest>,
    config: &OptionConfig,
    cli_opts: &BTreeMap<String, bool>,
    requests: &[OptionRequest],
) -> PkgResult<HashMap<String, BTreeMap<String, bool>>> {
    // Validate the persisted config against local recipes: an entry
    // for a known package naming an undeclared option is a hard error
    // (silently ignoring it is exactly what §4.3 forbids). An entry
    // for a package with no local recipe is stale state, warned about
    // rather than fatal — packages legitimately come and go.
    for (pkg, entries) in &config.packages {
        match manifest_of(pkg) {
            Some(m) => {
                for key in entries.keys() {
                    if !m.options.contains_key(key) {
                        return Err(PkgError::Parse(format!(
                            "options.toml: [package.{pkg}] names undeclared option '{key}'"
                        )));
                    }
                }
            }
            None => eprintln!(
                "pkg: warning: options.toml has entries for '{pkg}', which has no local recipe"
            ),
        }
    }

    // Widen-else-conflict within the per-command tier: brackets and
    // --opt flags are equally strong requests, so two assigning the
    // same package+key differently is a genuine conflict.
    let mut seen: HashMap<(String, String), (bool, String)> = HashMap::new();
    let all_requests = requests.iter().cloned().chain(cli_opts.iter().map(|(k, v)| {
        OptionRequest {
            package: None,
            key: k.clone(),
            value: *v,
            requester: "--opt".to_string(),
        }
    }));
    for req in all_requests {
        for target in match &req.package {
            Some(p) => vec![p.clone()],
            None => selected.to_vec(),
        } {
            let id = (target, req.key.clone());
            match seen.get(&id) {
                Some((v, who)) if *v != req.value => {
                    return Err(PkgError::Resolution(format!(
                        "option conflict on {}.{}: {} requests {}={}, but {} requests {}={}",
                        id.0,
                        id.1,
                        who,
                        id.1,
                        v,
                        req.requester,
                        id.1,
                        req.value,
                    )));
                }
                _ => {
                    seen.insert(id, (req.value, req.requester.clone()));
                }
            }
        }
    }

    let mut out = HashMap::new();
    for name in selected {
        let Some(manifest) = manifest_of(name) else { continue };
        let mut eff = BTreeMap::new();
        // Weakest first: [defaults], then [package.<name>], then the
        // per-command tier (spec brackets for this package, then the
        // transaction-wide --opt map).
        for (key, val) in &config.defaults {
            if manifest.options.contains_key(key) {
                eff.insert(key.clone(), *val);
            }
        }
        if let Some(entries) = config.packages.get(name) {
            eff.extend(entries.iter().map(|(k, v)| (k.clone(), *v)));
        }
        for req in requests {
            if req.package.as_deref() == Some(name.as_str()) {
                eff.insert(req.key.clone(), req.value);
            }
        }
        for (key, val) in cli_opts {
            if manifest.options.contains_key(key) {
                eff.insert(key.clone(), *val);
            }
        }
        out.insert(name.clone(), eff);
    }

    // Typo protection for the transaction-wide tier: an --opt key no
    // selected package declares can only be a mistake.
    for key in cli_opts.keys() {
        let declared = selected.iter().any(|name| {
            manifest_of(name).is_some_and(|m| m.options.contains_key(key))
        });
        if !declared {
            return Err(PkgError::Parse(format!(
                "unknown option in selection: '{key}' (no package in this transaction declares it)"
            )));
        }
    }
    Ok(out)
}

/// Convenience: the effective selection of one package, or the empty
/// (all-default) selection when the package has none.
pub fn selection_of<'m>(
    map: &'m HashMap<String, BTreeMap<String, bool>>,
    name: &str,
) -> &'m BTreeMap<String, bool> {
    static EMPTY: BTreeMap<String, bool> = BTreeMap::new();
    map.get(name).unwrap_or(&EMPTY)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GREETER_TOML: &str = r#"
        name = "greeter"
        version = "1"
        category = "app"

        [options.loud]
        default = false
        description = "SHOUT"

        [options.docs]
        default = true
        abi_affecting = false
        description = "ship docs"
    "#;

    fn manifest_of(name: &str) -> Option<PackageManifest> {
        (name == "greeter")
            .then(|| PackageManifest::from_toml_str(GREETER_TOML).unwrap())
    }

    fn selected() -> Vec<String> {
        vec!["greeter".to_string()]
    }

    #[test]
    fn config_parses_defaults_and_packages() {
        let cfg = parse_option_config(
            "[defaults]\nnls = false\n[package.greeter]\nloud = true\n",
        )
        .unwrap();
        assert_eq!(cfg.defaults.get("nls"), Some(&false));
        assert_eq!(cfg.packages["greeter"].get("loud"), Some(&true));
    }

    #[test]
    fn config_rejects_non_bool_and_unknown_tables() {
        assert!(parse_option_config("[defaults]\nnls = \"no\"\n").is_err());
        assert!(parse_option_config("[package.greeter]\nloud = 1\n").is_err());
        assert!(parse_option_config("[window]\nx11 = true\n").is_err());
    }

    #[test]
    fn bracket_splits_and_validates() {
        assert_eq!(
            split_opt_bracket("greeter@2[opt:loud=1,docs=off]").unwrap(),
            ("greeter@2".to_string(), vec![
                ("loud".to_string(), true),
                ("docs".to_string(), false)
            ])
        );
        assert_eq!(split_opt_bracket("greeter").unwrap(), ("greeter".to_string(), vec![]));
        assert!(split_opt_bracket("greeter[opt:]").is_err());
        assert!(split_opt_bracket("greeter[opt:loud]").is_err());
        assert!(split_opt_bracket("greeter[opt:loud=maybe]").is_err());
        assert!(split_opt_bracket("greeter[foo:loud=1]").is_err());
    }

    #[test]
    fn precedence_cli_over_package_over_defaults() {
        let cfg = parse_option_config(
            "[defaults]\nloud = false\ndocs = false\n[package.greeter]\nloud = false\n",
        )
        .unwrap();
        let cli = BTreeMap::from([("loud".to_string(), true)]);
        let eff = effective_selections(&selected(), &manifest_of, &cfg, &cli, &[]).unwrap();
        let g = &eff["greeter"];
        assert_eq!(g.get("loud"), Some(&true), "--opt beats config");
        assert_eq!(g.get("docs"), Some(&false), "[defaults] applies to declared keys");
    }

    #[test]
    fn config_keys_not_declared_by_a_package_do_not_leak() {
        // [defaults] keys only apply to packages declaring them.
        let cfg = parse_option_config("[defaults]\nwayland = true\n").unwrap();
        let eff = effective_selections(&selected(), &manifest_of, &cfg, &BTreeMap::new(), &[])
            .unwrap();
        assert!(!eff["greeter"].contains_key("wayland"));
    }

    #[test]
    fn config_undeclared_option_on_known_package_is_a_hard_error() {
        let cfg = parse_option_config("[package.greeter]\nwayland = true\n").unwrap();
        assert!(effective_selections(&selected(), &manifest_of, &cfg, &BTreeMap::new(), &[])
            .is_err());
    }

    #[test]
    fn cli_key_declared_by_nobody_is_a_hard_error() {
        let cli = BTreeMap::from([("wayland".to_string(), true)]);
        assert!(effective_selections(&selected(), &manifest_of, &OptionConfig::default(), &cli, &[])
            .is_err());
    }

    #[test]
    fn per_command_conflict_names_both_requesters() {
        let cli = BTreeMap::from([("loud".to_string(), false)]);
        let reqs = vec![OptionRequest {
            package: Some("greeter".to_string()),
            key: "loud".to_string(),
            value: true,
            requester: "greeter[opt:loud=1]".to_string(),
        }];
        let err =
            effective_selections(&selected(), &manifest_of, &OptionConfig::default(), &cli, &reqs)
                .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("greeter.loud"), "{msg}");
        assert!(msg.contains("--opt"), "{msg}");
        assert!(msg.contains("greeter[opt:loud=1]"), "{msg}");
    }

    #[test]
    fn widen_merges_agreeing_requests() {
        let reqs = vec![
            OptionRequest {
                package: Some("greeter".to_string()),
                key: "loud".to_string(),
                value: true,
                requester: "a".to_string(),
            },
            OptionRequest {
                package: Some("greeter".to_string()),
                key: "docs".to_string(),
                value: false,
                requester: "b".to_string(),
            },
        ];
        let eff = effective_selections(
            &selected(),
            &manifest_of,
            &OptionConfig::default(),
            &BTreeMap::new(),
            &reqs,
        )
        .unwrap();
        assert_eq!(eff["greeter"].get("loud"), Some(&true));
        assert_eq!(eff["greeter"].get("docs"), Some(&false));
    }

    // --- includes (v2) ---

    /// A unique temp dir per test (pkg-cli has no tempfile dev-dep;
    /// same pattern as pkg-scanner's gate tests).
    fn test_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("pkg-options-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn include_merges_siblings_in_order_and_main_wins() {
        let dir = test_dir("merge");
        fs_write(&dir.join("a.toml"), "[defaults]\nnls = false\ndocs = false\n");
        fs_write(&dir.join("b.toml"), "[defaults]\ndocs = true\n[package.greeter]\nloud = true\n");
        fs_write(
            &dir.join("options.toml"),
            "include = [\"a.toml\", \"b.toml\"]\n[package.greeter]\nloud = false\n",
        );
        let cfg = option_config_from(&dir.join("options.toml")).unwrap();
        assert_eq!(cfg.defaults.get("nls"), Some(&false), "only a sets it");
        assert_eq!(cfg.defaults.get("docs"), Some(&true), "b later than a");
        assert_eq!(cfg.packages["greeter"].get("loud"), Some(&false), "main file wins");
        // Origins follow the winning write.
        assert_eq!(cfg.origins["defaults.docs"], "b.toml");
        assert_eq!(cfg.origins["package.greeter.loud"], "options.toml");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn include_rejects_absolute_and_parent_escapes() {
        let dir = test_dir("escape");
        for bad in ["/etc/passwd", "../escape.toml", "sub/dir.toml"] {
            fs_write(
                &dir.join("options.toml"),
                &format!("include = [\"{bad}\"]\n"),
            );
            let err = option_config_from(&dir.join("options.toml")).unwrap_err();
            assert!(err.to_string().contains("plain file name"), "{bad}: {err}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn include_missing_file_and_nested_include_are_hard_errors() {
        let dir = test_dir("missing");
        fs_write(&dir.join("options.toml"), "include = [\"nope.toml\"]\n");
        assert!(option_config_from(&dir.join("options.toml")).is_err());

        fs_write(&dir.join("nested.toml"), "include = [\"x.toml\"]\n");
        fs_write(&dir.join("options.toml"), "include = [\"nested.toml\"]\n");
        let err = option_config_from(&dir.join("options.toml")).unwrap_err();
        assert!(err.to_string().contains("one level"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn include_errors_name_the_included_file() {
        let dir = test_dir("named");
        fs_write(&dir.join("bad.toml"), "[defaults]\nnls = \"maybe\"\n");
        fs_write(&dir.join("options.toml"), "include = [\"bad.toml\"]\n");
        let err = option_config_from(&dir.join("options.toml")).unwrap_err();
        assert!(err.to_string().contains("bad.toml"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_option_config_still_works_without_files() {
        // Text-only parsing (no includes possible) keeps working for
        // callers that never touch the filesystem.
        let cfg = parse_option_config("[defaults]\nnls = true\n").unwrap();
        assert_eq!(cfg.defaults.get("nls"), Some(&true));
    }

    fn fs_write(path: &std::path::PathBuf, text: &str) {
        std::fs::write(path, text).unwrap();
    }
}
