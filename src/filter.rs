use crate::config::Source;
use crate::model::NewsItem;

/// Keep an item if its source is always-relevant, or if its title/snippet
/// matches one of the configured keywords (case-insensitive).
pub fn is_relevant(item: &NewsItem, src: &Source, keywords: &[String]) -> bool {
    if src.always_relevant {
        return true;
    }
    let haystack = format!("{} {}", item.title, item.snippet).to_lowercase();
    keywords.iter().any(|k| haystack.contains(&k.to_lowercase()))
}
