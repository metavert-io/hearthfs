use std::path::Path;

use tree_sitter::Parser;
use tree_sitter_language::LanguageFn;

use crate::tokens;

/// A named section within a file's structural outline.
#[derive(Debug, Clone)]
pub struct Section {
    pub name: String,
    pub kind: String, // e.g. "function", "class", "struct", "heading"
    pub start_line: usize,
    pub end_line: usize,
    pub start_byte: usize,
    pub end_byte: usize,
    pub token_estimate: u64,
    pub depth: usize,
}

/// Parse a file and extract its structural outline.
pub fn outline(filename: &str, source: &[u8]) -> Vec<Section> {
    let ext = Path::new(filename)
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    match ext.as_str() {
        "md" | "markdown" => outline_markdown(source),
        _ => {
            if let Some(lang) = language_for_extension(&ext) {
                outline_code(source, lang)
            } else {
                vec![]
            }
        }
    }
}

/// Extract a named section's content from source bytes.
pub fn extract_section(filename: &str, source: &[u8], section_name: &str) -> Option<Vec<u8>> {
    let sections = outline(filename, source);
    sections
        .iter()
        .find(|s| s.name == section_name)
        .map(|s| source[s.start_byte..s.end_byte].to_vec())
}

/// Format an outline as human-readable text with token counts.
pub fn format_outline(filename: &str, source: &[u8]) -> String {
    let sections = outline(filename, source);
    if sections.is_empty() {
        let total_tokens = tokens::estimate_tokens(source);
        return format!("{filename} ({total_tokens} tokens, no structural outline)\n");
    }

    let total_tokens = tokens::estimate_tokens(source);
    let mut out = format!("# {filename} ({total_tokens} tokens)\n\n");
    for section in &sections {
        let indent = "  ".repeat(section.depth);
        out.push_str(&format!(
            "{indent}- {kind} `{name}` (L{start}-L{end}, ~{tokens} tokens)\n",
            kind = section.kind,
            name = section.name,
            start = section.start_line + 1,
            end = section.end_line + 1,
            tokens = section.token_estimate,
        ));
    }
    out
}

/// Render the best representation of a file within a token budget.
///
/// If the whole file fits, returns it unchanged. Otherwise includes top-level
/// sections in document order while they fit, replacing the rest with stubs
/// that name their `@section` path and token cost. Files with no structural
/// outline fall back to head truncation with a footer.
pub fn fit(filename: &str, source: &[u8], budget: u64) -> String {
    let total = tokens::estimate_tokens(source);
    if total <= budget {
        return String::from_utf8_lossy(source).into_owned();
    }

    let mut out = format!(
        "<!-- hearthfs @fit: {filename} is ~{total} tokens; this is a ~{budget}-token view. \
         Omitted parts list how to read them. -->\n\n"
    );
    let mut remaining = budget.saturating_sub(tokens::estimate_tokens(out.as_bytes()));

    let sections = outline(filename, source);
    let Some(min_depth) = sections.iter().map(|s| s.depth).min() else {
        // No structure: head truncation. ~3.5 chars per token, minus footer room.
        let take_chars = (remaining.saturating_sub(40) * 7 / 2) as usize;
        let text = String::from_utf8_lossy(source);
        let head: String = text.chars().take(take_chars).collect();
        let shown = tokens::estimate_tokens(head.as_bytes());
        out.push_str(&head);
        out.push_str(&format!(
            "\n\n<!-- truncated: ~{} more tokens. Full file: {filename} -->\n",
            total.saturating_sub(shown)
        ));
        return out;
    };

    let top: Vec<&Section> = sections.iter().filter(|s| s.depth == min_depth).collect();

    // Content before the first section (frontmatter, imports, intro prose).
    let first_start = top.first().map(|s| s.start_byte).unwrap_or(0);
    if first_start > 0 {
        let preamble = &source[..first_start.min(source.len())];
        let cost = tokens::estimate_tokens(preamble);
        if cost <= remaining {
            out.push_str(&String::from_utf8_lossy(preamble));
            remaining = remaining.saturating_sub(cost);
        }
    }

    fill_sections(filename, source, &sections, &top, &mut remaining, &mut out, 4);
    out
}

const STUB_COST: u64 = 20;

/// Greedily emit sections that fit the remaining budget, in document order.
/// A section that doesn't fit is opened up: its intro and child sections are
/// tried individually (down to `depth_left` levels) before falling back to a
/// stub naming its `@section` path.
fn fill_sections(
    filename: &str,
    source: &[u8],
    all: &[Section],
    list: &[&Section],
    remaining: &mut u64,
    out: &mut String,
    depth_left: u8,
) {
    for section in list {
        let end = section.end_byte.min(source.len());
        if section.start_byte >= end {
            continue;
        }
        if section.token_estimate <= *remaining {
            out.push_str(&String::from_utf8_lossy(&source[section.start_byte..end]));
            out.push('\n');
            *remaining = remaining.saturating_sub(section.token_estimate);
            continue;
        }

        let child_depth = all
            .iter()
            .filter(|c| c.start_byte >= section.start_byte && c.end_byte <= end && c.depth > section.depth)
            .map(|c| c.depth)
            .min();
        if let (Some(child_depth), true) = (child_depth, depth_left > 0) {
            let children: Vec<&Section> = all
                .iter()
                .filter(|c| {
                    c.depth == child_depth && c.start_byte >= section.start_byte && c.end_byte <= end
                })
                .collect();
            // Intro: the section's own text before its first child.
            let first_child = children.first().map(|c| c.start_byte).unwrap_or(end);
            let intro = &source[section.start_byte..first_child.min(end)];
            let intro_cost = tokens::estimate_tokens(intro);
            if intro_cost <= *remaining {
                out.push_str(&String::from_utf8_lossy(intro));
                *remaining = remaining.saturating_sub(intro_cost);
            }
            fill_sections(filename, source, all, &children, remaining, out, depth_left - 1);
            continue;
        }

        out.push_str(&format!(
            "<!-- omitted: {kind} '{name}' (~{tokens} tokens) — cat '{filename}@section/{name}' -->\n",
            kind = section.kind,
            name = section.name,
            tokens = section.token_estimate,
        ));
        *remaining = remaining.saturating_sub(STUB_COST);
    }
}

// --- Code parsing via tree-sitter ---

fn outline_code(source: &[u8], lang: LanguageFn) -> Vec<Section> {
    let mut parser = Parser::new();
    parser
        .set_language(&lang.into())
        .expect("failed to set language");

    let Some(tree) = parser.parse(source, None) else {
        return vec![];
    };

    let mut sections = Vec::new();
    collect_nodes(source, tree.root_node(), 0, &mut sections);
    sections
}

fn collect_nodes(
    source: &[u8],
    node: tree_sitter::Node<'_>,
    depth: usize,
    sections: &mut Vec<Section>,
) {
    // Extract named definitions based on node kind
    if let Some(name) = extract_definition_name(source, &node) {
        let kind = normalize_kind(node.kind());
        let start_byte = node.start_byte();
        let end_byte = node.end_byte();
        let content = &source[start_byte..end_byte];
        sections.push(Section {
            name,
            kind,
            start_line: node.start_position().row,
            end_line: node.end_position().row,
            start_byte,
            end_byte,
            token_estimate: tokens::estimate_tokens(content),
            depth,
        });
    }

    // Recurse into children for nested definitions
    let child_depth = if is_container_node(node.kind()) {
        depth + 1
    } else {
        depth
    };

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_nodes(source, child, child_depth, sections);
    }
}

/// Try to extract a definition name from a tree-sitter node.
fn extract_definition_name(source: &[u8], node: &tree_sitter::Node<'_>) -> Option<String> {
    match node.kind() {
        // Rust
        "function_item" | "struct_item" | "enum_item" | "trait_item"
        | "type_item" | "const_item" | "static_item" | "mod_item" | "macro_definition"
        // Python + C/C++
        | "function_definition" | "class_definition"
        // JavaScript/TypeScript/Go/Java
        | "function_declaration" | "class_declaration" | "method_definition"
        | "lexical_declaration" | "variable_declaration"
        | "method_declaration" | "type_declaration"
        // Java
        | "interface_declaration" | "enum_declaration" | "constructor_declaration"
        // C/C++
        | "struct_specifier" | "enum_specifier" | "class_specifier" | "namespace_definition"
        // Ruby
        | "method" | "singleton_method" | "class" | "module" => {
            find_name_child(source, node)
        }

        // impl blocks in Rust: use the type name
        "impl_item" => {
            find_child_by_kind(source, node, "type_identifier")
        }

        _ => None,
    }
}

/// Find a child node named "name" or "identifier" and return its text.
fn find_name_child(source: &[u8], node: &tree_sitter::Node<'_>) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "name" | "identifier" | "type_identifier" | "property_identifier"
            | "field_identifier" | "constant" => {
                let text = child.utf8_text(source).ok()?;
                return Some(text.to_string());
            }
            _ => {}
        }
    }
    None
}

fn find_child_by_kind(
    source: &[u8],
    node: &tree_sitter::Node<'_>,
    kind: &str,
) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == kind {
            return child.utf8_text(source).ok().map(|s| s.to_string());
        }
    }
    None
}

fn normalize_kind(kind: &str) -> String {
    match kind {
        "function_item" | "function_definition" | "function_declaration" | "method"
        | "method_definition" | "method_declaration" | "singleton_method"
        | "constructor_declaration" => "function".to_string(),
        "struct_item" | "struct_specifier" => "struct".to_string(),
        "enum_item" | "enum_declaration" | "enum_specifier" => "enum".to_string(),
        "class_definition" | "class_declaration" | "class_specifier" | "class" => {
            "class".to_string()
        }
        "trait_item" | "interface_declaration" => "trait".to_string(),
        "impl_item" => "impl".to_string(),
        "mod_item" | "module" | "namespace_definition" => "module".to_string(),
        "type_item" | "type_declaration" => "type".to_string(),
        "const_item" | "static_item" => "const".to_string(),
        "macro_definition" => "macro".to_string(),
        "lexical_declaration" | "variable_declaration" => "variable".to_string(),
        _ => kind.to_string(),
    }
}

fn is_container_node(kind: &str) -> bool {
    matches!(
        kind,
        "class_definition"
            | "class_declaration"
            | "class_specifier"
            | "class"
            | "impl_item"
            | "trait_item"
            | "interface_declaration"
            | "mod_item"
            | "module"
            | "namespace_definition"
    )
}

fn language_for_extension(ext: &str) -> Option<LanguageFn> {
    match ext {
        "rs" => Some(tree_sitter_rust::LANGUAGE),
        "py" | "pyi" => Some(tree_sitter_python::LANGUAGE),
        "js" | "mjs" | "cjs" | "jsx" => Some(tree_sitter_javascript::LANGUAGE),
        "ts" | "mts" | "cts" => Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT),
        "tsx" => Some(tree_sitter_typescript::LANGUAGE_TSX),
        "go" => Some(tree_sitter_go::LANGUAGE),
        "java" => Some(tree_sitter_java::LANGUAGE),
        "c" | "h" => Some(tree_sitter_c::LANGUAGE),
        "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" => Some(tree_sitter_cpp::LANGUAGE),
        "rb" => Some(tree_sitter_ruby::LANGUAGE),
        _ => None,
    }
}

// --- Markdown outline (heading-based, no tree-sitter needed) ---

fn outline_markdown(source: &[u8]) -> Vec<Section> {
    let text = String::from_utf8_lossy(source);
    let mut sections = Vec::new();
    let lines: Vec<&str> = text.lines().collect();

    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with('#') {
            continue;
        }
        let level = trimmed.chars().take_while(|&c| c == '#').count();
        let heading = trimmed[level..].trim().to_string();
        if heading.is_empty() {
            continue;
        }

        // Find where this section ends (next heading of same or higher level, or EOF)
        let end_line = lines[i + 1..]
            .iter()
            .enumerate()
            .find(|(_, l)| {
                let t = l.trim_start();
                if !t.starts_with('#') {
                    return false;
                }
                let l = t.chars().take_while(|&c| c == '#').count();
                l <= level
            })
            .map(|(idx, _)| i + idx) // line before the next heading
            .unwrap_or(lines.len() - 1);

        let start_byte = lines[..i].iter().map(|l| l.len() + 1).sum::<usize>();
        let end_byte = lines[..=end_line]
            .iter()
            .map(|l| l.len() + 1)
            .sum::<usize>()
            .min(source.len());
        let content = &source[start_byte..end_byte];

        sections.push(Section {
            name: heading,
            kind: format!("h{level}"),
            start_line: i,
            end_line,
            start_byte,
            end_byte,
            token_estimate: tokens::estimate_tokens(content),
            depth: level.saturating_sub(1),
        });
    }

    sections
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rust_outline() {
        let src = br#"
fn main() {
    println!("hello");
}

struct Foo {
    x: i32,
}

impl Foo {
    fn bar(&self) -> i32 {
        self.x
    }
}

enum Color {
    Red,
    Green,
    Blue,
}
"#;
        let sections = outline("test.rs", src);
        let names: Vec<&str> = sections.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"main"), "missing main: {:?}", names);
        assert!(names.contains(&"Foo"), "missing Foo: {:?}", names);
        assert!(names.contains(&"bar"), "missing bar: {:?}", names);
        assert!(names.contains(&"Color"), "missing Color: {:?}", names);
    }

    #[test]
    fn test_python_outline() {
        let src = br#"
class MyClass:
    def __init__(self):
        pass

    def method(self):
        return 42

def standalone():
    pass
"#;
        let sections = outline("test.py", src);
        let names: Vec<&str> = sections.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"MyClass"), "missing MyClass: {:?}", names);
        assert!(names.contains(&"__init__"), "missing __init__: {:?}", names);
        assert!(names.contains(&"method"), "missing method: {:?}", names);
        assert!(
            names.contains(&"standalone"),
            "missing standalone: {:?}",
            names
        );
    }

    #[test]
    fn test_markdown_outline() {
        let src = b"# Title\nSome intro.\n## Section A\nContent A.\n## Section B\nContent B.\n### Subsection\nMore.\n";
        let sections = outline("doc.md", src);
        assert_eq!(sections.len(), 4);
        assert_eq!(sections[0].name, "Title");
        assert_eq!(sections[0].kind, "h1");
        assert_eq!(sections[1].name, "Section A");
        assert_eq!(sections[2].name, "Section B");
        assert_eq!(sections[3].name, "Subsection");
        assert_eq!(sections[3].kind, "h3");
    }

    #[test]
    fn test_extract_section() {
        let src = br#"
fn foo() {
    1
}

fn bar() {
    2
}
"#;
        let content = extract_section("test.rs", src, "bar").unwrap();
        let text = std::str::from_utf8(&content).unwrap();
        assert!(text.contains("fn bar()"), "got: {text}");
        assert!(text.contains("2"), "got: {text}");
    }

    #[test]
    fn test_format_outline() {
        let src = b"fn hello() { 42 }\n";
        let out = format_outline("test.rs", src);
        assert!(out.contains("function"), "got: {out}");
        assert!(out.contains("`hello`"), "got: {out}");
        assert!(out.contains("tokens"), "got: {out}");
    }

    #[test]
    fn test_fit_whole_file_when_within_budget() {
        let src = b"# Small\n\nA tiny file.\n";
        let out = fit("small.md", src, 10_000);
        assert_eq!(out, String::from_utf8_lossy(src));
    }

    #[test]
    fn test_fit_stubs_sections_over_budget() {
        let mut src = String::from("# Alpha\n\nshort intro\n\n");
        src.push_str("# Beta\n\n");
        src.push_str(&"beta content word ".repeat(500));
        src.push_str("\n\n# Gamma\n\ntail\n");

        let out = fit("doc.md", src.as_bytes(), 150);
        assert!(out.contains("hearthfs @fit"), "got: {out}");
        assert!(out.contains("short intro"), "got: {out}");
        assert!(out.contains("omitted: h1 'Beta'"), "got: {out}");
        assert!(out.contains("doc.md@section/Beta"), "got: {out}");
        // Gamma is small and later; greedy in-order fill should include it.
        assert!(out.contains("tail"), "got: {out}");
    }

    #[test]
    fn test_fit_truncates_unstructured_files() {
        let src = "word ".repeat(2000);
        let out = fit("notes.txt", src.as_bytes(), 100);
        assert!(out.contains("truncated"), "got: {out}");
        let shown = tokens::estimate_tokens(out.as_bytes());
        assert!(shown < 200, "fit output too large: ~{shown} tokens");
    }
}
