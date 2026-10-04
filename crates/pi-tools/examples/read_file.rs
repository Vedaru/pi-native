//! Read a file through the `read` tool and print the retained size.
//! Usage: cargo run -p pi-tools --example read_file -- <path>

use pi_tools::{ReadTool, Tool, ToolContext};
use serde_json::json;

fn main() {
    let path = std::env::args().nth(1).expect("usage: read_file <path>");
    let ctx = ToolContext::new(std::env::current_dir().unwrap_or_default());
    let result = ReadTool.run(&json!({ "path": path }), &ctx);
    println!("retained bytes: {}", result.content.len());
}
