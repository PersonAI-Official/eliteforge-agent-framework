use std::{fs, path::Path};

use async_trait::async_trait;
use eliteforge_agent_core::{RunContext, Tool, ToolBinding, ToolDescriptor, ToolError, ToolOutput};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::edits::{self, Change};
use crate::filesystem::{FileSystemRoot, SystemCommandConfig};
use crate::tasks::{
    CommandRequest, CommandTaskManager, INITIAL_READ_BYTES, MAX_READ_BYTES, MAX_READ_WAIT,
};

pub(crate) fn read_only_bindings(filesystem: FileSystemRoot) -> Vec<ToolBinding> {
    vec![
        ToolBinding::new(
            read_text_descriptor(),
            ReadTextTool {
                filesystem: filesystem.clone(),
            },
        ),
        ToolBinding::new(
            list_descriptor(),
            ListTool {
                filesystem: filesystem.clone(),
            },
        ),
        ToolBinding::new(
            stat_descriptor(),
            StatTool {
                filesystem: filesystem.clone(),
            },
        ),
    ]
    .into_iter()
    .map(|tool| tool.with_domain_code("fs"))
    .chain(crate::search::bindings(filesystem))
    .collect()
}

pub(crate) fn write_bindings(filesystem: FileSystemRoot) -> Vec<ToolBinding> {
    vec![
        ToolBinding::new(
            write_text_descriptor(),
            WriteTextTool {
                filesystem: filesystem.clone(),
            },
        ),
        ToolBinding::new(
            patch_descriptor(),
            PatchTool {
                filesystem: filesystem.clone(),
            },
        ),
        edits::binding(filesystem),
    ].into_iter().map(|tool| tool.with_domain_code("fs")).collect()
}

pub(crate) fn system_command_bindings(
    config: SystemCommandConfig,
    tasks: CommandTaskManager,
) -> Vec<ToolBinding> {
    vec![
        ToolBinding::new(
            {
                let mut descriptor = system_command_descriptor();
                let shell = config
                    .configured_shell()
                    .map(std::path::Path::to_path_buf)
                    .unwrap_or_else(crate::command_shell::default_shell_program);
                descriptor.description.push_str(&format!(
                    " Host shell mode: command is one complete command string interpreted by the host shell; lists, pipes and redirections use that shell's syntax and are not translated between operating systems. Host OS: {}; shell: {}.",
                    std::env::consts::OS,
                    shell.display()
                ));
                descriptor
            },
            SystemCommandTool {
                config: config.clone(),
                tasks: tasks.clone(),
            },
        )
        .parallel_safe(),
        ToolBinding::new(
            system_command_task_read_descriptor(),
            SystemCommandTaskReadTool {
                config: config.clone(),
                tasks: tasks.clone(),
            },
        ),
        ToolBinding::new(
            system_command_task_stop_descriptor(),
            SystemCommandTaskStopTool {
                config: config.clone(),
                tasks: tasks.clone(),
            },
        ),
        ToolBinding::new(
            system_command_task_write_descriptor(),
            SystemCommandTaskWriteTool {
                config: config.clone(),
                tasks: tasks.clone(),
            },
        ),
        ToolBinding::new(
            system_command_list_descriptor(),
            SystemCommandListTool { tasks, config },
        )
        .parallel_safe(),
    ].into_iter().map(|tool| tool.with_domain_code("command")).collect()
}

fn read_text_descriptor() -> ToolDescriptor {
    ToolDescriptor::new(
        "filesystem.read_text",
        "fs_read_text",
        "Read UTF-8 text from a file below the configured filesystem root.",
        json!({
            "type":"object", "additionalProperties":false, "oneOf":[{"required":["path"],"not":{"required":["fileRef"]}},{"required":["fileRef"],"not":{"required":["path"]}}],
            "properties": {
                "fileRef":{"type":"object","description":"Host-authorized file reference; workspace_path references use the normal directory grant."},
                "path":{"type":"string", "minLength":1},
                "startLine":{"type":"integer", "minimum":1},
                "endLine":{"type":"integer", "minimum":1},
                "maxBytes":{"type":"integer", "minimum":1}
            }
        }),
    ).with_display_resource(include_str!("tool-display.json")).with_operation_kind(eliteforge_agent_core::ToolOperationKind::Query)
}

fn list_descriptor() -> ToolDescriptor {
    ToolDescriptor::new(
        "filesystem.list",
        "fs_list",
        "List files and directories below the configured filesystem root without following symlinks.",
        json!({
            "type":"object", "additionalProperties":false, "required":["path"],
            "properties": {
                "path":{"type":"string", "minLength":1},
                "recursive":{"type":"boolean"},
                "maxDepth":{"type":"integer", "minimum":0},
                "includeHidden":{"type":"boolean"},
                "filesOnly":{"type":"boolean"},
                "directoriesOnly":{"type":"boolean"},
                "maxResults":{"type":"integer", "minimum":1}
            }
        }),
    ).with_display_resource(include_str!("tool-display.json")).with_operation_kind(eliteforge_agent_core::ToolOperationKind::Query)
}

fn stat_descriptor() -> ToolDescriptor {
    ToolDescriptor::new(
        "filesystem.stat",
        "fs_stat",
        "Read metadata for a filesystem file or directory without following symlinks.",
        json!({
            "type":"object", "additionalProperties":false, "required":["path"],
            "properties": { "path":{"type":"string", "minLength":1}, "followSymlink":{"type":"boolean"} }
        }),
    ).with_display_resource(include_str!("tool-display.json")).with_operation_kind(eliteforge_agent_core::ToolOperationKind::Query)
}

fn write_text_descriptor() -> ToolDescriptor {
    ToolDescriptor::new(
        "filesystem.write_text",
        "fs_write_text",
        "Create, overwrite, or append UTF-8 text below the configured filesystem root. Single-file operation: pass path and content directly, without an items array. Use separate calls for separate files.",
        json!({
            "type":"object", "additionalProperties":false, "required":["path", "content"],
            "properties": {
                "path":{"type":"string", "minLength":1},
                "content":{"type":"string"},
                "mode":{"type":"string", "enum":["create", "overwrite", "append"]},
                "ensureParentDirs":{"type":"boolean"},
                "expectedVersion":{"type":"string", "minLength":1}
            }
        }),
    ).with_display_resource(include_str!("tool-display.json"))
    .with_single_input()
    .with_operation_kind(eliteforge_agent_core::ToolOperationKind::Command)
}

fn patch_descriptor() -> ToolDescriptor {
    ToolDescriptor::new(
        "filesystem.apply_patch",
        "fs_apply_patch",
        "Edit UTF-8 text using path/action/target, or a *** Begin Patch envelope to add/update/delete/move multiple files. Ambiguous contexts are rejected. dryRun previews envelopes without writing.",
        json!({
            "type":"object", "additionalProperties":false,
            "oneOf":[
                {"required":["patch"], "not":{"anyOf":[{"required":["path"]},{"required":["action"]},{"required":["target"]},{"required":["content"]},{"required":["replaceAll"]},{"required":["expectedCount"]},{"required":["expectedVersion"]}]}},
                {"required":["path","action","target"], "not":{"anyOf":[{"required":["patch"]},{"required":["cwd"]},{"required":["dryRun"]}]}}
            ],
            "properties": {
                "path":{"type":"string", "minLength":1},
                "patch":{"type":"string","minLength":1,"maxLength":4194304},
                "cwd":{"type":"string","minLength":1},
                "dryRun":{"type":"boolean"},
                "expectedCount":{"type":"integer","minimum":1},
                "action":{"type":"string", "enum":["replace", "delete", "insert_before", "insert_after"]},
                "target":{"type":"string", "minLength":1},
                "content":{"type":"string"},
                "replaceAll":{"type":"boolean"},
                "expectedVersion":{"type":"string", "minLength":1}
            }
        }),
    ).with_display_resource(include_str!("tool-display.json")).with_single_input().with_operation_kind(eliteforge_agent_core::ToolOperationKind::Command)
}

fn system_command_descriptor() -> ToolDescriptor {
    let input_schema = json!({
        "type":"object", "additionalProperties":false, "required":["command"],
        "properties": {
            "command":{"type":"string", "minLength":1, "description":"Complete command text. The host shell interprets quoting, lists, pipes, redirections and paths with spaces."},
            "captureToFile":{"type":"boolean", "default":false, "description":"Save raw output as host-managed session files independently of the preview ring buffer."},
            "args":{"type":"array", "items":{"type":"string"}, "description":"Removed from the model contract. Kept only so legacy calls fail with an explicit retry message."},
            "cwd":{"type":"string", "minLength":1},
            "stdin":{"type":"string", "maxLength":65536},
            "sudo":{"type":"boolean", "description":"Elevate with sudo using private host password input only when required. Supply the underlying command, without sudo. Passwords must NEVER appear in command, stdin, env or ask_user. Incompatible with stdin, interactive and tty; elevated commands receive EOF after authentication."},
            "interactive":{"type":"boolean", "description":"Keep stdin open for command_exec_write."},
            "tty":{"type":"boolean", "description":"Use a PTY; implies interactive input and combines stdout/stderr into stdout."},
            "rows":{"type":"integer", "minimum":1, "maximum":1000},
            "cols":{"type":"integer", "minimum":1, "maximum":1000},
            "env":{"type":"object", "additionalProperties":{"type":"string"}}
        }
    });
    ToolDescriptor::new(
        "system.command.exec",
        "command_exec",
        "Run a complete command string in the authorized working directory. The host shell interprets chaining, pipes, redirections, quoting and paths with spaces. Returns taskId and a 16 KiB preview per stream, including for completed commands. stdout/stderr are objects with content, pageStartCursor, nextCursor, endCursor, hasMore, droppedBytes, captureStatus and captureError. Follow nextCursor using command_exec_read until both streams have hasMore=false and captureStatus=eof; all pages from zero must have droppedBytes=0 to establish complete output. Ring buffers can discard older bytes. Set captureToFile=true to save output independently of the ring buffer; fileCapture reports status, capturedBytes, maxBytes, fileRef and error. saved means file publication succeeded, not that capture reached EOF: require captureStatus=eof and capturedBytes=endCursor too. Empty streams have no fileRef. File limits and failures are explicit. Use command_exec_stop to stop a task. A nonzero exit code is a command result, not a tool failure. This is not a sandbox. For sudo=true supply the command without sudo; never put a password in tool arguments. args is not accepted.",
        input_schema.clone(),
    )
    .with_model_input_schema(command_model_schema(input_schema))
    .with_display_resource(include_str!("tool-display.json"))
    .with_operation_kind(eliteforge_agent_core::ToolOperationKind::Command)
}

fn command_model_schema(mut schema: Value) -> Value {
    if let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) {
        properties.remove("args");
    }
    schema
}

fn system_command_task_read_descriptor() -> ToolDescriptor {
    ToolDescriptor::new(
        "system.command.task.read",
        "command_exec_read",
        "Read a command task preview, including after process completion. Pass stdout.nextCursor/stderr.nextCursor as stdoutCursor/stderrCursor. hasMore means more captured bytes remain; captureStatus=eof means that stream reached EOF, not that the page contains everything. droppedBytes reports a gap from the requested cursor; discarded bytes cannot be recovered from memory. maxBytes is per stream (a UTF-8 scalar may exceed it by up to 3 bytes). A short waitMs can wait for more output. Tasks are available only in the session that started them.",
        json!({
            "type":"object", "additionalProperties":false, "required":["taskId"],
            "properties": {
                "taskId":{"type":"string", "minLength":1},
                "stdoutCursor":{"type":"integer", "minimum":0},
                "stderrCursor":{"type":"integer", "minimum":0},
                "waitMs":{"type":"integer", "minimum":0, "maximum":10000},
                "maxBytes":{"type":"integer", "minimum":1, "maximum":65536}
            }
        }),
    ).with_display_resource(include_str!("tool-display.json")).with_operation_kind(eliteforge_agent_core::ToolOperationKind::Query)
}

fn system_command_task_stop_descriptor() -> ToolDescriptor {
    ToolDescriptor::new(
        "system.command.task.stop",
        "command_exec_stop",
        "Stop a running command task from this session and return its status and the next stdout/stderr bytes. Pass the cursors returned by the last read to continue from there.",
        json!({
            "type":"object", "additionalProperties":false, "required":["taskId"],
            "properties": {
                "taskId":{"type":"string", "minLength":1},
                "stdoutCursor":{"type":"integer", "minimum":0},
                "stderrCursor":{"type":"integer", "minimum":0},
                "maxBytes":{"type":"integer", "minimum":1, "maximum":65536}
            }
        }),
    ).with_display_resource(include_str!("tool-display.json")).with_operation_kind(eliteforge_agent_core::ToolOperationKind::Command)
}

fn system_command_task_write_descriptor() -> ToolDescriptor {
    ToolDescriptor::new(
        "system.command.task.write",
        "command_exec_write",
        "Write stdin to an interactive command from this session. closeStdin sends EOF after the data. Input may be partially delivered on failure; read command output before retrying.",
        json!({"type":"object", "additionalProperties":false, "required":["taskId"], "properties":{
            "taskId":{"type":"string", "minLength":1}, "data":{"type":"string", "maxLength":65536},
            "closeStdin":{"type":"boolean"}, "stdoutCursor":{"type":"integer", "minimum":0},
            "stderrCursor":{"type":"integer", "minimum":0}, "waitMs":{"type":"integer", "minimum":0, "maximum":10000}
        }}),
    ).with_display_resource(include_str!("tool-display.json")).with_single_input().with_operation_kind(eliteforge_agent_core::ToolOperationKind::Command)
}

fn system_command_list_descriptor() -> ToolDescriptor {
    ToolDescriptor::new(
        "system.command.list",
        "command_exec_list",
        "Read the command inventory (命令清单) for this session, including command IDs and status. Completed records are optional and bounded by the Agent retention limit.",
        json!({"type":"object", "additionalProperties":false, "properties":{
            "includeCompleted":{"type":"boolean"}
        }}),
    ).with_display_resource(include_str!("tool-display.json")).with_operation_kind(eliteforge_agent_core::ToolOperationKind::Query)
}

#[derive(Clone)]
struct ReadTextTool {
    filesystem: FileSystemRoot,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReadTextInput {
    path: Option<String>,
    file_ref: Option<Value>,
    start_line: Option<usize>,
    end_line: Option<usize>,
    max_bytes: Option<usize>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ReadTextOutput {
    path: Option<String>,
    file_name: Option<String>,
    file_ref: Option<Value>,
    content: String,
    start_line: usize,
    end_line: usize,
    version: String,
    truncated: bool,
}

#[async_trait]
impl Tool for ReadTextTool {
    async fn call(&self, arguments: Value, context: &RunContext) -> Result<ToolOutput, ToolError> {
        let mut input: ReadTextInput = parse(arguments)?;
        if input.path.is_some() == input.file_ref.is_some() {
            return Err(ToolError::Failed(
                "provide exactly one of path and fileRef".into(),
            ));
        }
        if let Some(reference) = input.file_ref.clone() {
            if reference.get("kind").and_then(Value::as_str) == Some("workspace_path") {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct WorkspacePath {
                    kind: String,
                    path: String,
                }
                let reference: WorkspacePath = parse(reference)?;
                debug_assert_eq!(reference.kind, "workspace_path");
                input.path = Some(reference.path);
            } else {
                let reader = context
                    .extensions
                    .get::<crate::FileReferenceReaderService>()
                    .ok_or_else(|| {
                        ToolError::Failed(
                            "this host does not provide a file reference reader".into(),
                        )
                    })?;
                let source = reader
                    .0
                    .read(reference, self.filesystem.max_edit_bytes(), context)
                    .await?;
                if source.bytes.len() > self.filesystem.max_edit_bytes() {
                    return Err(ToolError::Failed(
                        "referenced file exceeds input limit".into(),
                    ));
                }
                let filesystem = self.filesystem.clone();
                let context = context.clone();
                return blocking(move || {
                    render_text(
                        &filesystem,
                        input,
                        source.bytes,
                        None,
                        Some(source.name),
                        Some(source.reference),
                        &context,
                    )
                })
                .await;
            }
        }
        let filesystem = self.filesystem.clone();
        let context = context.clone();
        blocking(move || {
            edits::ensure_active(&context)?;
            read_text(filesystem, input, &context)
        })
        .await
    }
}

fn read_text(
    filesystem: FileSystemRoot,
    input: ReadTextInput,
    context: &RunContext,
) -> Result<ToolOutput, ToolError> {
    let resolved = filesystem.resolve(
        input
            .path
            .as_deref()
            .ok_or_else(|| ToolError::Failed("path is required".into()))?,
    )?;
    let bytes = edits::read(&resolved.path, filesystem.max_edit_bytes())?
        .ok_or_else(|| ToolError::Failed("file does not exist".into()))?;
    let path = filesystem.display(&resolved.root, &resolved.path);
    let name = resolved
        .path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned());
    let reference = Some(json!({"kind":"workspace_path", "path":path}));
    render_text(
        &filesystem,
        input,
        bytes,
        Some(path),
        name,
        reference,
        context,
    )
}

fn render_text(
    filesystem: &FileSystemRoot,
    input: ReadTextInput,
    bytes: Vec<u8>,
    path: Option<String>,
    file_name: Option<String>,
    file_ref: Option<Value>,
    context: &RunContext,
) -> Result<ToolOutput, ToolError> {
    edits::ensure_active(context)?;
    let source = edits::text(&bytes)?;
    let version = edits::version(&bytes);
    let start_line = input.start_line.unwrap_or(1);
    if start_line == 0 {
        return Err(ToolError::Failed(
            "startLine must be greater than zero".into(),
        ));
    }
    let end_limit = input.end_line;
    if end_limit.is_some_and(|end_line| end_line < start_line) {
        return Err(ToolError::Failed(
            "endLine must be greater than or equal to startLine".into(),
        ));
    }
    let max_bytes = filesystem.max_read_bytes(input.max_bytes);
    let mut content = String::new();
    let mut truncated = false;
    let mut last_line = start_line.saturating_sub(1);
    for (index, line) in source.lines().enumerate() {
        let line_number = index + 1;
        if line_number < start_line {
            continue;
        }
        if end_limit.is_some_and(|end_line| line_number > end_line) {
            break;
        }
        let prefix = (!content.is_empty()).then_some("\n");
        if prefix.is_some_and(|prefix| append_limited(&mut content, prefix, max_bytes))
            || append_limited(&mut content, line, max_bytes)
        {
            truncated = true;
            break;
        }
        last_line = line_number;
    }
    json_output(ReadTextOutput {
        path,
        file_name,
        file_ref,
        content,
        start_line,
        end_line: last_line,
        version,
        truncated,
    })
}

#[derive(Clone)]
struct ListTool {
    filesystem: FileSystemRoot,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ListInput {
    path: String,
    recursive: Option<bool>,
    max_depth: Option<usize>,
    include_hidden: Option<bool>,
    files_only: Option<bool>,
    directories_only: Option<bool>,
    max_results: Option<usize>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ListOutput {
    path: String,
    entries: Vec<PathEntry>,
    truncated: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PathEntry {
    path: String,
    name: String,
    is_file: bool,
    is_directory: bool,
    is_symlink: bool,
    size_bytes: Option<u64>,
}

#[async_trait]
impl Tool for ListTool {
    async fn call(&self, arguments: Value, _: &RunContext) -> Result<ToolOutput, ToolError> {
        let input = parse(arguments)?;
        let filesystem = self.filesystem.clone();
        blocking(move || list(filesystem, input)).await
    }
}

fn list(filesystem: FileSystemRoot, input: ListInput) -> Result<ToolOutput, ToolError> {
    if input.files_only.unwrap_or(false) && input.directories_only.unwrap_or(false) {
        return Err(ToolError::Failed(
            "filesOnly and directoriesOnly cannot both be true".into(),
        ));
    }
    let resolved = filesystem.resolve_directory(Some(&input.path))?;
    let mut entries = Vec::new();
    let max_results = filesystem.max_results(input.max_results);
    let recursive = input.recursive.unwrap_or(false);
    let max_depth = if recursive {
        filesystem.max_depth(input.max_depth)
    } else {
        0
    };
    let include_hidden = input.include_hidden.unwrap_or(false);
    let files_only = input.files_only.unwrap_or(false);
    let directories_only = input.directories_only.unwrap_or(false);
    let truncated = walk_entries(
        &resolved.path,
        0,
        max_depth,
        include_hidden,
        &mut |path, metadata| {
            let is_file = metadata.is_file();
            let is_directory = metadata.is_dir();
            if (files_only && !is_file) || (directories_only && !is_directory) {
                return false;
            }
            entries.push(PathEntry {
                path: filesystem.display(&resolved.root, path),
                name: path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string(),
                is_file,
                is_directory,
                is_symlink: metadata.file_type().is_symlink(),
                size_bytes: is_file.then_some(metadata.len()),
            });
            entries.len() >= max_results
        },
    )?;
    json_output(ListOutput {
        path: filesystem.display(&resolved.root, &resolved.path),
        entries,
        truncated,
    })
}

#[derive(Clone)]
struct StatTool {
    filesystem: FileSystemRoot,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StatInput {
    path: String,
    follow_symlink: Option<bool>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StatOutput {
    path: String,
    exists: bool,
    is_file: bool,
    is_directory: bool,
    is_symlink: bool,
    size_bytes: Option<u64>,
    readonly: bool,
}

#[async_trait]
impl Tool for StatTool {
    async fn call(&self, arguments: Value, _: &RunContext) -> Result<ToolOutput, ToolError> {
        let input = parse(arguments)?;
        let filesystem = self.filesystem.clone();
        blocking(move || stat(filesystem, input)).await
    }
}

fn stat(filesystem: FileSystemRoot, input: StatInput) -> Result<ToolOutput, ToolError> {
    let resolved = filesystem.resolve(&input.path)?;
    let metadata = if input.follow_symlink.unwrap_or(false) {
        fs::metadata(&resolved.path)
    } else {
        fs::symlink_metadata(&resolved.path)
    }
    .map_err(|error| ToolError::Failed(format!("stat `{}` failed: {error}", input.path)))?;
    json_output(StatOutput {
        exists: true,
        path: filesystem.display(&resolved.root, &resolved.path),
        is_file: metadata.is_file(),
        is_directory: metadata.is_dir(),
        is_symlink: metadata.file_type().is_symlink(),
        size_bytes: metadata.is_file().then_some(metadata.len()),
        readonly: metadata.permissions().readonly(),
    })
}

#[derive(Clone)]
struct WriteTextTool {
    filesystem: FileSystemRoot,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WriteTextInput {
    path: String,
    content: String,
    mode: Option<WriteMode>,
    ensure_parent_dirs: Option<bool>,
    expected_version: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WriteMode {
    Create,
    Overwrite,
    Append,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WriteTextOutput {
    path: String,
    mode: &'static str,
    bytes_written: usize,
    change: Value,
}

#[async_trait]
impl Tool for WriteTextTool {
    async fn call(&self, arguments: Value, context: &RunContext) -> Result<ToolOutput, ToolError> {
        let input = parse(arguments)?;
        let filesystem = self.filesystem.clone();
        let context = context.clone();
        blocking(move || {
            edits::ensure_active(&context)?;
            write_text(filesystem, input, &context)
        })
        .await
    }
}

fn write_text(
    filesystem: FileSystemRoot,
    input: WriteTextInput,
    context: &RunContext,
) -> Result<ToolOutput, ToolError> {
    let _guard = edits::lock();
    let mut resolved = filesystem.resolve(&input.path)?;
    if input.content.len() > filesystem.max_edit_bytes() {
        return Err(ToolError::Failed("write exceeds max_edit_bytes".into()));
    }
    let before = edits::read(&resolved.path, filesystem.max_edit_bytes())?;
    edits::check_version(before.as_deref(), input.expected_version.as_deref())?;
    let mode = input.mode.unwrap_or(WriteMode::Overwrite);
    if matches!(mode, WriteMode::Create) && before.is_some() {
        return Err(ToolError::Failed("file already exists".into()));
    }
    if input.ensure_parent_dirs.unwrap_or(true)
        && let Some(parent) = resolved.path.parent()
    {
        fs::create_dir_all(parent).map_err(|error| ToolError::Failed(error.to_string()))?;
        resolved = filesystem.resolve(&input.path)?;
    }
    let mut after = if matches!(mode, WriteMode::Append) {
        before.clone().unwrap_or_default()
    } else {
        Vec::new()
    };
    after.extend_from_slice(input.content.as_bytes());
    if after.len() > filesystem.max_edit_bytes() {
        return Err(ToolError::Failed("write exceeds max_edit_bytes".into()));
    }
    let change = Change {
        path: filesystem.display(&resolved.root, &resolved.path),
        before,
        after: Some(after),
        permissions: None,
    };
    let preview = edits::preview(&change)?;
    edits::commit(&filesystem, &[change], Some(context))?;
    json_output(WriteTextOutput {
        path: filesystem.display(&resolved.root, &resolved.path),
        mode: match mode {
            WriteMode::Create => "create",
            WriteMode::Overwrite => "overwrite",
            WriteMode::Append => "append",
        },
        bytes_written: input.content.len(),
        change: preview,
    })
}

#[derive(Clone)]
struct PatchTool {
    filesystem: FileSystemRoot,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PatchInput {
    path: String,
    action: PatchAction,
    target: String,
    content: Option<String>,
    replace_all: Option<bool>,
    expected_version: Option<String>,
    expected_count: Option<usize>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PatchAction {
    Replace,
    Delete,
    InsertBefore,
    InsertAfter,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PatchOutput {
    path: String,
    action: &'static str,
    replacements: usize,
    change: Value,
}

#[async_trait]
impl Tool for PatchTool {
    async fn call(&self, arguments: Value, context: &RunContext) -> Result<ToolOutput, ToolError> {
        if arguments.get("patch").is_some() {
            let filesystem = self.filesystem.clone();
            let context = context.clone();
            return blocking(move || {
                crate::patch_envelope::apply(&filesystem, arguments, &context)
            })
            .await;
        }
        let input = parse(arguments)?;
        let filesystem = self.filesystem.clone();
        let context = context.clone();
        blocking(move || {
            edits::ensure_active(&context)?;
            patch(filesystem, input, &context)
        })
        .await
    }
}

fn patch(
    filesystem: FileSystemRoot,
    input: PatchInput,
    context: &RunContext,
) -> Result<ToolOutput, ToolError> {
    let _guard = edits::lock();
    if input.target.is_empty() {
        return Err(ToolError::Failed("target is required".into()));
    }
    let resolved = filesystem.resolve(&input.path)?;
    let before = edits::read(&resolved.path, filesystem.max_edit_bytes())?
        .ok_or_else(|| ToolError::Failed("file does not exist".into()))?;
    edits::check_version(Some(&before), input.expected_version.as_deref())?;
    let mut source = edits::text(&before)?.to_owned();
    let replacement = input.content.unwrap_or_default();
    let replace_all = input.replace_all.unwrap_or(false);
    let occurrences = source.matches(&input.target).count();
    if input
        .expected_count
        .is_some_and(|expected| expected != occurrences)
        || (!replace_all && occurrences > 1)
    {
        return Err(ToolError::Failed(format!(
            "target has {occurrences} matches; provide unique context or replaceAll with expectedCount"
        )));
    }
    let count = if replace_all {
        occurrences
    } else {
        occurrences.min(1)
    };
    let removed = if matches!(input.action, PatchAction::Replace | PatchAction::Delete) {
        input.target.len()
    } else {
        0
    };
    let added = if matches!(input.action, PatchAction::Delete) {
        0
    } else {
        replacement.len()
    };
    let projected = source
        .len()
        .saturating_sub(removed.saturating_mul(count))
        .saturating_add(added.saturating_mul(count));
    if projected > filesystem.max_edit_bytes() {
        return Err(ToolError::Failed("patch exceeds max_edit_bytes".into()));
    }
    let replacements = match input.action {
        PatchAction::Replace => {
            replace_text(&mut source, &input.target, &replacement, replace_all)?
        }
        PatchAction::Delete => replace_text(&mut source, &input.target, "", replace_all)?,
        PatchAction::InsertBefore => {
            insert_text(&mut source, &input.target, &replacement, replace_all, true)?
        }
        PatchAction::InsertAfter => {
            insert_text(&mut source, &input.target, &replacement, replace_all, false)?
        }
    };
    if source.len() > filesystem.max_edit_bytes() {
        return Err(ToolError::Failed("patch exceeds max_edit_bytes".into()));
    }
    let change = Change {
        path: filesystem.display(&resolved.root, &resolved.path),
        before: Some(before),
        after: Some(source.into_bytes()),
        permissions: None,
    };
    let preview = edits::preview(&change)?;
    edits::commit(&filesystem, &[change], Some(context))?;
    json_output(PatchOutput {
        path: filesystem.display(&resolved.root, &resolved.path),
        action: match input.action {
            PatchAction::Replace => "replace",
            PatchAction::Delete => "delete",
            PatchAction::InsertBefore => "insert_before",
            PatchAction::InsertAfter => "insert_after",
        },
        replacements,
        change: preview,
    })
}

struct SystemCommandTool {
    config: SystemCommandConfig,
    tasks: CommandTaskManager,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SystemCommandInput {
    command: String,
    #[serde(default)]
    capture_to_file: bool,
    #[serde(default)]
    args: Option<Vec<String>>,
    cwd: Option<String>,
    stdin: Option<String>,
    #[serde(default)]
    sudo: bool,
    #[serde(default)]
    interactive: bool,
    #[serde(default)]
    tty: bool,
    rows: Option<u16>,
    cols: Option<u16>,
    env: Option<std::collections::BTreeMap<String, String>>,
}

#[async_trait]
impl Tool for SystemCommandTool {
    async fn call(&self, arguments: Value, context: &RunContext) -> Result<ToolOutput, ToolError> {
        let input = parse::<SystemCommandInput>(arguments)?;
        let command = input.command.trim().to_owned();
        if command.is_empty() {
            return Err(ToolError::Failed("command is required".into()));
        }
        if input.args.as_ref().is_some_and(|args| !args.is_empty()) {
            return Err(ToolError::Failed(
                "command_exec no longer accepts args; pass the complete command string in command and retry without args".into(),
            ));
        }
        let (command, args) =
            self.config
                .invocation(command, None, input.interactive || input.tty)?;
        let cwd = self.config.resolve_cwd(input.cwd.as_deref())?;
        let (command, args, sudo) = if input.sudo {
            if !cfg!(unix) || input.stdin.is_some() || input.tty || input.interactive {
                return Err(ToolError::Failed(
                    "sudo requires a Unix host and cannot combine with stdin, interactive or tty"
                        .into(),
                ));
            }
            let marker = format!("[framework-sudo-{}]", uuid::Uuid::new_v4());
            let sudo = crate::sudo::SudoInput {
                marker: marker.clone(),
                command: command.clone(),
                args: args.clone(),
                submitted: tokio::sync::Mutex::new(false),
            };
            // The wrapper gives the target EOF even for cached/NOPASSWD execution.
            // No model-controlled text is interpolated into shell source.
            let mut elevated = vec![
                "-S".into(),
                "-p".into(),
                marker,
                "--".into(),
                "/bin/sh".into(),
                "-c".into(),
                "exec \"$@\" </dev/null".into(),
                "sudo-command".into(),
                command,
            ];
            elevated.extend(args);
            ("sudo".into(), elevated, Some(sudo))
        } else {
            (command, args, None)
        };
        if input.stdin.as_ref().is_some_and(|data| data.len() > 65536) {
            return Err(ToolError::Failed(
                "initial stdin exceeds 65536 bytes".into(),
            ));
        }
        if input.rows.is_some_and(|v| v == 0 || v > 1000)
            || input.cols.is_some_and(|v| v == 0 || v > 1000)
        {
            return Err(ToolError::Failed(
                "PTY rows and cols must be between 1 and 1000".into(),
            ));
        }
        let capture = if input.capture_to_file {
            let provider = context
                .extensions
                .get::<crate::CommandOutputCaptureService>()
                .ok_or_else(|| {
                    ToolError::Failed(
                        "this host does not provide command output file capture".into(),
                    )
                })?;
            Some((
                provider.0.create("stdout", context).await?,
                provider.0.create("stderr", context).await?,
            ))
        } else {
            None
        };
        let task = self.tasks.start(
            CommandRequest {
                capture,
                env_remove: Vec::new(),
                session_id: context.session_id.clone(),
                command,
                args,
                cwd,
                stdin: input.stdin,
                env: input.env.unwrap_or_default(),
                interactive: input.interactive || input.sudo,
                sudo,
                tty: input.tty,
                rows: input.rows.unwrap_or(24),
                cols: input.cols.unwrap_or(80),
            },
            self.config.timeout(),
            self.config.max_output_bytes(),
        )?;
        // A task may outlive its run only after the caller accepts its taskId.
        // Dropped/per-tool-timed-out initial waits otherwise leave an orphan.
        struct PendingCommand {
            task: std::sync::Arc<crate::tasks::CommandTask>,
            accepted: bool,
        }
        impl Drop for PendingCommand {
            fn drop(&mut self) {
                if !self.accepted {
                    self.task.request_stop();
                }
            }
        }
        let mut pending = PendingCommand {
            task: task.clone(),
            accepted: false,
        };
        let waiting = async {
            let progress = context
                .extensions
                .get::<crate::CommandProgressSinkService>();
            let deadline = tokio::time::Instant::now() + self.config.async_after();
            let (mut stdout_cursor, mut stderr_cursor) = (0, 0);
            loop {
                task.supply_sudo_password(context).await?;
                let page = task.view(stdout_cursor, stderr_cursor, INITIAL_READ_BYTES);
                let (stdout, stderr, next_stdout, next_stderr, terminal) = page.progress();
                if let Some(progress) = &progress {
                    if !stdout.is_empty() {
                        progress.0.chunk(context, "stdout", stdout);
                    }
                    if !stderr.is_empty() {
                        progress.0.chunk(context, "stderr", stderr);
                    }
                }
                stdout_cursor = next_stdout;
                stderr_cursor = next_stderr;
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if terminal || remaining.is_zero() {
                    break;
                }
                task.wait_for_output(stdout_cursor, stderr_cursor, remaining)
                    .await;
            }
            Ok::<(), ToolError>(())
        };
        tokio::select! {
            result = waiting => result?,
            _ = context.cancellation.cancelled() => {
                task.request_stop();
                task.wait_for_terminal(std::time::Duration::from_secs(3)).await;
                return Err(ToolError::Failed("command was stopped because its Agent run was cancelled".into()));
            }
        }
        let output = task.view(0, 0, INITIAL_READ_BYTES);
        // Command status, stderr and a nonzero exit code are result data. The
        // tool call itself succeeded once the task started and was observed.
        pending.accepted = true;
        json_output(output)
    }
}

struct SystemCommandTaskReadTool {
    config: SystemCommandConfig,
    tasks: CommandTaskManager,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SystemCommandTaskReadInput {
    task_id: String,
    stdout_cursor: Option<u64>,
    stderr_cursor: Option<u64>,
    wait_ms: Option<u64>,
    max_bytes: Option<usize>,
}

#[async_trait]
impl Tool for SystemCommandTaskReadTool {
    async fn call(&self, arguments: Value, context: &RunContext) -> Result<ToolOutput, ToolError> {
        let input = parse::<SystemCommandTaskReadInput>(arguments)?;
        let task = self.tasks.get(&context.session_id, &input.task_id)?;
        self.config
            .authorize_existing_cwd(task.cwd().and_then(|cwd| cwd.to_str()))?;
        task.supply_sudo_password(context).await?;
        let stdout_cursor = input.stdout_cursor.unwrap_or(0);
        let stderr_cursor = input.stderr_cursor.unwrap_or(0);
        let wait_ms = input
            .wait_ms
            .unwrap_or(0)
            .min(MAX_READ_WAIT.as_millis() as u64);
        task.wait_for_output(
            stdout_cursor,
            stderr_cursor,
            std::time::Duration::from_millis(wait_ms),
        )
        .await;
        task.supply_sudo_password(context).await?;
        let output = task.view(
            stdout_cursor,
            stderr_cursor,
            input
                .max_bytes
                .unwrap_or(INITIAL_READ_BYTES)
                .min(MAX_READ_BYTES),
        );
        json_output(output)
    }
}

struct SystemCommandTaskStopTool {
    config: SystemCommandConfig,
    tasks: CommandTaskManager,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SystemCommandTaskStopInput {
    task_id: String,
    stdout_cursor: Option<u64>,
    stderr_cursor: Option<u64>,
    max_bytes: Option<usize>,
}

#[async_trait]
impl Tool for SystemCommandTaskStopTool {
    async fn call(&self, arguments: Value, context: &RunContext) -> Result<ToolOutput, ToolError> {
        let input = parse::<SystemCommandTaskStopInput>(arguments)?;
        let task = self.tasks.get(&context.session_id, &input.task_id)?;
        let stop_requested = task.request_stop();
        if stop_requested {
            task.wait_for_terminal(std::time::Duration::from_secs(3))
                .await;
        }
        self.config
            .authorize_existing_cwd(task.cwd().and_then(|cwd| cwd.to_str()))?;
        let output = task
            .view(
                input.stdout_cursor.unwrap_or(0),
                input.stderr_cursor.unwrap_or(0),
                input
                    .max_bytes
                    .unwrap_or(INITIAL_READ_BYTES)
                    .clamp(1, MAX_READ_BYTES),
            )
            .with_stop_requested(stop_requested);
        json_output(output)
    }
}

struct SystemCommandTaskWriteTool {
    config: SystemCommandConfig,
    tasks: CommandTaskManager,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SystemCommandTaskWriteInput {
    task_id: String,
    #[serde(default)]
    data: String,
    #[serde(default)]
    close_stdin: bool,
    stdout_cursor: Option<u64>,
    stderr_cursor: Option<u64>,
    wait_ms: Option<u64>,
}

#[async_trait]
impl Tool for SystemCommandTaskWriteTool {
    async fn call(&self, arguments: Value, context: &RunContext) -> Result<ToolOutput, ToolError> {
        let input = parse::<SystemCommandTaskWriteInput>(arguments)?;
        let task = self.tasks.get(&context.session_id, &input.task_id)?;
        if task.is_sudo() {
            return Err(ToolError::Failed(
                "sudo input is managed by the private host channel".into(),
            ));
        }
        self.config
            .authorize_existing_cwd(task.cwd().and_then(|cwd| cwd.to_str()))?;
        tokio::select! {
            result = task.write(input.data, input.close_stdin) => result?,
            _ = context.cancellation.cancelled() => {
                task.request_stop();
                return Err(ToolError::Indeterminate("stdin write cancelled; some input may already have been delivered".into()));
            }
        }
        let stdout = input.stdout_cursor.unwrap_or(0);
        let stderr = input.stderr_cursor.unwrap_or(0);
        task.wait_for_output(
            stdout,
            stderr,
            std::time::Duration::from_millis(input.wait_ms.unwrap_or(0).min(10000)),
        )
        .await;
        let output = task.view(stdout, stderr, INITIAL_READ_BYTES);
        // A successful stdin write is a successful tool operation even when
        // the command later exits nonzero or has reached a terminal status.
        json_output(output)
    }
}

struct SystemCommandListTool {
    config: SystemCommandConfig,
    tasks: CommandTaskManager,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SystemCommandListInput {
    #[serde(default)]
    include_completed: bool,
}

#[async_trait]
impl Tool for SystemCommandListTool {
    async fn call(&self, arguments: Value, context: &RunContext) -> Result<ToolOutput, ToolError> {
        let input = parse::<SystemCommandListInput>(arguments)?;
        json_output_with_error(
            json!({"commands":self.tasks.list(&context.session_id, input.include_completed).into_iter().filter(|task| self.config.authorize_existing_cwd(task.cwd.as_deref()).is_ok()).collect::<Vec<_>>()}),
            false,
        )
    }
}

pub(crate) fn parse<T: for<'de> Deserialize<'de>>(value: Value) -> Result<T, ToolError> {
    serde_json::from_value(value)
        .map_err(|error| ToolError::Failed(format!("tool input is invalid: {error}")))
}

pub(crate) async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, ToolError> + Send + 'static,
) -> Result<T, ToolError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| ToolError::Failed(format!("filesystem tool task failed: {error}")))?
}

pub(crate) fn json_output<T: Serialize>(value: T) -> Result<ToolOutput, ToolError> {
    json_output_with_error(value, false)
}

fn json_output_with_error<T: Serialize>(value: T, is_error: bool) -> Result<ToolOutput, ToolError> {
    let structured = serde_json::to_value(&value)
        .map_err(|error| ToolError::Failed(format!("tool output serialization failed: {error}")))?;
    let content = serde_json::to_string(&structured)
        .map_err(|error| ToolError::Failed(format!("tool output serialization failed: {error}")))?;
    let mut output = if is_error {
        ToolOutput::error(content)
    } else {
        ToolOutput::text(content)
    };
    output.structured = Some(structured);
    Ok(output)
}

fn append_limited(target: &mut String, value: &str, limit: usize) -> bool {
    let remaining = limit.saturating_sub(target.len());
    if value.len() <= remaining {
        target.push_str(value);
        return false;
    }
    if remaining > 0 {
        let mut boundary = remaining.min(value.len());
        while boundary > 0 && !value.is_char_boundary(boundary) {
            boundary -= 1;
        }
        target.push_str(&value[..boundary]);
    }
    true
}

fn walk_entries(
    directory: &Path,
    depth: usize,
    max_depth: usize,
    include_hidden: bool,
    visitor: &mut dyn FnMut(&Path, &fs::Metadata) -> bool,
) -> Result<bool, ToolError> {
    let entries = fs::read_dir(directory).map_err(|error| {
        ToolError::Failed(format!("list `{}` failed: {error}", directory.display()))
    })?;
    for entry in entries {
        let entry =
            entry.map_err(|error| ToolError::Failed(format!("list entry failed: {error}")))?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if !include_hidden && name.starts_with('.') {
            continue;
        }
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            ToolError::Failed(format!("inspect `{}` failed: {error}", path.display()))
        })?;
        if visitor(&path, &metadata) {
            return Ok(true);
        }
        if metadata.is_dir()
            && !metadata.file_type().is_symlink()
            && depth < max_depth
            && walk_entries(&path, depth + 1, max_depth, include_hidden, visitor)?
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn replace_text(
    source: &mut String,
    target: &str,
    replacement: &str,
    replace_all: bool,
) -> Result<usize, ToolError> {
    let occurrences = source.matches(target).count();
    if occurrences == 0 {
        return Err(ToolError::Failed("target text was not found".into()));
    }
    *source = if replace_all {
        source.replace(target, replacement)
    } else {
        source.replacen(target, replacement, 1)
    };
    Ok(if replace_all { occurrences } else { 1 })
}

fn insert_text(
    source: &mut String,
    target: &str,
    content: &str,
    replace_all: bool,
    before: bool,
) -> Result<usize, ToolError> {
    let replacement = if before {
        format!("{content}{target}")
    } else {
        format!("{target}{content}")
    };
    replace_text(source, target, &replacement, replace_all)
}
