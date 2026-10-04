use super::*;

#[test]
fn canonicalizes_node_builtins() {
    assert_eq!(canonical_node_builtin("fs"), Some("node:fs"));
    assert_eq!(canonical_node_builtin("node:path"), Some("node:path"));
    assert_eq!(canonical_node_builtin("three"), None);
}

#[test]
fn resolves_relative_paths() {
    assert_eq!(
        resolve_relative("src/plugin.ts", "./util.ts"),
        "src/util.ts"
    );
    assert_eq!(resolve_relative("src/plugin.ts", "../lib/x.js"), "lib/x.js");
}

#[test]
fn provides_path_os_fs_and_process_virtual_modules() {
    assert!(virtual_module_source("node:path").is_some());
    assert!(virtual_module_source("node:os").is_some());
    assert!(virtual_module_source("node:process").is_some());
    assert!(virtual_module_source("node:fs").is_some());
    assert!(virtual_module_source("node:fs/promises").is_some());
    assert!(virtual_module_source("node:crypto").is_some());
    assert!(virtual_module_source("node:events").is_some());
    assert!(virtual_module_source("node:child_process").is_some());
    assert!(virtual_module_source("node:buffer").is_some());
    assert!(virtual_module_source("node:url").is_some());
    assert!(virtual_module_source("node:module").is_some());
    assert!(virtual_module_source("node:util").is_some());
    assert!(virtual_module_source("node:zlib").is_some());
    assert!(virtual_module_source("node:readline").is_some());
    assert!(virtual_module_source("some-unknown-pkg").is_none());
}

#[test]
fn bare_specifier_gets_a_generic_proxy_stub() {
    let source = stub_module_source("some-npm-pkg");
    assert!(source.contains("export default __stub"));
    // Generic: no per-package or per-export branches.
    assert!(!source.contains("Type:"));
    assert!(!source.contains("StringEnum"));
}

#[test]
fn all_bare_packages_are_stubs() {
    assert!(virtual_module_source("typebox").is_none());
    assert!(virtual_module_source("@earendil-works/pi-ai").is_none());
    assert!(virtual_module_source("node:events").is_some());
}
