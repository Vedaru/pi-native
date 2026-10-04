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

use swc_common::{sync::Lrc, FileName, Globals, Mark, SourceMap, GLOBALS};
use swc_ecma_ast::{Module as SwcModule, Pass, Program as SwcProgram};
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

#[cfg(test)]
mod tests {
    use super::*;

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
