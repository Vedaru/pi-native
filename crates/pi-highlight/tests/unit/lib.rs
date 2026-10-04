use super::*;

#[test]
fn highlights_rust_keywords_and_strings() {
    let lines = highlight("fn main() { let s = \"hi\"; }", "rs").expect("rust grammar");
    let scopes: Vec<Scope> = lines.iter().flatten().map(|span| span.scope).collect();
    assert!(scopes.contains(&Scope::Keyword), "no keyword: {lines:?}");
    assert!(scopes.contains(&Scope::String), "no string: {lines:?}");
}

#[test]
fn classifies_comments_and_numbers() {
    let lines = highlight("let x = 42; // note", "rs").expect("rust grammar");
    let scopes: Vec<Scope> = lines.iter().flatten().map(|span| span.scope).collect();
    assert!(scopes.contains(&Scope::Number), "no number: {lines:?}");
    assert!(scopes.contains(&Scope::Comment), "no comment: {lines:?}");
}

#[test]
fn highlights_python() {
    let lines = highlight("def f():\n    return 1\n", "python").expect("python grammar");
    let scopes: Vec<Scope> = lines.iter().flatten().map(|span| span.scope).collect();
    assert!(scopes.contains(&Scope::Keyword), "no keyword: {lines:?}");
}

#[test]
fn unknown_language_is_none() {
    assert!(highlight("x", "definitely-not-a-language").is_none());
}

#[test]
fn reports_supported_languages() {
    assert!(supports_language("rs"));
    assert!(supports_language("python"));
    assert!(!supports_language("definitely-not-a-language"));
}

#[test]
fn spans_reconstruct_the_source() {
    let source = "fn main() { let s = \"hi\"; }\n";
    let lines = highlight(source, "rs").expect("rust grammar");
    let rebuilt: String = lines
        .iter()
        .map(|line| {
            line.iter()
                .map(|span| span.text.as_str())
                .collect::<String>()
        })
        .collect();
    assert_eq!(rebuilt, source);
}
