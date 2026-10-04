//! Read a session file and report its size. Used to measure retained memory.
//!
//! Usage: cargo run -p pi-session --example read_session -- <session.jsonl>

fn main() {
    let path = std::env::args().nth(1).expect("usage: read_session <path>");
    let session = pi_session::SessionFile::read(std::path::Path::new(&path)).expect("read session");
    let messages = session.message_entries().count();
    println!("{} entries, {} messages", session.entries.len(), messages);
}
