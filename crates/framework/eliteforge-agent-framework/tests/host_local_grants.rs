use async_trait::async_trait;
use eliteforge_agent_framework::*;
use eliteforge_agent_official_plugin_tools_local::*;
use eliteforge_agent_test_support::TestBuilder;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

#[derive(Clone)]
struct Grant {
    cwd: PathBuf,
    roots: Vec<PathBuf>,
    ids: BTreeSet<String>,
}
struct Scope;
#[async_trait]
impl LocalToolScopeProvider for Scope {
    async fn grant(&self, run: &RunContext) -> Result<LocalToolGrant, AgentError> {
        let grant = run
            .extensions
            .get::<Grant>()
            .ok_or_else(|| AgentError::ToolSetup("missing host grant".into()))?;
        Ok(LocalToolGrant {
            terminal_owner: None,
            filesystem: FileSystemToolsConfig::new(&grant.cwd)
                .with_authorized_roots(grant.roots.clone())
                .with_authorized_symlinks(true),
            commands: SystemCommandConfig::new()
                .with_shell("/bin/sh")
                .with_async_after(Duration::from_millis(30)),
            tool_ids: grant.ids.clone(),
            call_names: BTreeMap::new(),
            exposed_tool_ids: None,
        })
    }
}
struct Invocation {
    name: String,
    arguments: Value,
    output: Mutex<Option<Value>>,
}
struct InvokeModel;
#[async_trait]
impl ChatModel for InvokeModel {
    async fn generate(
        &self,
        _: &str,
        request: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        let invocation = request.extensions.get::<Invocation>().unwrap();
        if request
            .messages
            .last()
            .is_some_and(|message| message.role == Role::Tool)
        {
            let message = request.messages.last().unwrap();
            *invocation.output.lock().unwrap() = Some(
                message
                    .tool_result
                    .as_ref()
                    .and_then(|result| result.structured.clone())
                    .unwrap_or_else(|| json!({"error":message.content.protocol_text()})),
            );
            return Ok(ChatResponse::text("done"));
        }
        assert!(
            request
                .tools
                .iter()
                .any(|tool| tool.call_name == invocation.name)
        );
        Ok(ChatResponse::tool_calls(vec![ToolCall::new(
            "provider-id",
            &invocation.name,
            invocation.arguments.clone(),
        )]))
    }
}
async fn invoke(agent: &Agent, session: &str, grant: Grant, name: &str, arguments: Value) -> Value {
    let mut request = RunRequest::new(session, "execute");
    request.extensions.insert(grant);
    request.extensions.insert(Invocation {
        name: name.into(),
        arguments,
        output: Mutex::new(None),
    });
    let invocation = request.extensions.get::<Invocation>().unwrap();
    agent.run(request).await.unwrap();
    invocation.output.lock().unwrap().take().unwrap()
}
fn grant(cwd: PathBuf, roots: Vec<PathBuf>) -> Grant {
    Grant {
        cwd,
        roots,
        ids: [
            "filesystem.read_text",
            "system.command.exec",
            "system.command.task.read",
            "system.command.task.stop",
            "filesystem.apply_patch",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect(),
    }
}

#[cfg(unix)]
struct ConcurrentCommandScope;

#[cfg(unix)]
#[async_trait]
impl LocalToolScopeProvider for ConcurrentCommandScope {
    async fn grant(&self, run: &RunContext) -> Result<LocalToolGrant, AgentError> {
        let grant = run
            .extensions
            .get::<Grant>()
            .ok_or_else(|| AgentError::ToolSetup("missing host grant".into()))?;
        Ok(LocalToolGrant {
            terminal_owner: None,
            filesystem: FileSystemToolsConfig::new(&grant.cwd)
                .with_authorized_roots(grant.roots.clone()),
            commands: SystemCommandConfig::new()
                .with_shell("/bin/sh")
                .with_async_after(Duration::from_secs(3)),
            tool_ids: ["system.command.exec"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            call_names: BTreeMap::from([("system.command.exec".into(), "bash_exec".into())]),
            exposed_tool_ids: None,
        })
    }
}

#[cfg(unix)]
#[derive(Clone, Default)]
struct ConcurrentCommandsModel {
    calls: Arc<AtomicUsize>,
    outputs: Arc<Mutex<Vec<Value>>>,
}

#[cfg(unix)]
#[async_trait]
impl ChatModel for ConcurrentCommandsModel {
    async fn generate(
        &self,
        _: &str,
        request: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            let calls = (0..3)
                .map(|index| {
                    let command = format!(
                        "printf x > started-{index}; n=0; until [ -f started-0 ] && [ -f started-1 ] && [ -f started-2 ]; do n=$((n+1)); if [ \"$n\" -ge 100 ]; then exit 9; fi; sleep 0.01; done; printf ready-{index}"
                    );
                    ToolCall::new(format!("command-{index}"), "bash_exec", json!({"command":command}))
                })
                .collect();
            return Ok(ChatResponse::tool_calls(calls));
        }
        *self.outputs.lock().unwrap() = request
            .messages
            .iter()
            .filter(|message| message.role == Role::Tool)
            .filter_map(|message| message.tool_result.as_ref()?.structured.clone())
            .collect();
        Ok(ChatResponse::text("done"))
    }
}

#[cfg(unix)]
#[tokio::test]
async fn commands_from_one_model_batch_execute_concurrently() {
    let temp = tempfile::tempdir().unwrap();
    let model = ConcurrentCommandsModel::default();
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(model.clone()))
        .plugin(ScopedLocalToolsPlugin::new(Arc::new(
            ConcurrentCommandScope,
        )))
        .build()
        .await
        .unwrap();
    let mut request = RunRequest::new("concurrent-commands", "run three commands");
    request.extensions.insert(grant(
        temp.path().to_path_buf(),
        vec![temp.path().to_path_buf()],
    ));
    request.limits.max_parallel_tools = 3;
    let result = tokio::time::timeout(Duration::from_secs(8), agent.run(request))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.status, RunStatus::Completed);
    let outputs = model.outputs.lock().unwrap().clone();
    assert_eq!(outputs.len(), 3);
    for (index, output) in outputs.iter().enumerate() {
        assert_eq!(output["exitCode"], 0, "command {index}: {output}");
        assert_eq!(output["stdout"]["content"], format!("ready-{index}"));
    }
    assert!(agent.framework().close().await.is_success());
}
#[tokio::test]
async fn host_grants_confine_multiple_directories_and_commands_survive_turns() {
    let temp = tempfile::tempdir().unwrap();
    let a = temp.path().join("a");
    let b = temp.path().join("b");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    std::fs::write(a.join("value.txt"), "from-a").unwrap();
    std::fs::write(b.join("value.txt"), "from-b").unwrap();
    let scope = grant(b.clone(), vec![a.clone(), b.clone()]);
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(InvokeModel))
        .plugin(ScopedLocalToolsPlugin::new(Arc::new(Scope)))
        .build()
        .await
        .unwrap();
    let relative = invoke(
        &agent,
        "s",
        scope.clone(),
        "fs_read_text",
        json!({"path":"value.txt"}),
    )
    .await;
    assert_eq!(relative["content"], "from-b");
    let absolute = invoke(
        &agent,
        "s",
        scope.clone(),
        "fs_read_text",
        json!({"path":a.join("value.txt")}),
    )
    .await;
    assert_eq!(absolute["content"], "from-a");
    let denied = invoke(
        &agent,
        "s",
        scope.clone(),
        "fs_read_text",
        json!({"path":temp.path().join("outside.txt")}),
    )
    .await;
    assert!(denied.to_string().contains("outside authorized roots"));
    let task = invoke(
        &agent,
        "s",
        scope.clone(),
        "command_exec",
        json!({"command":"printf start; sleep 20"}),
    )
    .await;
    let id = task["taskId"].as_str().expect("background command ID");
    let foreign = invoke(
        &agent,
        "other",
        scope.clone(),
        "command_exec_read",
        json!({"taskId":id}),
    )
    .await;
    assert!(foreign.get("error").is_some());
    let stopped = invoke(
        &agent,
        "s",
        scope.clone(),
        "command_exec_stop",
        json!({"taskId":id}),
    )
    .await;
    assert_eq!(stopped["taskId"], id);
    let no_shell = invoke(
        &agent,
        "s",
        scope.clone(),
        "command_exec",
        json!({"command":"printf not-a-program", "args":[]}),
    )
    .await;
    assert!(
        no_shell.get("error").is_some(),
        "explicit argv must not be interpreted as a shell string"
    );
    std::fs::remove_file(b.join("value.txt")).unwrap();
    std::fs::remove_dir(&b).unwrap();
    let missing_cwd = invoke(
        &agent,
        "s",
        scope,
        "command_exec",
        json!({"command":"printf should-not-run", "cwd":a}),
    )
    .await;
    assert!(
        missing_cwd.get("error").is_some(),
        "missing selected cwd must never fall back"
    );
    assert!(agent.framework().close().await.is_success());
}

#[tokio::test]
async fn context_patch_envelope_is_atomic_and_dry_run_does_not_write() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_path_buf();
    std::fs::write(root.join("a.txt"), "before\n").unwrap();
    let scope = grant(root.clone(), vec![root.clone()]);
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(InvokeModel))
        .plugin(ScopedLocalToolsPlugin::new(Arc::new(Scope)))
        .build()
        .await
        .unwrap();
    let patch = "*** Begin Patch\n*** Update File: a.txt\n@@\n-before\n+after\n*** Add File: nested/new.txt\n+new\n*** End Patch";
    let preview = invoke(
        &agent,
        "p",
        scope.clone(),
        "fs_apply_patch",
        json!({"patch":patch,"dryRun":true}),
    )
    .await;
    assert_eq!(preview["applied"], false);
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt")).unwrap(),
        "before\n"
    );
    assert!(!root.join("nested").exists());
    let applied = invoke(
        &agent,
        "p",
        scope.clone(),
        "fs_apply_patch",
        json!({"patch":patch}),
    )
    .await;
    assert_eq!(applied["applied"], true);
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt")).unwrap(),
        "after\n"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("nested/new.txt")).unwrap(),
        "new\n"
    );
    let bad = "*** Begin Patch\n*** Delete File: a.txt\n*** Update File: nested/new.txt\n@@\n-missing\n+bad\n*** End Patch";
    let rejected = invoke(&agent, "p", scope, "fs_apply_patch", json!({"patch":bad})).await;
    assert!(rejected.get("error").is_some());
    assert!(
        root.join("a.txt").exists(),
        "earlier operations cannot commit before all contexts validate"
    );
    assert!(agent.framework().close().await.is_success());
}

#[cfg(unix)]
#[tokio::test]
async fn symlink_targets_must_stay_inside_the_host_grant() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let a = temp.path().join("a");
    let b = temp.path().join("b");
    std::fs::create_dir(&a).unwrap();
    std::fs::create_dir(&b).unwrap();
    std::fs::write(b.join("value.txt"), "granted").unwrap();
    std::fs::write(temp.path().join("private.txt"), "private").unwrap();
    symlink(b.join("value.txt"), a.join("inside.txt")).unwrap();
    symlink(temp.path().join("private.txt"), a.join("outside.txt")).unwrap();
    let scope = grant(a.clone(), vec![a.clone(), b]);
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(InvokeModel))
        .plugin(ScopedLocalToolsPlugin::new(Arc::new(Scope)))
        .build()
        .await
        .unwrap();
    assert_eq!(
        invoke(
            &agent,
            "links",
            scope.clone(),
            "fs_read_text",
            json!({"path":"inside.txt"})
        )
        .await["content"],
        "granted"
    );
    let denied = invoke(
        &agent,
        "links",
        scope,
        "fs_read_text",
        json!({"path":"outside.txt"}),
    )
    .await;
    assert!(denied.get("error").is_some(), "{denied}");
    agent.framework().close().await;
}

#[cfg(unix)]
#[tokio::test]
async fn official_terminal_tools_run_through_framework_with_scoped_identity_and_host_service() {
    let temp = tempfile::tempdir().unwrap();
    let cwd = temp.path().canonicalize().unwrap();
    let mut scope = grant(cwd.clone(), vec![cwd.clone()]);
    scope.ids = pty_tools::terminal_tool_catalog()
        .into_iter()
        .map(|tool| tool.id)
        .collect();
    let pty = pty::PtyService::default();
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(InvokeModel))
        .plugin(ScopedLocalToolsPlugin::new(Arc::new(Scope)).with_pty(pty.clone()))
        .build()
        .await
        .unwrap();
    let started = invoke(
        &agent,
        "terminal-owner",
        scope.clone(),
        "terminal_start",
        json!({"command":"stty -echo; printf ready; read line; printf 'received=%s' \"$line\""}),
    )
    .await;
    let id = started["terminalId"]
        .as_str()
        .unwrap_or_else(|| panic!("start failed: {started}"));
    let denied = invoke(
        &agent,
        "other-owner",
        scope.clone(),
        "terminal_read",
        json!({"terminalId":id}),
    )
    .await;
    assert!(denied["error"].as_str().unwrap().contains("owner"));
    let escaped = invoke(
        &agent,
        "terminal-owner",
        scope.clone(),
        "terminal_start",
        json!({"command":"printf escaped", "cwd":cwd.parent().unwrap()}),
    )
    .await;
    assert!(escaped["error"].is_string(), "{escaped}");
    let bare = invoke(
        &agent,
        "terminal-owner",
        scope.clone(),
        "terminal_start",
        json!({"command":"bash"}),
    )
    .await;
    assert!(
        bare["error"]
            .as_str()
            .unwrap()
            .contains("interactive: true")
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !pty
        .read("terminal-owner", id, 0)
        .unwrap()
        .output
        .contains("ready")
    {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let written = invoke(
        &agent,
        "terminal-owner",
        scope.clone(),
        "terminal_write",
        json!({"terminalId":id,"data":"你好\n"}),
    )
    .await;
    assert_eq!(written["written"], true);
    let mut output;
    loop {
        output = invoke(
            &agent,
            "terminal-owner",
            scope.clone(),
            "terminal_read",
            json!({"terminalId":id}),
        )
        .await;
        if output["running"] == false && output["outputClosed"] == true {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "{output}");
    }
    assert!(output["output"].as_str().unwrap().contains("received=你好"));
    let started = invoke(
        &agent,
        "terminal-owner",
        scope.clone(),
        "terminal_start",
        json!({"command":"sleep 60"}),
    )
    .await;
    let active_id = started["terminalId"].as_str().unwrap();
    let stopped = invoke(
        &agent,
        "terminal-owner",
        scope.clone(),
        "terminal_stop",
        json!({"terminalId":active_id}),
    )
    .await;
    assert_eq!(stopped["stopped"], true);
    let list = invoke(&agent, "terminal-owner", scope, "terminal_list", json!({})).await;
    assert_eq!(list.as_array().unwrap().len(), 2);
    assert!(agent.framework().close().await.is_success());
    assert!(pty.read("terminal-owner", id, 0).is_err());
}
