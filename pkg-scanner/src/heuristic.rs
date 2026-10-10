//! Heuristic scanning: `dlopen()` string-grep and known plugin-config
//! parsing (§8's pipeline, §15).
//!
//! Both techniques exist for the same reason: `elf.rs`'s DT_NEEDED list
//! only captures libraries a binary is *linked* against at build time.
//! A library only ever reached via a runtime `dlopen("libfoo.so", ...)`
//! call — common for optional codecs, database drivers, plugins — never
//! shows up there at all, even though the package almost certainly needs
//! it to actually work. Neither technique here is exact; both are
//! explicitly heuristic, the same framing §8.3 already used for this
//! category of check. A false positive costs a wasted look; a false
//! negative costs a missed optional dependency — worse for this
//! scanner's purpose, so both functions lean permissive.

use std::collections::BTreeSet;

/// Recognizes a token that plausibly looks like a shared library or
/// plugin soname — `libfoo.so`, `libfoo.so.3`, `pam_unix.so`. Requires a
/// `.so` marker with at least a few characters before it and nothing but
/// dots and digits after (the standard versioned-soname suffix
/// convention). Deliberately does *not* require a `lib` prefix: plugin
/// and module files commonly don't follow that convention even though
/// they're unambiguously the same kind of thing this function looks
/// for — PAM's `pam_unix.so` is exactly this case, and requiring `lib`
/// here would silently miss it.
fn looks_like_soname(token: &str) -> bool {
    let Some(so_pos) = token.find(".so") else {
        return false;
    };
    let before = &token[..so_pos];
    if before.len() < 3 {
        return false;
    }
    let after = &token[so_pos + 3..];
    after.is_empty() || after.chars().all(|c| c == '.' || c.is_ascii_digit())
}

/// Extracts runs of printable ASCII text at least `min_len` bytes long —
/// the same basic technique the Unix `strings` utility uses (whose
/// default minimum length is also 4, which is what this defaults to via
/// its callers below — not a coincidence so much as the same reasonable
/// threshold arrived at independently). Binary files interleave these
/// runs with non-printable machine code and data; finding them doesn't
/// need to understand ELF structure at all, which is the point — a
/// `dlopen()` argument string lives in an ordinary data section, not
/// anywhere `elf.rs`'s structured parsing looks.
fn extract_printable_strings(bytes: &[u8], min_len: usize) -> Vec<String> {
    let mut strings = Vec::new();
    let mut current = Vec::new();

    let flush = |current: &mut Vec<u8>, strings: &mut Vec<String>| {
        if current.len() >= min_len {
            if let Ok(s) = String::from_utf8(current.clone()) {
                strings.push(s);
            }
        }
        current.clear();
    };

    for &byte in bytes {
        if byte.is_ascii_graphic() || byte == b' ' {
            current.push(byte);
        } else {
            flush(&mut current, &mut strings);
        }
    }
    flush(&mut current, &mut strings);

    strings
}

/// Finds soname-like strings embedded in `bytes` that aren't already
/// present in `known_needed` (`elf.rs`'s `ElfInfo::needed`, in practice)
/// — candidates for a library reached only via runtime `dlopen()`.
///
/// A single extracted printable-string run can contain a soname as a
/// substring of something longer (a full path, a format string with the
/// soname embedded mid-sentence), so each run is tokenized on `/` and
/// spaces before checking — the two separators most likely to appear
/// *within* one printable run without a non-printable byte between them.
pub fn find_dlopen_candidates(bytes: &[u8], known_needed: &[String]) -> Vec<String> {
    let known: BTreeSet<&str> = known_needed.iter().map(String::as_str).collect();
    let mut found = BTreeSet::new();

    for s in extract_printable_strings(bytes, 4) {
        for token in s.split(['/', ' ']) {
            if looks_like_soname(token) && !known.contains(token) {
                found.insert(token.to_string());
            }
        }
    }

    found.into_iter().collect()
}

/// Extracts `.so` references from a text configuration file's content —
/// the kind of module/plugin directive line many Linux config formats
/// use (PAM's `pam_*.so` entries and similar).
///
/// Deliberately generic rather than a dedicated parser for any one
/// format's exact grammar: it recognizes soname-*shaped* tokens on
/// non-comment lines, which works reasonably across many simple
/// line-based config formats without needing a hand-written grammar per
/// ecosystem. That genericity has a real cost — a plugin referenced by a
/// bare module name the loader itself expands into a filename (no `lib`
/// prefix, no `.so` in the config line at all) won't be caught here. A
/// dedicated parser for a specific format the project cares a lot about
/// could catch that case; this function is the width-first baseline
/// that doesn't require writing one per format.
pub fn find_plugin_references(config_text: &str) -> Vec<String> {
    let mut found = BTreeSet::new();

    for line in config_text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        for token in line.split_whitespace() {
            let cleaned =
                token.trim_matches(|c: char| !c.is_alphanumeric() && c != '.' && c != '-' && c != '_');
            if looks_like_soname(cleaned) {
                found.insert(cleaned.to_string());
            }
        }
    }

    found.into_iter().collect()
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::process::Command;
    use tempfile::tempdir;

    fn compile_c(source: &str, output_path: &Path, extra_args: &[&str]) {
        let src_path = output_path.with_extension("c");
        std::fs::write(&src_path, source).unwrap();
        let status = Command::new("cc")
            .arg(&src_path)
            .arg("-o")
            .arg(output_path)
            .args(extra_args)
            .status()
            .expect("failed to invoke cc — is a C compiler installed?");
        assert!(status.success(), "cc failed to compile the test fixture");
    }

    // --- looks_like_soname ---

    #[test]
    fn looks_like_soname_recognizes_valid_patterns() {
        assert!(looks_like_soname("libfoo.so"));
        assert!(looks_like_soname("libfoo.so.1"));
        assert!(looks_like_soname("libfoo.so.1.2.3"));
        assert!(looks_like_soname("libfoo-bar.so.2"));
    }

    #[test]
    fn looks_like_soname_recognizes_names_without_a_lib_prefix() {
        // The exact case that motivated dropping the "lib" requirement:
        // PAM modules are unambiguously the same kind of thing this
        // function looks for, and don't follow that convention.
        assert!(looks_like_soname("pam_unix.so"));
        assert!(looks_like_soname("pam_permit.so"));
    }

    #[test]
    fn looks_like_soname_rejects_too_short_a_prefix() {
        assert!(!looks_like_soname(".so"));
        assert!(!looks_like_soname("a.so"));
    }

    #[test]
    fn looks_like_soname_rejects_non_numeric_suffix() {
        assert!(!looks_like_soname("libfoo.socket"));
        assert!(!looks_like_soname("libfoo.so.abc"));
    }

    // --- extract_printable_strings ---

    #[test]
    fn extract_printable_strings_finds_embedded_text() {
        let mut bytes = vec![0x00, 0x01, 0x02];
        bytes.extend_from_slice(b"libfoo.so.1");
        bytes.extend_from_slice(&[0xff, 0x00]);
        bytes.extend_from_slice(b"short");
        bytes.push(0x00);
        bytes.extend_from_slice(b"ab"); // shorter than min_len, should be dropped

        let found = extract_printable_strings(&bytes, 4);
        assert!(found.contains(&"libfoo.so.1".to_string()));
        assert!(found.contains(&"short".to_string()));
        assert!(!found.iter().any(|s| s == "ab"));
    }

    // --- find_dlopen_candidates, against real compiled binaries ---

    #[test]
    fn finds_dlopen_string_in_a_real_binary() {
        let dir = tempdir().unwrap();
        let binary_path = dir.path().join("test_bin");
        compile_c(
            r#"
            #include <dlfcn.h>
            int main() {
                void *h = dlopen("libfoo-optional.so.1", RTLD_LAZY);
                return h != 0;
            }
            "#,
            &binary_path,
            &[],
        );

        let bytes = std::fs::read(&binary_path).unwrap();
        let candidates = find_dlopen_candidates(&bytes, &[]);

        assert!(
            candidates.contains(&"libfoo-optional.so.1".to_string()),
            "expected to find the dlopen()'d soname, got {candidates:?}"
        );
    }

    #[test]
    fn dlopen_candidates_exclude_already_known_needed_libraries() {
        let dir = tempdir().unwrap();
        let binary_path = dir.path().join("test_bin");
        compile_c(
            r#"
            #include <dlfcn.h>
            int main() {
                void *h = dlopen("libfoo-optional.so.1", RTLD_LAZY);
                return h != 0;
            }
            "#,
            &binary_path,
            &[],
        );

        let bytes = std::fs::read(&binary_path).unwrap();
        let elf_info = crate::parse_elf(&bytes).unwrap();
        // Not hardcoding which specific libraries end up in DT_NEEDED —
        // an earlier version of this test assumed `-lm` always produces
        // a separate `libm.so.6` entry, which stopped being true once
        // glibc 2.34 merged libm into libc.so.6 directly (this sandbox
        // runs glibc 2.39, well past that merge). `libc.so.6` itself is
        // the one thing guaranteed present for any dynamically-linked
        // binary, and its name is guaranteed to appear as a raw string
        // in the file too (it's literally stored in .dynstr) — a
        // realistic, always-available case to verify gets excluded.
        assert!(
            elf_info.needed.iter().any(|n| n == "libc.so.6"),
            "sanity check: libc.so.6 should be in DT_NEEDED, got {:?}",
            elf_info.needed
        );

        let candidates = find_dlopen_candidates(&bytes, &elf_info.needed);
        assert!(candidates.contains(&"libfoo-optional.so.1".to_string()));
        assert!(
            !candidates.iter().any(|c| elf_info.needed.contains(c)),
            "shouldn't re-report a library already known via DT_NEEDED: {candidates:?}"
        );
    }

    // --- find_plugin_references ---

    #[test]
    fn find_plugin_references_extracts_so_names_from_config_text() {
        let config = "\
            # PAM configuration\n\
            auth    required    pam_unix.so\n\
            auth    optional    pam_permit.so nullok\n\
            \n\
            session required pam_limits.so\n\
        ";

        let found = find_plugin_references(config);
        assert_eq!(
            found,
            vec![
                "pam_limits.so".to_string(),
                "pam_permit.so".to_string(),
                "pam_unix.so".to_string(),
            ]
        );
    }

    #[test]
    fn find_plugin_references_ignores_non_so_tokens_and_comments() {
        let config = "\
            # this whole line is a comment, including pam_fake.so\n\
            auth required pam_unix.so nullok use_first_pass\n\
        ";

        let found = find_plugin_references(config);
        assert_eq!(found, vec!["pam_unix.so".to_string()]);
        assert!(!found.contains(&"pam_fake.so".to_string()), "commented-out lines should be skipped");
    }
}
