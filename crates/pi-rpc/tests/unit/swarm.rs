use super::*;
use crate::Event;

/// A scratch swarm directory that removes itself on drop, so a test run does
/// not litter `/tmp` with `pi-swarm-*` directories.
struct Scratch(PathBuf);

impl Scratch {
    fn path(&self) -> PathBuf {
        self.0.clone()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn scratch(tag: &str) -> Scratch {
    let dir = std::env::temp_dir().join(format!("pi-swarm-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    Scratch(dir)
}

fn tool(name: &str, input: serde_json::Value) -> Event {
    Event::ToolStart {
        tool_call_id: "t".into(),
        name: name.into(),
        input,
    }
}

#[test]
fn a_peer_outlet_becomes_one_short_block_and_is_read_once() {
    let dir = scratch("roundtrip");
    let mut a = Swarm::join(dir.path(), "a".into()).unwrap();
    let mut b = Swarm::join(dir.path(), "b".into()).unwrap();
    a.emit(&Event::AssistantText {
        text: "hello   world".into(),
    });
    assert_eq!(
        b.poll(),
        Some("<shouts>\n[a] says: hello world\n</shouts>".to_string())
    );
    assert_eq!(b.poll(), None, "the read offset must advance");
}

#[test]
fn a_unit_never_reads_its_own_outlet() {
    let dir = scratch("own");
    let mut a = Swarm::join(dir.path(), "a".into()).unwrap();
    a.emit(&Event::AssistantText {
        text: "self".into(),
    });
    assert_eq!(a.poll(), None);
}

#[test]
fn reads_are_noise_and_tool_args_are_summarised() {
    let dir = scratch("summary");
    let mut a = Swarm::join(dir.path(), "a".into()).unwrap();
    let mut b = Swarm::join(dir.path(), "b".into()).unwrap();
    a.emit(&tool("read", serde_json::json!({ "path": "x" })));
    assert_eq!(b.poll(), None, "a read is not worth shouting");
    a.emit(&tool(
        "bash",
        serde_json::json!({ "command": "cargo test", "timeout": 10 }),
    ));
    assert_eq!(
        b.poll(),
        Some("<shouts>\n[a] tool: bash cargo test\n</shouts>".to_string())
    );
}
