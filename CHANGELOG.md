# 更新日志

## [1.0.0] - 2026-11-23

- 明确批量文本替换的匹配数量约束和文件移动语法，并改进未执行时的冲突与授权提示。
- 文件准备阶段区分调用身份、授权及路径变化，并在确认尚未写入时给出恢复建议。

本次按 `1.0.0` 同名 tag 重建发布，内容包含工具发现入口拆分与原生工具暴露。

### 工具契约

- `tool_search` 只返回已授权工具的 `call_name` 和 `description` 摘要，不再返回 `input_schema`，并删除 `tool` 查询形式；旧调用返回指向 `tool_schema` 的明确错误。
- 新增 `tool_schema({items:[{tool:"<call_name>"}]})`，按精确调用名批量返回执行时实际使用的完整 `input_schema`：批量工具带 `items` 包装，单项工具保持声明结构，局部引用与约束保持不变。
- 搜索、schema 查询和网关目录共用当前已授权、已激活的工具目录；隐藏、禁用、撤销或不存在的工具不泄露定义。
- 网关模式模型工具列表改为三个基础入口（`tool_search`、`tool_schema`、`tool_execute`）加运行级白名单中的原生工具，保持确定性排序。

### 原生工具暴露

### 原生命令接口

- `command_exec` 的模型 schema 只保留完整 `command` 字符串，由宿主命令解释器处理命令串联、管道、重定向和含空格路径；旧调用携带非空 `args` 会明确提示参数不兼容。
- 底层 `CommandRequest`／`invocation(command, Some(args))` 的直接程序加独立参数接口保留，供能力包、插件和内部调用使用。
- `command_exec`、`command_exec_read`、`command_exec_stop` 的工具调用状态与命令结果分离：非零退出、超时或取消状态放在结果内，不再单独触发工具失败；任务无法启动、查询失败或参数无效仍返回工具失败。
- 新增 `ToolDescriptor::with_model_input_schema`，允许宿主隐藏仅用于兼容校验的旧字段，而不改变执行器使用的完整 schema。
- 移除旧的命令名白名单配置；命令权限统一由宿主工具策略和审批链路控制。

- 新增 `NativeToolWhitelist` 运行级扩展，按稳定工具 ID 选择已准入工具的暴露方式；默认空，不注册执行器、不授予权限。
- 白名单中的工具仍使用原 binding、执行器和工具 ID，直接进入相同的参数校验、`ToolPolicy`、审批、批量拆解、结果聚合、恢复和审计链路。
- 原生命令批量调用沿用 `{"items":[...]}` 契约；单项工具不接受批量包装。通过 `tool_execute` 调用同一工具仍保持兼容。
- 批量部分失败、审批恢复和 `SetExposedTools`/撤销后的目录可见性沿用既有语义；原生工具在暴露变化中保持运行级暴露，撤销仍会从目录移除并拒绝执行。

### 集成与文档

- 更新中英文网关提示词、目录变化通知、工具展示资源和 Framework 对接技能（`metadata.version` 1.1.0）。
- 更新受影响的调用样例、断言和 AG-UI 契约样例。

### 同名 tag 重建说明

- 本次保留版本号 `1.0.0` 并重建同名附注 tag。Git 不会自动移动其他消费者已生成的 `Cargo.lock`；已有锁文件仍固定旧提交，`cargo build` 不会自动切换。
- 先读取远端新提交：

  ```sh
  git ls-remote https://gitee.com/person-ai/eliteforge-agent-framework.git 'refs/tags/1.0.0^{}'
  ```

- 再在该项目锁文件所在的 Cargo workspace 中按提交重新解析（Cargo 需要先取得更新后的 tag）：

  ```sh
  cargo update -p eliteforge-agent-core --precise <上一步输出的提交>
  cargo fetch --locked
  ```

  也可以删除 `Cargo.lock` 中所有 `source = "git+https://gitee.com/person-ai/eliteforge-agent-framework.git?tag=1.0.0#<旧提交>"` 条目后运行 `cargo update`，或重新生成锁文件。
- 重新解析后核对锁文件中的来源提交与两个远端 `1.0.0` tag 指向的提交一致，且没有指向本仓库的本地 `path` 覆盖。
