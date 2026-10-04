use super::*;
use crate::prompt::{build_system_prompt, SystemPromptOptions};

fn default_options() -> SystemPromptOptions {
    SystemPromptOptions {
        selected_tools: ["read", "bash", "edit", "write", "grep", "find", "ls"]
            .iter()
            .map(|name| name.to_string())
            .collect(),
        package_dir: "PI_ROOT".to_string(),
        cwd: "/tmp/proj".to_string(),
        ..Default::default()
    }
}

/// The rendered prompt must match pi's `buildSystemPrompt` byte-for-byte.
#[test]
fn default_prompt_matches_pi() {
    let expected = include_str!("../fixtures/system_prompt.txt");
    assert_eq!(build_system_prompt(&default_options()), expected);
}

#[test]
fn append_and_context_files_add_sections() {
    let mut options = default_options();
    options.append = Some("Extra instructions.".to_string());
    options.context_files = vec![ProjectContextFile {
        path: "AGENTS.md".to_string(),
        content: "Be careful.".to_string(),
    }];
    let prompt = build_system_prompt(&options);
    assert!(prompt.contains("<addendum>\nExtra instructions.\n</addendum>"));
    assert!(prompt.contains(
        "<project_context>\nProject-specific instructions and guidelines:\n\n\
<project_instructions path=\"AGENTS.md\">\nBe careful.\n</project_instructions>\n</project_context>"
    ));
}

#[test]
fn backslashes_in_cwd_are_normalized() {
    let mut options = default_options();
    options.cwd = "C:\\Users\\me".to_string();
    assert!(build_system_prompt(&options).contains("<cwd>\nC:/Users/me\n</cwd>"));
}
