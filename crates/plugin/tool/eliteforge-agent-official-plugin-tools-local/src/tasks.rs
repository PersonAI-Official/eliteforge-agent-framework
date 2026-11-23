use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    path::PathBuf,
    process::Stdio,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use eliteforge_agent_core::ToolError;
use serde::Serialize;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::{Child, ChildStdin, Command},
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
    time::{Instant, sleep, timeout},
};
use uuid::Uuid;

use crate::process_tree::ProcessTree;

const MAX_RUNNING_TASKS: usize = 32;
const MAX_RETAINED_TASKS: usize = 256;
pub(crate) const MAX_READ_WAIT: Duration = Duration::from_secs(10);
pub(crate) const MAX_READ_BYTES: usize = 64 * 1024;
pub(crate) const INITIAL_READ_BYTES: usize = 16 * 1024;
const OUTPUT_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

const MAX_INPUT_BYTES: usize = 64 * 1024;
type InputControl = Arc<Mutex<Option<mpsc::Sender<InputMessage>>>>;

pub(crate) struct CommandRequest {
    pub capture: Option<(crate::CommandOutputFile, crate::CommandOutputFile)>,
    pub session_id: String,
    pub command: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: BTreeMap<String, String>,
    pub env_remove: Vec<String>,
    pub stdin: Option<String>,
    pub sudo: Option<crate::sudo::SudoInput>,
    pub interactive: bool,
    pub tty: bool,
    pub rows: u16,
    pub cols: u16,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CommandSummary {
    task_id: String,
    command: String,
    args: Vec<String>,
    pub(crate) cwd: Option<String>,
    status: CommandTaskStatus,
    exit_code: Option<i32>,
    tty: bool,
    pid: u32,
    stdin_open: bool,
    age_ms: u64,
}

struct InputMessage {
    data: zeroize::Zeroizing<String>,
    append_newline: bool,
    close: bool,
    ack: oneshot::Sender<Result<(), String>>,
}

struct ProcessExit {
    success: bool,
    code: Option<i32>,
}

enum NativeChild {
    Pipes(Child),
    Pty(crate::pty_process::PtyProcess),
}

struct OwnedChild {
    native: Option<NativeChild>,
    tree: Option<ProcessTree>,
    pid: u32,
    finished: bool,
}

impl OwnedChild {
    fn new(native: NativeChild) -> std::io::Result<Self> {
        let pid = match &native {
            NativeChild::Pipes(child) => child.id(),
            NativeChild::Pty(child) => child.process_id(),
        }
        .ok_or_else(|| std::io::Error::other("command has no process ID"))?;
        let mut owned = Self {
            native: Some(native),
            tree: None,
            pid,
            finished: false,
        };
        if matches!(owned.native, Some(NativeChild::Pipes(_))) {
            owned.tree = Some(ProcessTree::attach(pid)?);
        }
        Ok(owned)
    }

    async fn wait(&mut self) -> std::io::Result<ProcessExit> {
        loop {
            let exit = match self.native.as_mut().expect("owned child") {
                NativeChild::Pipes(child) => child.try_wait()?.map(|s| ProcessExit {
                    success: s.success(),
                    code: s.code(),
                }),
                NativeChild::Pty(child) => child.try_wait()?.map(|s| ProcessExit {
                    success: s.success(),
                    code: if s.signal().is_some() {
                        None
                    } else {
                        i32::try_from(s.exit_code()).ok()
                    },
                }),
            };
            if let Some(exit) = exit {
                self.finished = true;
                return Ok(exit);
            }
            sleep(Duration::from_millis(10)).await;
        }
    }

    fn terminate(&mut self) {
        if let Some(tree) = &mut self.tree {
            tree.terminate();
        }
        match self.native.as_mut() {
            Some(NativeChild::Pipes(child)) if !self.finished => {
                let _ = child.start_kill();
            }
            Some(NativeChild::Pty(child)) => {
                let _ = child.kill();
            }
            _ => {}
        }
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        self.terminate();
        // Tokio reaps pipe children on drop. PTY children need a waiter even if
        // the supervising future was aborted or the runtime has shut down.
        if let Some(NativeChild::Pty(mut child)) = self.native.take()
            && !self.finished
        {
            let _ = std::thread::Builder::new()
                .name("command-pty-reaper".into())
                .spawn(move || {
                    let _ = child.wait();
                });
        }
    }
}

struct RunningCommand {
    child: OwnedChild,
    input: InputControl,
    stdout_reader: Option<JoinHandle<()>>,
    stderr_reader: Option<JoinHandle<()>>,
    stdin_writer: Option<JoinHandle<()>>,
}

impl RunningCommand {
    fn spawn(
        request: &CommandRequest,
        stdout: Arc<Mutex<BoundedOutput>>,
        stderr: Arc<Mutex<BoundedOutput>>,
        limit: usize,
        changes: watch::Sender<u64>,
    ) -> Result<Self, ToolError> {
        let fail = |error: String| {
            ToolError::Failed(format!(
                "command `{}` could not start: {error}",
                request.command
            ))
        };
        let (sender, receiver) = mpsc::channel(4);
        let input = Arc::new(Mutex::new(None));
        let mut process = if request.tty {
            if !request.env_remove.is_empty() {
                return Err(ToolError::Failed(
                    "PTY command environment removal is unsupported".into(),
                ));
            }
            let mut command = crate::PtyCommand::new(&request.command);
            command.args(&request.args);
            if let Some(cwd) = &request.cwd {
                command.cwd(cwd);
            }
            for (name, value) in &request.env {
                command.env(name, value);
            }
            let (process, reader, writer) =
                crate::pty_process::PtyProcess::spawn(&command, request.cols, request.rows)
                    .map_err(|error| fail(error.to_string()))?;
            let child = OwnedChild::new(NativeChild::Pty(process))
                .map_err(|error| fail(error.to_string()))?;
            Self {
                child,
                input: input.clone(),
                stdout_reader: Some(tokio::task::spawn_blocking(move || {
                    pump_pty_output(reader, stdout, limit, changes)
                })),
                stderr_reader: None,
                stdin_writer: Some(tokio::task::spawn_blocking(move || {
                    write_pty_input(writer, receiver)
                })),
            }
        } else {
            let mut command = Command::new(&request.command);
            crate::command_shell::process_arguments(&mut command, &request.command, &request.args);
            command
                .kill_on_drop(true)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            #[cfg(windows)]
            command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
            if let Some(cwd) = &request.cwd {
                command.current_dir(cwd);
            }
            for key in &request.env_remove {
                command.env_remove(key);
            }
            command.envs(&request.env);
            command.stdin(if request.interactive || request.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            });
            #[cfg(unix)]
            command.process_group(0);
            let child = command.spawn().map_err(|e| fail(e.to_string()))?;
            let mut child =
                OwnedChild::new(NativeChild::Pipes(child)).map_err(|e| fail(e.to_string()))?;
            let Some(NativeChild::Pipes(native)) = child.native.as_mut() else {
                unreachable!()
            };
            let reader = native.stdout.take().expect("piped stdout");
            let error_reader = native.stderr.take().expect("piped stderr");
            let writer = native.stdin.take();
            Self {
                stdout_reader: Some(tokio::spawn(pump_output(
                    reader,
                    stdout,
                    limit,
                    changes.clone(),
                ))),
                stderr_reader: Some(tokio::spawn(pump_output(
                    error_reader,
                    stderr,
                    limit,
                    changes,
                ))),
                stdin_writer: writer.map(|writer| tokio::spawn(write_pipe_input(writer, receiver))),
                child,
                input: input.clone(),
            }
        };
        if process.stdin_writer.is_some() {
            if let Some(data) = &request.stdin {
                let (ack, _) = oneshot::channel();
                sender
                    .try_send(InputMessage {
                        data: zeroize::Zeroizing::new(data.clone()),
                        append_newline: false,
                        close: !request.interactive && !request.tty,
                        ack,
                    })
                    .map_err(|_| fail("initial stdin queue is unavailable".into()))?;
            }
            if request.interactive || request.tty {
                *lock(&input) = Some(sender);
            }
        }
        // Non-interactive stdin closes after its initial payload, as before.
        process.input = input;
        Ok(process)
    }
}

impl Drop for RunningCommand {
    fn drop(&mut self) {
        self.child.terminate();
        lock(&self.input).take();
        for task in [&self.stdout_reader, &self.stderr_reader, &self.stdin_writer]
            .into_iter()
            .flatten()
        {
            task.abort();
        }
    }
}

async fn write_pipe_input(mut writer: ChildStdin, mut receiver: mpsc::Receiver<InputMessage>) {
    while let Some(message) = receiver.recv().await {
        let result = async {
            writer.write_all(message.data.as_bytes()).await?;
            if message.append_newline {
                writer.write_all(b"\n").await?;
            }
            Ok::<(), std::io::Error>(())
        }
        .await
        .map_err(|e| e.to_string());
        let failed = result.is_err();
        let _ = message.ack.send(result);
        if failed || message.close {
            break;
        }
    }
}

fn write_pty_input(
    mut writer: Box<dyn std::io::Write + Send>,
    mut receiver: mpsc::Receiver<InputMessage>,
) {
    while let Some(message) = receiver.blocking_recv() {
        let result = writer
            .write_all(message.data.as_bytes())
            .and_then(|_| {
                if message.append_newline {
                    writer.write_all(b"\n")
                } else {
                    Ok(())
                }
            })
            .and_then(|_| writer.flush())
            .map_err(|e| e.to_string());
        let failed = result.is_err();
        let _ = message.ack.send(result);
        if failed || message.close {
            break;
        }
    }
}

fn pump_pty_output(
    mut reader: Box<dyn std::io::Read + Send>,
    output: Arc<Mutex<BoundedOutput>>,
    limit: usize,
    changes: watch::Sender<u64>,
) {
    let mut buffer = [0; 8192];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => {
                lock(&output).finish(CaptureStatus::Eof, None);
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => {
                lock(&output).finish(CaptureStatus::ReadError, Some(error.to_string()));
                break;
            }
            Ok(count) => {
                lock(&output).push(&buffer[..count], limit.max(1));
                changes.send_modify(|revision| *revision = revision.wrapping_add(1));
            }
        }
    }
}

#[derive(Clone, Default)]
pub(crate) struct CommandTaskManager {
    tasks: Arc<Mutex<HashMap<String, Arc<CommandTask>>>>,
}

impl CommandTaskManager {
    pub(crate) fn start(
        &self,
        mut request: CommandRequest,
        timeout_limit: Duration,
        output_limit: usize,
    ) -> Result<Arc<CommandTask>, ToolError> {
        let mut tasks = lock(&self.tasks);
        evict_finished_tasks(&mut tasks);
        let running = tasks
            .values()
            .filter(|task| !task.state_tx.borrow().status.is_terminal())
            .count();
        if running >= MAX_RUNNING_TASKS {
            return Err(ToolError::Failed(format!(
                "the Agent already has {MAX_RUNNING_TASKS} running commands"
            )));
        }
        if tasks.len() >= MAX_RETAINED_TASKS {
            return Err(ToolError::Failed(
                "the Agent command history is full".into(),
            ));
        }
        let (state_tx, _) = watch::channel(CommandTaskState::running());
        let (changes_tx, _) = watch::channel(0_u64);
        let (stop_tx, stop_rx) = watch::channel(false);
        let (stdout_capture, stderr_capture) = match request.capture.take() {
            Some((stdout, stderr)) => (Some(stdout), Some(stderr)),
            None => (None, None),
        };
        let stdout_buffer = Arc::new(Mutex::new(BoundedOutput::new(stdout_capture)));
        let stderr_buffer = Arc::new(Mutex::new(BoundedOutput::new(stderr_capture)));
        if request.tty {
            lock(&stderr_buffer).finish(CaptureStatus::Eof, None);
        }
        let process = RunningCommand::spawn(
            &request,
            stdout_buffer.clone(),
            stderr_buffer.clone(),
            output_limit,
            changes_tx.clone(),
        )?;
        let task_id = format!("cmd-{}", Uuid::new_v4());
        let task = Arc::new(CommandTask {
            task_id: task_id.clone(),
            session_id: request.session_id,
            command: request.command,
            args: request.args,
            cwd: request.cwd,
            tty: request.tty,
            sudo: request.sudo,
            pid: process.child.pid,
            input: process.input.clone(),
            created_at: Instant::now(),
            state_tx: state_tx.clone(),
            changes_tx: changes_tx.clone(),
            stop_tx,
            monitor: Mutex::new(None),
            stdout: stdout_buffer.clone(),
            stderr: stderr_buffer.clone(),
        });
        let monitor = tokio::spawn(supervise(
            process,
            state_tx,
            changes_tx,
            stop_rx,
            timeout_limit,
            stdout_buffer,
            stderr_buffer,
        ));
        *lock(&task.monitor) = Some(monitor);
        tasks.insert(task_id, task.clone());
        Ok(task)
    }

    pub(crate) fn list(&self, session_id: &str, include_completed: bool) -> Vec<CommandSummary> {
        let mut tasks: Vec<_> = lock(&self.tasks)
            .values()
            .filter(|task| task.session_id == session_id)
            .filter(|task| include_completed || !task.state_tx.borrow().status.is_terminal())
            .cloned()
            .collect();
        tasks.sort_by_key(|task| task.created_at);
        tasks.iter().map(|task| task.summary()).collect()
    }

    pub(crate) fn get(
        &self,
        session_id: &str,
        task_id: &str,
    ) -> Result<Arc<CommandTask>, ToolError> {
        let tasks = lock(&self.tasks);
        tasks
            .get(task_id)
            .filter(|task| task.session_id == session_id)
            .cloned()
            .ok_or_else(|| ToolError::Failed(format!("command task `{task_id}` was not found")))
    }

    pub(crate) fn stop_all(&self) {
        for task in lock(&self.tasks).values() {
            task.request_stop();
        }
    }

    pub(crate) async fn shutdown(&self) -> Result<(), String> {
        let tasks: Vec<_> = lock(&self.tasks).values().cloned().collect();
        for task in &tasks {
            task.request_stop();
        }
        timeout(Duration::from_secs(5), async {
            loop {
                if tasks
                    .iter()
                    .all(|task| task.state_tx.borrow().status.is_terminal())
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .map_err(|_| "command supervisors did not finish during shutdown".to_owned())
    }
}

fn evict_finished_tasks(tasks: &mut HashMap<String, Arc<CommandTask>>) {
    while tasks.len() >= MAX_RETAINED_TASKS {
        let oldest_finished = tasks
            .iter()
            .filter(|(_, task)| task.state_tx.borrow().status.is_terminal())
            .min_by_key(|(_, task)| task.created_at)
            .map(|(id, _)| id.clone());
        let Some(task_id) = oldest_finished else {
            return;
        };
        tasks.remove(&task_id);
    }
}

pub(crate) struct CommandTask {
    task_id: String,
    session_id: String,
    command: String,
    args: Vec<String>,
    cwd: Option<PathBuf>,
    tty: bool,
    pid: u32,
    input: InputControl,
    sudo: Option<crate::sudo::SudoInput>,
    created_at: Instant,
    state_tx: watch::Sender<CommandTaskState>,
    changes_tx: watch::Sender<u64>,
    stop_tx: watch::Sender<bool>,
    monitor: Mutex<Option<JoinHandle<()>>>,
    stdout: Arc<Mutex<BoundedOutput>>,
    stderr: Arc<Mutex<BoundedOutput>>,
}

impl CommandTask {
    pub(crate) fn is_sudo(&self) -> bool {
        self.sudo.is_some()
    }
    pub(crate) async fn supply_sudo_password(
        &self,
        run: &eliteforge_agent_core::RunContext,
    ) -> Result<(), ToolError> {
        if let Some(input) = &self.sudo {
            input.supply(self, run).await?;
        }
        Ok(())
    }

    pub(crate) fn cwd(&self) -> Option<&std::path::Path> {
        self.cwd.as_deref()
    }
    fn summary(&self) -> CommandSummary {
        let state = self.state_tx.borrow().clone();
        CommandSummary {
            task_id: self.task_id.clone(),
            command: self.command.clone(),
            args: self.args.clone(),
            cwd: self.cwd.as_ref().map(|p| p.display().to_string()),
            tty: self.tty,
            pid: self.pid,
            status: state.status,
            exit_code: state.exit_code,
            age_ms: self.created_at.elapsed().as_millis().min(u64::MAX as u128) as u64,
            stdin_open: self.stdin_open(),
        }
    }

    fn stdin_open(&self) -> bool {
        lock(&self.input)
            .as_ref()
            .is_some_and(|sender| !sender.is_closed())
    }

    pub(crate) async fn write(&self, data: String, close: bool) -> Result<(), ToolError> {
        self.write_input(zeroize::Zeroizing::new(data), close, false)
            .await
    }
    pub(crate) async fn write_secret(
        &self,
        data: zeroize::Zeroizing<String>,
    ) -> Result<(), ToolError> {
        self.write_input(data, true, true).await
    }
    async fn write_input(
        &self,
        data: zeroize::Zeroizing<String>,
        close: bool,
        append_newline: bool,
    ) -> Result<(), ToolError> {
        if data.len() > MAX_INPUT_BYTES {
            return Err(ToolError::Failed(format!(
                "stdin input exceeds {MAX_INPUT_BYTES} bytes"
            )));
        }
        if self.state_tx.borrow().status.is_terminal() {
            return Err(ToolError::Failed("the command has already finished".into()));
        }
        let sender = lock(&self.input).clone().ok_or_else(|| {
            ToolError::Failed(
                "stdin is closed; start the command with interactive=true or tty=true".into(),
            )
        })?;
        let (ack, receipt) = oneshot::channel();
        let permit = timeout(MAX_READ_WAIT, sender.reserve())
            .await
            .map_err(|_| ToolError::Failed("stdin queue is full; no input was sent".into()))?
            .map_err(|_| ToolError::Failed("command stdin is closed".into()))?;
        permit.send(InputMessage {
            data,
            append_newline,
            close,
            ack,
        });
        match timeout(MAX_READ_WAIT, receipt).await {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(Ok(Err(error))) => Err(ToolError::Indeterminate(format!("stdin write failed (input may be partial): {error}"))),
            _ => Err(ToolError::Indeterminate("stdin write was interrupted or timed out; input may have been delivered, do not blindly resend".into())),
        }
    }

    pub(crate) async fn wait_for_terminal(&self, max_wait: Duration) {
        let mut state = self.state_tx.subscribe();
        let _ = timeout(max_wait, async {
            loop {
                if state.borrow().status.is_terminal() || state.changed().await.is_err() {
                    return;
                }
            }
        })
        .await;
    }

    pub(crate) async fn wait_for_output(
        &self,
        stdout_cursor: u64,
        stderr_cursor: u64,
        max_wait: Duration,
    ) {
        let mut changes = self.changes_tx.subscribe();
        let revision = *changes.borrow_and_update();
        let has_output = [&self.stdout, &self.stderr]
            .into_iter()
            .zip([stdout_cursor, stderr_cursor])
            .any(|(stream, cursor)| {
                let page = lock(stream).read(cursor, MAX_READ_BYTES);
                !page.content.is_empty() || page.dropped_bytes > 0
            });
        if has_output || self.state_tx.borrow().status.is_terminal() || max_wait.is_zero() {
            return;
        }
        let _ = timeout(max_wait, async {
            loop {
                if *changes.borrow() != revision || self.state_tx.borrow().status.is_terminal() {
                    return;
                }
                if changes.changed().await.is_err() {
                    return;
                }
            }
        })
        .await;
    }

    pub(crate) fn request_stop(&self) -> bool {
        if self.state_tx.borrow().status.is_terminal() {
            return false;
        }
        self.stop_tx.send_replace(true);
        true
    }

    pub(crate) fn view(
        &self,
        stdout_cursor: u64,
        stderr_cursor: u64,
        max_bytes: usize,
    ) -> CommandTaskOutput {
        let state = self.state_tx.borrow().clone();
        let stdout = lock(&self.stdout).read(stdout_cursor, max_bytes);
        let stderr = lock(&self.stderr).read(stderr_cursor, max_bytes);
        CommandTaskOutput {
            task_id: self.task_id.clone(),
            status: state.status,
            command: self.command.clone(),
            args: self.args.clone(),
            cwd: self.cwd.as_ref().map(|path| path.display().to_string()),
            success: state.success,
            exit_code: state.exit_code,
            error: state.error.clone(),
            stdout,
            stderr,
            stop_requested: None,
            tty: self.tty,
            pid: self.pid,
            stdin_open: self.stdin_open(),
        }
    }
}

impl Drop for CommandTask {
    fn drop(&mut self) {
        self.stop_tx.send_replace(true);
        if let Some(monitor) = lock(&self.monitor).take() {
            monitor.abort();
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum CommandTaskStatus {
    Running,
    Completed,
    Cancelled,
    TimedOut,
    Failed,
}

impl CommandTaskStatus {
    fn is_terminal(self) -> bool {
        self != Self::Running
    }
}

#[derive(Debug, Clone)]
struct CommandTaskState {
    status: CommandTaskStatus,
    success: Option<bool>,
    exit_code: Option<i32>,
    error: Option<String>,
}

impl CommandTaskState {
    fn running() -> Self {
        Self {
            status: CommandTaskStatus::Running,
            success: None,
            exit_code: None,
            error: None,
        }
    }

    fn from_status(status: ProcessExit) -> Self {
        Self {
            status: CommandTaskStatus::Completed,
            success: Some(status.success),
            exit_code: status.code,
            error: None,
        }
    }

    fn terminal(status: CommandTaskStatus, error: Option<String>) -> Self {
        Self {
            status,
            success: Some(false),
            exit_code: None,
            error,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CommandTaskOutput {
    task_id: String,
    status: CommandTaskStatus,
    command: String,
    args: Vec<String>,
    cwd: Option<String>,
    /// Command-process result, not the tool invocation status. `None` while a
    /// task is still running; `false` is a normal result for a nonzero exit.
    success: Option<bool>,
    exit_code: Option<i32>,
    error: Option<String>,
    stdout: OutputPage,
    stderr: OutputPage,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop_requested: Option<bool>,
    tty: bool,
    pid: u32,
    stdin_open: bool,
}

impl CommandTaskOutput {
    pub(crate) fn stderr_contains(&self, marker: &str) -> bool {
        self.stderr.content.contains(marker)
    }

    pub(crate) fn progress(&self) -> (&str, &str, u64, u64, bool) {
        (
            &self.stdout.content,
            &self.stderr.content,
            self.stdout.next_cursor,
            self.stderr.next_cursor,
            self.status.is_terminal(),
        )
    }

    pub(crate) fn with_stop_requested(mut self, stop_requested: bool) -> Self {
        self.stop_requested = Some(stop_requested);
        self
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum CaptureStatus {
    Running,
    Eof,
    ReadError,
    DrainTimeout,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct FileCaptureMetadata {
    status: &'static str,
    captured_bytes: u64,
    max_bytes: u64,
    file_ref: Option<serde_json::Value>,
    error: Option<String>,
}

struct BoundedOutput {
    bytes: VecDeque<u8>,
    end_cursor: u64,
    capture_status: CaptureStatus,
    capture_error: Option<String>,
    file: Option<crate::CommandOutputFile>,
    file_capture: Option<FileCaptureMetadata>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OutputPage {
    content: String,
    page_start_cursor: u64,
    next_cursor: u64,
    end_cursor: u64,
    has_more: bool,
    dropped_bytes: u64,
    capture_status: CaptureStatus,
    capture_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    file_capture: Option<FileCaptureMetadata>,
}

impl BoundedOutput {
    fn new(file: Option<crate::CommandOutputFile>) -> Self {
        let file_capture = file.as_ref().map(|file| FileCaptureMetadata {
            status: "writing",
            captured_bytes: 0,
            max_bytes: file.max_bytes,
            file_ref: None,
            error: None,
        });
        Self {
            bytes: VecDeque::new(),
            end_cursor: 0,
            capture_status: CaptureStatus::Running,
            capture_error: None,
            file,
            file_capture,
        }
    }

    fn finish(&mut self, status: CaptureStatus, error: Option<String>) {
        if self.capture_status == CaptureStatus::Running {
            self.capture_status = status;
            self.capture_error = error;
        }
    }

    fn push(&mut self, incoming: &[u8], capacity: usize) {
        // A blocking PTY reader can outlive its drain deadline. Freeze the
        // snapshot before publishing files so no bytes arrive after finalization.
        if self.capture_status != CaptureStatus::Running {
            return;
        }
        if let (Some(file), Some(meta)) = (&mut self.file, &mut self.file_capture)
            && meta.status == "writing"
        {
            let count = incoming
                .len()
                .min(meta.max_bytes.saturating_sub(meta.captured_bytes) as usize);
            let mut written = 0;
            while written < count {
                match std::io::Write::write(&mut file.writer, &incoming[written..count]) {
                    Ok(0) => {
                        meta.status = "write_error";
                        meta.error = Some("output file writer made no progress".into());
                        break;
                    }
                    Ok(bytes) => {
                        written += bytes;
                        meta.captured_bytes += bytes as u64;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error) => {
                        meta.status = "write_error";
                        meta.error = Some(error.to_string());
                        break;
                    }
                }
            }
            if meta.status == "writing" && count < incoming.len() {
                meta.status = "limit_exceeded";
            }
        }
        self.end_cursor += incoming.len() as u64;
        self.bytes.extend(incoming);
        let excess = self.bytes.len().saturating_sub(capacity);
        self.bytes.drain(..excess);
    }

    fn read(&self, cursor: u64, max_bytes: usize) -> OutputPage {
        let retained_start = self.end_cursor.saturating_sub(self.bytes.len() as u64);
        let effective_cursor = cursor.clamp(retained_start, self.end_cursor);
        let mut offset = (effective_cursor - retained_start) as usize;
        // Never start in the middle of a valid UTF-8 scalar after eviction.
        while offset < self.bytes.len() && self.bytes[offset] & 0xc0 == 0x80 {
            offset += 1;
        }
        let mut end = (offset + max_bytes.max(1)).min(self.bytes.len());
        if end < self.bytes.len() && self.bytes[end] & 0xc0 == 0x80 {
            while end > offset && self.bytes[end] & 0xc0 == 0x80 {
                end -= 1;
            }
            // A page budget smaller than one scalar may exceed its budget by
            // up to three bytes so even maxBytes=1 makes forward progress.
            if end == offset {
                end = (offset + max_bytes.max(1)).min(self.bytes.len());
                while end < self.bytes.len() && self.bytes[end] & 0xc0 == 0x80 {
                    end += 1;
                }
            }
        }
        let mut bytes: Vec<_> = self
            .bytes
            .iter()
            .skip(offset)
            .take(end - offset)
            .copied()
            .collect();
        if self.capture_status == CaptureStatus::Running {
            // A read can observe a pipe chunk ending mid-scalar. Keep its
            // incomplete suffix for the next page, including after invalid bytes.
            let mut valid = 0;
            while valid < bytes.len() {
                match std::str::from_utf8(&bytes[valid..]) {
                    Ok(_) => break,
                    Err(error) => match error.error_len() {
                        Some(length) => valid += error.valid_up_to() + length,
                        None => {
                            bytes.truncate(valid + error.valid_up_to());
                            break;
                        }
                    },
                }
            }
        }
        let page_start_cursor = retained_start + offset as u64;
        let next_cursor = page_start_cursor + bytes.len() as u64;
        OutputPage {
            content: String::from_utf8_lossy(&bytes).into_owned(),
            page_start_cursor,
            next_cursor,
            end_cursor: self.end_cursor,
            has_more: next_cursor < self.end_cursor,
            dropped_bytes: page_start_cursor.saturating_sub(cursor),
            capture_status: self.capture_status,
            capture_error: self.capture_error.clone(),
            file_capture: self.file_capture.clone(),
        }
    }
}

async fn publish_capture(output: Arc<Mutex<BoundedOutput>>) {
    let file = lock(&output).file.take();
    let Some(mut file) = file else {
        return;
    };
    let mut meta = lock(&output)
        .file_capture
        .clone()
        .expect("capture metadata");
    let result = tokio::task::spawn_blocking(move || {
        use std::io::Write;
        file.writer.flush().map_err(|error| error.to_string())?;
        // Empty streams have no attachment; the zero byte count is explicit.
        if meta.captured_bytes == 0 && meta.status != "write_error" {
            return Ok(None);
        }
        file.writer.publish().map(Some)
    })
    .await;
    match result {
        Ok(Ok(reference)) => {
            meta.file_ref = reference;
            if meta.status == "writing" {
                meta.status = "saved";
            }
        }
        error => {
            meta.status = "publish_error";
            meta.error = Some(match error {
                Ok(Err(error)) => error,
                Err(error) => error.to_string(),
                _ => unreachable!(),
            });
        }
    }
    lock(&output).file_capture = Some(meta);
}

async fn pump_output<R>(
    mut reader: R,
    output: Arc<Mutex<BoundedOutput>>,
    limit: usize,
    changes: watch::Sender<u64>,
) where
    R: AsyncRead + Unpin,
{
    let mut buffer = [0_u8; 8192];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) => {
                lock(&output).finish(CaptureStatus::Eof, None);
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => {
                lock(&output).finish(CaptureStatus::ReadError, Some(error.to_string()));
                break;
            }
            Ok(count) => {
                lock(&output).push(&buffer[..count], limit.max(1));
                changes.send_modify(|revision| *revision = revision.wrapping_add(1));
            }
        }
    }
}

async fn supervise(
    mut process: RunningCommand,
    state: watch::Sender<CommandTaskState>,
    changes: watch::Sender<u64>,
    mut stop: watch::Receiver<bool>,
    timeout_limit: Duration,
    stdout: Arc<Mutex<BoundedOutput>>,
    stderr: Arc<Mutex<BoundedOutput>>,
) {
    let terminal_state = tokio::select! {
        result = process.child.wait() => match result {
            Ok(status) => CommandTaskState::from_status(status),
            Err(error) => CommandTaskState::terminal(CommandTaskStatus::Failed, Some(format!("waiting for the command failed: {error}"))),
        },
        _ = wait_for_stop(&mut stop) => CommandTaskState::terminal(CommandTaskStatus::Cancelled, None),
        _ = sleep(timeout_limit) => CommandTaskState::terminal(CommandTaskStatus::TimedOut, Some("command exceeded its configured timeout".into())),
    };
    // Always reap the direct child and terminate remaining members of its group,
    // including when the shell exits successfully while children remain alive.
    process.child.terminate();
    if !process.child.finished {
        let _ = timeout(OUTPUT_DRAIN_TIMEOUT, process.child.wait()).await;
    }
    lock(&process.input).take();
    // ConPTY closes after its output reader has started draining.
    let drain = async {
        for (reader, output) in [
            (&mut process.stdout_reader, &stdout),
            (&mut process.stderr_reader, &stderr),
        ] {
            if let Some(reader) = reader
                && let Err(error) = reader.await
            {
                lock(output).finish(CaptureStatus::ReadError, Some(error.to_string()));
            }
        }
        if let Some(writer) = &mut process.stdin_writer {
            let _ = writer.await;
        }
    };
    if timeout(OUTPUT_DRAIN_TIMEOUT, drain).await.is_err() {
        for reader in [
            &process.stdout_reader,
            &process.stderr_reader,
            &process.stdin_writer,
        ]
        .into_iter()
        .flatten()
        {
            reader.abort();
        }
    }
    for output in [&stdout, &stderr] {
        lock(output).finish(
            CaptureStatus::DrainTimeout,
            Some("output reader did not reach EOF before the drain deadline".into()),
        );
    }
    // Publish outside stream locks and runtime worker threads.
    tokio::join!(publish_capture(stdout), publish_capture(stderr));
    state.send_replace(terminal_state);
    changes.send_modify(|revision| *revision = revision.wrapping_add(1));
}

async fn wait_for_stop(stop: &mut watch::Receiver<bool>) {
    loop {
        if *stop.borrow() || stop.changed().await.is_err() {
            return;
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
