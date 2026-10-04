//! Native syntax highlighting.
//!
//! Replaces highlight.js. pi's renderer consumes scopes (its `hljs-*` classes)
//! and maps them to theme colors; this crate produces the same idea natively:
//! a sequence of `(scope, text)` spans, where `scope` is a normalized category
//! the theme can style.
//!
//! Scope classification is a mapping table from TextMate scopes (via syntect) to
//! pi's category names — one rule per category, not per language.

use syntect::parsing::{ParseState, ScopeStack, SyntaxSet};

/// The categories a theme can style. The names match pi's highlight theme keys
/// (`buildCliHighlightTheme`), so the TUI theme can look a span's category up
/// directly. [`Scope::Plain`] has no key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Keyword,
    BuiltIn,
    Literal,
    Number,
    Regexp,
    String,
    Comment,
    DocTag,
    Meta,
    Function,
    Title,
    Class,
    Type,
    Tag,
    Name,
    Attr,
    Variable,
    Params,
    Operator,
    Punctuation,
    Plain,
}

impl Scope {
    /// pi's highlight theme key for this category (`""` means plain).
    pub fn theme_key(self) -> &'static str {
        match self {
            Scope::Keyword => "keyword",
            Scope::BuiltIn => "built_in",
            Scope::Literal => "literal",
            Scope::Number => "number",
            Scope::Regexp => "regexp",
            Scope::String => "string",
            Scope::Comment => "comment",
            Scope::DocTag => "doctag",
            Scope::Meta => "meta",
            Scope::Function => "function",
            Scope::Title => "title",
            Scope::Class => "class",
            Scope::Type => "type",
            Scope::Tag => "tag",
            Scope::Name => "name",
            Scope::Attr => "attr",
            Scope::Variable => "variable",
            Scope::Params => "params",
            Scope::Operator => "operator",
            Scope::Punctuation => "punctuation",
            Scope::Plain => "",
        }
    }
}

/// A highlighted span of source text.
#[derive(Debug, Clone, PartialEq)]
pub struct Span {
    pub scope: Scope,
    pub text: String,
}

/// Whether the highlighter has a grammar for `language`.
pub fn supports_language(language: &str) -> bool {
    let syntaxes = SyntaxSet::load_defaults_newlines();
    syntaxes.find_syntax_by_token(language).is_some()
}

/// Classify a TextMate scope stack into a theme category. Most specific first.
fn classify(scope: &ScopeStack) -> Scope {
    let text = scope.to_string();
    if text.contains("comment") {
        if text.contains("doctag") {
            Scope::DocTag
        } else {
            Scope::Comment
        }
    } else if text.contains("regexp") {
        Scope::Regexp
    } else if text.contains("string") || text.contains("char") {
        Scope::String
    } else if text.contains("constant.numeric") {
        Scope::Number
    } else if text.contains("constant.language") || text.contains("constant") {
        Scope::Literal
    } else if text.contains("keyword.operator") {
        Scope::Operator
    } else if text.contains("keyword") {
        Scope::Keyword
    } else if text.contains("storage") {
        // TextMate puts `fn`/`let`/`def` in `storage`; highlight.js (pi) colors
        // these as keywords.
        Scope::Keyword
    } else if text.contains("support.type") || text.contains("support.class") {
        Scope::BuiltIn
    } else if text.contains("entity.name.function") || text.contains("support.function") {
        Scope::Function
    } else if text.contains("entity.name.class") {
        Scope::Class
    } else if text.contains("entity.name.type") {
        Scope::Type
    } else if text.contains("entity.name.tag") {
        Scope::Tag
    } else if text.contains("entity.other.attribute-name") {
        Scope::Attr
    } else if text.contains("entity.name") {
        Scope::Name
    } else if text.contains("variable.parameter") {
        Scope::Params
    } else if text.contains("variable") {
        Scope::Variable
    } else if text.contains("punctuation") {
        Scope::Punctuation
    } else {
        // TextMate `meta.*` scopes wrap whole regions (including whitespace), so
        // treat them as plain rather than pi's `meta` (muted).
        Scope::Plain
    }
}

/// Highlight `code` as `language`, returning spans per line.
///
/// Returns `None` when no grammar matches `language`.
pub fn highlight(code: &str, language: &str) -> Option<Vec<Vec<Span>>> {
    let syntaxes = SyntaxSet::load_defaults_newlines();
    let syntax = syntaxes.find_syntax_by_token(language)?;
    let mut parse_state = ParseState::new(syntax);
    let mut lines = Vec::new();

    for line in syntect::util::LinesWithEndings::from(code) {
        let ops = parse_state.parse_line(line, &syntaxes).ok()?;
        let mut spans: Vec<Span> = Vec::new();
        let mut stack = ScopeStack::new();
        let mut last = 0usize;
        for (offset, op) in ops {
            if offset > last {
                push_span(&mut spans, classify(&stack), &line[last..offset]);
            }
            stack.apply(&op).ok()?;
            last = offset;
        }
        if last < line.len() {
            push_span(&mut spans, classify(&stack), &line[last..]);
        }
        lines.push(spans);
    }

    Some(lines)
}

fn push_span(spans: &mut Vec<Span>, scope: Scope, text: &str) {
    if text.is_empty() {
        return;
    }
    match spans.last_mut() {
        Some(last) if last.scope == scope => last.text.push_str(text),
        _ => spans.push(Span {
            scope,
            text: text.to_string(),
        }),
    }
}

#[cfg(test)]
#[path = "../tests/unit/lib.rs"]
mod tests;
