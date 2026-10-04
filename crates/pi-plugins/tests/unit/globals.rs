use super::*;

#[test]
fn prelude_defines_buffer_and_process() {
    assert!(PRELUDE.contains("class Buffer"));
    assert!(PRELUDE.contains("globalThis.process"));
}
