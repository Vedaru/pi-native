//! TypeScript/JSX transpilation for the plugin host.
//!
//! Mirrors the reference port's pipeline (`extensions_js.rs`): parse as
//! TypeScript, run the resolver, strip type-only syntax, and emit JavaScript.
//! This replaces `jiti` so `.ts`/`.tsx` plugin entrypoints load without a
//! separate build step.
//!
//! Note: like the reference, this strips types but does **not** transform JSX
//! syntax; a `.tsx` file that contains actual JSX is parsed but the JSX itself
//! is emitted unchanged and will not run in QuickJS. Add a JSX transform only
//! if real plugins need it.

use std::path::Path;

use swc_common::{sync::Lrc, FileName, Globals, Mark, SourceMap, SyntaxContext, DUMMY_SP, GLOBALS};
use swc_ecma_ast::{
    BindingIdent, Decl, Expr, Ident, IdentName, ImportDecl, ImportDefaultSpecifier,
    ImportSpecifier, MemberExpr, MemberProp, Module as SwcModule, ModuleDecl, ModuleExportName,
    ModuleItem, Pass, Pat, Program as SwcProgram, Stmt, VarDecl, VarDeclKind, VarDeclarator,
};
use swc_ecma_codegen::{text_writer::JsWriter, Emitter};
use swc_ecma_parser::{Parser as SwcParser, StringInput, Syntax, TsSyntax};
use swc_ecma_transforms_base::resolver;
use swc_ecma_transforms_typescript::strip;

/// Extensions that need transpilation before QuickJS can run them.
pub fn needs_transpile(name: &str) -> bool {
    let ext = Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "ts" | "tsx" | "mts" | "cts" | "jsx"
    )
}

/// Transpile a TypeScript/JSX source string to plain JavaScript.
///
/// `name` is used for syntax selection (`.tsx`/`.jsx` enable JSX) and error
/// messages. Plain `.js` sources can be passed through unchanged by callers.
pub fn transpile(name: &str, source: &str) -> Result<String, String> {
    let cm: Lrc<SourceMap> = Default::default();
    let globals = Globals::new();

    GLOBALS.set(&globals, || {
        let fm = cm.new_source_file(
            FileName::Custom(name.to_string()).into(),
            source.to_string(),
        );

        let tsx = Path::new(name)
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("tsx") || ext.eq_ignore_ascii_case("jsx"));

        let syntax = Syntax::Typescript(TsSyntax {
            tsx,
            decorators: true,
            ..Default::default()
        });

        let mut parser = SwcParser::new(syntax, StringInput::from(&*fm), None);
        let module: SwcModule = parser
            .parse_module()
            .map_err(|err| format!("parse {name}: {err:?}"))?;

        let unresolved_mark = Mark::new();
        let top_level_mark = Mark::new();
        let mut program = SwcProgram::Module(module);
        resolver(unresolved_mark, top_level_mark, false).process(&mut program);
        strip(unresolved_mark, top_level_mark).process(&mut program);

        if let SwcProgram::Module(module) = &mut program {
            rewrite_stub_imports(module, is_stub_specifier);
        }

        let SwcProgram::Module(module) = program else {
            return Err(format!("transpile {name}: expected a module"));
        };

        let mut buf = Vec::new();
        {
            let mut emitter = Emitter {
                cfg: swc_ecma_codegen::Config::default(),
                comments: None,
                cm: cm.clone(),
                wr: JsWriter::new(cm, "\n", &mut buf, None),
            };
            emitter
                .emit_module(&module)
                .map_err(|err| format!("emit {name}: {err}"))?;
        }

        String::from_utf8(buf).map_err(|err| format!("utf8 {name}: {err}"))
    })
}

/// Bare specifiers resolve to a proxy stub in the plugin loader, so their named
/// imports must be rewritten to property accesses. `node:` builtins, relative
/// paths, and URLs are real modules and keep their named imports.
fn is_stub_specifier(spec: &str) -> bool {
    !(spec.starts_with("node:")
        || spec.starts_with('.')
        || spec.starts_with('/')
        || spec.starts_with("file:")
        || spec.starts_with("http:")
        || spec.starts_with("https:"))
}

fn ident(name: &str) -> Ident {
    Ident::new(name.into(), DUMMY_SP, SyntaxContext::empty())
}

fn member(obj: &Ident, prop: &str) -> Expr {
    Expr::Member(MemberExpr {
        span: DUMMY_SP,
        obj: Box::new(Expr::Ident(obj.clone())),
        prop: MemberProp::Ident(IdentName::new(prop.into(), DUMMY_SP)),
    })
}

fn const_decl(local: Ident, init: Expr) -> ModuleItem {
    ModuleItem::Stmt(Stmt::Decl(Decl::Var(Box::new(VarDecl {
        span: DUMMY_SP,
        ctxt: SyntaxContext::empty(),
        kind: VarDeclKind::Const,
        declare: false,
        decls: vec![VarDeclarator {
            span: DUMMY_SP,
            name: Pat::Ident(BindingIdent {
                id: local,
                type_ann: None,
            }),
            init: Some(Box::new(init)),
            definite: false,
        }],
    }))))
}

/// Rewrite named/namespace imports from stubbed packages into a default import
/// plus `const` destructuring, so a proxy default satisfies arbitrary names.
/// Static ESM named imports require the names to exist; a proxy cannot declare
/// them, so we turn them into runtime property reads.
pub fn rewrite_stub_imports(module: &mut SwcModule, is_stub: fn(&str) -> bool) {
    let mut out = Vec::with_capacity(module.body.len());
    let mut counter = 0usize;
    for item in module.body.drain(..) {
        let ModuleItem::ModuleDecl(ModuleDecl::Import(import)) = &item else {
            out.push(item);
            continue;
        };
        if !is_stub(import.src.value.as_str().unwrap_or("")) || import.specifiers.is_empty() {
            out.push(item);
            continue;
        }
        counter += 1;
        let ns = ident(&format!("__pi_ns_{counter}"));
        let mut consts = Vec::new();
        for spec in &import.specifiers {
            match spec {
                ImportSpecifier::Default(default) => {
                    consts.push(const_decl(default.local.clone(), member(&ns, "default")));
                }
                ImportSpecifier::Named(named) => {
                    let imported = match &named.imported {
                        Some(ModuleExportName::Ident(identifier)) => identifier.sym.to_string(),
                        Some(ModuleExportName::Str(string)) => {
                            string.value.to_string_lossy().into_owned()
                        }
                        None => named.local.sym.to_string(),
                    };
                    consts.push(const_decl(named.local.clone(), member(&ns, &imported)));
                }
                ImportSpecifier::Namespace(namespace) => {
                    consts.push(const_decl(namespace.local.clone(), Expr::Ident(ns.clone())));
                }
            }
        }
        let default_import = ImportDecl {
            span: DUMMY_SP,
            specifiers: vec![ImportSpecifier::Default(ImportDefaultSpecifier {
                span: DUMMY_SP,
                local: ns,
            })],
            src: import.src.clone(),
            type_only: false,
            with: None,
            phase: Default::default(),
        };
        out.push(ModuleItem::ModuleDecl(ModuleDecl::Import(default_import)));
        out.extend(consts);
    }
    module.body = out;
}

#[cfg(test)]
mod tests {
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
        let source = "enum E { A, B }\nconst v = { a: 1 } satisfies Record<string, number>;\nexport { E, v };";
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
}
