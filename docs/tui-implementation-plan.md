# Koda Agent TUI Implementation Plan

Baseline upstream: `/tmp/genericagent-inspect/frontends/tuiapp.py` from `lsdefine/GenericAgent` target `9024af7`.
Rust workspace: `/Users/vanzheng/projects/rust/koda-agent`.

## 1. Goal

Build a full-screen, good-looking, reliable, and powerful terminal UI for the Rust GenericAgent recreation without weakening the already validated CLI/runtime/tool/memory/browser behavior.

The TUI should feel like an agent cockpit rather than a plain REPL:

- fast multi-session navigation;
- readable long conversations;
- clear live task state;
- low-noise tool/result rendering;
- useful memory/browser/runtime visibility;
- safe stop/cancel behavior;
- deterministic tests for state, layout, and event handling.

The existing line-mode TUI remains as a fallback until the full-screen TUI is stable.

## 2. Current State

Implemented today in `crates/koda-agent-cli/src/main.rs`:

- line-mode `koda-agent tui`;
- multiple sessions with `/new`, `/branch`, `/switch`, `/close`, `/rename`;
- runtime history operations with `/rewind`;
- display controls with `/fold`, `/tail`, `/view`, `/search`, `/history`, `/save`, `/panel`;
- runtime slash command pass-through for `/status`, `/llm`, `/llms`, `/stop`, `/continue`, `/btw`;
- event streaming through `AgentRuntime::put_task_with_events`;
- tests for session panel, input history, scrollback window, and search helpers.

Upstream `frontends/tuiapp.py` provides the parity target:

- Textual full-screen app;
- left session sidebar;
- status panel;
- rich log timeline;
- prompt input;
- multiple agent sessions;
- per-task queue streaming;
- task id tracking;
- fold rendering for completed turns;
- key bindings: new session, stop, fold, quit, prev/next session;
- branch, rewind, clear, close, switch, llm commands.

Main gaps:

- no full-screen Rust TUI yet;
- no concurrent live session display;
- no first-class composer widget;
- no inspector panel for tools/memory/browser;
- folding is line-based, not structured cards;
- no mouse/scrollback integration;
- no code block/tool card rendering model;
- no terminal snapshot or reducer-level golden tests for full-screen UI.

## 3. Non-Goals

These are intentionally out of scope for the first full-screen TUI pass:

- full IM frontends;
- native desktop/Qt/desktop pet;
- replacing the existing HTTP/Web UI;
- pixel-perfect clone of Python Textual CSS;
- dependency-heavy GUI frameworks;
- storing or printing secrets from `.env`;
- changing the 9 atomic tool surface.

## 4. Recommended Architecture

### 4.1 Crate layout

Start with a new module inside `koda-agent-cli`, then split into a crate only if it grows too large:

- `crates/koda-agent-cli/src/tui_full.rs` for full-screen TUI;
- keep current line-mode TUI in `main.rs` or move to `tui_line.rs`;
- expose `run_tui_full(cfg)` and `run_tui_line(cfg)`;
- CLI behavior:
  - `koda-agent tui` starts full-screen TUI once stable;
  - `koda-agent tui --line` or env `KODA_TUI_LINE=1` starts current line-mode fallback;
  - during rollout, `koda-agent tui --full` can be hidden/experimental.

If the full TUI becomes large, split later:

- `crates/koda-agent-tui` for reusable UI model/rendering;
- `koda-agent-cli` only wires command-line entrypoints.

### 4.2 Dependencies

Add only focused terminal dependencies:

- `ratatui` for layout/widgets;
- `crossterm` already exists for terminal backend;
- `tui-textarea` for multi-line composer;
- optional later: `syntect` or `bat`-style highlighting, but not in phase 1.

Rationale:

- `ratatui` is mature and Rust-native;
- avoids Python/Textual dependency;
- works cross-platform in macOS/Linux/Windows terminals;
- keeps the core runtime independent of UI.

### 4.3 State model

Use a reducer-like state model so behavior is testable without a terminal:

```rust
struct TuiAppState {
    sessions: BTreeMap<usize, TuiSessionState>,
    active: usize,
    next_id: usize,
    focus: FocusPane,
    layout_mode: LayoutMode,
    command_palette: CommandPaletteState,
    status: StatusLine,
}

struct TuiSessionState {
    id: usize,
    name: String,
    status: SessionStatus,
    runtime: AgentRuntime,
    timeline: Vec<TimelineItem>,
    current_task: Option<TaskState>,
    scroll: ScrollState,
    search: SearchState,
    fold: FoldState,
    inspector: InspectorState,
}

enum TimelineItem {
    User(MessageBlock),
    Assistant(AssistantBlock),
    ToolCall(ToolBlock),
    ToolResult(ToolBlock),
    System(SystemBlock),
}
```

Agent events become UI events:

```rust
enum UiEvent {
    Input(KeyEvent),
    Tick,
    Runtime { session_id: usize, event: AgentEvent },
    RuntimeDone { session_id: usize, result: Result<String> },
    Resize { width: u16, height: u16 },
}
```

### 4.4 Async model

The UI loop must never block on LLM/tool calls.

- Main TUI loop owns `TuiAppState` and rendering.
- Submitting a task spawns a Tokio task.
- Runtime emits `AgentEvent` into an `mpsc` channel.
- UI receives events and updates timeline incrementally.
- Stop/cancel calls `AgentRuntime::abort()` for the target session.
- Concurrent sessions can run; active session is rendered in the center, non-active running sessions show progress in sidebar.

Important invariant:

- A running session rejects a second prompt unless explicitly configured to queue. This matches current ACP and avoids ambiguous history ordering.

## 5. Layout Design

### 5.1 Default desktop layout

```text
┌──────────────────────────────────────────────────────────────────────────────┐
│ Koda Agent                             model: deepseek...  cwd: ...  tokens │
├───────────────┬──────────────────────────────────────────────┬───────────────┤
│ Sessions      │ Timeline                                     │ Inspector     │
│ > #1 main     │  You                                         │ Turn          │
│   running     │  ...                                         │ Tool          │
│   last query  │                                              │ Memory        │
│   #2 branch   │  Agent                                       │ Browser       │
│   idle        │  <summary folded>                            │ LLM/Usage     │
│               │  tool: file_read  ok  34ms                   │               │
│               │  final answer ...                            │               │
├───────────────┴──────────────────────────────────────────────┴───────────────┤
│ Composer: multiline input, slash completion, @file hints                     │
├──────────────────────────────────────────────────────────────────────────────┤
│ Ctrl+Enter send | Ctrl+C stop | Tab switch | /help | search: none            │
└──────────────────────────────────────────────────────────────────────────────┘
```

### 5.2 Narrow terminal layout

If width is below about 100 columns:

- hide inspector first;
- if below about 75 columns, hide sidebar;
- expose hidden panes through `F2` sessions and `F3` inspector overlays.

### 5.3 Pane responsibilities

Session sidebar:

- id, name, status;
- current task id;
- model marker;
- last user query;
- last summary line;
- unread/running marker for background sessions.

Timeline:

- virtualized rendering over recent visible rows;
- user messages as compact blue cards;
- assistant messages as readable green/neutral cards;
- completed turns folded by default;
- latest turn expanded while streaming;
- tool calls/results as compact cards;
- code blocks preserved and optionally highlighted in later phase.

Inspector:

- active session status;
- current/last tool call;
- tool args/result summary;
- current model and failover config;
- usage summary from logs when available;
- memory settlement state;
- browser bridge connection hints.

Composer:

- multi-line editing;
- command completion;
- input history;
- `@file` hints later;
- validation before send.

Status bar:

- model, cwd, session id, task state;
- stop/cancel hint when running;
- recent error or retry state.

## 6. Visual System

Use a professional cockpit style:

- background: near-black slate, not pure black;
- primary text: warm off-white;
- muted text: blue-gray;
- active session: cyan/blue edge;
- running: amber;
- success: green;
- error: red;
- tool cards: cyan;
- memory/plan: restrained violet;
- warnings: amber with bold label.

Avoid heavy ASCII art and avoid animated noise. The UI should look intentional but not distracting.

## 7. Feature Phases

### Phase TUI-0: Preparation

Scope:

- keep current line-mode TUI stable;
- add CLI flag/env fallback design;
- add dependency plan;
- move line-mode helpers into a module if needed.

Acceptance:

- existing `cargo test --workspace --all-features` remains green;
- current `koda-agent tui` behavior not broken until full-screen is ready.

### Phase TUI-1: Full-screen skeleton

Scope:

- add `ratatui` app loop;
- render header, sidebar, timeline placeholder, inspector placeholder, composer, status bar;
- handle quit, resize, focus movement;
- use mock/demo state for first render tests.

Acceptance:

- `koda-agent tui --full` opens and exits cleanly;
- terminal raw mode is always restored on panic/error path;
- state/layout unit tests pass.

### Phase TUI-2: Runtime event integration

Scope:

- submit composer text to `AgentRuntime::put_task_with_events`;
- convert `AgentEvent` into timeline items;
- stream assistant chunks into latest assistant block;
- implement stop/cancel;
- reject second prompt for running session.

Acceptance:

- mock LLM task streams to timeline;
- `/stop` creates stop signal and UI marks stopping/stopped;
- background session can continue while user switches session;
- no UI blocking during slow mock LLM.

### Phase TUI-3: Session management parity

Scope:

- new, branch, switch, close, rename;
- rewind;
- list sessions;
- input history;
- preserve current line-mode command semantics.

Acceptance:

- reducer tests for new/branch/switch/close/rename;
- branch copies runtime history through `AgentRuntime::fork_session()`;
- closing last session is rejected;
- active session never points to missing id.

### Phase TUI-4: Timeline polish

Scope:

- fold completed turns using upstream-inspired `fold_turns` logic;
- render summary cards;
- render tool call/result cards;
- implement `/tail`, `/view`, `/search`, jump-to-match;
- preserve code block content.

Acceptance:

- fold parser tests with `<summary>`, tool output, code fences, CJK text;
- search tests across user/assistant/tool entries;
- long timeline render bounded by visible area.

### Phase TUI-5: Inspector

Scope:

- current task/turn;
- active tool call/result;
- model/failover/session override info;
- memory update/settlement hints;
- browser bridge connected sessions from TMWebDriver master if available.

Acceptance:

- no network/browser requirement for default rendering;
- browser info degrades to `not connected`;
- inspector tests use deterministic state.

### Phase TUI-6: Advanced interaction

Scope:

- command palette;
- slash command completion;
- copy/export message;
- save transcript;
- restore/continue UI;
- mouse scroll and click session switching.

Acceptance:

- keyboard-only workflow remains complete;
- mouse support optional and does not break SSH/CI terminals;
- transcript export matches current markdown save semantics.

### Phase TUI-7: Default switch

Scope:

- make full-screen TUI the default `koda-agent tui`;
- retain `koda-agent tui --line` fallback;
- update README/Makefile/docs;
- add smoke instructions.

Acceptance:

- all existing quality gates pass;
- manual smoke on macOS Terminal/iTerm and a narrow terminal;
- fallback starts current line-mode UI.

## 8. Keyboard Map

Initial keymap:

- `Ctrl+Q`: quit;
- `Ctrl+Enter`: send;
- `Ctrl+C`: stop current task, second press quits if idle;
- `Tab` / `Shift+Tab`: next/previous focus pane;
- `Ctrl+Left` / `Ctrl+Right`: previous/next session;
- `Ctrl+N`: new session;
- `Ctrl+B`: branch session;
- `Ctrl+F`: toggle fold/search focus depending mode;
- `Esc`: close overlay/cancel composer completion;
- `PageUp/PageDown`: timeline scroll;
- `Home/End`: top/bottom timeline;
- `/help`: command help.

Keep slash commands as the reliable fallback for all key actions.

## 9. Testing Strategy

Unit tests:

- layout breakpoint decisions;
- reducer actions;
- event-to-timeline mapping;
- fold parser;
- scroll/search indexes;
- session invariants.

Integration tests:

- mock LLM prompt completes and emits timeline;
- slow mock LLM does not block UI state updates;
- concurrent prompt rejection;
- stop/cancel path;
- branch and rewind.

Manual smoke:

- `cargo run -p koda-agent-cli -- tui --full`;
- send `/llms`;
- send a mock/real prompt;
- create branch;
- switch sessions while one is running;
- stop a running task;
- search transcript;
- resize terminal.

Quality gate:

```bash
cargo fmt --all --check
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo run -q -p xtask -- acp-client-smoke
cargo run -q -p xtask -- tmwd-static-parity-smoke
```

## 10. Risks and Mitigations

Risk: UI complexity pollutes CLI/runtime.

- Mitigation: keep TUI in a separate module/crate; communicate through `AgentEvent` and `AgentRuntime` only.

Risk: terminal raw mode remains broken after panic.

- Mitigation: RAII terminal guard; tests for guard drop where possible; manual panic smoke before making default.

Risk: rendering long conversations is slow.

- Mitigation: virtualized timeline; cache rendered blocks; only re-render visible region and current streaming block.

Risk: concurrent sessions corrupt shared runtime files.

- Mitigation: runtime already serializes task operations per session; UI should document shared temp/memory; avoid running destructive UI actions automatically.

Risk: full-screen UI breaks simple SSH or non-interactive terminals.

- Mitigation: keep line-mode fallback; detect non-TTY and recommend `--input`, `--task`, or `tui --line`.

Risk: new dependencies enlarge build and increase cross-platform issues.

- Mitigation: use mainstream terminal crates only; no GUI/system SDK dependencies; gate optional highlighting later.

Risk: visual polish delays functional parity.

- Mitigation: implement skeleton and event integration first; visual polish after reducer and smoke tests are stable.

## 11. Implementation Order Recommendation

Implement in this order:

1. Add `ratatui`/`tui-textarea` dependencies and hidden `tui --full` entrypoint.
2. Create `tui_full.rs` with app state, terminal guard, layout render, and quit handling.
3. Add reducer tests for session and layout state.
4. Wire composer submit to `put_task_with_events` with mockable runtime path.
5. Render timeline items from `AgentEvent`.
6. Add session sidebar operations.
7. Add fold/search/scrollback.
8. Add inspector.
9. Polish visual theme.
10. Make full-screen default only after manual smoke and regression gates pass.

## 12. Self Review

### Completeness

The plan covers architecture, layout, state, async runtime integration, session behavior, rendering, keyboard UX, tests, rollout, and fallback. It uses upstream Textual behavior as a parity reference without copying Python internals directly.

### Feasibility

The approach is feasible because Rust already exposes the needed runtime event stream via `put_task_with_events`, and current line-mode TUI already has most command semantics. The biggest new work is terminal rendering and app-loop state management, not agent logic.

### Complexity Control

The plan avoids adding GUI/IM scope and keeps the full-screen UI behind a flag until stable. It also avoids early syntax highlighting and mouse-first workflows, which are common sources of complexity.

### Parity Accuracy

The plan mirrors upstream concepts: sessions, task ids, streaming queue/event updates, folding completed turns, sidebar/status/log/prompt layout, stop, branch, rewind, model commands. It intentionally differs in implementation by using `ratatui` and Rust channels rather than Textual threads.

### Acceptance Strength

The acceptance gates include reducer/unit tests, integration-like mock LLM tests, manual terminal smoke, and the existing repo-wide fmt/test/clippy gates. This is stronger than only doing a visual/manual implementation.

### Remaining Open Questions

- Whether to add `tui --full` as hidden experimental first or immediately make `tui` full-screen with `--line` fallback. Recommendation: hidden experimental first.
- Whether to split a `koda-agent-tui` crate immediately. Recommendation: start in `koda-agent-cli/src/tui_full.rs`, split only if the module becomes too large.
- Whether to include syntax highlighting in the first release. Recommendation: defer until timeline virtualization and folding are stable.

## 13. Second Self Review

### What Looks Strong

- The plan correctly keeps the existing line-mode TUI as a fallback, which protects current usability while the full-screen UI matures.
- The architecture uses `AgentEvent` and `AgentRuntime` as boundaries, so UI work should not leak into LLM/tool/memory crates.
- The phased rollout is conservative: skeleton, event integration, session management, timeline polish, inspector, then default switch.
- The testing strategy emphasizes reducer/state tests before terminal visuals, which is the right way to keep a TUI maintainable.
- The non-goals are clear enough to avoid accidentally starting IM/GUI/desktop-pet work.

### Weak Spots Found

- The plan says `koda-agent tui --full`, but the current Clap structure has `Tui` as a unit subcommand. Implementation needs a small CLI shape change, for example `Tui { full: bool, line: bool }`, without breaking `koda-agent tui`.
- The plan mentions `tui-textarea`, but does not define fallback behavior if the crate conflicts with Rust 1.95 or terminal backends. The first implementation should isolate composer behind a small trait/state wrapper.
- The inspector scope may be too broad for early phases. Browser/memory/model usage panels should be data placeholders first, then wired one by one.
- The plan does not explicitly define snapshot testing. Ratatui supports rendering into `TestBackend`; we should use that for deterministic layout tests before manual smoke.
- The plan does not mention terminal capability detection in enough detail. Non-TTY, dumb terminals, CI, and Windows ConHost should fall back to line mode or fail with a clear message.
- Concurrent sessions are useful but risky because tools share the same workspace. The UI should start with concurrent display support but preserve per-session single-running-task enforcement and show a shared-workspace warning.
- Copy/export features are listed later, but clipboard support is platform-sensitive. Export-to-file should come before clipboard.

### Required Adjustments Before Implementation

1. Add CLI compatibility rule: `koda-agent tui` keeps current line-mode until TUI-7; `koda-agent tui --full` is experimental; `koda-agent tui --line` forces fallback once full-screen becomes default.
2. Add a `TerminalModeGuard` requirement in TUI-1 acceptance: raw mode, alternate screen, mouse capture if enabled, and panic-safe restoration.
3. Add `ratatui::backend::TestBackend` snapshot/state tests to TUI-1/TUI-2 acceptance.
4. Narrow TUI-5 inspector MVP to current task, active tool, and model only; memory/browser panels can be placeholders until event/data sources are stable.
5. Add terminal detection: if stdin/stdout is not TTY or `TERM=dumb`, print a clear message and suggest `--input`, `--task`, or `tui --line`.
6. Add performance budget: rendering 1,000 timeline items should remain bounded by visible rows; streaming update should not re-render cached completed blocks.
7. Add config/env safety: no `.env` values are displayed; model names are okay, API keys are never rendered.

### Revised First Implementation Slice

The first coding slice should be smaller than the full TUI vision:

- Add dependencies and hidden CLI flag.
- Add `tui_full.rs` with terminal guard and static layout.
- Add `TuiAppState`, `TuiSessionState`, `TimelineItem`, and reducer actions.
- Add `TestBackend` render test for desktop and narrow layouts.
- Add no real LLM call yet; use mock/demo timeline and quit handling.

Only after this slice passes fmt/test/clippy should we wire `put_task_with_events`.

### Self Review Verdict

The plan is implementation-ready after the adjustments above. The biggest correction is to reduce the first coding slice: do not combine new dependencies, full layout, composer, runtime streaming, sessions, and inspector in one PR-sized change. Start with a hidden static full-screen skeleton and deterministic rendering tests, then connect runtime events in the next slice.

## 14. Third Self Review

### Final Pre-Implementation Check

The plan is safe to implement if the first coding slice stays deliberately small. The key is to avoid mixing UI infrastructure with runtime behavior in the same step.

### Confirmed Implementation Constraints

- `koda-agent tui` must continue to run the current line-mode TUI.
- `koda-agent tui --full` is the only new runtime entrypoint in the first slice.
- `koda-agent tui --line` is accepted now as an explicit fallback alias, even though line-mode remains default.
- No real LLM call is added in the full-screen skeleton.
- No tool/memory/browser inspector data is wired yet; placeholders only.
- No mouse support in the first slice.
- No syntax highlighting or clipboard support in the first slice.
- No changes to core runtime, LLM, tool schemas, or memory semantics.

### Implementation Safety Checklist

- Add only minimal dependencies needed for static TUI rendering and TTY detection.
- Keep the terminal guard local to `tui_full.rs`.
- Ensure terminal cleanup runs through `Drop`.
- If stdout is not a terminal or `TERM=dumb`, return a clear error suggesting `tui --line`.
- Add tests that render with `ratatui::backend::TestBackend`; do not require a real terminal.
- Make layout adaptive: desktop shows sidebar/timeline/inspector; narrow hides inspector or sidebar.
- Keep colors readable and conservative.

### Verdict

No blocker found. Start implementation with the static full-screen skeleton and deterministic layout tests only. Runtime streaming should be the next separate step after this one passes the full quality gate.

## 15. Implementation Note - TUI-1 Static Skeleton

Implemented first slice in `crates/koda-agent-cli/src/tui_full.rs`:

- `koda-agent tui` still uses the existing stable line-mode TUI.
- `koda-agent tui --line` is accepted as an explicit line-mode fallback.
- `koda-agent tui --full` starts an experimental full-screen Ratatui preview.
- The full-screen path has TTY/`TERM=dumb` detection and a local terminal guard for raw mode plus alternate-screen cleanup.
- The initial layout includes header, session sidebar, timeline, inspector, composer, and status bar.
- Narrow terminals hide sidebar/inspector and keep timeline plus composer visible.
- The first slice intentionally does not call the real LLM/runtime yet; it only validates the layout/state foundation.
- Deterministic tests render the UI with `ratatui::backend::TestBackend` and cover wide/narrow layouts plus reducer behavior.

## 16. Implementation Note - TUI-2 Live Runtime Slice

Implemented the first live full-screen path while keeping `koda-agent tui` line-mode as default:

- `koda-agent tui --full` now owns a real `AgentRuntime` per session.
- Composer accepts simple text input, `Backspace` edits, and `Enter` submits the active prompt.
- Runtime execution is spawned through Tokio and sends `AgentEvent` updates back to the UI via an unbounded channel.
- The UI loop remains responsive while a task runs; runtime events append timeline cards for turns, assistant messages, tool calls, tool results, stop, completion, and errors.
- `Ctrl-S` calls `AgentRuntime::abort()` for the active session.
- A running session rejects a second prompt instead of queueing it, preserving history order.
- `n` creates a new real runtime session when the composer is empty.
- Tests cover composer submission, running-session rejection, command-key vs text-input behavior, stop action mapping, runtime-event timeline updates, and the previous layout snapshots.

Still intentionally deferred:

- multiline editor widget;
- scrollback virtualization;
- mouse support;
- concurrent background session progress indicators beyond status labels;
- command palette and richer slash-command discovery;
- structured inspector data from memory/browser/model usage.

## 17. Implementation Note - TUI-3 Scrollback Starter

Added the first keyboard scrollback state for full-screen timeline:

- each session tracks an independent `timeline_scroll` offset;
- `PageDown` increases the offset by a page-sized step;
- `PageUp` saturates the offset back toward zero;
- `Home` jumps back to the top;
- the inspector displays the active session scroll offset;
- tests cover bounded scroll reducer behavior.

This is intentionally still a simple offset over Ratatui `Paragraph::scroll`; true viewport-aware bottom anchoring and virtualization are deferred until the timeline card model is richer.

## 18. Implementation Note - TUI-4 Session Operations

Added the first full-screen session-management parity slice:

- `Ctrl-B` branches the active session by calling `AgentRuntime::fork_session()`.
- `Ctrl-W` closes the active session when it is not running and not the last session.
- `Ctrl-L` clears the active timeline.
- composer local commands now support `/branch [name]`, `/clear`, `/close`, `/rename <name>`, `/sessions`, and `/switch <id|name>`.
- unknown runtime slash commands such as `/status`, `/llm`, `/continue`, and `/btw` still pass through to `AgentRuntime`.
- branching/closing a running session is rejected to avoid dangling event streams and ambiguous history ordering.
- tests cover local command parsing, branch/switch/rename/clear/close behavior, runtime fork wiring, and running-session rejection.

This closes more of the upstream Textual session-control gap while keeping the existing line-mode TUI unchanged.

## 19. Implementation Note - TUI-5 Help, Command Palette, Multiline Composer

Added the first usability layer for full-screen TUI:

- `?` opens an in-terminal help overlay.
- `Ctrl-P` opens a command palette overlay.
- command palette number shortcuts insert command templates into the composer.
- `/help`, `/commands`, and `/palette` are handled as full-screen local commands.
- `Esc` closes an overlay before quitting the TUI; `Ctrl-Q` still quits immediately.
- composer supports multiline prompts with `Ctrl-J` newline and `Enter` submit.
- composer rendering shows the last few input lines and keeps the shortcut hint visible.
- deterministic tests cover overlay behavior, overlay rendering, command-template insertion, and multiline prompt submission.

This makes the full-screen UI more usable for longer sessions without adding a heavyweight editor widget yet.

## 20. Implementation Note - TUI-6 Structured Timeline and Inspector Details

Added the first structured timeline/inspector pass:

- timeline entries now render with role-specific markers for user, assistant, tool, system, and error items.
- folded tool results include a compact single-line summary instead of a generic placeholder.
- each session tracks the active LLM turn.
- each session tracks the latest tool detail: turn, index, name, args, and result.
- inspector now displays active turn and last tool details instead of only placeholder planned panes.
- tool-start and tool-finish `AgentEvent` handling keeps the latest tool detail in sync.
- tests cover tool detail extraction, inspector rendering, active-turn tracking, and folded result summarization.

This moves the full-screen UI closer to the upstream Textual mental model of a live agent cockpit while keeping the event protocol and runtime untouched.

## 21. Implementation Note - TUI-7 Background Session Awareness

Added the first concurrent-session visibility layer:

- each session tracks unread background events;
- background `AgentEvent` updates increment unread count and update sidebar status without stealing focus;
- background completion increments completed-task counters and shows a status-bar notification;
- background failure increments failed-task counters, stores the error, and shows a status-bar notification;
- switching to a session clears its unread counter;
- sidebar renders unread counts for idle/error sessions and active turn markers for running sessions;
- inspector shows unread, completed/failed counters, and latest notice for the active session;
- tests cover active-vs-background event handling, completion counters, failure counters, unread clearing on switch, and sidebar rendering.

This closes the most important usability gap before considering `tui --full` as the default: concurrent sessions can now progress in the background without becoming invisible.

## 22. Implementation Note - TUI-8 Trial Entry and Operator Docs

Added default-switch preparation without changing the stable default:

- `koda-agent tui` still starts line-mode by default.
- `koda-agent tui --full` remains the explicit full-screen entrypoint.
- `KODA_TUI_FULL=1 koda-agent tui` now starts the full-screen TUI for opt-in dogfooding.
- `koda-agent tui --line` overrides `KODA_TUI_FULL=1` and forces line-mode.
- README documents full-screen startup, fallback, shortcuts, local commands, and runtime slash pass-through behavior.
- The env flag parser only accepts explicit truthy values: `1`, `true`, `yes`, `on`, and `full`.

This provides a low-risk way to use the full-screen TUI regularly before making it the default.

## 23. Implementation Note - TUI-9 Automated Entry Smoke

Added an `xtask` smoke for default-switch readiness:

- `cargo run -p xtask -- tui-smoke` verifies full-screen TUI startup behavior without requiring an interactive terminal.
- `make smoke-tui` wraps the same check.
- The smoke confirms `tui --help` exposes the stable `--line` fallback.
- The smoke confirms `tui --full` fails safely outside a TTY with the fallback message.
- The smoke confirms `KODA_TUI_FULL=1 tui` selects the full-screen path and fails safely outside a TTY.
- The smoke confirms `tui --line --help` remains accepted while `KODA_TUI_FULL=1` is set.

This is not a replacement for manual terminal dogfooding, but it prevents regressions in the entrypoint and non-TTY safety contract.

## 24. Implementation Note - macOS Shortcut and Scrollback Fix

Adjusted full-screen terminal behavior for macOS-style terminals:

- enabled crossterm mouse capture while full-screen TUI is active;
- disabled mouse capture on terminal guard drop;
- mouse wheel/trackpad scroll now updates the TUI timeline offset instead of scrolling the terminal's previous shell buffer;
- added F-key alternatives for macOS users whose terminal reserves common shortcuts:
  - `F1` help;
  - `F2` command palette;
  - `F3` new session;
  - `F4` branch;
  - `F5` clear timeline;
  - `F6` close session;
- documented why `Command-*` is not the primary shortcut scheme: Terminal/iTerm/Warp generally intercept Command key combinations before a crossterm app can receive them.

This keeps `Ctrl-*` as the portable terminal baseline while making macOS use more comfortable and preventing accidental scrollback leakage during full-screen mode.

## 25. Implementation Note - Scrollbars and Composer Cleanup

Fixed the first visual issues found during full-screen dogfooding:

- Timeline now renders a vertical scrollbar when content exceeds the visible viewport.
- Inspector has its own independent scroll offset and vertical scrollbar.
- Mouse clicks now focus the pane under the cursor, and trackpad/wheel scrolling targets the pane under the cursor when possible.
- Mouse wheel scrolls Inspector when the Inspector pane is targeted; otherwise it scrolls Timeline.
- Composer no longer renders a duplicated `Composer` label inside the block body; the block title is the single title.
- Tests cover scrollbar rendering, independent Inspector mouse scrolling, and the single Composer label invariant.

## 26. Implementation Note - ToolCard Adapter Plan and First Slice

Tool output should be rendered as structured cards rather than generic text. The design keeps the core `AgentEvent` wire shape unchanged and adapts only in the TUI layer:

- `ToolStarted` renders a Chinese tool card with a stable title and an argument summary tailored to the tool name.
- `ToolFinished` keeps the original args next to the result so result cards can still be interpreted in tool context.
- The initial adapter covers the original nine atomic tools:
  - `code_run`: language/type, timeout, cwd, script/code preview;
  - `file_read`: path, start/count, keyword, line-number setting;
  - `file_patch`: path plus old/new content previews;
  - `file_write`: path, mode, content/source preview;
  - `web_scan`: active/switch tab and scan flags;
  - `web_execute_js`: tab, save target, monitor flag, JS/bridge-command preview;
  - `ask_user`: question and candidate count;
  - `update_working_checkpoint`: key info and related SOP;
  - `start_long_term_update`: long-term memory settlement marker.
- Folded result cards summarize common result fields such as `status`, `ok`, `exit_code`, `stdout`, `stderr`, `path`, and byte counts.
- Expanded result cards expose the most useful raw fields without dumping long JSON by default.

Self-review:

- This is intentionally display-only: no changes to LLM protocol, tool schemas, or dispatcher behavior.
- The first slice avoids heavy syntax highlighting, diff rendering, or markdown dependencies until the timeline line model is stable.
- Result cards still need a second pass for per-tool rich details such as mini-diff, file previews, browser tab diffs, and command duration.
- Tests cover argument adaptation and result summary extraction so future rendering changes do not regress the basic card contract.

## 27. Implementation Note - Timeline Follow Tail and First TUI Split

Fixed the Timeline scroll model and started decomposing the large full-screen TUI module:

- Timeline scroll is now clamped to `max(content_lines - viewport_lines, 0)` instead of growing without bound.
- Sessions track `timeline_follow_tail`; new active output auto-scrolls to the bottom while follow-tail is enabled.
- User scroll-up and `Home` disable follow-tail so older output stays readable.
- `End` returns to the latest output, clears unseen output, and re-enables follow-tail.
- When follow-tail is paused, active-session events increment `timeline_unseen` and the status bar tells the user to press `End`.
- Tool card rendering was split into `crates/koda-agent-cli/src/tui_full/tool_cards.rs` to keep future TUI work modular.

Self-review:

- `tui_full.rs` is still too large because it contains runtime event handling, layout, reducers, rendering, and tests in one file.
- The first split intentionally moved only the lowest-risk pure rendering adapter; this avoids destabilizing runtime/session behavior.
- Next safe split candidates are `state.rs` for data types/session reducers, `layout.rs` for layout/render shell, and `tests/` or test helper modules for the large unit-test block.

## 28. Implementation Note - Lightweight Markdown and Chinese Timeline Labels

Added the first terminal-friendly Markdown rendering slice for full-screen Timeline messages:

- Assistant/user/error text now goes through a lightweight Markdown renderer instead of raw line rendering.
- Supported blocks: headings, unordered/ordered lists, blockquotes, horizontal rules, fenced code blocks, and inline code spans.
- Code blocks render with a compact terminal frame and optional language label.
- Timeline role labels now show Chinese first while retaining English for familiarity: `用户 You`, `助手 Assistant`, `错误 Error`.
- Major pane titles and Inspector headings now include Chinese labels while keeping existing English suffixes to avoid breaking operator muscle memory.
- Markdown rendering was split into `crates/koda-agent-cli/src/tui_full/markdown.rs`, continuing the gradual TUI module decomposition.

Self-review:

- This is intentionally a Markdown subset, not a full CommonMark renderer; tables and nested blocks still fall back to plain-ish text.
- The renderer avoids new dependencies and keeps output deterministic for snapshot-like terminal tests.
- A future pass should add width-aware wrapping, CJK display-width handling, and optional syntax highlighting for code/diff blocks.

## 29. Implementation Note - TUI State Module Split

Continued reducing the full-screen TUI file size by extracting state/data definitions:

- moved `AppLayout`, `FocusPane`, `LayoutMode`, `TuiAppState`, `TuiSessionState`, `ToolDetail`, `SessionStatus`, `TimelineItem`, and `Overlay` into `crates/koda-agent-cli/src/tui_full/state.rs`;
- kept reducer/runtime/render functions in `tui_full.rs` for now to avoid a large behavioral refactor in the same slice;
- preserved existing state methods (`from_config`, `active_session`, `active_session_mut`, `create_session`) with `pub(super)` visibility;
- retained all existing TUI behavior and tests.

Self-review:

- This is a safe decomposition step because it moves data definitions without changing event handling or rendering logic.
- `tui_full.rs` is still larger than ideal; the next high-value split is `reducer.rs` for keyboard/mouse/session actions, followed by `render.rs` for pane rendering.

## 30. Implementation Note - TUI Reducer Module Split

Continued the full-screen TUI decomposition by extracting interaction reducers and session actions:

- moved keyboard reducer, mouse reducer, local slash-command parsing, command palette templates, session branch/switch/close helpers, and Timeline/Inspector scroll reducers into `crates/koda-agent-cli/src/tui_full/reducer.rs`;
- kept runtime event application and rendering in `tui_full.rs` because those still share Timeline line-count and runtime event details;
- kept `KeyAction` and `LocalCommand` in the reducer module with `pub(super)` visibility for the event loop and tests;
- removed duplicated overlay/unread mutations found during the extraction pass.

Self-review:

- This is still a mechanical split; it does not alter keyboard/mouse semantics.
- `reducer.rs` depends on a few parent helpers (`build_runtime`, `timeline_viewport_lines`, `max_timeline_scroll`, `trim_chars`), so a future cleanup should move scroll metrics and runtime factory behind narrower interfaces.
- The next split should be `render.rs`, which will move layout and pane rendering out of `tui_full.rs` and leave the root file mostly as the event loop/wiring layer.

## 31. Implementation Note - TUI Render Module Split

Completed the next full-screen TUI decomposition step by moving layout and pane rendering into `crates/koda-agent-cli/src/tui_full/render.rs`:

- moved layout computation, header/session/timeline/inspector/composer/status rendering, overlays, scrollbars, and text trimming helpers out of `tui_full.rs`;
- kept runtime event application and async event-loop wiring in `tui_full.rs`, so the root module now focuses on terminal setup, runtime orchestration, and tests;
- preserved shared pure helpers (`timeline_viewport_lines`, `max_timeline_scroll`, `summarize_tool_result`, `trim_chars`) with narrow `pub(super)` visibility for reducer/tool-card/tests;
- validated the split with targeted full-screen TUI tests.

Self-review:

- This is still mostly structural, but it reduces merge risk for the upcoming feature work because state, reducers, rendering, Markdown, and tool cards now live in separate modules.
- `tui_full.rs` still contains a large test block and runtime-event application; the next safe cleanup is splitting runtime event adapters or moving tests to a dedicated submodule.
- The render module intentionally stays dependency-light and avoids a full Markdown/syntax stack until Timeline wrapping and CJK width behavior are more mature.

## 32. Implementation Note - TUI History Sessions and Rich Tool Details

Improved the full-screen TUI toward Codex-style session continuity and tool readability:

- left sidebar now loads recent `memory/L4_raw_sessions/session_*.json` entries as historical sessions on startup;
- historical sessions render a read-only preview timeline and their saved `messages`/`history_info` are restored into a runtime so submitting a new prompt continues from that saved context;
- added `AgentRuntime::restore_session_snapshot(history_info, messages)` to restore both LLM messages and GenericAgent-style working-history lines together;
- expanded tool results now adapt per tool: `code_run` separates stdout/stderr, `file_patch` shows a mini diff preview, `file_write` shows path/bytes/content preview, and browser/file-read tools show useful output sections.

Self-review:

- History loading is bounded to recent sessions and line previews to keep TUI startup fast even with a large L4 archive.
- This preserves the original 9-tool surface; all changes are display/restore behavior, not new tool schemas.
- Next work should refine keyboard selection in the session sidebar and add CJK-aware timeline wrapping; both are UX improvements rather than protocol changes.

## 33. Implementation Note - Sidebar Session Selection

Added the first direct-manipulation behavior to the full-screen TUI session sidebar:

- clicking a visible row in the Sessions pane now switches to that session instead of only moving focus;
- switching by click clears unread counters for the selected session, matching keyboard `/switch` behavior;
- behavior is covered by a focused unit test and does not change line-mode TUI.

Self-review:

- This makes the sidebar useful now that historical L4 sessions are loaded there.
- The sidebar is still bounded rather than scrollable; this is acceptable while startup history loading is capped, but a later pass should add sidebar scrolling if we expose more than the recent-session window.

## 34. Implementation Note - Live SSE Streaming and Thinking/Summary Events

Fixed the gap where SSE was parsed after full collection but not surfaced live to frontends:

- added `LlmStreamEvent` and `LlmClient::chat_with_events` so OpenAI Chat Completions, OpenAI Responses, Claude Messages, and text-protocol streaming can emit content/thinking deltas while the HTTP stream is still arriving;
- mapped provider thinking fields (`reasoning_content`, `reasoning`, Responses reasoning deltas, Claude thinking deltas) into runtime `ThinkingMessage*` events;
- mapped content deltas into `AssistantMessageDelta` so TUI/line-mode can show token-like streaming instead of waiting for the full response body;
- stopped rendering raw `<summary>...</summary>` as normal answer text: it is now converted to a thinking/summary annotation, while the assistant answer uses the summary-stripped body;
- ACP/frontends and full-screen TUI now understand assistant/thinking delta events.

Self-review:

- This keeps the original GenericAgent summary contract for model prompting/history, but avoids exposing raw XML-ish tags directly in the operator UI.
- Tool-call streaming is still finalized after the model completes the function-call JSON; this is intentional because partial tool args are not dispatchable safely.
- The next hardening pass should add mock HTTP SSE integration tests, not only parser/unit coverage, to verify chunk-boundary behavior across providers.

## 35. Implementation Note - Pulldown CommonMark Renderer

Replaced the handwritten Markdown subset parser with a `pulldown-cmark` based renderer for the full-screen TUI timeline:

- added `pulldown-cmark` as the Markdown parser dependency instead of maintaining ad-hoc string matching;
- kept the existing Koda visual language for headings, blockquotes, code blocks, bullets, and inline code;
- added parser-backed support for CommonMark/GFM features including nested lists, task list markers, strikethrough, tables, footnote references, hard/soft breaks, fenced code metadata, and heading attributes;
- preserved deterministic `Vec<Line>` output so timeline scroll metrics and tests remain stable.

Self-review:

- This is intentionally `pulldown-cmark` plus a Koda renderer, not a direct dependency on a Ratatui Markdown widget, because the current TUI needs custom Chinese labels, summary/thinking separation, and stable line-count accounting.
- The renderer still does not fetch images or resolve links interactively; it renders them as terminal text, which is safer for an agent TUI.
- Next pass can add width-aware wrapping and syntax highlighting, but those should stay optional to avoid slowing token streaming.

## 36. Implementation Note - Timeline Typewriter Animation

Added a visual typewriter layer for the full-screen TUI timeline:

- assistant and thinking messages now enter the timeline as empty rows and reveal a few Unicode scalar characters per UI tick;
- SSE deltas extend the same typewriter buffer, so true provider streaming and non-streaming full responses share the same visual rhythm;
- tool calls, tool results, turn boundaries, completion, failure, and stop events flush any pending typewriter text first so message ordering stays deterministic;
- tick cadence was reduced from 250ms to 33ms for smoother rendering without changing runtime/LLM protocol semantics.

Self-review:

- This is intentionally a UI-only animation layer; it does not delay tool dispatch or alter LLM history.
- The implementation is char-boundary safe for Chinese and emoji-like scalar values, but it is not yet full grapheme-cluster aware.
- If needed later, the reveal speed should become a config key such as `tui.typewriter_chars_per_tick` / `tui.typewriter_enabled`.

## 37. Implementation Note - TUI Scroll Responsiveness and Timeline Growth Guard

Fixed two full-screen TUI issues found after adding typewriter animation:

- the event loop now renders immediately after keyboard/mouse input instead of waiting for the next animation tick, removing the perceived half-frame delay on mouse wheel scroll;
- typewriter ticks now reconcile the active Timeline scroll position, so `follow=on` continues to track newly revealed lines and the latest content stays visible while text is being typed;
- added a bounded Timeline retention guard (`MAX_TIMELINE_ITEMS = 2000`) for completed/non-typewriter sessions to prevent unbounded in-memory growth during very long TUI sessions;
- tool/result/system events still flush pending typewriter text first, preserving deterministic ordering.

Self-review:

- This was not a classic Rust memory leak; it was an unbounded UI transcript plus repeated Markdown line-count rendering. The retention cap addresses the unbounded part without altering runtime history.
- The fix keeps old history in L4/session files; the TUI only caps the visible in-memory timeline.
- A future optimization should cache rendered Markdown line counts per timeline item to reduce CPU work on huge sessions.
