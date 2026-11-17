<!-- Copyright 2026 The oc-build Authors. SPDX-License-Identifier: Apache-2.0 -->

# TUI feature matrix: Grok Build backend vs opencode v2

This document lists the user-facing features of the pager TUI (`crates/codegen/xai-grok-pager`)
and maps each one to the opencode v2 server that `--opencode-url` connects to through the
in-process ACP shim (`src/acp/opencode_shim.rs`).

**How the TUI reaches the backend.** Every feature goes through one of three paths:

- **ACP core methods**: `initialize`, `authenticate`, `session/new`, `session/load`,
  `session/prompt`, `session/cancel`, `session/set_mode`, `session/set_model`, and the
  agent→client request `session/request_permission`. Streaming goes through `session/update`.
  The `SessionUpdate` enum in `xai-acp-lib` is extended with xAI-only kinds such as `Plan`,
  `SubagentSpawned`, `UsageUpdate` and `AutoCompact*`.
- **`x.ai/*` extension methods and notifications**. The pager sends these from
  `src/app/effects/mod.rs`, and the shell serves them in `xai-grok-shell/src/extensions/*.rs`
  and `agent/mvp_agent/acp_agent.rs`.
- **Purely local**: rendering, keybindings, local config in `~/.grok`, the local file walker,
  the clipboard, and the external editor.

**What the shim does today (main @ b21f0c13; draft PRs #3 log/tests and #4 reject + model picker are noted in the table):**

- It implements `initialize`, `authenticate` (a no-op auth method `opencode`), `session/new`,
  `session/prompt`, `session/cancel` and `session/request_permission`.
- `session/new` maps to `POST /api/session`. It sets the model and an `ask` rule for `shell`/`bash`.
- `session/prompt` maps to `POST /api/session/{id}/prompt` with the text only. The shim then
  streams `/api/event` while the turn runs.
- `session/cancel` maps to `POST /api/session/{id}/interrupt`.
- `permission.asked` becomes `session/request_permission`, and the answer goes back through
  `POST …/permission/{requestID}/reply`.
- Translated events: `session.text.delta` becomes `AgentMessageChunk`,
  `session.reasoning.delta` becomes `AgentThoughtChunk`, and `session.tool.*` becomes
  `ToolCall`/`ToolCallUpdate`. Only `shell`/`bash` is typed as `Execute`; every other tool is `Other`.
- Stubs: `session/load` returns OK but replays nothing. `session/set_mode` and
  `session/set_model` return OK and are ignored. (feat/oc-modes-agents: `session/set_mode`
  switches the opencode agent; `session/new`/`session/load` return modes.)
- Every `x.ai/*` method returns `method_not_found`, and every `x.ai/*` notification is dropped.
- (feat/oc-hide-xai-only) `x.ai/plugins/list`, `x.ai/skills/list` and `x.ai/mcp/list` return
  `method_not_found` with the message "not available in opencode mode yet". In opencode mode
  (`opencode_shim::is_active()`) the pager hides the xAI-only commands
  (`opencode_shim::UNAVAILABLE_COMMANDS`) and extensions tabs, and drops the xAI-only effects
  (`opencode_shim::skips_effect`).
- `initialize` sends `_meta.modelState` with exactly one model (`--opencode-model`, default
  `opencode/big-pickle`) and `cancelRewind:false`.
- Prompt content blocks other than `Text` are dropped. A `ResourceLink` is sent as its URI text.
- No `AvailableCommandsUpdate`, no modes (until feat/oc-modes-agents), and no tool list are ever sent. Gated commands
  (`/loop`, `/imagine*`, `/memory`, `/flush`, `/dream`, `/auto`, `/dashboard`, `/recap`, `/voice`)
  therefore stay hidden.
- Headless `-p` (`src/headless.rs`) is not wired to `--opencode-url`. Only the interactive TUI
  path in `src/app/mod.rs` is.

**Status legend**

- `works`: already works through the current shim.
- `local`: no backend involved, so it works unchanged. Counted as `works` in the summary.
- `partial`: some of it works. The gap is in the opencode column.
- `mappable`: opencode has an equivalent. The row names the endpoint or event and the shim work needed.
- `no equivalent`: xAI-only. See [No equivalent](#no-equivalent).

opencode endpoints are under `/api/*` in `/workspace/research/oc2-openapi.json`. Events are on
`GET /api/event`. Tool names come from `packages/core/src/tool/plugin/*.ts`: `shell`, `read`,
`edit`, `write`, `patch`, `glob`, `grep`, `todowrite`, `question`, `subagent`, `skill`,
`webfetch`, `websearch`, `image_generate`, `automation_*`, `artifact_*`.

## Main table

### Core conversation

| Feature | TUI backend call | opencode v2 equivalent | Status |
|---|---|---|---|
| Startup handshake / capabilities | `initialize` (+ `_meta.modelState`, `cancelRewind`) | `GET /api/info`, `GET /api/model/default` | works |
| Auth gate on startup | `authenticate`; `app/mod.rs` skips xAI login when `--opencode-url` is set | server password (basic auth), already handled by `oc-opencode-client` | works |
| New session (startup, `/new`, `Ctrl+N`) | `session/new` (`Effect::CreateSession`) | `POST /api/session` | works |
| Send prompt (Enter) | `session/prompt` (`Effect::SendPrompt`/`SendPromptBlocks`) | `POST /api/session/{id}/prompt` | works |
| Streaming assistant text | `session/update` `AgentMessageChunk` | `session.text.delta` | works |
| Thinking / reasoning blocks (`Ctrl+E` fold) | `AgentThoughtChunk` | `session.reasoning.delta` | works |
| Tool call blocks (execute) | `ToolCall` / `ToolCallUpdate` | `session.tool.input.started`, `.called`, `.success`, `.failed` | works |
| Rich tool rendering (read/edit/write/grep/glob diff and file blocks) | `ToolCall` with `kind` = Read/Edit/Search…, `locations`, `content` Diff, `x.ai/tool` meta | same `session.tool.*` events. The shim must map `read`/`edit`/`write`/`patch`/`glob`/`grep`/`webfetch` to `ToolKind`, set `locations` from the input path, and emit `ToolCallContent::Diff` for edits (from `input`, `session.tool.success`, or `GET /api/session/{id}/diff`) | partial |
| Live tool output streaming | `ToolCallDeltaChunk`, `x.ai/incrementalBashOutput` | `session.tool.progress` (not translated yet) | mappable |
| Cancel turn (`Ctrl+C`) | `session/cancel` (`Effect::CancelTurn`) | `POST /api/session/{id}/interrupt`, then `session.execution.interrupted` | works |
| Permission prompt (1/2/3, allow once/always/reject) | `session/request_permission` | `permission.asked`, then `POST …/permission/{requestID}/reply` | works; reject with/without feedback keeps the turn going — mapped in #4 (shell only; rules ask only for `shell`/`bash`) |
| Permission prompts for other tools (edit/write/webfetch) | `session/request_permission` | same, by adding `ask` rules for `edit`/`write`/… in `POST /api/session` `permissions` or via `PATCH /api/session/{id}` | mappable |
| "Always" scope editing (`←/→`, `e` pattern) | `request_permission` options + `_meta` | `Decision::Always` + `GET/PATCH/DELETE /api/permission/saved` | partial |
| Turn completed / idle state | `PromptResponse` + `TurnCompleted`, `x.ai/session/prompt_complete` | `session.execution.succeeded`/`failed`/`interrupted`, `session.idle` | works |
| Error / retry banner | `RetryState`, prompt error | `session.retry.scheduled`, `session.step.failed`, `session.execution.failed` | partial (failures shown, retries not) |
| Session status line (busy/idle) | `SessionStatus` | `session.status` | mappable |
| Served-model badge | `ServedModel`, `ModelChanged`, `ModelAutoSwitched` | `session.model.selected`, `session.step.started` (carries the model) | mappable |
| Session title (auto) | `SessionSummaryGenerated`, `x.ai/session/info` | `session.renamed` event, `GET /api/session/{id}` `.title` | mappable |
| Follow-up suggestions | `x.ai/follow_ups` notification | none (could use `POST /api/session/{id}/generate`) | no equivalent |
| Prompt ghost suggestion | `x.ai/suggestPrompt` | none built in (`POST /api/session/{id}/generate` possible) | no equivalent; request skipped in opencode mode (feat/oc-hide-xai-only) |
| Shell-command autocomplete | `x.ai/suggest` | none | no equivalent; request skipped in opencode mode (feat/oc-hide-xai-only) |

### Modes, agents, models

| Feature | TUI backend call | opencode v2 equivalent | Status |
|---|---|---|---|
| Plan mode (`/plan`, `Shift+Tab`, plan nudge) | `session/set_mode {"plan"}` (`app/dispatch/modes.rs`), `x.ai/toggle_plan_mode` | `POST /api/session/{id}/agent {"agent":"plan"}`. Back to Normal is `{"agent":"build"}`. Advertise modes in `session/new` `modes` from `GET /api/agent` (primary agents) and emit `CurrentModeUpdate` on `session.agent.selected` | verified live (feat/oc-modes-agents): modes `default`=Build, `plan`=Plan from `GET /api/agent`; `set_mode` → `POST …/agent`, confirmed with `CurrentModeUpdate`; `x.ai/toggle_plan_mode` toggles plan↔build |
| `/plan <description>` (enter plan and prompt) | `Effect::SetModeThenPrompt` = `set_mode` + `prompt` | agent switch + prompt | mapped (feat/oc-modes-agents): same `set_mode` path, then the prompt |
| Plan approval card (exit plan mode) | `x.ai/exit_plan_mode` client request, `PlanExecuting`/`PlanKept`/`PlanCleared` | none. opencode `plan` is an agent without an exit tool. Approximate with "switch to `build` and prompt `execute the plan`" (`Effect::ExecutePlan`) | mapped (feat/oc-modes-agents): a prompt with `_meta.executePlan` switches to `build` and prompts "implement the approved plan" + the plan text. Unreachable today: the approval card only opens on `PlanKept`, which needs the xAI plan-file tools |
| `/view-plan` | `SessionUpdate::Plan` (stored plan) | `todowrite` tool input (a todo list, not a plan document) | partial |
| Always-approve / YOLO (`Ctrl+O`, `/always-approve`) | `session/set_mode` `bypass_permissions` + `x.ai/settings/update`, `x.ai/yolo_mode_changed` | `PATCH /api/session/{id} {"permissions":[{action:"*",resource:"*",effect:"allow"}]}`, or the shim auto-replies `once` to `permission.asked` | verified live (feat/oc-modes-agents): `_meta.yoloMode` on `session/new` and `x.ai/yolo_mode_changed` set a per-session flag; while on, the shim replies `once` itself (the pager also auto-allows). No ruleset change, so turning it off just asks again |
| Auto-review mode (`/auto`, classifier) | `permission_mode = "auto"`, shell-side classifier | none (no classifier). Keep hidden | hidden in opencode mode (feat/oc-modes-agents): `auto_mode_gate` is off, so Auto is not in the Shift+Tab cycle |
| Mode cycle (`Shift+Tab`: Normal → Plan → Auto → Always) | local cycle + `session/set_mode` | `build`/`plan` agents + permission ruleset | verified live (feat/oc-modes-agents): Build → Plan → Always-Approve → Build |
| Agent / persona picker (`/config-agents`, `/personas`) | bundle `x.ai/bundle/status`, roster `x.ai/sessions/list` | `GET /api/agent` (primary and subagent, `Agent.Info`), `POST /api/session/{id}/agent` | mappable |
| Model picker (`Ctrl+M`, `/model`) | list from `initialize._meta.modelState` + `x.ai/models/update`; switch with `session/set_model` | `GET /api/model` (+ `GET /api/provider`) for the list, `POST /api/session/{id}/model {"model":{id,providerID}}` to switch | mapped in #4 (list from `GET /api/model`, switch via `POST …/model`) |
| `/model <name>` set default | `Effect::PersistPreferredModel` (local `~/.grok/config.toml`) + `set_model` | local persist + `POST …/model` | partial |
| `/effort <level>` (reasoning effort) | `session/set_model` with an effort meta (`acp/model_state.rs` effort levels) | `Model.Ref.variant` (`Model.Info.variants`) in `POST …/model` | mappable |
| Fast mode / model variant | model id suffix / variant meta | `Model.Ref.variant` (e.g. a `fast` variant if the provider defines one) | mappable |
| `/context-window <size>` | `session/set_model` with a context-window meta | none (the window is fixed per model) | hidden in opencode mode (feat/oc-hide-xai-only) |
| Model notice banner / auto-switch | `ModelAutoSwitched`, `x.ai/models/update` | `model.updated`, `models-dev.refreshed` events | mappable (low value) |

### Slash commands, skills, workflows

| Feature | TUI backend call | opencode v2 equivalent | Status |
|---|---|---|---|
| Slash dropdown / command palette (`Ctrl+P`, `?`) | local registry `src/slash/commands/mod.rs` + `AvailableCommandsUpdate` | `GET /api/command`, `command.updated` event | partial (builtins only) |
| Server commands as `/name` (opencode `init`, `review`, `goal`) | `AvailableCommandsUpdate`, `x.ai/commands/list` (`Effect::RefreshAvailableCommands`); unknown `/x` is `PassThrough`, i.e. sent as a prompt | `GET /api/command` → `AvailableCommandsUpdate`. The shim routes a prompt starting with `/name` to `POST /api/session/{id}/command {"name","text"}` | mappable |
| Skills as `/skill` commands | `AvailableCommandsUpdate` entries with `_meta` skill scope/path; pager reads SKILL.md (`CommandResult::InjectSkill`) | `GET /api/skill`; activation through prompt `skills:[…]` attachment or `POST /api/experimental/session/{id}/skill` | mappable |
| `/skills` view (list/toggle) | `x.ai/skills/list`, `x.ai/skills/config` toggle, `x.ai/skills/refresh-baseline` | `GET /api/skill`, `skill.updated` (no enable/disable endpoint) | mappable (read-only) |
| `/workflows`, `/workflow`, workflow runs | `x.ai/workflows/list`, `WorkflowUpdated` | none | hidden in opencode mode (feat/oc-hide-xai-only) |
| `/goal`, `/deep-research` (shell builtins) | shell `AvailableCommandsUpdate` | opencode `goal` command via `POST …/command`; no deep-research | partial |
| `/loop [interval] <prompt>` (scheduler) | requires tool `scheduler_create`; `ScheduledTask*` updates, `x.ai/scheduler/delete` | `automation_*` tools; `POST/GET/DELETE /api/experimental/schedule`, `…/pause`, `…/resume` | mappable (experimental API) |
| `/btw` side question | `x.ai/btw` | `POST /api/session/{id}/generate` (generate text from the session context without changing the transcript) | mappable |
| `/recap` | `x.ai/recap`, `SessionRecap` | `POST /api/session/{id}/generate` with a recap prompt | mappable |
| `/imagine`, `/imagine-video` | requires tools `image_gen` / `image_to_video` | `image_generate` tool, `POST /api/experimental/generate/image`; no video | partial (image mappable, video none) |
| `/help` | local | n/a | local |

### Sessions

| Feature | TUI backend call | opencode v2 equivalent | Status |
|---|---|---|---|
| Session picker / `/resume` / `Ctrl+R` | `x.ai/session/list` (`Effect::FetchSessionList`, `src/views/session_picker.rs`) | `GET /api/session?limit&order&search` (filter `parentID` to hide children) | mappable (today it fails with `method_not_found` → error state) |
| Resume a session (load + history replay) | `session/load` replays history as `session/update`; `x.ai/replayed`, `x.ai/session/load_history` | `GET /api/session/{id}/message` → synthesize `UserMessageChunk`/`AgentMessageChunk`/`ToolCall` replay | mappable (today `session/load` returns OK with no replay, so the scrollback is empty) |
| `--continue` / `--resume <id>` CLI | `session/load` | same as above (`GET /api/session` newest) | mappable |
| Session deep search (picker `/`) | `x.ai/session/search` | `GET /api/session?search=` (title search only) | mappable |
| `/fork` | `x.ai/session/fork` (`Effect::ForkSession`) | `POST /api/session/{id}/fork {"before"}`, `session.forked` | mappable |
| `/rewind` / `/undo` / `Esc Esc` picker | `x.ai/rewind/points`, `x.ai/rewind/execute`, `x.ai/restore_code` | `GET …/message` for points; `POST …/revert/stage {"messageID","files"}` → `…/revert/commit` (or `DELETE …/revert`); events `session.revert.*` | mappable |
| Cancel-and-rewind on `Ctrl+C` | `cancelRewind` capability + `rewind_prompt_id` | `revert/stage` + `revert/commit` of the last user message | mappable (disabled via `cancelRewind:false`) |
| `/compact` | `x.ai/compact_conversation` (`Effect::Compact`) | `POST /api/session/{id}/compact`; `session.compaction.started`/`ended`/`failed` | mappable |
| Auto-compact banners | `AutoCompactStarted`/`Completed`/`Failed`/`Cancelled` | `session.compaction.*` (opencode compacts on its own) | mappable |
| `/rename`, auto title reset | `x.ai/session/rename` | `PATCH /api/session/{id} {"title"}`, `session.renamed` | mappable |
| `/delete` | `x.ai/session/delete` | `DELETE /api/session/{id}` | mappable |
| `/session-info` | `x.ai/session/info` | `GET /api/session/{id}` (+ `/api/location`, `/api/vcs`) | mappable |
| `/context` (context usage) | `x.ai/session/info` (`Effect::ShowContextInfo`) | `GET /api/session/{id}/context` | mappable |
| Context bar / token meter | `UsageUpdate`, `x.ai/session/usage` | `session.usage.updated` / `session.usage.recorded`, `GET …/context` | mappable |
| `/export` | local render of scrollback (`Action::ExportConversation`) | (`GET /api/experimental/session/{id}/export` is an alternative) | local |
| `/transcript`, `/copy`, `/find`, `/jump`, `/timeline`, `/expand` | local scrollback | n/a | local |
| `/share` | `x.ai/share_session` (also disabled in code: "Session sharing is temporarily disabled") | none in v2 spec | no equivalent |
| Import foreign sessions (Claude) / `/import-claude` | local scan + `x.ai/session/import` | `POST /api/experimental/session/import` (format differs) | no equivalent (keep local settings import only) |
| Session list sync across clients | `x.ai/sessions/changed`, `x.ai/session/close` | `session.created`/`deleted`/`renamed`/`archived` events | mappable |
| `/home` (exit to welcome) | local | n/a | local |

### Prompt input

| Feature | TUI backend call | opencode v2 equivalent | Status |
|---|---|---|---|
| `@file` mentions + fuzzy file search | local nucleo walker (`src/views/file_search/state.rs`); mention sent as text in the prompt | `PromptInput.FileAttachment {uri, mention}` in `POST …/prompt` `files`; optionally `GET /api/fs/find` for remote servers | partial (search works locally, the server only sees `@path` text) |
| Images: paste / drag-drop / `Alt+V` | `ContentBlock::Image` in `session/prompt`; `ImageCompressed`/`ImageDropped` | `files: [{uri:"data:image/…;base64,…"}]` (FileAttachment) | mappable (today the shim silently drops non-text blocks) |
| `@agent` mention | n/a | `Prompt.AgentAttachment` in prompt `agents` | mappable (new) |
| Bash mode (`!cmd`) | `session/prompt` with block meta `kind:"bash"` (`Effect::SendBashCommand`) | `POST /api/session/{id}/shell {"command"}`; `session.shell.started`/`ended` | mappable (today it goes to the model as a plain prompt) |
| Prompt history (`↑`, `/history`, `Ctrl+R` search in prompt) | local history + `x.ai/prompt_history` (`Effect::FetchPromptHistory`) | `GET /api/session/{id}/message` (user messages) | partial (local history works) |
| Prompt queue while running (`Ctrl+;`, queue pane) | `x.ai/queue/{remove,reorder,clear,edit,hold_edit,release_edit,interject}`, `x.ai/queue/changed` | `POST …/prompt {"delivery":"queue"}`; `GET /api/session/{id}/inbox`, `PATCH/DELETE …/inbox/{inboxID}`; `session.inbox.*` events | mappable |
| Send now / interject mid-turn (`Ctrl+Enter`) | `x.ai/interject`, `x.ai/queue/interject` | `POST …/prompt {"delivery":"steer"}` | mappable |
| Draft stash (`Ctrl+S`), external editor (`Ctrl+G`, `/edit-prompt`), multiline (`/multiline`) | local | n/a | local |
| `/remember`, `#` note mode | prompt / memory note (`Effect::SaveMemoryNote`) | `PUT /api/experimental/session/{id}/instructions/entries/{key}` (closest) | no equivalent; `/remember` hidden in opencode mode (feat/oc-hide-xai-only) |

### Questions, forms, elicitation

| Feature | TUI backend call | opencode v2 equivalent | Status |
|---|---|---|---|
| Question card (`ask_user_question`) | client request `x.ai/ask_user_question` (`src/views/question_view.rs`) | `question` tool → `form.created` event; `GET /api/session/{id}/form/{formID}`; answer with `POST …/form/{formID}/reply {"answer"}`; dismiss with `DELETE …/form/{formID}` | mappable (today the turn blocks with no card: the `form.created` event is ignored) |
| MCP elicitation card | `x.ai/mcp/elicit`, `x.ai/mcp/elicit_complete` | forms (MCP elicitation surfaces as a form) | mappable |

### Subagents, tasks, todos, dashboard

| Feature | TUI backend call | opencode v2 equivalent | Status |
|---|---|---|---|
| Subagent blocks (spawn/progress/finish) | `SubagentSpawned`/`SubagentProgress`/`SubagentFinished` | `subagent` tool call (`metadata.sessionID`) + child-session events (`session.created` with `parentID`, then the child's `session.*` stream); `GET /api/experimental/job` (`Job.Kind` subagent) | mappable |
| Subagent drill-in view | `subagent_views`, child session updates | stream events filtered by the child `sessionID`; history via `GET /api/session/{child}/message` | mappable |
| Kill subagent | `x.ai/subagent/cancel` (`Effect::KillSubagent`) | `POST /api/session/{child}/interrupt` | mappable |
| Cancel-turn panel ("cancel subagents too") | `session/cancel` `_meta.cancelSubagents` | interrupt the parent plus each running child | mappable |
| Todos pane (`Ctrl+T`) | `SessionUpdate::Plan` entries | `todowrite` tool input `{todos:[{content,status}]}` → `SessionUpdate::Plan` | mappable |
| Background tasks pane (`Ctrl+G`, `/tasks`) | `TaskBackgrounded`/`TaskCompleted`, `x.ai/task_backgrounded`, `x.ai/task/kill` | `GET /api/shell`, `GET /api/shell/{id}/output`, `DELETE /api/shell/{id}`; `shell.created`/`exited` events; `GET /api/experimental/job` | mappable |
| Send command to background (`Ctrl+B`) | `x.ai/terminal/background` | `POST /api/session/{id}/background` | mappable |
| Monitor events | `MonitorEvent`, `x.ai/monitor_event` | `watch_mcp_event` tool / `experimental.mcp.event` | no equivalent (niche) |
| Agent dashboard (`Ctrl+\`, `/dashboard`) | `x.ai/sessions/list` roster, `x.ai/session/list`, leader multi-session (`src/views/dashboard`); disabled in opencode mode (`use_leader && opencode.is_none()`) | `GET /api/session/active`, `GET /api/session`, `session.status` events, multi-session through one `/api/event` stream | mappable (L) |
| Dashboard actions (rename, pin, stop, always-approve per row, location picker `/cd`) | `x.ai/session/rename`, `session/cancel`, settings, `x.ai/local_workspace` | `PATCH /api/session/{id}`, `…/interrupt`, `POST /api/session/{id}/move` | mappable (after dashboard) |
| `/btw` overlay | `x.ai/btw` | see Slash commands | mappable |

### Files, git, worktrees, review

| Feature | TUI backend call | opencode v2 equivalent | Status |
|---|---|---|---|
| Diff / review of session edits (hunk tracker) | `x.ai/hunk-tracker/*`, `x.ai/hunkTracker` meta | `GET /api/session/{id}/diff`; `GET /api/vcs/diff`, `/api/vcs/status` | mappable |
| Review comments | `x.ai/review/comment` | none (the opencode `review` command runs a review prompt) | no equivalent |
| Git branch / HEAD in status line | local `git_info.rs`, `x.ai/git_head_changed` | `GET /api/vcs`, `vcs.branch.updated` | local |
| Worktrees (`Ctrl+W` dialog, `Ctrl+N` in worktree, `grok worktree …`) | `x.ai/git/worktree/{create,list,show,remove,gc,detach,salvage,resume_session,…}` | `GET/POST/DELETE /api/worktree`, `POST /api/worktree/refresh`; `worktree.updated`/`resolved`; `POST /api/session/{id}/move` | partial (list/create/remove map; salvage/gc/db have no equivalent) |
| Working directory / `/cd` | `session/new` `cwd`, `x.ai/session/add_local_workspace` | `Location.directory` on create; `POST /api/session/{id}/move` | partial (cwd at create works) |
| Code navigation (goto definition) | `x.ai/code/*` | none in the API (LSP is internal) | no equivalent |
| PR status | `x.ai/pr/status` | none | no equivalent |
| File change notifications | `x.ai/fs_notify` | `filesystem.changed`, `POST /api/experimental/fs/watch` | mappable (low value) |

### Extensions: MCP, plugins, hooks, marketplace

| Feature | TUI backend call | opencode v2 equivalent | Status |
|---|---|---|---|
| Extensions modal (`Ctrl+L`, `/plugins`, `/hooks`, `/mcps`, `/skills`) | `src/views/extensions_modal*` fed by the list calls below | per-tab mapping below | partial; Hooks/Marketplace/Workflows tabs hidden in opencode mode (feat/oc-hide-xai-only); Plugins/Skills/MCP tabs say "not available in opencode mode yet" |
| `/mcps` list + status | `x.ai/mcp/list`, `x.ai/mcp/server_status`, `x.ai/mcp/init_progress`, `x.ai/mcp_initialized` | `GET /api/mcp`, `mcp.status.changed` | mappable |
| MCP add/remove | `x.ai/mcp/upsert`, `x.ai/mcp/delete` | `PUT/DELETE /api/experimental/mcp/{server}` | mappable |
| MCP enable/disable | `x.ai/mcp/toggle` | `POST /api/experimental/mcp/{server}/connect`/`disconnect` | mappable |
| MCP per-tool toggle | `x.ai/mcp/toggle_tool` | none (permission rules can deny a tool) | no equivalent |
| MCP OAuth login | `x.ai/mcp/auth_trigger`, `x.ai/mcp/setup` | integrations: `POST /api/integration/{id}/connect/oauth` + attempt polling | mappable (M) |
| MCP resources | n/a | `GET /api/mcp/resource` | n/a |
| Plugins list | `x.ai/plugins/list`, `PluginsChanged` | `GET /api/plugin`, `plugin.updated` | mappable |
| Plugin actions (enable/disable/reload/update) | `x.ai/plugins/action`, `x.ai/plugins/reload`, `x.ai/plugins/notify-updates` | `POST /api/plugin/check`, `POST /api/plugin/update`, `POST /api/location/reload`; no enable/disable | partial |
| Marketplace (`/marketplace`, plugin CTA) | `x.ai/marketplace/list`, `x.ai/marketplace/action` | none | hidden in opencode mode (feat/oc-hide-xai-only) |
| Hooks (`/hooks`, hook annotations) | `x.ai/hooks/list`, `x.ai/hooks/action`, `HookExecution`/`HookAnnotation`/`HooksChanged` | none (opencode extends behaviour through plugins) | hidden in opencode mode (feat/oc-hide-xai-only) |
| Project rules / AGENTS.md | shell-side discovery | opencode reads instructions itself; `GET /api/experimental/session/{id}/instructions/entries` | works (server-side) |

### Memory

| Feature | TUI backend call | opencode v2 equivalent | Status |
|---|---|---|---|
| `/memory` modal | `x.ai/memory/list` (`MEMORY_LIST_METHOD`), toggle/forget/rewrite | closest: session instruction entries (`…/instructions/entries`) | no equivalent |
| `/flush`, `/dream` | `x.ai/memory/flush`, `x.ai/memory/dream`, `MemoryFlush*`, `MemoryDreamCompleted` | none | no equivalent |
| Memory capture activity | `MemoryCaptureActivity`, `MemorySessionSaved` | none | no equivalent |

### Account, billing, telemetry

| Feature | TUI backend call | opencode v2 equivalent | Status |
|---|---|---|---|
| `/login`, `/logout`, account switch | `x.ai/auth/{get_url,submit_code,cancel,logout,info}`, `x.ai/getApiKey`/`setApiKey` | provider credentials: `GET/POST/PATCH/DELETE /api/credential`, `…/activate`; `POST /api/pair` | hidden in opencode mode (feat/oc-hide-xai-only) |
| Subscription / paywall / team capability | `x.ai/auth/check_subscription`, `x.ai/auth/hydrate_team_capability` | none | no equivalent; checks skipped in opencode mode (feat/oc-hide-xai-only) |
| `/usage` billing, credit bar, auto top-up | `x.ai/billing`, `x.ai/auto-topup-rule`, `x.ai/limit` | `GET /api/experimental/usage/limits` (provider limits, not billing) | hidden in opencode mode (feat/oc-hide-xai-only); background billing fetches skipped |
| Token / cost usage of this session | `x.ai/session/usage`, `UsageUpdate` | `session.usage.updated` | mappable (see context bar) |
| `/privacy` data retention | `x.ai/privacy/setCodingDataRetention` | none | hidden in opencode mode (feat/oc-hide-xai-only) |
| Consent / folder trust | `x.ai/consent/record`, `x.ai/folderTrust` | none | no equivalent; consent upload skipped in opencode mode (feat/oc-hide-xai-only) |
| `/feedback` + drafts + trace upload | `x.ai/feedback`, `x.ai/feedback/drafts/*`, `x.ai/feedback/upload-trace`, `x.ai/feedback/dismiss` | none | hidden in opencode mode (feat/oc-hide-xai-only) |
| Rollout survey | `x.ai/rollout/survey` | none | no equivalent |
| Announcements, release notes | `x.ai/announcements/update`; release notes are local/bundled | none | no equivalent / local |
| Remote settings / feature gates | `x.ai/settings/update`, remote config fetch (skipped in opencode mode, `app/mod.rs:715`) | `GET /api/config`, `PATCH /api/experimental/config`, `config.updated` | no equivalent (gates use local defaults) |
| Bundle sync (personas, roles) | `x.ai/bundle/status`, `x.ai/bundle/sync` | `GET /api/agent` covers agents | no equivalent; `x.ai/bundle/status` skipped in opencode mode (feat/oc-hide-xai-only) |
| Cloud environments / remote workspaces | `x.ai/cloud/env/*`, `x.ai/cloud_server_id`, `x.ai/cloud_existing_workspace` | none | no equivalent |
| Leader mode (multi-client daemon) | `x.ai/leader*` | the opencode server already supports multiple clients | no equivalent (disabled in opencode mode) |

### Local UI (no backend)

| Feature | TUI backend call | opencode v2 equivalent | Status |
|---|---|---|---|
| Scrollback navigation, folding, raw toggle, block viewer, copy (`j/k`, `h/l`, `e`, `r`, `y`, `Enter`) | local (`src/actions/defaults.rs`) | n/a | local |
| Links (`o`/`O`), mouse capture toggle, drag-select | local | n/a | local |
| `/theme`, `/vim-mode`, `/compact-mode`, `/timestamps`, `/toggle-mouse-reporting` | local config | n/a | local |
| `/minimal` / `/fullscreen` screen mode | local relaunch (`app/mode_switch.rs`) | n/a | local |
| `/settings` (`F2`) | local `~/.grok` config; some rows write `x.ai/settings/update` | n/a | local (shell-backed rows are no-ops) |
| Shortcuts help (`Ctrl+.`), `/tutorial`, `/docs` | local | n/a | local |
| Status line (custom command) | local `RunStatusLineCommand` | n/a | local |
| `/doctor` | local diagnostics (`src/doctor_cmd`) | n/a | local (xAI checks irrelevant) |
| `/quit`, `Ctrl+Q` | local | n/a | local |
| Voice input (`/voice`, voice key) | xAI voice service (`src/voice`), gated, hidden | `GET /api/experimental/voice/models`, `POST /api/experimental/voice/session` (webrtc) | hidden in opencode mode (feat/oc-hide-xai-only) (needs a new client) |
| Terminal notifications / bell | local | n/a | local |

## No equivalent

These are xAI-only. Unless noted, they stay unreachable in opencode mode because their gate is
fail-closed (hidden until the shell advertises them or a remote gate enables them). The rest
call an `x.ai/*` method and get `method_not_found`.

| Feature | Current behaviour in opencode mode (from code) | Recommendation |
|---|---|---|
| `/login`, `/logout` | Hidden (feat/oc-hide-xai-only): not in the menu/palette; typed → toast "/login is not available in opencode mode". Eager auth succeeds through the fake `opencode` auth method | Hide both commands in opencode mode. Provider keys live in opencode (`/api/credential`) |
| `/usage` billing, credit bar, paywall, auto top-up | `/usage` (`/cost`) hidden (feat/oc-hide-xai-only); background `x.ai/billing` refreshes (`FetchBilling` silent, `FetchAppBilling` nonce 0) skipped | Hide `/usage`. Later, show token/cost from `session.usage.updated` in the context bar instead |
| Subscription / team capability checks | Skipped (feat/oc-hide-xai-only): `CheckSubscription`, `HydrateTeamCapability`, `CreditLimitRecheck` effects dropped | Skip the effect when the backend is opencode |
| `/privacy`, consent, folder trust | `/privacy` hidden, `RecordConsentUpstream` skipped (feat/oc-hide-xai-only); folder trust untouched | Hide `/privacy`; skip consent effects |
| `/feedback` (+ drafts, trace upload) | Hidden (feat/oc-hide-xai-only) | Hide, or show "not available in opencode mode" |
| `/share` | Already returns "Session sharing is temporarily disabled" | Leave as is |
| `/marketplace`, plugin install CTA | Hidden (feat/oc-hide-xai-only): command, Marketplace tab and palette entry removed; marketplace list / CTA catalog / update-check effects skipped | Hide the Marketplace tab and the CTA |
| `/hooks` | Hidden (feat/oc-hide-xai-only): command, Hooks tab and palette entry removed; `FetchHooksList` skipped | Hide the tab ("opencode uses plugins") |
| `/workflows`, `/workflow` | Hidden (feat/oc-hide-xai-only): commands, Workflows tab and palette entry removed; `FetchWorkflowsList` skipped | Hide |
| `/memory`, `/flush`, `/dream`, `/remember` | `/memory`/`/flush`/`/dream` hidden (`SHELL_GATED_COMMANDS`, no catalog). `/remember` hidden (feat/oc-hide-xai-only) | Hide `/remember` too |
| `/auto` (classifier auto-review) | Hidden (`set_auto_mode_available` never true); Auto also left out of the Shift+Tab cycle (feat/oc-modes-agents) | Keep hidden |
| `/context-window` | Hidden (feat/oc-hide-xai-only) | Hide, or message "fixed per model in opencode" |
| `/imagine-video` | Hidden (tool `image_to_video` not advertised) | Keep hidden |
| `/voice` | Visible once the voice gate resolves (seen live); now hidden (feat/oc-hide-xai-only) | Keep hidden. Optional later work on `/api/experimental/voice/*` |
| Prompt suggestions, `/btw`-free follow-ups, shell autocomplete (`x.ai/suggest*`, `x.ai/follow_ups`) | `FetchPromptSuggestion` / `FetchShellSuggestions` skipped (feat/oc-hide-xai-only); follow-ups untouched | Skip the effects in opencode mode (saves round-trips). Optional later work on `POST …/generate` |
| Code navigation (`x.ai/code/*`), PR status, review comments | Fail silently | Hide the affordances |
| Worktree salvage / gc / db / clean-artifacts (`grok worktree …`) | CLI subcommands talk to the local shell, not opencode | Leave the CLI as is; don't expose these in opencode mode |
| MCP per-tool toggle | `x.ai/mcp/toggle_tool` fails | Hide the toggle in the MCP tab |
| Announcements, rollout survey, remote config, bundle sync, telemetry upload | Skipped or failing quietly | Skip the effects |
| Cloud environments, leader / multi-client daemon | Leader disabled (`app/mod.rs:823`); cloud never reached | Nothing to do |
| Monitor events | Not produced | Nothing to do |
| Headless `-p` | Ignores `--opencode-url` and uses the built-in agent | Out of scope here. If wanted, reuse the shim in `headless.rs` (`--opencode-url` → `opencode_shim::connect`) |

Suggested mechanism: add a single `BackendKind::Opencode` flag (or reuse `args.opencode_config()`)
to the `CommandRegistry` deny list (`set_restricted_commands`) and to the extensions-modal tab
set. That hides everything in the left column at once, so you don't need per-command
"not available" messages. Keep a short toast, `"/x is not available with --opencode-url"`, for
commands typed in full (`get_for_dispatch` path).

## Proposed PR plan

Each PR stays inside `opencode_shim.rs` and `oc-opencode-client` (new endpoints, events and
types) where possible. Pager changes are limited to small `// oc-build:` gates. Sizes: S < 200
LOC, M 200–600, L > 600.

1. **Hide xAI-only features (S).** A pager gate keyed on opencode mode:
   - Deny list in `CommandRegistry` (`/login`, `/logout`, `/usage`, `/privacy`, `/feedback`,
     `/marketplace`, `/hooks`, `/workflows`, `/workflow`, `/remember`, `/context-window`).
   - Hide the Marketplace and Hooks tabs in `extensions_modal`.
   - Skip the `CheckSubscription`, `FetchBilling`, `RecordConsentUpstream`, `FetchShellSuggestions`
     and `FetchPromptSuggestion` effects.
   - No opencode calls.
2. **Modes & agents (M).**
   - Client: `list_agents` (`GET /api/agent`), `switch_agent` (`POST /api/session/{id}/agent`).
   - Shim: return `SessionModeState` in `session/new`/`session/load`, built from the primary agents.
     Map `plan` ↔ the pager's `plan` id and `build` ↔ `default`. Implement `set_session_mode`,
     and emit `CurrentModeUpdate` on `session.agent.selected`.
   - YOLO: on `bypass_permissions`, `PATCH /api/session/{id}` with an allow-all ruleset (or
     auto-reply `once` in `ask_permission`), and restore the ask rules on exit.
   - Plan exit: map `Effect::ExecutePlan` to switch to `build` + prompt.
3. **Models & variants (S–M).**
   - Client: `list_models` (`GET /api/model`), `default_model`, `switch_model` (`POST …/model`
     with `variant`).
   - Shim: fill `modelState` from the real list (`provider/id` ids). Implement
     `set_session_model`, map `/effort` and fast to `Model.Ref.variant`, and emit `ModelChanged`
     on `session.model.selected`.
4. **Slash commands & skills (M).**
   - Client: `list_commands`, `run_command`, `list_skills`.
   - Shim: send `AvailableCommandsUpdate` after `session/new`/`load` and on
     `command.updated`/`skill.updated`, with skills carrying the meta the registry expects.
     Route a prompt `/name args` to `POST …/command`.
   - Answer `x.ai/commands/list` and `x.ai/skills/list` (read-only).
5. **Rich tool rendering + todos (M).**
   - Shim: `tool_kind` for `read`/`edit`/`write`/`patch`/`glob`/`grep`/`webfetch`/`websearch`,
     `locations` from the inputs, and `ToolCallContent::Diff` for edit/write/patch.
   - `session.tool.progress` becomes `ToolCallDeltaChunk`.
   - `todowrite` input becomes `SessionUpdate::Plan` (todos pane).
   - Add `ask` permission rules for edit/write.
6. **Prompt attachments: @files, images, bash mode (S–M).**
   - Client: a prompt body with `files[]`.
   - Shim: map `ContentBlock::Image` and `Resource`/`ResourceLink` to `FileAttachment`
     (`data:` URIs for images, `file://` for mentions), and parse `@path` tokens into `mention`
     attachments.
   - A `kind:"bash"` block meta goes to `POST …/shell` and is streamed through
     `session.shell.*`.
7. **Sessions: list, resume with replay, fork, rename, delete, info (L).**
   - Client: `list_sessions`, `get_session`, `messages`, `fork`, `patch_session`,
     `delete_session`.
   - Shim: `x.ai/session/list` (+ `search`) from `GET /api/session` in the picker's payload
     shape (`app/effects/session_list.rs`).
   - `load_session` replays `GET …/message` as `session/update` and ends with `x.ai/replayed`.
   - `x.ai/session/fork`, `rename`, `delete`, `info`.
8. **Compaction, context & usage (S).**
   - Client: `compact`, `context`.
   - Shim: `x.ai/compact_conversation` → `POST …/compact`.
   - `session.compaction.*` → `AutoCompact*`.
   - `x.ai/session/usage` and `UsageUpdate` from `session.usage.updated` and `GET …/context`
     (context bar, `/context`).
   - Needs a session-scoped event listener that runs outside of turns. Today `run_turn` only
     reads events while a prompt is active. This is a shared prerequisite for PRs 7–10, so it
     could land here first.
9. **Rewind / revert (M).** `x.ai/rewind/points` from user messages, and
   `x.ai/rewind/execute` → `revert/stage` + `revert/commit` (with `files` for code restore).
   Turn `cancelRewind` on once it works.
10. **Questions & forms (M).**
    - Client: `get_form`, `reply_form`, `cancel_form`.
    - Shim: `form.created` → client request `x.ai/ask_user_question` (the shape
      `question_view.rs` expects). The answer goes to `POST …/form/{id}/reply`, and a dismissal
      to `DELETE`. MCP elicitation forms use the same path to `x.ai/mcp/elicit`.
11. **Prompt queue & steer (M).** Turns queued while a turn runs go out as `POST …/prompt
    {"delivery":"queue"}`. Interject becomes `delivery:"steer"`. The `x.ai/queue/*`
    notifications map to `PATCH`/`DELETE …/inbox/{id}`, and `x.ai/queue/changed` is built from
    `session.inbox.*`.
12. **Subagents & background tasks (L).**
    - Track child sessions (`session.created.parentID` / the `subagent` tool `metadata.sessionID`)
      and emit `SubagentSpawned`/`Progress`/`Finished`, plus nested updates for the drill-in view.
    - `x.ai/subagent/cancel` → child interrupt.
    - The background tasks pane uses `/api/shell` and `shell.*`. `x.ai/terminal/background`
      → `POST …/background`, and `x.ai/task/kill` → `DELETE /api/shell/{id}`.
13. **MCP & plugins views (M).** `x.ai/mcp/list` from `GET /api/mcp` + `mcp.status.changed`.
    `upsert`/`delete`/`toggle` go to `/api/experimental/mcp/{server}` (PUT/DELETE,
    connect/disconnect). `x.ai/plugins/list` comes from `GET /api/plugin`, and update/check maps
    to `/api/plugin/{check,update}`. OAuth through `/api/integration/*` can come later.
14. **Dashboard / multi-session (L, last).** Enable `/dashboard` in opencode mode and feed the
    roster from `GET /api/session/active` + `session.status`, with one shared `/api/event`
    subscription demultiplexed per session. Depends on PRs 7, 8 and 12.
15. **Optional extras (S each).**
    - `/btw` and `/recap` via `POST …/generate`.
    - `/loop` via `/api/experimental/schedule` (advertise a `scheduler_create` tool).
    - `/imagine` via `image_generate`.
    - Worktree list/create/remove via `/api/worktree`.
    - Session diff review via `GET …/diff`.

Suggested order: 1 → 2 → 3 → 5 → 6 → 4 → 8 → 7 → 9 → 10 → 11 → 12 → 13 → 14.
