//! Fluent resources for built-in UI text.
use proteus_i18n::{FluentArgs, FluentValue};
use std::ops::{Deref, DerefMut};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Catalog(proteus_i18n::Catalog);
impl Deref for Catalog {
    type Target = proteus_i18n::Catalog;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl DerefMut for Catalog {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
impl Catalog {
    pub fn new(locale: &str) -> Result<Self, String> {
        let mut catalog = proteus_i18n::Catalog::new(locale)?;
        catalog.add_resource("en", include_str!("locales/en.ftl"))?;
        catalog.add_resource("zh", include_str!("locales/zh.ftl"))?;
        Ok(Self(catalog))
    }
    /// Cell UI convenience; Lua and WASM use Fluent's typed arguments directly.
    pub fn t(&self, id: &str, values: &[(&str, &str)]) -> String {
        let mut args = FluentArgs::new();
        for (name, value) in values {
            if *name == "count" {
                args.set(*name, FluentValue::try_number(value));
            } else {
                args.set(*name, *value);
            }
        }
        self.format(id, Some(&args)).unwrap_or_else(|_| id.to_owned())
    }
}
pub fn defaults() -> Catalog {
    Catalog::new(&proteus_i18n::system_locale()).expect("valid built-in Fluent resources")
}
pub fn action_id(id: &str) -> String {
    format!("action-{}", id.replace(['.', '_'], "-"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn catalogs_cover_builtin_actions_and_have_unique_ids() {
        for (locale, source) in [("en", include_str!("locales/en.ftl")), ("zh", include_str!("locales/zh.ftl"))] {
            // Check each language independently, without masking gaps via English fallback.
            let mut catalog = proteus_i18n::Catalog::new(locale).unwrap();
            catalog.add_resource(locale, source).unwrap();
            for preset in [crate::modes::Preset::Zellij, crate::modes::Preset::Leader, crate::modes::Preset::Tmux] {
                let keymap = crate::modes::Keymap::new(&crate::modes::KeyConfig { preset, ..Default::default() });
                for mode in crate::modes::MODES {
                    for (_, id) in keymap.hints(mode) {
                        assert!(catalog.format(id, None).is_ok(), "missing {locale} hint: {id}");
                    }
                }
            }
            for action in crate::modes::Action::all() {
                let id = super::action_id(&action.name());
                assert!(catalog.format(&id, None).is_ok(), "missing {locale} action: {id}");
            }
        }
    }
}
