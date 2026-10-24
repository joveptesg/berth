//! Languages of books: ISO 639-1 codes from a fixed list, as the tags of files and the
//! admin give them.

/// Languages a book can have, in the order the forms list them.
pub const LANGUAGES: [&str; 12] = [
    "ru", "en", "uk", "be", "kk", "de", "fr", "es", "it", "pl", "zh", "ja",
];

/// The code of a tag value: `ru`, `rus`, `Russian`, `русский`…; `None` for anything else.
#[must_use]
pub fn normalize(value: &str) -> Option<&'static str> {
    let value = value.trim().to_lowercase();
    let code = match value.as_str() {
        "ru" | "rus" | "russian" | "русский" => "ru",
        "en" | "eng" | "english" | "английский" => "en",
        "uk" | "ukr" | "ukrainian" | "українська" | "украинский" => "uk",
        "be" | "bel" | "belarusian" | "беларуская" | "белорусский" => "be",
        "kk" | "kaz" | "kazakh" | "қазақ" | "казахский" => "kk",
        "de" | "deu" | "ger" | "german" | "deutsch" | "немецкий" => "de",
        "fr" | "fra" | "fre" | "french" | "français" | "французский" => "fr",
        "es" | "spa" | "spanish" | "español" | "испанский" => "es",
        "it" | "ita" | "italian" | "italiano" | "итальянский" => "it",
        "pl" | "pol" | "polish" | "polski" | "польский" => "pl",
        "zh" | "zho" | "chi" | "chinese" | "中文" | "китайский" => "zh",
        "ja" | "jpn" | "japanese" | "日本語" | "японский" => "ja",
        _ => return None,
    };
    Some(code)
}

/// A guess when the files say nothing: Cyrillic in the title or names is Russian.
#[must_use]
pub fn guess<'a>(texts: impl IntoIterator<Item = &'a str>) -> Option<&'static str> {
    texts
        .into_iter()
        .any(|t| t.chars().any(|c| ('\u{400}'..='\u{4ff}').contains(&c)))
        .then_some("ru")
}

/// Known codes of `list` ("en,ru"), deduplicated, in the order of [`LANGUAGES`].
#[must_use]
pub fn parse_list(list: &str) -> Vec<String> {
    let picked: Vec<&str> = list.split(',').map(str::trim).collect();
    LANGUAGES
        .iter()
        .filter(|code| picked.contains(code))
        .map(|code| (*code).to_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_from_tags_and_guesses() {
        assert_eq!(normalize(" RUS "), Some("ru"));
        assert_eq!(normalize("English"), Some("en"));
        assert_eq!(normalize("ger"), Some("de"));
        assert_eq!(normalize("und"), None);
        assert_eq!(guess(["Основание", "Айзек Азимов"]), Some("ru"));
        assert_eq!(guess(["Foundation"]), None);
        assert_eq!(parse_list("ru, xx,en,ru"), ["ru", "en"]);
    }
}
