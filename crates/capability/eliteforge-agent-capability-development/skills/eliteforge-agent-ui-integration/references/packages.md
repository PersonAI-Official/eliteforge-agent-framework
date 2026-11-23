# 包选择、安装与样式

## 包选择

表中包名统一以 `@personai/agent-ui-` 为前缀。

| 用途 | 包后缀 | 对接入口 |
| --- | --- | --- |
| React / Vue 聊天 | `chat-react` / `chat-vue`、`chat-core` | `ChatRuntimeWindow`、`ChatPanel`、`ChatWindow`、`ChatAdapter` |
| 可选全局主题 | `chat-theme` | 根入口、`styles.css` 或 `theme.scss` |
| 独立输入区 | `composer-react` / `composer-vue` | `Composer`；core 类型由渲染包重导出 |
| 输入区共享类型和工具 | `composer-core` | 按需直接使用；渲染包依赖它 |
| 独立 A2UI | `a2ui-core` + `a2ui-react` / `a2ui-vue` | `A2UIClient`、`A2UISurface` |
| 自定义动效 | `motion-react` / `motion-vue` | `MotionProvider`、`AgentMotion`、`useMotion` |
| 动效公共参数与策略样式 | `motion-core` | 框架动效包依赖它 |
| 浏览器终端 | `terminal` | `initTerminal`、`TerminalTransport`、`AITransport` |
| React Native 聊天 | `chat-react-native` | `ChatPanel`、`MessageBubble`、`ToolCard`、`ApprovalCard` |
| React Native A2UI | `a2ui-react-native` | `A2UISurface`、`A2UIRenderer` |

Chat 与 A2UI 的框架包也重导出 MotionProvider 等动效 API，无需仅为 Provider 重复安装框架 motion 包。直接从某个包导入时，应在宿主清单中声明该包，不依赖偶然的依赖提升。

## 安装同一版本

在宿主项目配置作用域 registry，不替换其他作用域的源：

```ini
@personai:registry=https://registry.npmjs.org/
```

当前基线 React 聊天示例：

```sh
npm install --save-exact @personai/agent-ui-chat-react@1.0.0-preview.31 @personai/agent-ui-chat-core@1.0.0-preview.31
```

Vue 将 `chat-react` 换成 `chat-vue`；React Native 使用 `chat-react-native`（peer 为 `react`/`react-native`，无 CSS 入口）。需要完整全局主题时另装同版本 `chat-theme`；其他包按表选择。所有包都是 ESM，构建工具需要处理对应 CSS。

当前 React 包要求 React `^19.2.5`（Chat 还要求同范围 react-dom）；Vue Chat/Composer/Motion 要求 Vue `^3.5.18`，A2UI Vue 要求 `^3.5.0`。图标、Markdown 与终端 xterm peers 以目标包元数据为准；不要用强制忽略 peer 冲突作为默认方案。

终端当前依赖 xterm `5.5.0`，安装脚本 `scripts/patch-xterm-ime.mjs` 会修补所解析的 xterm 文件以支持输入法。若宿主包管理器限制安装脚本，核查该包并按宿主策略允许其脚本、重新构建该依赖；不能静默忽略后宣称终端输入法已验证。

核查版本、依赖与安装来源：

```sh
npm view @personai/agent-ui-chat-react@1.0.0-preview.31 version peerDependencies exports --json --registry=https://registry.npmjs.org/
npm ls --all
```

`latest` 默认指向最近一次正式或预发布版本，`next` 等附加标签也可能存在；它们都是可移动标签，不是版本一致性的证明。检查锁文件中的所有 Agent UI 项，包括间接依赖，确认版本一致、没有 `link: true` 或 `file:`，且 resolved 指向公共 npm tarball；再检查实际安装包的 realpath。

从本地依赖迁移时，清理 manifest/lockfile、bundler alias、TS paths、prepare/build 脚本及 `.agent-deps` 遗留软链接。删除软链接本身，不删除它指向的源码仓库。Agent UI 自身 monorepo 的工作区链接不属于宿主迁移范围。

## 样式入口

```ts
// 按实际使用模块选择，框架不要混装。
import '@personai/agent-ui-chat-react/styles.css';
// 独立输入区：@personai/agent-ui-composer-react/styles.css
// 独立 A2UI：@personai/agent-ui-a2ui-react/styles.css
// 终端：@personai/agent-ui-terminal/styles.css
```

Vue 使用对应 `-vue/styles.css`。Chat 已合入其使用的 Composer/A2UI 等样式，无需按仓库相对路径手工串联。

需要完整全局主题时，先导入 `@personai/agent-ui-chat-theme`，再导入组件 CSS，最后加载宿主覆盖。它含根色板和 html/body 等全局规则；已有设计系统可自行提供变量而不加载全局主题。独立 Composer/A2UI 有局部默认回退，不要求 Tailwind。

`motion-react` / `motion-vue` 入口加载 `motion-core/styles.css`，框架 motion 包没有公开 `styles.css` 子路径。不要导入未列在 exports 中的 `src/`、`dist/` 或 core 内部 CSS 文件。

## 最小 React 验证

```tsx
import { ChatRuntimeWindow } from '@personai/agent-ui-chat-react';
import { createMockChatAdapter } from '@personai/agent-ui-chat-core';
import '@personai/agent-ui-chat-react/styles.css';

const adapter = createMockChatAdapter();
export function ChatPage() {
  return <ChatRuntimeWindow adapter={adapter} config={{ floating: false }} />;
}
```

这是内存 mock，不持久化业务数据，也不证明后端接通。真实接入替换 adapter，并保持其实例稳定。

源码导航：`packages/*/package.json`、各包 `src/index.ts`、各包 README、`docs/integration-guide.md`。从 npm 接入时先读取已安装版本的声明；不要要求宿主存在上述源码目录。
