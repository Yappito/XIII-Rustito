//! Localisation provider boundary for the VM.
//!
//! `xiii-script` is filesystem-free: it never opens `.int` files. The host
//! (`xiii-world::runtime`, backed by `xiii-locale`) implements
//! [`LocalizationData`] over the installation's active language and installs it
//! with [`crate::Vm::set_localization`]. The VM then:
//!
//! * resolves `Object.Localize(SectionName, KeyName, PackageName)` through
//!   [`LocalizationData::get`];
//! * fills class defaults for properties whose decoded property flags contain
//!   `CPF_Localized` ([`crate::reflect::property_flags::LOCALIZED`]) from the
//!   class package's `.int` (section = class name, key = property name).
//!
//! A miss returns the UE2 placeholder `<?language?Package.Section.Key?>`
//! (measured: `UObject::execLocalize` -> `Localize` in `Core.dll` formats the
//! literal `"<?%s?%s.%s.%s?>"` with the active language, package, section and
//! key), never a silent empty string.

/// Host localisation lookup, in the install's active language with the usual
/// fallback to the international/`int` file (the implementation decides).
pub trait LocalizationData {
    /// `Localize(Section, Key, Package)` text, or `None` when the file, section
    /// or key is absent.
    fn get(&self, package: &str, section: &str, key: &str) -> Option<String>;

    /// Active language code (for example `int`, `frt`), used to build the
    /// miss placeholder exactly as `Core.dll` does.
    fn language(&self) -> &str;
}

/// Formats the UE2 `Localize` miss placeholder:
/// `<?language?Package.Section.Key?>`.
pub fn placeholder(language: &str, package: &str, section: &str, key: &str) -> String {
    format!("<?{language}?{package}.{section}.{key}?>")
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake;

    impl LocalizationData for Fake {
        fn get(&self, package: &str, section: &str, key: &str) -> Option<String> {
            (package.eq_ignore_ascii_case("Pkg")
                && section.eq_ignore_ascii_case("Section")
                && key.eq_ignore_ascii_case("Key"))
            .then(|| "hello".to_owned())
        }

        fn language(&self) -> &str {
            "int"
        }
    }

    #[test]
    fn placeholder_matches_the_decoded_format() {
        assert_eq!(
            placeholder("int", "XIII", "XIIIGameInfo", "GameName"),
            "<?int?XIII.XIIIGameInfo.GameName?>"
        );
    }

    #[test]
    fn fake_provider_is_case_insensitive_on_miss_and_hit() {
        let f = Fake;
        assert_eq!(
            f.get("pkg", "section", "key").as_deref(),
            Some("hello"),
            "provider contract is case-insensitive"
        );
        assert_eq!(f.get("Pkg", "Section", "Other"), None);
    }
}
