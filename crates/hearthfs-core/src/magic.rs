//! Magic path parsing.
//!
//! Magic paths use `@` as a separator: `file.txt@tokens`, `dir/@search/query`.
//! The `@` splits a real filesystem path from a virtual operation.

/// A parsed magic path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MagicPath<'a> {
    /// No magic — just a regular path component.
    Plain(&'a str),
    /// `file@tokens` — estimated LLM token count
    Tokens(&'a str),
    /// `file@outline` — structural outline
    Outline(&'a str),
    /// `file@summary` — short summary
    Summary(&'a str),
    /// `file@section/name` — extract a named section
    Section(&'a str, &'a str),
    /// `file@fit/budget` — best representation within a token budget
    Fit(&'a str, &'a str),
    /// `@search/query` — semantic search within a directory
    Search(&'a str),
}

/// Parse a filename component into a MagicPath.
///
/// Returns `Plain` if there's no `@` in the name.
pub fn parse(name: &str) -> MagicPath<'_> {
    let Some(at_pos) = name.find('@') else {
        return MagicPath::Plain(name);
    };

    let real = &name[..at_pos];
    let op = &name[at_pos + 1..];

    // `@search/query` — real part is empty (used at directory level)
    if let Some(query) = op.strip_prefix("search/") {
        return MagicPath::Search(query);
    }
    if op == "search" {
        return MagicPath::Search("");
    }

    match op {
        "tokens" => MagicPath::Tokens(real),
        "outline" => MagicPath::Outline(real),
        "summary" => MagicPath::Summary(real),
        "section" => MagicPath::Section(real, ""),
        _ if op.starts_with("section/") => {
            let section_name = &op[8..];
            MagicPath::Section(real, section_name)
        }
        "fit" => MagicPath::Fit(real, ""),
        _ if op.starts_with("fit/") => {
            let budget = &op[4..];
            MagicPath::Fit(real, budget)
        }
        // Unknown operation — treat as plain
        _ => MagicPath::Plain(name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plain() {
        assert_eq!(parse("hello.txt"), MagicPath::Plain("hello.txt"));
        assert_eq!(parse("subdir"), MagicPath::Plain("subdir"));
    }

    #[test]
    fn test_tokens() {
        assert_eq!(parse("main.rs@tokens"), MagicPath::Tokens("main.rs"));
    }

    #[test]
    fn test_outline() {
        assert_eq!(parse("main.rs@outline"), MagicPath::Outline("main.rs"));
    }

    #[test]
    fn test_summary() {
        assert_eq!(parse("doc.md@summary"), MagicPath::Summary("doc.md"));
    }

    #[test]
    fn test_section() {
        assert_eq!(
            parse("main.rs@section/my_function"),
            MagicPath::Section("main.rs", "my_function")
        );
    }

    #[test]
    fn test_fit() {
        assert_eq!(parse("doc.md@fit/2000"), MagicPath::Fit("doc.md", "2000"));
        assert_eq!(parse("doc.md@fit"), MagicPath::Fit("doc.md", ""));
    }

    #[test]
    fn test_search() {
        assert_eq!(parse("@search/auth flows"), MagicPath::Search("auth flows"));
        assert_eq!(parse("@search"), MagicPath::Search(""));
    }
}
