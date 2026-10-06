use super::*;
use crate::Event;

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pi-swarm-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn a_peer_outlet_becomes_a_framed_shout_and_is_read_once() {
    let dir = scratch("roundtrip");
    let mut a = Swarm::join(dir.clone(), "a".into()).unwrap();
    let mut b = Swarm::join(dir, "b".into()).unwrap();
    a.emit(&Event::AssistantText {
        text: "hello   world".into(),
    });
    assert_eq!(
        b.poll(),
        vec!["<shout from=\"a\" kind=\"says\">hello world</shout>".to_string()]
    );
    assert!(b.poll().is_empty(), "the read offset must advance");
}

#[test]
fn a_unit_never_reads_its_own_outlet() {
    let dir = scratch("own");
    let mut a = Swarm::join(dir, "a".into()).unwrap();
    a.emit(&Event::AssistantText {
        text: "self".into(),
    });
    assert!(a.poll().is_empty());
}
