---
name: eliteforge-agent-framework-integration
description: 使用 EliteForge Agent Framework 构建 Rust Agent 应用、插件、工具与能力包。新建 Tron 能力包时先调用随技能提供的生成器创建完整骨架，再填写业务内容；提供模型、上下文、Skills、MCP、审批、持久化、流式输出教程，以及 Tron 菜单编码与导航校验。
metadata:
  version: 1.4.0
---

# EliteForge Agent Framework 使用手册

本技能由 Framework 的 `eliteforge.agent-development` 能力包统一维护和分发；更新 Framework 接口时，同步此目录的说明、references 和模板。

本技能的 API 基线为携带此技能能力包的 Framework 提交，以宿主 Cargo.lock 锁定版本为准。正文和随附章节包含实现所需的代码，无需先取得 Framework 仓库或阅读 `docs/`。仅在目标项目使用其他版本、接口不匹配或需要修改框架内部实现时，查实际锁定版本的源码。

执行任务时，先读目标项目的 Cargo 清单与装配入口，再按下方教程实现。已有应用沿用其配置、目录和身份体系，不照搬示例的数据和全局作用域。

## 新建 Tron 能力包：先生成骨架，再填内容

内部开发者、外部开发者和 Tron 中的 Agent 均使用本技能。接到新建 Tron 能力包的任务后，执行以下流程；修改已有包时沿用原项目，不重新生成。

1. 从任务确定包 ID、名称、版本和输出目录；缺少业务需求时先澄清。检查 Node.js 22+ 是否可用。
2. 以本 `SKILL.md` 所在目录替换下方 `<skill-dir>`，用命令工具执行生成器。无需先读取全部模板文件或逐个编写基础文件。

   ```sh
   node "<skill-dir>/assets/tron-harness-cap-template/scripts/tron-cap.mjs" create --id com.example.assistant --name "示例助手" --version 0.1.0 --out "<用户工作目录>/example-assistant"
   ```

3. 生成器一次建立 `skills/`、`native/`、`ui/`、`tron/`、`mcp/`、`resources/`、`scripts/`，以及清单、Makefile 和开发说明。不分类型，不覆盖已有目录。若生成器文件缺失，说明技能资源不完整并修复分发，不能悄悄改成让模型重写模板。
4. 读取生成项目的 `AGENTS.md`、`README.md` 和需求相关文件，填写业务工具、页面、技能、导航、MCP 配置和资源。根据实际贡献调整清单，保留完整目录结构。示例 echo 工具与页面仅是骨架，不代表任务完成。
5. 按项目命令执行 `make install`、`make lint`、`make build`；原生构建另需 Rust 2024 工具链。核对业务行为与最终 ZIP，说明未验证的安装或外部服务环节。是否写测试遵循用户要求。

```mermaid
flowchart LR
    A[能力需求] --> B[本技能]
    B --> C[执行生成器]
    C --> D[完整项目骨架]
    D --> E[Agent 填写业务内容]
    E --> F[校验与构建]
```

模板与打包契约见[能力包约定](assets/tron-harness-cap-template/docs/pack-contract.md)。只按需读取；普通 Framework 应用仍使用下方入门程序或订单助手模板。

## Tron 能力包菜单编码（强制）

为 Tron 创建、修改、打包或发布带菜单的能力包时，必须先读取[Tron 导航契约](references/tron-navigation.md)，并遵守以下规则；这些是 Tron 宿主约束，不是所有 Framework 宿主的通用限制。

- `tron/navigation.json` 的 `domains[].code`、`menus[].code` 必须原样匹配 `^[A-Z0-9_]{1,64}$`：仅 ASCII 大写字母、数字和下划线，长度 1–64。禁止小写、点、连字符、空白、中文及其他符号；不能依赖导入时自动 trim 或转大写。
- `layouts` 的键必须精确引用本包声明的领域编码，`menuCode` 必须精确引用本包声明的菜单编码；非空 `parentMenuCode` 必须引用同一布局中的 `Group` 菜单。修改编码时必须同步全部引用。
- 领域编码和菜单编码分别在各自全局空间内唯一；不能覆盖内置、用户自建或其他包的同类编码。生成时使用稳定的包专属大写下划线前缀，包内也不得重复。正常升级保持编码稳定，不把版本号加入编码。
- 不混淆包 ID、菜单编码与页面 ID：包 `example.page-demo` 的菜单可为 `EXAMPLE_PAGE_DEMO_OVERVIEW`，`PackPage.pageCode` 则填写本包 `tron/pages.json` 中的原始页面 ID（如 `overview`），不得改成菜单编码、URL 或手工拼上包 ID。
- 打包、上传或声称可安装前，必须核对最终 ZIP 中的编码、引用、资源声明与宿主校验结果。仅 JSON 可解析或 Framework 通用 ZIP 校验通过，不代表 Tron 菜单可安装。失败时先修复，不能通过删除菜单或改包 ID 绕过问题；未执行实际导入/升级时明确说明未验证。

## 1. 五个核心对象

| 对象 | 用途 | 创建时机 |
| --- | --- | --- |
| `Framework` | 持有数据库、模型连接和插件资源 | 应用启动，按隔离范围创建 |
| `Plugin` | 注册服务，管理初始化和关闭 | 加入 `FrameworkBuilder` |
| `CapabilityPack` | 组合插件、声明式实现和 Skills | 加入 `FrameworkBuilder` |
| `Agent` | 选择已安装的模型、工具、上下文 | 创建角色或配置不同权限时 |
| `RunRequest` | 在某个会话下发起一轮执行 | 每次提交输入时 |

```mermaid
flowchart LR
    P[Plugin / CapabilityPack] --> B[FrameworkBuilder]
    B --> F[Framework：共享资源]
    F --> A[AgentSelection：选择能力]
    A --> G[Agent]
    G --> R[RunRequest：session_id + input]
    R --> O[RunResult / 事件流 / 等待交互]
```

安装提供候选，选择决定 Agent 能用什么。默认 Agent 没有工具、技能和额外上下文。`agent.close()` 关闭单个 Agent；应用退出时调用 `framework.close().await` 释放共享资源。

## 2. 第一个能对话的 Agent

准备支持 edition 2024 的 Rust 工具链，以及一个 OpenAI Chat Completions 兼容服务的端点、模型名和凭据。其他协议见[模型接入](references/models.md)。

新项目 `Cargo.toml`：

```toml
[package]
name = "my-agent"
version = "0.1.0"
edition = "2024"

[dependencies]
eliteforge-agent-framework = { git = "https://gitee.com/person-ai/eliteforge-agent-framework.git", tag = "1.0.0", default-features = false }
eliteforge-agent-official-capability-pack = { git = "https://gitee.com/person-ai/eliteforge-agent-framework.git", tag = "1.0.0" }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

`src/main.rs` 完整内容：

```rust
use eliteforge_agent_framework::{AgentSelection, FrameworkBuilder, RunRequest};
use eliteforge_agent_official_capability_pack::{
    EliteForgeOfficialCapabilityPack,
    model::{AdapterKind, AuthData, GenaiPlugin},
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let model = std::env::var("ELITEFORGE_SKILL_AGENT_FRAMEWORK_MODEL")?;
    let endpoint = std::env::var("ELITEFORGE_SKILL_AGENT_FRAMEWORK_ENDPOINT")?;
    std::fs::create_dir_all("agent-data/artifacts")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            "agent-data/artifacts", std::fs::Permissions::from_mode(0o700),
        )?;
    }
    let framework = FrameworkBuilder::new()
        .default_models(false)
        .capability_pack(
            EliteForgeOfficialCapabilityPack::new(
                "agent-data/state.sqlite", "agent-data/artifacts",
            ).model(
                GenaiPlugin::new(model)
                    .adapter(AdapterKind::OpenAI)
                    .endpoint(endpoint)
                    .auth(AuthData::from_env("ELITEFORGE_SKILL_AGENT_FRAMEWORK_API_KEY")),
            ),
        )
        .build().await?;
    let outcome = async {
        let agent = framework.agent(AgentSelection::default()).build().await?;
        agent.run(RunRequest::new("chat-1", "用中文介绍你的用途")).await
    }.await;
    // 创建 Agent 或运行失败时，也要执行关闭路径。
    let shutdown = framework.close().await;
    let result = outcome?;
    if !shutdown.is_success() {
        return Err(format!("关闭失败：{shutdown:?}").into());
    }
    println!("状态：{:?}\n{}", result.status, result.output);
    Ok(())
}
```

通过用户的配置或秘密管理方式设置下方三个环境变量，再执行 `cargo run`。`ENDPOINT` 是服务根地址，例如服务商给出的 `/v1/` 地址，不是完整 `/chat/completions` URL；`MODEL` 填实际型号。程序会请求真实模型、打印结果，在 `agent-data/` 保存状态和工件。

下一次输入沿用 `chat-1` 就能延续会话；换 `session_id` 会使用另一份历史。服务端复用 Framework 和 Agent，不要每个 HTTP 请求都重新安装插件。

## 3. 从对话到业务操作

要让模型查询订单，使用随技能分发的[订单助手模板](assets/order-agent/Cargo.toml)。目录含完整 `Cargo.toml`、`src/main.rs`、`src/orders.rs`，可以复制为新项目，也可以合入现有应用。

模板包含真实模型配置、声明式能力包、查询工具、系统指令、Agent 选择和关闭路径。演示订单为 `A100 → 已发货`；把 `orders.rs` 中的内存查询替换成宿主已授权的订单服务。逐步讲解见[扩展开发](references/extensions.md)。

| 要实现什么 | 教程 | 章内内容 |
| --- | --- | --- |
| 多 Agent、装配和权限选择 | [装配与配置](references/integration.md) | 默认项、选择字段、共享范围、重配置 |
| 模型端点、多连接、Embedding、自定义模型 | [模型接入](references/models.md) | 配置、ModelRef、策略、直接调用 |
| 自定义工具、插件、业务能力包 | [扩展开发](references/extensions.md) | 订单实现、宏、配置、服务与生命周期 |
| 文件/命令、MCP、Skills、Web/Media | [官方工具与技能](references/tools.md) | 启用代码、Provider ID、调用参数 |
| 系统指令、RAG、历史预算、记忆 | [上下文与存储](references/context.md) | Contributor、布局、SQLite、记忆接法 |
| SSE、取消、问答审批、恢复 | [运行与交互](references/runtime.md) | 请求处理、状态流转、恢复与排错 |
| Workflow/DAG、协作、PersonAI、动态包 | [进阶能力](references/capabilities.md) | 接入步骤、API、输入输出、宿主职责 |

按任务读取相关章节即可。章节是技能内的完整教程；代码片段会说明所需依赖和已有变量，标有文件名的完整文件可直接复制。

## 4. 编写应用时的规则

- Framework crate 使用相同 Git URL 和 tag；内部 path 依赖由 Cargo 在同一 checkout 解析。业务 crate 可以使用本项目的 path 依赖。
- 替换官方包中的模型或存储用 `.model(...)` / `.storage(...)`；重复安装同 ID 插件或第二套唯一存储会失败。
- `tool_providers` 填插件 ID，`tools` 填工具内部 ID，`skills` 填技能名，`context_sources` 填来源插件 ID。`call_name` 是模型调用名，不能替代内部 ID。
- 宿主从已认证身份绑定会话、工作目录、数据范围和凭据。模型参数不能授予新权限；文件目录限制和工具审批不是 OS 沙箱。
- `AwaitingInput` 表示等待交互。恢复沿用保存的 run/interaction，不重新执行原始请求；结果不明的写操作先核对外部状态。
- 完成后运行目标项目已有编译检查。真实模型、外部服务调用遵循任务授权；编译通过不能证明供应商运行兼容。是否写测试遵循用户要求。

## Environment Variables

以下变量用于入门程序和订单模板；Tron 骨架生成命令不需要这些变量。合入现有应用时可改用宿主配置对象。

- `ELITEFORGE_SKILL_AGENT_FRAMEWORK_MODEL` [required] 服务实际提供的 Chat 模型 ID。
- `ELITEFORGE_SKILL_AGENT_FRAMEWORK_ENDPOINT` [required] 模型协议基础 URL，包含服务所需的版本前缀。
- `ELITEFORGE_SKILL_AGENT_FRAMEWORK_API_KEY` [required] 模型服务凭据；从秘密管理或环境读取，不写入代码或日志。

## Output Path

依次采用：用户指定的项目与文件 → 当前项目已有 Cargo 清单、配置入口和 Agent 装配模块。需要新项目时才复制 `assets/order-agent/`；目录参数视为项目根，不覆盖已有同名文件。现有项目按其模块布局合入示例，不修改 Cargo 依赖缓存。

新建 Tron 能力包优先采用用户指定的目录，否则在当前已授权工作目录下创建独立子目录；将完整新目录传给生成器的 `--out`。构建归档写入生成项目的 `output/`。不能在技能的 `assets/` 中生成业务项目或构建产物。
