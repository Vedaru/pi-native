//! Core coding-agent tools.
//!
//! A `Tool` is name + JSON schema + `run`. Tools resolve paths against a
//! working directory and truncate their output with [`truncate`], matching pi's
//! limits. This slice covers `read`, `bash`, and `ls`; `edit`, `write`, `grep`,
//! and `find` follow.

use std::path::{Path, PathBuf};
use std::process::Command;

use pi_providers::ToolSpec;
use serde_json::{json, Value};

pub mod truncate;

pub use truncate::{truncate_head, truncate_tail, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES};

/// The environment a tool runs in.
#[derive(Debug, Clone)]
pub struct ToolContext {
    pub cwd: PathBuf,
}

impl ToolContext {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self { cwd: cwd.into() }
    }

    /// Resolve a tool-supplied path against the working directory.
    pub fn resolve(&self, path: &str) -> PathBuf {
        let candidate = Path::new(path);
        if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            self.cwd.join(candidate)
        }
    }
}

/// A tool result.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
}

impl ToolResult {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }

    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
        }
    }
}

/// A tool the agent can call.
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn input_schema(&self) -> Value;
    fn run(&self, input: &Value, ctx: &ToolContext) -> ToolResult;

    /// The provider-facing tool definition.
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name().to_string(),
            description: self.description().to_string(),
            input_schema: self.input_schema(),
        }
    }
}

/// `read`: file contents, truncated head.
pub struct ReadTool;

impl Tool for ReadTool {
    fn name(&self) -> &'static str {
        "read"
    }

    fn description(&self) -> &'static str {
        "Read the contents of a file. Output is truncated to 2000 lines or 50KB (whichever is hit first). Use offset/limit for large files."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path, relative to the working directory" },
                "offset": { "type": "integer", "description": "Lines to skip from the start" },
                "limit": { "type": "integer", "description": "Maximum lines to read" }
            },
            "required": ["path"]
        })
    }

    fn run(&self, input: &Value, ctx: &ToolContext) -> ToolResult {
        let Some(path) = input.get("path").and_then(Value::as_str) else {
            return ToolResult::error("read: missing required field `path`");
        };
        let resolved = ctx.resolve(path);
        let contents = match std::fs::read(&resolved) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(error) => return ToolResult::error(format!("read: {path}: {error}")),
        };

        // `offset` and `limit` select a window before truncation.
        let offset = input.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
        let limit = input
            .get("limit")
            .and_then(Value::as_u64)
            .map(|v| v as usize);
        let window: String;
        let selected: &str = if offset > 0 || limit.is_some() {
            let lines: Vec<&str> = contents.lines().collect();
            let start = offset.min(lines.len());
            let end = limit
                .map(|l| (start + l).min(lines.len()))
                .unwrap_or(lines.len());
            let mut slice = lines[start..end].join("\n");
            if end < lines.len() {
                slice.push('\n');
            }
            window = slice;
            window.as_str()
        } else {
            contents.as_str()
        };

        let truncated = truncate_head(selected, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
        if truncated.truncated {
            ToolResult::ok(format!(
                "{}\n\n[Showing the first {} lines of {} (50KB limit). Use offset to continue.]",
                truncated.content.trim_end_matches('\n'),
                truncated.content.lines().count(),
                truncated.total_lines
            ))
        } else {
            ToolResult::ok(truncated.content)
        }
    }
}

/// `bash`: run a shell command, truncated tail.
pub struct BashTool;

impl Tool for BashTool {
    fn name(&self) -> &'static str {
        "bash"
    }

    fn description(&self) -> &'static str {
        "Execute a shell command in the working directory. Returns stdout and stderr, truncated to the last 2000 lines or 50KB."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "The shell command to run" },
                "timeout": { "type": "integer", "description": "Timeout in seconds" }
            },
            "required": ["command"]
        })
    }

    fn run(&self, input: &Value, ctx: &ToolContext) -> ToolResult {
        let Some(command) = input.get("command").and_then(Value::as_str) else {
            return ToolResult::error("bash: missing required field `command`");
        };
        let output = match Command::new("sh")
            .arg("-c")
            .arg(command)
            .current_dir(&ctx.cwd)
            .output()
        {
            Ok(output) => output,
            Err(error) => return ToolResult::error(format!("bash: {error}")),
        };
        let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.is_empty() {
            combined.push_str(&stderr);
        }
        let truncated = truncate_tail(&combined, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
        let content = if truncated.truncated {
            format!(
                "[Output truncated to the last {} lines.]\n{}",
                truncated.content.lines().count(),
                truncated.content
            )
        } else {
            truncated.content
        };
        if output.status.success() {
            ToolResult::ok(content)
        } else {
            ToolResult::error(if content.is_empty() {
                format!("bash: exited with {}", output.status)
            } else {
                content
            })
        }
    }
}

/// `ls`: directory listing, sorted, directories suffixed with `/`.
pub struct LsTool;

impl Tool for LsTool {
    fn name(&self) -> &'static str {
        "ls"
    }

    fn description(&self) -> &'static str {
        "List directory contents, sorted alphabetically with `/` for directories."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Directory path, defaults to the working directory" }
            }
        })
    }

    fn run(&self, input: &Value, ctx: &ToolContext) -> ToolResult {
        let resolved = match input.get("path").and_then(Value::as_str) {
            Some(path) => ctx.resolve(path),
            None => ctx.cwd.clone(),
        };
        let entries = match std::fs::read_dir(&resolved) {
            Ok(entries) => entries,
            Err(error) => return ToolResult::error(format!("ls: {}: {error}", resolved.display())),
        };
        let mut names: Vec<String> = entries
            .flatten()
            .map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                if entry.path().is_dir() {
                    format!("{name}/")
                } else {
                    name
                }
            })
            .collect();
        names.sort();
        let truncated = truncate_head(&names.join("\n"), 500, 1024 * 1024);
        ToolResult::ok(truncated.content)
    }
}

/// The default tool set.
pub fn default_tools() -> Vec<Box<dyn Tool>> {
    vec![Box::new(ReadTool), Box::new(BashTool), Box::new(LsTool)]
}

/// Provider-facing specs for a tool set.
pub fn tool_specs(tools: &[Box<dyn Tool>]) -> Vec<ToolSpec> {
    tools.iter().map(|tool| tool.spec()).collect()
}

#[cfg(test)]
#[path = "../tests/unit/tools.rs"]
mod tests;
