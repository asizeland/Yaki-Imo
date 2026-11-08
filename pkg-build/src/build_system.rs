use pkg_core::manifest::BuildSystem;

use crate::recipe::Phase;

pub fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

pub fn generated_phase(
    system: BuildSystem,
    phase: Phase,
    configure_args: &[String],
) -> Option<String> {
    let args = configure_args
        .iter()
        .map(|arg| format!(" {}", shell_quote(arg)))
        .collect::<String>();
    let script = match (system, phase) {
        (BuildSystem::Autotools, Phase::Build) => {
            format!("./configure --prefix=/usr{args}\nmake\n")
        }
        (BuildSystem::Meson, Phase::Build) => format!(
            "meson setup .pkg-build --prefix=/usr --libdir=/usr/lib{args}\n\
             meson compile -C .pkg-build\n"
        ),
        (BuildSystem::CMake, Phase::Build) => format!(
            "cmake -S . -B .pkg-build -DCMAKE_INSTALL_PREFIX=/usr{args}\n\
             cmake --build .pkg-build\n"
        ),
        (BuildSystem::Make, Phase::Build) => "make\n".to_string(),
        (BuildSystem::Autotools | BuildSystem::Make, Phase::Install) => {
            "make DESTDIR=\"$PKGDIR\" install\n".to_string()
        }
        (BuildSystem::Meson, Phase::Install) => {
            "DESTDIR=\"$PKGDIR\" meson install -C .pkg-build\n".to_string()
        }
        (BuildSystem::CMake, Phase::Install) => {
            "DESTDIR=\"$PKGDIR\" cmake --install .pkg-build\n".to_string()
        }
        _ => return None,
    };
    Some(script)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_arguments_as_shell_words() {
        assert_eq!(shell_quote("--simple"), "'--simple'");
        assert_eq!(shell_quote("two words"), "'two words'");
        assert_eq!(shell_quote("it's"), "'it'\"'\"'s'");
    }

    #[test]
    fn generates_managed_build_and_install_phases() {
        let args = vec!["--disable-nls".to_string()];
        let auto = generated_phase(BuildSystem::Autotools, Phase::Build, &args).unwrap();
        assert!(auto.contains("./configure --prefix=/usr '--disable-nls'"));
        assert!(auto.ends_with("make\n"));
        assert_eq!(
            generated_phase(BuildSystem::Make, Phase::Install, &[]).as_deref(),
            Some("make DESTDIR=\"$PKGDIR\" install\n")
        );
        assert!(generated_phase(BuildSystem::Meson, Phase::Build, &[])
            .unwrap()
            .contains("meson setup .pkg-build"));
        assert!(generated_phase(BuildSystem::CMake, Phase::Install, &[])
            .unwrap()
            .contains("cmake --install .pkg-build"));
    }

    #[test]
    fn custom_and_none_have_no_generated_phases() {
        assert!(generated_phase(BuildSystem::Custom, Phase::Build, &[]).is_none());
        assert!(generated_phase(BuildSystem::None, Phase::Install, &[]).is_none());
        assert!(generated_phase(BuildSystem::Make, Phase::Check, &[]).is_none());
    }
}
