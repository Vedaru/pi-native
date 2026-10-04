use super::*;

#[test]
fn rewrites_named_imports_from_stubbed_packages() {
    let source = r#"import { Box, Editor as Ed } from "@earendil-works/pi-tui";
import typeboxDefault, { Type } from "typebox";
import { EventEmitter } from "node:events";
Box; Ed; typeboxDefault; Type; EventEmitter;"#;
    let out = transpile("plugin.ts", source).expect("transpiles");
    // Stubbed packages become a default import plus consts.
    assert!(!out.contains("import { Box"), "pi-tui not rewritten: {out}");
    assert!(
        !out.contains("import typeboxDefault, { Type } from"),
        "typebox not rewritten: {out}"
    );
    // Node builtins are real modules and keep their named imports.
    assert!(out.contains("from \"node:events\""), "{out}");
    assert!(out.contains("import { EventEmitter }"), "{out}");
}

#[test]
fn detects_transpilable_extensions() {
    assert!(needs_transpile("plugin.ts"));
    assert!(needs_transpile("plugin.tsx"));
    assert!(needs_transpile("plugin.jsx"));
    assert!(!needs_transpile("plugin.js"));
    assert!(!needs_transpile("plugin.mjs"));
}

#[test]
fn strips_type_annotations() {
    let source = "const x: number = 1;\ninterface Foo { a: string }\nfunction f(y: string): string { return y; }\nf('a');";
    let out = transpile("plugin.ts", source).expect("transpiles");
    assert!(!out.contains(": number"), "types remain: {out}");
    assert!(!out.contains("interface Foo"), "interface remains: {out}");
    assert!(out.contains("const x = 1"), "value lost: {out}");
}

#[test]
fn strips_enums_and_satisfies() {
    let source =
        "enum E { A, B }\nconst v = { a: 1 } satisfies Record<string, number>;\nexport { E, v };";
    let out = transpile("plugin.ts", source).expect("transpiles");
    assert!(!out.contains("satisfies"), "satisfies remains: {out}");
    assert!(out.contains("E"), "enum lost: {out}");
}

#[test]
fn strips_types_in_tsx_files() {
    // Mirrors the reference: `.tsx` is parsed as TypeScript; type syntax is
    // stripped. JSX syntax itself is not transformed at this stage.
    let source = "const n: number = 1;\nexport { n };";
    let out = transpile("plugin.tsx", source).expect("transpiles");
    assert!(!out.contains(": number"), "types remain: {out}");
    assert!(out.contains("const n = 1"), "value lost: {out}");
}

#[test]
fn reports_parse_errors_with_the_name() {
    let error = transpile("broken.ts", "const = ;").unwrap_err();
    assert!(error.contains("broken.ts"), "error: {error}");
}
