//! Fluent resources shared by the terminal, Lua config, and WASM plugins.
//!
//! Parsing, interpolation, select expressions, and CLDR plural rules are supplied
//! by Project Fluent. This adapter owns resources and the application's locale
//! preference/fallback policy.
use std::collections::BTreeMap;
use std::sync::Arc;

use fluent_bundle::{FluentResource, concurrent::FluentBundle};
use fluent_langneg::{NegotiationStrategy, negotiate_languages};
use unic_langid::LanguageIdentifier;

pub use fluent_bundle::{FluentArgs, FluentValue};

type Bundle = FluentBundle<Arc<FluentResource>>;

/// Accept POSIX environment locale names, then validate/canonicalize with unic-langid.
pub fn normalize(locale: &str) -> Result<String, String> {
    let locale = locale.split(['.', '@']).next().unwrap_or("").replace('_', "-");
    if locale.eq_ignore_ascii_case("c") || locale.eq_ignore_ascii_case("posix") {
        return Ok("en".into());
    }
    locale.parse::<LanguageIdentifier>().map(|id| id.to_string()).map_err(|e| format!("invalid locale: {e}"))
}

pub fn system_locale() -> String {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .find_map(|key| std::env::var(key).ok().filter(|s| !s.is_empty()).and_then(|s| normalize(&s).ok()))
        .unwrap_or_else(|| "en".into())
}

#[derive(Clone)]
struct Resources {
    sources: Vec<String>,
    bundle: Arc<Bundle>,
}

/// Compiled Fluent resources, negotiated with fluent-langneg and English fallback.
/// Clones share immutable compiled bundles; adding resources is copy-on-write.
#[derive(Clone)]
pub struct Catalog {
    locale: String,
    resources: BTreeMap<String, Resources>,
    fallbacks: Vec<String>,
}
impl std::fmt::Debug for Catalog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Catalog").field("locale", &self.locale).field("locales", &self.resources.keys()).finish()
    }
}
impl PartialEq for Catalog {
    fn eq(&self, other: &Self) -> bool {
        self.locale == other.locale
            && self
                .resources
                .iter()
                .map(|(k, r)| (k, &r.sources))
                .eq(other.resources.iter().map(|(k, r)| (k, &r.sources)))
    }
}
impl Eq for Catalog {}
impl Default for Catalog {
    fn default() -> Self {
        Self::new(&system_locale()).expect("validated system locale")
    }
}
impl Catalog {
    pub fn new(locale: &str) -> Result<Self, String> {
        Ok(Self { locale: normalize(locale)?, resources: BTreeMap::new(), fallbacks: vec!["en".into()] })
    }
    pub fn locale(&self) -> &str {
        &self.locale
    }
    pub fn set_locale(&mut self, locale: &str) -> Result<(), String> {
        self.locale = normalize(locale)?;
        self.negotiate();
        Ok(())
    }
    /// Load a standard `.ftl` resource. Later resources override existing entries.
    /// Syntax errors and duplicate entries within a resource reject the entire load.
    pub fn add_resource(&mut self, locale: &str, source: &str) -> Result<(), String> {
        let locale = normalize(locale)?;
        let resource =
            Arc::new(FluentResource::try_new(source.to_owned()).map_err(|(_, e)| format!("Fluent syntax: {e:?}"))?);
        let mut bundle = Self::bundle(&locale);
        bundle.add_resource(resource.clone()).map_err(|e| format!("Fluent resource: {e:?}"))?;
        let mut sources = Vec::new();
        if let Some(old) = self.resources.get(&locale) {
            bundle = Self::bundle(&locale);
            for source in &old.sources {
                let r = FluentResource::try_new(source.clone()).expect("previously validated Fluent resource");
                bundle.add_resource_overriding(Arc::new(r));
            }
            sources.clone_from(&old.sources);
            bundle.add_resource_overriding(resource);
        }
        sources.push(source.to_owned());
        self.resources.insert(locale, Resources { sources, bundle: Arc::new(bundle) });
        self.negotiate();
        Ok(())
    }
    /// Format a Fluent message (or `message.attribute`) using typed arguments.
    /// Missing messages/arguments and other Fluent resolution errors are reported.
    pub fn format(&self, id: &str, args: Option<&FluentArgs<'_>>) -> Result<String, String> {
        let (message, attribute) = id.split_once('.').map_or((id, None), |(m, a)| (m, Some(a)));
        for locale in &self.fallbacks {
            let Some(resource) = self.resources.get(locale) else { continue };
            let Some(msg) = resource.bundle.get_message(message) else { continue };
            let Some(pattern) = (match attribute {
                Some(a) => msg.get_attribute(a).map(|a| a.value()),
                None => msg.value(),
            }) else {
                continue;
            };
            let mut errors = Vec::new();
            let result = resource.bundle.format_pattern(pattern, args, &mut errors).into_owned();
            return if errors.is_empty() { Ok(result) } else { Err(format!("Fluent message {id}: {errors:?}")) };
        }
        Err(format!("missing Fluent message: {id}"))
    }
    fn bundle(locale: &str) -> Bundle {
        let mut bundle = Bundle::new_concurrent(vec![locale.parse().expect("validated locale")]);
        // Terminal text is positioned in cells; its renderer doesn't implement bidi.
        bundle.set_use_isolating(false);
        bundle
    }
    fn negotiate(&mut self) {
        let requested: LanguageIdentifier = self.locale.parse().expect("validated locale");
        let available: Vec<LanguageIdentifier> =
            self.resources.keys().map(|l| l.parse().expect("validated locale")).collect();
        let default: LanguageIdentifier = "en".parse().unwrap();
        self.fallbacks = negotiate_languages(&[requested], &available, Some(&default), NegotiationStrategy::Filtering)
            .into_iter()
            .map(ToString::to_string)
            .collect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fluent_terms_attributes_typed_arguments_and_fallback() {
        let mut c = Catalog::new("zh_Hans_CN.UTF-8").unwrap();
        c.add_resource("en", "fallback = English\n").unwrap();
        c.add_resource("zh", "-brand = Proteus\nhello = { -brand }：你好 { $name }\n    .title = 标题\n").unwrap();
        let mut args = FluentArgs::new();
        args.set("name", "{ $count }");
        assert_eq!(c.format("hello", Some(&args)).unwrap(), "Proteus：你好 { $count }");
        assert_eq!(c.format("hello.title", None).unwrap(), "标题");
        assert_eq!(c.format("fallback", None).unwrap(), "English");
        assert!(c.format("hello", None).is_err());
        assert!(c.format("missing", None).is_err());
        assert_eq!(c.locale(), "zh-Hans-CN");
        assert_eq!(normalize("C.UTF-8").unwrap(), "en");
        assert!(normalize("../bad").is_err());
    }
    #[test]
    fn cldr_polish_cardinals_and_decimal_operands() {
        let mut c = Catalog::new("pl-PL").unwrap();
        c.add_resource(
            "pl",
            "files = { $count ->\n    [one] one\n    [few] few\n    [many] many\n   *[other] other\n}\n",
        )
        .unwrap();
        for (n, expected) in [(1.0, "one"), (2.0, "few"), (5.0, "many"), (12.0, "many"), (22.0, "few"), (1.5, "other")]
        {
            let mut args = FluentArgs::new();
            args.set("count", n);
            assert_eq!(c.format("files", Some(&args)).unwrap(), expected);
        }
    }
    #[test]
    fn overrides_are_atomic_and_clones_keep_their_resources() {
        let mut c = Catalog::new("fr-CA").unwrap();
        c.add_resource("fr", "hello = bonjour\n").unwrap();
        c.add_resource("fr-CA", "hello = salut\n").unwrap();
        assert_eq!(c.format("hello", None).unwrap(), "salut");
        let old = c.clone();
        assert!(c.add_resource("fr-CA", "hello = broken\nbad = {\n").is_err());
        assert!(c.add_resource("fr-CA", "hello = first\nhello = second\n").is_err());
        assert_eq!(c, old);
        c.add_resource("fr-CA", "hello = allô\n").unwrap();
        assert_eq!(c.format("hello", None).unwrap(), "allô");
        assert_eq!(old.format("hello", None).unwrap(), "salut");
        c.set_locale("fr-FR").unwrap();
        assert_eq!(c.format("hello", None).unwrap(), "bonjour");
        fn send_sync<T: Send + Sync>() {}
        send_sync::<Catalog>();
    }
}
