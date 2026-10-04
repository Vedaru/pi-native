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

pub mod io;
pub mod truncate;

pub use truncate::{truncate_head, truncate_tail, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES};

/// Max bytes buffered per line while grepping.
const GREP_MAX_LINE_BYTES: usize = 64 * 1024;
/// Max characters of a grep match shown in output.
const GREP_MAX_LINE_LENGTH: usize = 500;

/// Compile a regex, reusing a small cache. Agents often repeat the same pattern
/// across calls; recompiling it every call is pure CPU.
fn cached_regex(pattern: &str) -> Result<regex::Regex, regex::Error> {
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<std::collections::HashMap<String, regex::Regex>>> =
        OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    let mut map = cache.lock().expect("regex cache");
    if let Some(regex) = map.get(pattern) {
        return Ok(regex.clone());
    }
    let regex = regex::Regex::new(pattern)?;
    if map.len() >= 64 {
        map.clear();
    }
    map.insert(pattern.to_string(), regex.clone());
    Ok(regex)
}

/// Compile a glob, reusing a small cache (finds often repeat a pattern).
fn cached_glob(pattern: &str) -> Result<globset::GlobMatcher, globset::Error> {
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<std::collections::HashMap<String, globset::GlobMatcher>>> =
        OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    let mut map = cache.lock().expect("glob cache");
    if let Some(matcher) = map.get(pattern) {
        return Ok(matcher.clone());
    }
    let matcher = globset::Glob::new(pattern)?.compile_matcher();
    if map.len() >= 64 {
        map.clear();
    }
    map.insert(pattern.to_string(), matcher.clone());
    Ok(matcher)
}

/// Human-readable byte size, matching pi's `formatSize`.
fn format_size(bytes: usize) -> String {
    if bytes >= 1024 * 1024 {
        format!("{}MB", bytes / (1024 * 1024))
    } else {
        format!("{}KB", bytes / 1024)
    }
}

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

    /// Whether running this tool needs explicit approval. Read-only tools do
    /// not; anything that mutates the workspace or spawns a process does.
    fn requires_approval(&self) -> bool {
        false
    }

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
        "Read the contents of a file. Supports text files and images (jpg, png, gif, webp, bmp). Images are sent as attachments. For text files, output is truncated to 2000 lines or 50KB (whichever is hit first). Use offset/limit for large files. When you need the full file, continue with offset until complete."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["path"],
            "properties": {
                "path": { "type": "string", "description": "Path to the file to read (relative or absolute)" },
                "offset": { "type": "number", "description": "Line number to start reading from (1-indexed)" },
                "limit": { "type": "number", "description": "Maximum number of lines to read" }
            }
        })
    }

    fn run(&self, input: &Value, ctx: &ToolContext) -> ToolResult {
        let Some(path) = input.get("path").and_then(Value::as_str) else {
            return ToolResult::error("read: missing required field `path`");
        };
        let resolved = ctx.resolve(path);
        // Stream line by line; never load the whole file.
        let file = match std::fs::File::open(&resolved) {
            Ok(file) => file,
            Err(error) => return ToolResult::error(format!("read: {path}: {error}")),
        };
        let mut reader = std::io::BufReader::new(file);

        // pi treats `offset` as a 1-indexed starting line (default 1).
        let start_line = input
            .get("offset")
            .and_then(Value::as_u64)
            .unwrap_or(1)
            .max(1) as usize;
        let limit = input
            .get("limit")
            .and_then(Value::as_u64)
            .map(|v| v as usize);

        let max_line = 1024 * 1024;
        let mut buffer: Vec<u8> = Vec::new();
        let mut output = String::new();
        let mut bytes = 0usize;
        let mut line_number = 0usize;
        let mut taken = 0usize;
        let mut byte_truncated = false;

        loop {
            match io::read_line_capped(&mut reader, &mut buffer, max_line) {
                Ok(false) => break,
                Ok(true) => {}
                Err(error) => return ToolResult::error(format!("read: {path}: {error}")),
            }
            line_number += 1;
            if line_number < start_line {
                continue;
            }
            if let Some(limit) = limit {
                if taken >= limit {
                    break;
                }
            }
            let line = String::from_utf8_lossy(&buffer);
            if bytes + line.len() > DEFAULT_MAX_BYTES {
                let remaining = DEFAULT_MAX_BYTES.saturating_sub(bytes);
                output.push_str(&line[..remaining.min(line.len())]);
                byte_truncated = true;
                break;
            }
            output.push_str(&line);
            bytes += line.len();
            taken += 1;
        }

        if byte_truncated {
            ToolResult::ok(format!(
                "{}\n\n[Output truncated at {}. Use offset to continue.]",
                output.trim_end_matches('\n'),
                format_size(DEFAULT_MAX_BYTES)
            ))
        } else {
            ToolResult::ok(output)
        }
    }
}

/// `bash`: run a shell command, truncated tail.
pub struct BashTool;

impl Tool for BashTool {
    fn name(&self) -> &'static str {
        "bash"
    }

    fn requires_approval(&self) -> bool {
        true
    }

    fn description(&self) -> &'static str {
        "Execute a bash command in the current working directory. Returns stdout and stderr. Output is truncated to last 2000 lines or 50KB (whichever is hit first). If truncated, full output is saved to a temp file. Optionally provide a timeout in seconds."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["command"],
            "properties": {
                "command": { "type": "string", "description": "Shell command to execute" },
                "timeout": { "type": "number", "description": "Timeout in seconds (optional, no default timeout)" }
            }
        })
    }

    fn run(&self, input: &Value, ctx: &ToolContext) -> ToolResult {
        let Some(command) = input.get("command").and_then(Value::as_str) else {
            return ToolResult::error("bash: missing required field `command`");
        };
        let timeout = input.get("timeout").and_then(Value::as_u64);
        let spawned = Command::new("sh")
            .arg("-c")
            .arg(command)
            .current_dir(&ctx.cwd)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(error) => return ToolResult::error(format!("bash: {error}")),
        };
        if let Some(seconds) = timeout {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) => {
                        if std::time::Instant::now() >= deadline {
                            let _ = child.kill();
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                    Err(error) => return ToolResult::error(format!("bash: {error}")),
                }
            }
        }
        let output = match child.wait_with_output() {
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
        "List directory contents. Returns entries sorted alphabetically, with '/' suffix for directories. Includes dotfiles. Output is truncated to 500 entries or 50KB (whichever is hit first)."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Directory to list (default: current directory)" },
                "limit": { "type": "number", "description": "Maximum number of entries to return (default: 500)" }
            }
        })
    }

    fn run(&self, input: &Value, ctx: &ToolContext) -> ToolResult {
        let resolved = match input.get("path").and_then(Value::as_str) {
            Some(path) => ctx.resolve(path),
            None => ctx.cwd.clone(),
        };
        let limit = input
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(500)
            .max(1) as usize;
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
        // pi sorts case-insensitively.
        names.sort_by_key(|name| name.to_lowercase());
        names.truncate(limit);
        let truncated = truncate_head(&names.join("\n"), DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
        ToolResult::ok(truncated.content)
    }
}

/// `write`: create or overwrite a file.
pub struct WriteTool;

impl Tool for WriteTool {
    fn name(&self) -> &'static str {
        "write"
    }

    fn requires_approval(&self) -> bool {
        true
    }

    fn description(&self) -> &'static str {
        "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. Automatically creates parent directories."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["path", "content"],
            "properties": {
                "path": { "type": "string", "description": "Path to the file to write (relative or absolute)" },
                "content": { "type": "string", "description": "Content to write to the file" }
            }
        })
    }

    fn run(&self, input: &Value, ctx: &ToolContext) -> ToolResult {
        let Some(path) = input.get("path").and_then(Value::as_str) else {
            return ToolResult::error("write: missing required field `path`");
        };
        let Some(content) = input.get("content").and_then(Value::as_str) else {
            return ToolResult::error("write: missing required field `content`");
        };
        let resolved = ctx.resolve(path);
        if let Some(parent) = resolved.parent() {
            if let Err(error) = std::fs::create_dir_all(parent) {
                return ToolResult::error(format!("write: {path}: {error}"));
            }
        }
        match std::fs::write(&resolved, content) {
            Ok(()) => ToolResult::ok(format!("Wrote {} bytes to {path}", content.len())),
            Err(error) => ToolResult::error(format!("write: {path}: {error}")),
        }
    }
}

/// `edit`: replace a unique exact string in a file.
pub struct EditTool;

impl Tool for EditTool {
    fn name(&self) -> &'static str {
        "edit"
    }

    fn requires_approval(&self) -> bool {
        true
    }

    fn description(&self) -> &'static str {
        "Edit a single file using exact text replacement. Every edits[].oldText must match a unique, non-overlapping region of the original file. If two changes affect the same block or nearby lines, merge them into one edit instead of emitting overlapping edits. Do not include large unchanged regions just to connect distant changes."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["path", "edits"],
            "properties": {
                "path": { "type": "string", "description": "Path to the file to edit (relative or absolute)" },
                "edits": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "required": ["oldText", "newText"],
                        "properties": {
                            "oldText": { "type": "string", "description": "Exact text for one targeted replacement. It must be unique in the original file and must not overlap with any other edits[].oldText in the same call." },
                            "newText": { "type": "string", "description": "Replacement text for this targeted edit." }
                        }
                    },
                    "description": "One or more targeted replacements. Each edit is matched against the original file, not incrementally. Do not include overlapping or nested edits. If two changes touch the same block or nearby lines, merge them into one edit instead."
                }
            }
        })
    }

    fn run(&self, input: &Value, ctx: &ToolContext) -> ToolResult {
        let Some(path) = input.get("path").and_then(Value::as_str) else {
            return ToolResult::error("edit: requires `path` and `edits`");
        };
        // pi's shape is `edits: [{oldText, newText}]`; also accept the legacy
        // single `oldText`/`newText` pair.
        let mut edits: Vec<(String, String)> = Vec::new();
        if let Some(items) = input.get("edits").and_then(Value::as_array) {
            for item in items {
                let (Some(old), Some(new)) = (
                    item.get("oldText").and_then(Value::as_str),
                    item.get("newText").and_then(Value::as_str),
                ) else {
                    return ToolResult::error("edit: each edit needs `oldText` and `newText`");
                };
                edits.push((old.to_string(), new.to_string()));
            }
        }
        if edits.is_empty() {
            if let (Some(old), Some(new)) = (
                input.get("oldText").and_then(Value::as_str),
                input.get("newText").and_then(Value::as_str),
            ) {
                edits.push((old.to_string(), new.to_string()));
            }
        }
        if edits.is_empty() {
            return ToolResult::error("edit: `edits` must contain at least one replacement");
        }

        let resolved = ctx.resolve(path);
        let contents = match std::fs::read_to_string(&resolved) {
            Ok(contents) => contents,
            Err(error) => return ToolResult::error(format!("edit: {path}: {error}")),
        };

        // Every oldText must match a unique, non-overlapping region of the
        // original content; edits are applied together, not incrementally.
        let mut ranges: Vec<(usize, usize, String)> = Vec::new();
        for (old, new) in &edits {
            let mut positions = contents.match_indices(old.as_str());
            let Some((start, _)) = positions.next() else {
                return ToolResult::error(format!("edit: {path}: `{old}` not found"));
            };
            if positions.next().is_some() {
                return ToolResult::error(format!("edit: {path}: `{old}` is not unique"));
            }
            ranges.push((start, start + old.len(), new.clone()));
        }
        ranges.sort_by_key(|(start, _, _)| *start);
        for pair in ranges.windows(2) {
            if pair[1].0 < pair[0].1 {
                return ToolResult::error(format!("edit: {path}: edits overlap"));
            }
        }

        let mut updated = String::with_capacity(contents.len());
        let mut cursor = 0;
        for (start, end, new) in &ranges {
            updated.push_str(&contents[cursor..*start]);
            updated.push_str(new);
            cursor = *end;
        }
        updated.push_str(&contents[cursor..]);

        match std::fs::write(&resolved, updated) {
            Ok(()) => ToolResult::ok(format!(
                "Successfully replaced {} block(s) in {path}.",
                edits.len()
            )),
            Err(error) => ToolResult::error(format!("edit: {path}: {error}")),
        }
    }
}

/// `grep`: search file contents with a regex, respecting .gitignore.
pub struct GrepTool;

impl Tool for GrepTool {
    fn name(&self) -> &'static str {
        "grep"
    }

    fn description(&self) -> &'static str {
        "Search file contents for a pattern. Returns matching lines with file paths and line numbers. Respects .gitignore. Output is truncated to 100 matches or 50KB (whichever is hit first). Long lines are truncated to 500 chars."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["pattern"],
            "properties": {
                "pattern": { "type": "string", "description": "Search pattern (regex or literal string)" },
                "path": { "type": "string", "description": "Directory or file to search (default: current directory)" },
                "glob": { "type": "string", "description": "Filter files by glob pattern, e.g. '*.ts' or '**/*.spec.ts'" },
                "ignoreCase": { "type": "boolean", "description": "Case-insensitive search (default: false)" },
                "literal": { "type": "boolean", "description": "Treat pattern as literal string instead of regex (default: false)" },
                "context": { "type": "number", "description": "Number of lines to show before and after each match (default: 0)" },
                "limit": { "type": "number", "description": "Maximum number of matches to return (default: 100)" }
            }
        })
    }

    fn run(&self, input: &Value, ctx: &ToolContext) -> ToolResult {
        let Some(pattern) = input.get("pattern").and_then(Value::as_str) else {
            return ToolResult::error("grep: missing required field `pattern`");
        };
        let literal = input
            .get("literal")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let ignore_case = input
            .get("ignoreCase")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let context = input.get("context").and_then(Value::as_u64).unwrap_or(0) as usize;
        let limit = input
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(100)
            .max(1) as usize;

        let glob = input.get("glob").and_then(Value::as_str);
        let glob_matcher = match glob {
            Some(glob) => match cached_glob(glob) {
                Ok(matcher) => Some((glob.contains('/'), matcher)),
                Err(error) => return ToolResult::error(format!("grep: invalid glob: {error}")),
            },
            None => None,
        };

        let mut source = if literal {
            regex::escape(pattern)
        } else {
            pattern.to_string()
        };
        if ignore_case {
            source = format!("(?i){source}");
        }
        let regex = match cached_regex(&source) {
            Ok(regex) => regex,
            Err(error) => return ToolResult::error(format!("grep: invalid pattern: {error}")),
        };

        let root = input
            .get("path")
            .and_then(Value::as_str)
            .map(|path| ctx.resolve(path))
            .unwrap_or_else(|| ctx.cwd.clone());
        let is_dir = root.is_dir();

        let mut output: Vec<String> = Vec::new();
        let mut matches = 0usize;
        'files: for entry in ignore::WalkBuilder::new(&root)
            .hidden(false)
            .build()
            .flatten()
        {
            if !entry
                .file_type()
                .map(|kind| kind.is_file())
                .unwrap_or(false)
            {
                continue;
            }
            let path = entry.path();
            let relative = path.strip_prefix(&root).unwrap_or(path);
            if let Some((full_path, matcher)) = &glob_matcher {
                let candidate = if *full_path {
                    relative.to_string_lossy().replace('\\', "/")
                } else {
                    path.file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_default()
                };
                if !matcher.is_match(candidate.as_str()) {
                    continue;
                }
            }
            let display = if is_dir {
                relative.to_string_lossy().replace('\\', "/")
            } else {
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default()
            };

            let Ok(file) = std::fs::File::open(path) else {
                continue;
            };
            if context == 0 {
                // Common case: stream, never load the whole file.
                let mut reader = std::io::BufReader::new(file);
                let mut buffer: Vec<u8> = Vec::new();
                let mut line_number = 0usize;
                loop {
                    match io::read_line_capped(&mut reader, &mut buffer, GREP_MAX_LINE_BYTES) {
                        Ok(false) | Err(_) => break,
                        Ok(true) => {}
                    }
                    line_number += 1;
                    let raw = String::from_utf8_lossy(&buffer);
                    let line = raw.trim_end_matches(['\n', '\r']);
                    if regex.is_match(line) {
                        let shown = &line[..line.len().min(GREP_MAX_LINE_LENGTH)];
                        output.push(format!("{display}:{line_number}: {shown}"));
                        matches += 1;
                        if matches >= limit {
                            break 'files;
                        }
                    }
                }
            } else {
                let text = std::fs::read_to_string(path).unwrap_or_default();
                let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
                let lines: Vec<&str> = normalized.split('\n').collect();
                for (index, line) in lines.iter().enumerate() {
                    if !regex.is_match(line) {
                        continue;
                    }
                    let number = index + 1;
                    let start = number.saturating_sub(context).max(1);
                    let end = (number + context).min(lines.len());
                    for current in start..=end {
                        let text = lines.get(current - 1).copied().unwrap_or("");
                        let shown = &text[..text.len().min(GREP_MAX_LINE_LENGTH)];
                        if current == number {
                            output.push(format!("{display}:{current}: {shown}"));
                        } else {
                            output.push(format!("{display}-{current}- {shown}"));
                        }
                    }
                    matches += 1;
                    if matches >= limit {
                        break 'files;
                    }
                }
            }
        }
        let truncated = truncate_head(&output.join("\n"), DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
        ToolResult::ok(truncated.content)
    }
}

/// `find`: list files matching a glob, respecting .gitignore.
pub struct FindTool;

impl Tool for FindTool {
    fn name(&self) -> &'static str {
        "find"
    }

    fn description(&self) -> &'static str {
        "Search for files by glob pattern. Returns matching file paths relative to the search directory. Respects .gitignore. Output is truncated to 1000 results or 50KB (whichever is hit first)."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["pattern"],
            "properties": {
                "pattern": { "type": "string", "description": "Glob pattern to match files, e.g. '*.ts', '**/*.json', or 'src/**/*.spec.ts'" },
                "path": { "type": "string", "description": "Directory to search in (default: current directory)" },
                "limit": { "type": "number", "description": "Maximum number of results (default: 1000)" }
            }
        })
    }

    fn run(&self, input: &Value, ctx: &ToolContext) -> ToolResult {
        let Some(pattern) = input.get("pattern").and_then(Value::as_str) else {
            return ToolResult::error("find: missing required field `pattern`");
        };
        let matcher = match cached_glob(pattern) {
            Ok(matcher) => matcher,
            Err(error) => return ToolResult::error(format!("find: invalid pattern: {error}")),
        };
        let root = input
            .get("path")
            .and_then(Value::as_str)
            .map(|path| ctx.resolve(path))
            .unwrap_or_else(|| ctx.cwd.clone());
        let limit = input
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(1000)
            .max(1) as usize;
        // fd matches the basename unless the pattern contains a path separator.
        let full_path = pattern.contains('/');

        let mut paths: Vec<String> = Vec::new();
        for entry in ignore::WalkBuilder::new(&root)
            .hidden(false)
            .build()
            .flatten()
        {
            if !entry
                .file_type()
                .map(|kind| kind.is_file())
                .unwrap_or(false)
            {
                continue;
            }
            let path = entry.path();
            let relative = path.strip_prefix(&root).unwrap_or(path);
            let candidate = if full_path {
                relative.to_string_lossy().replace('\\', "/")
            } else {
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default()
            };
            if matcher.is_match(candidate.as_str()) || matcher.is_match(relative) {
                paths.push(relative.to_string_lossy().replace('\\', "/"));
                if paths.len() >= limit {
                    break;
                }
            }
        }
        paths.sort();
        ToolResult::ok(paths.join("\n"))
    }
}

/// The default tool set.
pub fn default_tools() -> Vec<Box<dyn Tool>> {
    // Order matches pi's tool registry: the system prompt lists tools in this
    // order and the provider's `tools` array is sent in it.
    vec![
        Box::new(ReadTool),
        Box::new(BashTool),
        Box::new(EditTool),
        Box::new(WriteTool),
        Box::new(GrepTool),
        Box::new(FindTool),
        Box::new(LsTool),
    ]
}

/// Provider-facing specs for a tool set.
pub fn tool_specs(tools: &[Box<dyn Tool>]) -> Vec<ToolSpec> {
    tools.iter().map(|tool| tool.spec()).collect()
}

#[cfg(test)]
#[path = "../tests/unit/tools.rs"]
mod tests;
