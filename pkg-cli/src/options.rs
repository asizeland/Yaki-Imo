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

use pkg_core::manifest::PackageManifest;
use pkg_core::{PkgError, PkgResult};

/// The persisted option-selection file (§4.3).
pub const OPTIONS_PATH: &str = "/etc/pkg/options.toml";

/// Parsed `/etc/pkg/options.toml`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct OptionConfig {
    /// `[defaults]` — applies to every package that declares the key.
    pub defaults: BTreeMap<String, bool>,
    /// `[package.<name>]` — applies to that package only.
    pub packages: BTreeMap<String, BTreeMap<String, bool>>,
}

/// Parses options.toml text. A missing `[defaults]`/`[package]` table
/// is simply absent; a non-boolean value or an unknown top-level table
/// is a hard error.
pub fn parse_option_config(text: &str) -> PkgResult<OptionConfig> {
    let value: toml::Value = toml::from_str(text)
        .map_err(|e| PkgError::Parse(format!("{OPTIONS_PATH}: {e}")))?;
    let Some(table) = value.as_table() else {
        return Err(PkgError::Parse(format!("{OPTIONS_PATH}: not a TOML table")));
    };
    let bool_map = |t: &toml::value::Table, where_: &str| -> PkgResult<BTreeMap<String, bool>> {
        t.iter()
            .map(|(k, v)| {
                v.as_bool()
                    .map(|b| (k.clone(), b))
                    .ok_or_else(|| {
                        PkgError::Parse(format!(
                            "{OPTIONS_PATH}: {where_}.{k} must be true or false"
                        ))
                    })
            })
            .collect()
    };
    let mut out = OptionConfig::default();
    for (key, val) in table {
        match key.as_str() {
            "defaults" => {
                let t = val.as_table().ok_or_else(|| {
                    PkgError::Parse(format!("{OPTIONS_PATH}: [defaults] must be a table"))
                })?;
                out.defaults = bool_map(t, "defaults")?;
            }
            "package" => {
                let t = val.as_table().ok_or_else(|| {
                    PkgError::Parse(format!("{OPTIONS_PATH}: [package] must be a table"))
                })?;
                for (pkg, entries) in t {
                    let entries = entries.as_table().ok_or_else(|| {
                        PkgError::Parse(format!(
                            "{OPTIONS_PATH}: [package.{pkg}] must be a table"
                        ))
                    })?;
                    out.packages
                        .insert(pkg.clone(), bool_map(entries, &format!("package.{pkg}"))?);
                }
            }
            other => {
                return Err(PkgError::Parse(format!(
                    "{OPTIONS_PATH}: unknown table [{other}] (expected [defaults] or [package.<name>])"
                )))
            }
        }
    }
    Ok(out)
}

/// Reads and parses the config file at `path`; a missing file is an
/// empty config, an unreadable or invalid one a hard error.
pub fn option_config_from(path: &std::path::Path) -> PkgResult<OptionConfig> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse_option_config(&text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(OptionConfig::default()),
        Err(e) => Err(PkgError::Io(e)),
    }
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
                            "{OPTIONS_PATH}: [package.{pkg}] names undeclared option '{key}'"
                        )));
                    }
                }
            }
            None => eprintln!(
                "pkg: warning: {OPTIONS_PATH} has entries for '{pkg}', which has no local recipe"
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
}
