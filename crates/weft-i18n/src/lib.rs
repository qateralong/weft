use fluent_bundle::concurrent::FluentBundle;
use std::collections::BTreeMap;
use std::sync::RwLock;

use fluent_bundle::{FluentArgs, FluentResource};
use unic_langid::{LanguageIdentifier, langid};

const EN: &str = include_str!("../locales/en.ftl");
const RU: &str = include_str!("../locales/ru.ftl");
const ES: &str = include_str!("../locales/es.ftl");
const AR: &str = include_str!("../locales/ar.ftl");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Language {
    English,
    Russian,
    Spanish,
    Arabic,
}

impl Language {
    pub const ALL: [Language; 4] = [Language::English, Language::Russian, Language::Spanish, Language::Arabic];

    /// The ISO 639-1 code.
    pub fn code(self) -> &'static str {
        match self {
            Language::English => "en",
            Language::Russian => "ru",
            Language::Spanish => "es",
            Language::Arabic => "ar",
        }
    }

    /// The language's name in itself.
    pub fn name(self) -> &'static str {
        match self {
            Language::English => "English",
            Language::Russian => "\u{0420}\u{0443}\u{0441}\u{0441}\u{043a}\u{0438}\u{0439}",
            Language::Spanish => "Espa\u{f1}ol",
            Language::Arabic => "\u{0627}\u{0644}\u{0639}\u{0631}\u{0628}\u{064a}\u{0629}",
        }
    }

    pub fn from_code(code: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|language| language.code() == code)
    }

    pub fn is_rtl(self) -> bool {
        self == Language::Arabic
    }

    pub fn from_env() -> Self {
        ["LC_ALL", "LC_MESSAGES", "LANG"]
            .into_iter()
            .filter_map(|name| std::env::var(name).ok())
            .find(|value| !value.is_empty())
            .or_else(sys_locale::get_locale)
            .map_or(Language::English, |locale| Self::from_locale(&locale))
    }

    pub fn from_locale(locale: &str) -> Self {
        let locale = locale.to_ascii_lowercase();
        Self::ALL.into_iter().find(|language| locale.starts_with(language.code())).unwrap_or(Language::English)
    }

    fn source(self) -> (&'static str, LanguageIdentifier) {
        match self {
            Language::English => (EN, langid!("en")),
            Language::Russian => (RU, langid!("ru")),
            Language::Spanish => (ES, langid!("es")),
            Language::Arabic => (AR, langid!("ar")),
        }
    }
}

/// Translates messages; the language can be switched while it is shared.
pub struct Localizer {
    current: RwLock<(Language, FluentBundle<FluentResource>)>,
    fallback: FluentBundle<FluentResource>,
}

impl Localizer {
    pub fn new(language: Language) -> Self {
        let (source, id) = language.source();
        Self { current: RwLock::new((language, bundle(source, id))), fallback: bundle(EN, langid!("en")) }
    }

    pub fn set_language(&self, language: Language) {
        let (source, id) = language.source();
        *self.current.write().unwrap_or_else(|poisoned| poisoned.into_inner()) = (language, bundle(source, id));
    }

    pub fn from_env() -> Self {
        Self::new(Language::from_env())
    }

    pub fn language(&self) -> Language {
        self.current.read().unwrap_or_else(|poisoned| poisoned.into_inner()).0
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
        let current = self.current.read().unwrap_or_else(|poisoned| poisoned.into_inner());
        for bundle in [&current.1, &self.fallback] {
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
        for source in [RU, ES, AR] {
            assert_eq!(ids(EN), ids(source));
        }
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
        assert_eq!(Language::from_locale("es_MX.UTF-8"), Language::Spanish);
        assert_eq!(Language::from_locale("ar_EG.UTF-8"), Language::Arabic);
    }

    #[test]
    fn switches_language() {
        let l = Localizer::new(Language::English);
        l.set_language(Language::Spanish);
        assert_eq!(l.language(), Language::Spanish);
        assert_eq!(l.tr("state-connected"), "conectado");
        assert_eq!(Language::from_code("ar"), Some(Language::Arabic));
        assert!(Language::Arabic.is_rtl());
    }
}
