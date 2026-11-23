# 官方工具与 Skills

本章沿用入门程序的官方包依赖。`official` 是构造中的 `EliteForgeOfficialCapabilityPack`，最终传给 `builder.capability_pack(official)`。`selection` 用于 `framework.agent(selection)`。

## 1. 读取工作区文件

```rust
use eliteforge_agent_official_capability_pack::tools::FileSystemToolsConfig;
use eliteforge_agent_framework::AgentSelection;

let official = official.read_only_filesystem(
    FileSystemToolsConfig::new("./workspace").with_max_read_bytes(64 * 1024),
);
let selection = AgentSelection {
    tool_providers: Some(vec!["framework.filesystem.readonly.tools".into()]),
    ..Default::default()
};
```

先由宿主创建/确定 `workspace`。文件工具参数使用相对根目录路径；普通静态配置拒绝 `..`、绝对路径和符号链接。模型可以读取、列目录和搜索，但不能写入。

要开放写入，额外调用 `.write_filesystem(FileSystemToolsConfig::new("./workspace"))`，并将 `framework.filesystem.write.tools` 加入同一 `tool_providers` 列表。不要用新的 selection 覆盖已有的业务工具选择。

| 模型工具 | 使用方法 | 返回值需要检查什么 |
| --- | --- | --- |
| `fs_read_text` | 传文件相对路径 | 读取范围、截断信息、`version` |
| `search_grep` / `search_glob` / `search_path` | 传查询及根内范围；有 `nextCursor` 可续页 | `complete`、`limitReason`、`truncated` |
| `fs_write_text` / `fs_apply_patch` | 修改已有文件时传读取所得 `expectedVersion` | 变更前后版本与 diff |
| `fs_apply_unified_patch` | 先 `dryRun: true`，再同参数正式提交 | 每个路径的预期版本与失败状态 |

统一补丁参数示例：

```json
{
  "patch": "--- note.txt\n+++ note.txt\n@@ -1 +1 @@\n-old\n+new\n",
  "expectedVersions": {"note.txt": "sha256:替换为读取返回的真实摘要"},
  "dryRun": true
}
```

新建路径的版本值为 `absent`。这不是跨进程文件系统事务；遇到 `Indeterminate` 需核对受影响路径，不能盲目重放补丁。

## 2. 执行命令

```rust
use eliteforge_agent_official_capability_pack::tools::{FileSystemToolsConfig, SystemCommandConfig};
let official = official.system_commands(
    SystemCommandConfig::new()
        .with_directory_scope(FileSystemToolsConfig::new("./workspace"))
        .with_timeout(std::time::Duration::from_secs(30)),
);
```

Agent 选择 `framework.system-command.tools`。命令权限由宿主工具策略和审批链路控制；`SystemCommandConfig` 不再提供旧的命令名白名单配置。`SystemCommandConfig::new()` 不是安全沙箱，不应直接用于未受信任用户。

模型入口 `command_exec` 只接收一个完整 `command` 字符串，由宿主 Shell 解释命令串联、管道、重定向和含空格路径。底层 `CommandRequest`／`invocation(command, Some(args))` 仍支持直接程序加独立参数，供能力包和内部调用使用；不要把这个旧接口重新暴露成模型字段。需要展示给模型的 schema 更窄时，可用 `ToolDescriptor::with_model_input_schema`，执行和校验仍使用完整 schema。

需要持续终端时，添加同版本 `eliteforge-agent-capability-tools`，构造共享 `PtyService` 并用 `pty_pack(service)` 安装。宿主终端 UI 和 Agent 复用同一个 service，关闭由其 owner 管理。每轮工作目录/权限变化的应用使用 `scoped_local_tools_pack(provider, service)`：`LocalToolScopeProvider` 从已认证的运行上下文返回授权快照；同一 revision 不能表示不同授权。不要让模型提交 root 后直接作为授权目录。

## 3. 接入 MCP

不为 MCP 已有工具再写一层 `Tool`。下面连接一个宿主已经安装的 stdio 服务；路径需要替换成真实可执行文件：

```rust
use eliteforge_agent_official_capability_pack::mcp::{McpConfig, McpToolsPlugin, McpTransport};

let mcp = McpToolsPlugin::new(
    "orders.mcp",
    McpConfig::new("orders", McpTransport::Stdio {
        program: "/opt/order-service/bin/order-mcp".into(),
        args: vec!["--stdio".into()],
        env: Default::default(),
        current_dir: None,
        inherit_env: false,
    }),
);
let official = official.mcp(mcp);
```

在 `AgentSelection.tool_providers` 中加入 `orders.mcp`。服务运行所需环境通过 `env` 显式传入；关闭环境继承后不要依赖 shell 初始化脚本。连接已有 HTTP 服务时改为：

```rust
let transport = McpTransport::StreamableHttp {
    url: "https://your-mcp-service.example/mcp".into(),
    headers: Default::default(),
};
```

该 URL 是示意值，替换为用户的真实服务；认证 header 由宿主秘密配置注入。构建 Framework 会连接 MCP，stdio 会启动进程。多个 MCP 插件使用不同 ID 与命名空间。

构建后可由宿主查看真实工具目录：

```rust
use eliteforge_agent_official_capability_pack::mcp::DiscoverMcpTools;
let tools = agent.execute(DiscoverMcpTools { plugin_id: "orders.mcp".into() }).await?;
for tool in tools { println!("{} -> {}", tool.id, tool.call_name); }
```

再按返回的内部 ID 收紧 `selection.tools`，不要猜测 MCP 的调用名和 ID。仅提供 resources 的 MCP 不会自动变成系统提示；宿主按资源工具/上下文需求接入。

## 4. 让 Agent 使用本地 Skills

目录结构：

```text
company-skills/
└── order-review/
    ├── SKILL.md
    └── references/
        └── policy.md
```

`SKILL.md` 最少包含：

```markdown
---
name: order-review
description: 查询订单并按售后规则判断是否需要人工处理。
---
先用 lookup_order 查询真实状态，再按 references/policy.md 的规则处理。
缺少订单号时向用户询问，不编造状态。
```

配置来源并完成三项选择：

```rust
use eliteforge_agent_official_capability_pack::skills::{FileSystemSkillsConfig, SkillsPlugin};
use eliteforge_agent_framework::AgentSelection;

let official = official.skills(SkillsPlugin::filesystem(
    FileSystemSkillsConfig::new()
        .without_system_directory()
        .with_local_directory("./company-skills"),
));
let selection = AgentSelection {
    tool_providers: Some(vec!["skills.default".into(), "shop.orders".into()]),
    context_sources: Some(vec!["skills.default".into(), "shop.instructions".into()]),
    skills: Some(vec!["order-review".into()]),
    ..Default::default()
};
```

没有订单包时删去两个 `shop.*` 项，并相应修改技能中的业务工具说明。

第一次模型输入含技能摘要；模型按需 `skill_load` 读取正文，`skill_read_resource` 读取 `references/policy.md`。来源已安装、技能名已授权、工具 Provider 已选择、目录上下文已选择，这四步缺一不可。`allowed-tools` 不会自动授予权限；技能里的脚本不会自动执行。

## 5. 随业务包发布 Skills

在业务 crate 的 `skills/order-review/` 中放入上述文件。`Cargo.toml` 同时声明：

```toml
[build-dependencies]
eliteforge-agent-core = { git = "https://gitee.com/person-ai/eliteforge-agent-framework.git", tag = "1.0.0" }
```

`build.rs` 完整内容：

```rust
fn main() { eliteforge_agent_core::capability_skills_build!(); }
```

包入口改为 `.skills(eliteforge_agent_core::capability_skills!())`；宿主 builder 配置 `.capability_skills_root("./agent-data/resources")`。只用内嵌来源时，官方包的 Skills 可改为 `SkillsPlugin::sources_only()`，避免额外扫描目录。Agent 的技能名、Provider 和上下文选择仍需要显式配置。

产品如果有“技能管理中心”，安装时还要遍历实际选中的包及其子包，读取 `CapabilityPack::registry_skill_sources()` 同步名称、来源、版本与资源。运行时 Provider 注册不会自动写入产品数据库，也不会自动给产品中的 Agent 分配技能。

## 6. Web、Media、Discovery

在官方包的 Cargo 依赖中加 `features = ["web", "media", "discovery"]`。按需使用，不必全部启用：

```rust
use eliteforge_agent_official_capability_pack::{
    web::{WebToolsPlugin, WebFetchConfig}, media::MediaToolsConfig,
};
let official = official
    .web(WebToolsPlugin::new().fetch(WebFetchConfig::restricted(["docs.rs"])))
    .media(MediaToolsConfig::new("./workspace"));
```

Agent 分别选择 `framework.web.tools`、`framework.media.tools`。搜索还需 `.search(backend)`；只有 fetch 配置不会凭空产生搜索服务。Media 提供根目录内图片/PDF 读取，不等于浏览器自动化。

工具太多时，把 Provider 交给 Discovery，而不是同时直接安装：

```rust
use eliteforge_agent_official_capability_pack::{
    discovery::ToolDiscoveryPlugin, web::{WebToolsPlugin, WebFetchConfig},
};
let discovery = ToolDiscoveryPlugin::new().provider(
    WebToolsPlugin::new().fetch(WebFetchConfig::restricted(["docs.rs"])),
);
let official = official.tool_discovery(discovery);
```

此时只选择 `framework.tools.discovery`；模型先看见 `tool_search`、`tool_schema`、`tool_execute` 三个基础入口。`tool_search` 只返回调用名和描述摘要，`tool_schema` 按精确调用名返回执行时使用的完整 schema（批量工具带 `items` 包装，单项工具保持原结构），`tool_execute` 执行网关后的工具。这里的 Web Provider 不再通过 `.web(...)` 安装第二次。发现只改变已授权工具的可见性，不扩大权限。

需要让少数已授权工具直接出现在模型工具列表中时，在 `RunRequest.extensions` 放入运行级白名单。白名单按稳定工具 ID 选择暴露方式，不注册执行器、不授予权限；未准入、隐藏或撤权的工具不会被暴露：

```rust
request.extensions.insert(eliteforge_agent_core::NativeToolWhitelist::new()
    .with("system.command.exec"));
```

原生工具仍走同一 binding、权限、审批、批量拆解和审计链路，调用时沿用 `{"items":[...]}` 契约（单项工具除外）。白名单默认为空，网关工具列表保持“三个基础入口 + 白名单中可用的原生工具”的确定性排序。

### 命令输出完整性与文件捕获

`command_exec` 默认只返回 stdout/stderr 各 16KiB 的预览，即使进程已完成也不能直接当作全量。两路输出现在都是对象：`content`、`pageStartCursor`、`nextCursor`、`endCursor`、`hasMore`、`droppedBytes`、`captureStatus`、`captureError`，不再提供旧游标和 Truncated 字段。把 `stdout.nextCursor`／`stderr.nextCursor` 传给 `command_exec_read` 的 `stdoutCursor`／`stderrCursor`。从零连续读取、所有页无丢失、两路正常 EOF 且无后续，才是完整输出。`captureStatus` 为 `running`、`eof`、`read_error` 或 `drain_timeout`；不要用进程退出状态替代采集状态。游标按原始字节计数，页边界保留完整 UTF-8 字符；小预算可能超出最多 3 字节。

大输出设置 `captureToFile: true`，宿主须在运行扩展注入 `CommandOutputCaptureService(Arc<dyn CommandOutputCaptureProvider>)`。Provider 的 `create(stream, run)` 返回 `CommandOutputFile { writer, max_bytes }`，writer 实现 `Write + CommandOutputWriter`，`publish()` 返回当前会话可读取的 `fileRef`；临时文件在 drop 时清理，已发布文件沿用会话资源生命周期。Tron 使用会话附件实现此服务。stdout/stderr 的 `fileCapture` 包含 `status`（`writing`、`saved`、`limit_exceeded`、`write_error`、`publish_error`）、`capturedBytes`、`maxBytes`、`fileRef`、`error`。文件独立于内存缓冲保存，正常 EOF、`saved` 且 `capturedBytes == endCursor` 才可认定文件完整；空流不生成附件，fileRef 为 null。文件超限、失败、取消及读取异常仍可保留部分文件，但不得把它当作完整证据。宿主不提供服务时，显式请求文件捕获会在进程启动前报错。
