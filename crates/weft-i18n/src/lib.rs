use fluent_bundle::concurrent::FluentBundle;
use std::collections::BTreeMap;

use fluent_bundle::{FluentArgs, FluentResource};
use unic_langid::{LanguageIdentifier, langid};

const EN: &str = include_str!("../locales/en.ftl");
const RU: &str = include_str!("../locales/ru.ftl");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Language {
    English,
    Russian,
}

impl Language {
    pub fn from_env() -> Self {
        ["LC_ALL", "LC_MESSAGES", "LANG"]
            .into_iter()
            .filter_map(|name| std::env::var(name).ok())
            .find(|value| !value.is_empty())
            .or_else(sys_locale::get_locale)
            .map_or(Language::English, |locale| Self::from_locale(&locale))
    }

    pub fn from_locale(locale: &str) -> Self {
        if locale.to_ascii_lowercase().starts_with("ru") { Language::Russian } else { Language::English }
    }
}

pub struct Localizer {
    language: Language,
    bundle: FluentBundle<FluentResource>,
    fallback: FluentBundle<FluentResource>,
}

impl Localizer {
    pub fn new(language: Language) -> Self {
        let (source, id) = match language {
            Language::English => (EN, langid!("en")),
            Language::Russian => (RU, langid!("ru")),
        };
        Self { language, bundle: bundle(source, id), fallback: bundle(EN, langid!("en")) }
    }

    pub fn from_env() -> Self {
        Self::new(Language::from_env())
    }

    pub fn language(&self) -> Language {
        self.language
    }

    /// Every message, with variables left as `{$name}`.
    pub fn catalog(&self) -> BTreeMap<String, String> {
        message_ids(EN).map(|id| (id.to_string(), self.tr(id))).collect()
    }

    pub fn tr(&self, id: &str) -> String {
        self.format(id, None)
    }

    pub fn tr_args(&self, id: &str, args: &[(&str, &str)]) -> String {
        let mut fluent_args = FluentArgs::new();
        for &(name, value) in args {
            fluent_args.set(name, value.to_string());
        }
        self.format(id, Some(&fluent_args))
    }

    fn format(&self, id: &str, args: Option<&FluentArgs<'_>>) -> String {
        for bundle in [&self.bundle, &self.fallback] {
            if let Some(pattern) = bundle.get_message(id).and_then(|message| message.value()) {
                let mut errors = Vec::new();
                return bundle.format_pattern(pattern, args, &mut errors).into_owned();
            }
        }
        id.to_string()
    }
}

fn message_ids(source: &str) -> impl Iterator<Item = &str> {
    source.lines().filter_map(|line| line.split_once(" = ").map(|(id, _)| id)).filter(|id| !id.starts_with(' '))
}

fn bundle(source: &str, id: LanguageIdentifier) -> FluentBundle<FluentResource> {
    let resource = FluentResource::try_new(source.to_string()).expect("valid fluent resource");
    let mut bundle = FluentBundle::new_concurrent(vec![id]);
    bundle.set_use_isolating(false);
    bundle.add_resource(resource).expect("unique fluent messages");
    bundle
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    fn ids(source: &str) -> BTreeSet<&str> {
        message_ids(source).collect()
    }

    #[test]
    fn locales_have_the_same_messages() {
        assert_eq!(ids(EN), ids(RU));
        assert!(ids(EN).len() > 40);
    }

    #[test]
    fn formats_with_arguments() {
        let ru = Localizer::new(Language::Russian);
        assert_eq!(ru.tr_args("done-create", &[("name", "друзья")]), "Сеть «друзья» создана");
        let en = Localizer::new(Language::English);
        assert_eq!(en.tr("state-connected"), "connected");
        assert_eq!(en.tr("no-such-message"), "no-such-message");
        let catalog = ru.catalog();
        assert_eq!(catalog["done-create"], "Сеть «{$name}» создана");
        assert_eq!(catalog.len(), ids(EN).len());
    }

    #[test]
    fn detects_language() {
        assert_eq!(Language::from_locale("ru_RU.UTF-8"), Language::Russian);
        assert_eq!(Language::from_locale("en_US.UTF-8"), Language::English);
        assert_eq!(Language::from_locale("C"), Language::English);
        assert_eq!(Language::from_locale("ru-RU"), Language::Russian);
    }
}
