//! Plugin-owned Fluent resources; independent of the browser's mutable state.
use proteus_plugin::i18n::{Catalog, FluentArgs};
use std::cell::RefCell;

pub const EN: &str = include_str!("../locales/en.ftl");
pub const ZH: &str = include_str!("../locales/zh.ftl");

fn catalog() -> Catalog {
    let mut c = Catalog::new("en").expect("valid locale");
    c.add_resource("en", EN).expect("valid English file-manager resource");
    c.add_resource("zh", ZH).expect("valid Chinese file-manager resource");
    c
}
thread_local! { static MESSAGES: RefCell<Catalog> = RefCell::new(catalog()); }

pub fn set_locale(locale: &str) -> Result<bool, String> {
    MESSAGES.with_borrow_mut(|c| {
        let old = c.locale().to_owned();
        c.set_locale(locale)?;
        Ok(c.locale() != old)
    })
}
pub fn format(id: &str, args: Option<&FluentArgs<'_>>) -> String {
    MESSAGES.with_borrow(|c| c.format(id, args).unwrap_or_else(|_| id.to_owned()))
}
macro_rules! tr {
    ($id:expr) => { $crate::i18n::format($id, None) };
    ($id:expr, $($name:ident = $value:expr),+ $(,)?) => {{
        let mut args = proteus_plugin::i18n::FluentArgs::new();
        $(args.set(stringify!($name), $value);)+
        $crate::i18n::format($id, Some(&args))
    }};
}
pub(crate) use tr;

#[cfg(test)]
pub(super) fn with_locale<T>(locale: &str, f: impl FnOnce() -> T) -> T {
    struct Reset(String);
    impl Drop for Reset {
        fn drop(&mut self) {
            let _ = set_locale(&self.0);
        }
    }
    let _reset = Reset(MESSAGES.with_borrow(|c| c.locale().to_owned()));
    set_locale(locale).unwrap();
    f()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn both_catalogs_cover_every_message_and_argument() {
        let mut expected = None;
        for (locale, source) in [("en", EN), ("zh", ZH)] {
            let mut c = Catalog::new(locale).unwrap();
            c.add_resource(locale, source).unwrap();
            let mut args = FluentArgs::new();
            for name in [
                "host", "name", "layout", "paths", "path", "key", "query", "size", "date", "program", "error", "done",
                "total", "mode",
            ] {
                args.set(name, "原样 { $name }");
            }
            for name in ["count", "width", "height", "failed", "names"] {
                args.set(name, 2);
            }
            let ids: std::collections::BTreeSet<_> = source
                .lines()
                .filter(|s| !s.starts_with(char::is_whitespace))
                .filter_map(|s| s.split_once(" = ").map(|(id, _)| id))
                .collect();
            for id in &ids {
                assert!(c.format(id, Some(&args)).is_ok(), "{locale}: {id}");
            }
            if let Some(expected) = &expected {
                assert_eq!(&ids, expected);
            } else {
                expected = Some(ids);
            }
        }
    }
    #[test]
    fn language_fallback_plurals_and_literal_controls() {
        with_locale("en", || {
            assert_eq!(tr!("inserted-paths", count = 1), "inserted 1 path");
            assert_eq!(tr!("inserted-paths", count = 2), "inserted 2 paths");
            assert_eq!(tr!("copied-paths", paths = "中文 { $count }"), "copied 中文 { $count }");
            assert!(set_locale("zh-CN").unwrap());
            assert!(!set_locale("zh-CN").unwrap());
            assert_eq!(tr!("inserted-paths", count = 2), "已插入 2 个路径");
            assert!(tr!("delete-prompt", count = 1).contains("yes"));
            let conflict = tr!("conflict", name = "file");
            for key in ["o", "s", "r", "O/S/R", "Esc"] {
                assert!(conflict.contains(key));
            }
            assert!(set_locale("../invalid").is_err());
            assert_eq!(tr!("title-files"), "文件");
            set_locale("de-DE").unwrap();
            assert_eq!(tr!("title-files"), "Files");
        });
    }
}
