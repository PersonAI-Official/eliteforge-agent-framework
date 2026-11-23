use async_trait::async_trait;
use eliteforge_agent_framework::*;
use eliteforge_agent_official_plugin_tools_local::*;
use eliteforge_agent_test_support::TestBuilder;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
struct LiveScope(Mutex<LocalToolScopeSnapshot>);
#[async_trait]
impl LocalToolScopeProvider for LiveScope {
    async fn grant(&self, _: &RunContext) -> Result<LocalToolGrant, AgentError> {
        Ok(self.0.lock().unwrap().grant.clone())
    }
    async fn snapshot(&self, _: &RunContext) -> Result<LocalToolScopeSnapshot, AgentError> {
        Ok(self.0.lock().unwrap().clone())
    }
}
fn snapshot(rev: &str, cwd: &std::path::Path, roots: Vec<PathBuf>) -> LocalToolScopeSnapshot {
    LocalToolScopeSnapshot::new(
        rev,
        LocalToolGrant {
            terminal_owner: None,
            filesystem: FileSystemToolsConfig::new(cwd)
                .with_authorized_roots(roots)
                .with_authorized_symlinks(true),
            commands: SystemCommandConfig::new()
                .with_shell("/bin/sh")
                .with_async_after(Duration::from_millis(100)),
            tool_ids: [
                "filesystem.read_text",
                "filesystem.search.glob",
                "system.command.task.read",
                "framework.terminal.write",
                "system.command.exec",
                "system.command.task.stop",
                "framework.terminal.start",
                "framework.terminal.read",
                "framework.terminal.stop",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect::<BTreeSet<_>>(),
            call_names: BTreeMap::new(),
            exposed_tool_ids: None,
        },
    )
}
struct Script {
    scope: Arc<LiveScope>,
    a: PathBuf,
    b: PathBuf,
    mode: u8,
    turn: Mutex<usize>,
    outputs: Mutex<Vec<Value>>,
}
#[derive(Clone)]
struct Model(Arc<Script>);
#[async_trait]
impl ChatModel for Model {
    async fn generate(
        &self,
        _: &str,
        request: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        let s = &self.0;
        if let Some(m) = request.messages.last().filter(|m| m.role == Role::Tool) {
            s.outputs.lock().unwrap().push(
                m.tool_result
                    .as_ref()
                    .and_then(|r| r.structured.clone())
                    .unwrap_or_else(|| json!({"error":m.content.protocol_text()})),
            );
        }
        let step = {
            let mut t = s.turn.lock().unwrap();
            let step = *t;
            *t += 1;
            step
        };
        let call = if s.mode == 3 || s.mode == 4 {
            match step {
                0 => Some((
                    if s.mode == 3 {
                        "terminal_start"
                    } else {
                        "command_exec"
                    },
                    json!({"command":"printf started; sleep 20"}),
                )),
                1 => {
                    *s.scope.0.lock().unwrap() = snapshot("B", &s.b, vec![s.b.clone()]);
                    let out = s.outputs.lock().unwrap();
                    Some(if s.mode == 3 {
                        (
                            "terminal_write",
                            json!({"terminalId":out[0]["terminalId"],"data":"input"}),
                        )
                    } else {
                        ("command_exec_read", json!({"taskId":out[0]["taskId"]}))
                    })
                }
                2 => {
                    let out = s.outputs.lock().unwrap();
                    Some(if s.mode == 3 {
                        ("terminal_stop", json!({"terminalId":out[0]["terminalId"]}))
                    } else {
                        ("command_exec_stop", json!({"taskId":out[0]["taskId"]}))
                    })
                }
                _ => None,
            }
        } else if s.mode == 5 {
            match step {
                0 => Some(("search_glob", json!({"patterns":["*.txt"],"maxResults":1}))),
                1 => {
                    let cursor = s.outputs.lock().unwrap()[0]["nextCursor"].clone();
                    Some((
                        "search_glob",
                        json!({"patterns":["*.txt"],"maxResults":1,"cursor":cursor}),
                    ))
                }
                _ => None,
            }
        } else if s.mode != 0 {
            match step {
                0 => Some(("fs_read_text", json!({"path":"value.txt"}))),
                1 => {
                    *s.scope.0.lock().unwrap() =
                        snapshot(if s.mode == 1 { "B" } else { "A" }, &s.b, vec![s.b.clone()]);
                    Some((
                        "fs_read_text",
                        if s.mode == 1 {
                            json!({"path":s.a.join("value.txt")})
                        } else {
                            json!({"path":"value.txt"})
                        },
                    ))
                }
                _ => None,
            }
        } else {
            match step {
                0 => Some(("fs_read_text", json!({"path":"value.txt"}))),
                1 => Some((
                    "command_exec",
                    json!({"command":"printf started; sleep 20"}),
                )),
                2 => {
                    *s.scope.0.lock().unwrap() =
                        snapshot("B", &s.b, vec![s.a.clone(), s.b.clone()]);
                    Some(("fs_read_text", json!({"path":"value.txt"})))
                }
                3 => Some(("command_exec", json!({"command":"pwd"}))),
                4 => Some(("terminal_start", json!({"command":"pwd; sleep 20"}))),
                5 => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    let terminal = s.outputs.lock().unwrap()[4]["terminalId"].clone();
                    Some(("terminal_read", json!({"terminalId":terminal})))
                }
                6 => {
                    let terminal = s.outputs.lock().unwrap()[4]["terminalId"].clone();
                    Some(("terminal_stop", json!({"terminalId":terminal})))
                }
                7 => {
                    let task = s.outputs.lock().unwrap()[1]["taskId"].clone();
                    Some(("command_exec_stop", json!({"taskId":task})))
                }
                _ => None,
            }
        };
        Ok(match call {
            Some((name, input)) => {
                ChatResponse::tool_calls(vec![ToolCall::new(format!("call-{step}"), name, input)])
            }
            None => ChatResponse::text("done"),
        })
    }
}
async fn run(mode: u8) -> (tempfile::TempDir, Arc<Script>) {
    let temp = tempfile::tempdir().unwrap();
    let a = temp.path().join("a");
    let b = temp.path().join("b");
    std::fs::create_dir(&a).unwrap();
    std::fs::create_dir(&b).unwrap();
    let a = a.canonicalize().unwrap();
    let b = b.canonicalize().unwrap();
    std::fs::write(a.join("value.txt"), "from-a").unwrap();
    std::fs::write(b.join("value.txt"), "from-b").unwrap();
    std::fs::write(a.join("second.txt"), "second").unwrap();
    let scope = Arc::new(LiveScope(Mutex::new(snapshot("A", &a, vec![a.clone()]))));
    let script = Arc::new(Script {
        scope: scope.clone(),
        a,
        b,
        mode,
        turn: Mutex::new(0),
        outputs: Mutex::new(vec![]),
    });
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(Model(script.clone())))
        .plugin(ScopedLocalToolsPlugin::new(scope))
        .build()
        .await
        .unwrap();
    agent
        .run(RunRequest::new("same-run", "execute"))
        .await
        .unwrap();
    assert!(agent.framework().close().await.is_success());
    (temp, script)
}
#[tokio::test]
async fn same_run_scope_switch_changes_fs_command_and_pty_without_losing_task_owner() {
    let (_temp, s) = run(0).await;
    let out = s.outputs.lock().unwrap();
    assert_eq!(out[0]["content"], "from-a");
    assert_eq!(out[2]["content"], "from-b");
    assert!(
        out[3]["stdout"]["content"]
            .as_str()
            .unwrap()
            .contains(s.b.to_str().unwrap())
    );
    assert_eq!(out[4]["cwd"], s.b.to_str().unwrap());
    assert!(
        out[5]["output"]
            .as_str()
            .unwrap()
            .contains(s.b.to_str().unwrap())
    );
    assert_eq!(out[7]["taskId"], out[1]["taskId"]);
    assert_eq!(out[7]["status"], "cancelled");
}
#[tokio::test]
async fn invocation_scope_revokes_previous_paths() {
    let (_temp, s) = run(1).await;
    let out = s.outputs.lock().unwrap();
    assert_eq!(out[0]["content"], "from-a");
    assert!(
        out[1]["error"]
            .as_str()
            .unwrap()
            .contains("outside authorized roots")
    );
}
#[tokio::test]
async fn same_revision_cannot_change_authority() {
    let (_temp, s) = run(2).await;
    assert!(
        s.outputs.lock().unwrap()[1]["error"]
            .as_str()
            .unwrap()
            .contains("reused for different authority")
    );
}

struct BlockingScope {
    grant: LocalToolGrant,
    calls: std::sync::atomic::AtomicUsize,
    entered: tokio::sync::Notify,
    dropped: Arc<std::sync::atomic::AtomicBool>,
}
#[async_trait]
impl LocalToolScopeProvider for BlockingScope {
    async fn grant(&self, _: &RunContext) -> Result<LocalToolGrant, AgentError> {
        if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            return Ok(self.grant.clone());
        }
        struct DropGuard(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for DropGuard {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let _guard = DropGuard(self.dropped.clone());
        self.entered.notify_one();
        std::future::pending().await
    }
}
struct OneCall;
#[async_trait]
impl ChatModel for OneCall {
    async fn generate(
        &self,
        _: &str,
        _: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        Ok(ChatResponse::tool_calls(vec![ToolCall::new(
            "read",
            "fs_read_text",
            json!({"path":"value.txt"}),
        )]))
    }
}
async fn blocked_scope(deadline: bool) {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let scope = Arc::new(BlockingScope {
        grant: snapshot("one", &root, vec![root.clone()]).grant,
        calls: AtomicUsize::new(0),
        entered: tokio::sync::Notify::new(),
        dropped: Arc::new(AtomicBool::new(false)),
    });
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(OneCall))
        .plugin(ScopedLocalToolsPlugin::new(scope.clone()))
        .build()
        .await
        .unwrap();
    let mut request = RunRequest::new("blocked", "run");
    if deadline {
        request.limits.run_timeout = Duration::from_millis(100);
    }
    let handle = agent.start(request).unwrap();
    tokio::time::timeout(Duration::from_secs(2), scope.entered.notified())
        .await
        .unwrap();
    if !deadline {
        agent.cancel(&handle).unwrap();
    }
    assert!(agent.wait(handle).await.is_err());
    assert!(scope.dropped.load(Ordering::SeqCst));
    assert!(agent.framework().close().await.is_success());
}
#[tokio::test]
async fn async_scope_snapshot_is_cancelled_with_the_run() {
    blocked_scope(false).await;
}
#[tokio::test]
async fn async_scope_snapshot_obeys_the_run_deadline() {
    blocked_scope(true).await;
}

#[tokio::test]
async fn persistent_pty_input_is_revoked_but_stop_keeps_cleanup_access() {
    let (_temp, s) = run(3).await;
    let out = s.outputs.lock().unwrap();
    assert!(
        out[1]["error"]
            .as_str()
            .unwrap()
            .contains("outside authorized roots")
    );
    assert_eq!(out[2]["stopped"], true);
}
#[tokio::test]
async fn persistent_command_output_is_revoked_but_stop_keeps_cleanup_access() {
    let (_temp, s) = run(4).await;
    let out = s.outputs.lock().unwrap();
    assert!(
        out[1]["error"]
            .as_str()
            .unwrap()
            .contains("outside authorized roots")
    );
    assert_eq!(out[2]["taskId"], out[0]["taskId"]);
    assert!(out[2].get("stdout").is_none());
}
#[tokio::test]
async fn scope_rebinding_keeps_search_snapshot_pagination() {
    let (_temp, s) = run(5).await;
    let out = s.outputs.lock().unwrap();
    assert!(out[0]["nextCursor"].is_string());
    assert_eq!(out[1]["snapshot"], out[0]["snapshot"]);
    assert_eq!(out[1]["offset"], 1);
}
