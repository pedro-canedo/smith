# Graph Report - .  (2026-08-05)

## Corpus Check
- Corpus is ~28,780 words - fits in a single context window. You may not need a graph.

## Summary
- 755 nodes · 1711 edges · 44 communities (38 shown, 6 thin omitted)
- Extraction: 98% EXTRACTED · 2% INFERRED · 0% AMBIGUOUS · INFERRED: 35 edges (avg confidence: 0.93)
- Token cost: 87,305 input · 0 output

## Community Hubs (Navigation)
- TUI App State & Events
- CLI Orchestrator (main.rs)
- MCP Tool Bridge
- Setup Wizard
- Anthropic Provider
- OpenAI/Ollama Provider
- Permission Classes & Tool Trait
- Markdown Rendering
- Agent Construction API
- Agent Event Types
- TUI Rendering (ui.rs)
- Shell Tool (run_bash)
- CLAUDE.md Architecture Notes
- Goal Persistence
- Agent Core State
- Stream Consumption & Test Providers
- File Staging
- Permission Detail Formatting
- Tool Registry
- Roadmap Packages (P2-P7)
- Agent Turn/Tool Dispatch
- File Edit Tool Tests
- Tool Interception & Ollama Bug
- Permission Policy Parsing
- Read-only FS Tools
- Slash Command Completion
- ask_user Tool
- write_tasks Tool
- Ollama 400 Bug Report
- LlmProvider Trait & Test Fakes
- Action/AgentEvent Pattern Docs
- Resource Monitoring
- Permission Policy Tests
- Project Identity & Sessions
- smith-persist & Delivery Principles
- Cost Estimation
- Terminal Init/Restore
- Config & Goal File Refs
- Glob Tool
- ASCII Banner
- Banner Asset
- Language Policy
- Out of Scope Notes

## God Nodes (most connected - your core abstractions)
1. `test_app()` - 58 edges
2. `App` - 50 edges
3. `Agent` - 34 edges
4. `Message` - 27 edges
5. `AgentEvent` - 26 edges
6. `ToolContext` - 25 edges
7. `run_orchestrator()` - 23 edges
8. `ToolResult` - 20 edges
9. `McpClient` - 16 edges
10. `ToolRegistry` - 16 edges

## Surprising Connections (you probably didn't know these)
- `messages_to_wire function` --semantically_similar_to--> `Tool interception pattern (ask_user)`  [INFERRED] [semantically similar]
  docs/bugs1.md → CLAUDE.md
- `Ollama (OpenAI-compatible adapter)` --conceptually_related_to--> `smith-providers crate`  [INFERRED]
  docs/bugs1.md → CLAUDE.md
- `crates/smith-providers/src/openai.rs` --conceptually_related_to--> `smith-providers crate`  [INFERRED]
  docs/bugs1.md → CLAUDE.md
- `smith-providers crate` --conceptually_related_to--> `smith-providers crate`  [INFERRED]
  README.md → CLAUDE.md
- `P4 - /plan package` --conceptually_related_to--> `Permission model (PermissionClass/PermissionPolicy/plan_gated)`  [INFERRED]
  docs/goal1.md → CLAUDE.md

## Import Cycles
- 1-file cycle: `crates/smith-tui/src/terminal.rs -> crates/smith-tui/src/terminal.rs`

## Hyperedges (group relationships)
- **Workspace crates depending on smith-core** — claude_smith_core, claude_smith_providers, claude_smith_tools, claude_smith_mcp, claude_smith_persist, claude_smith_tui, claude_smith_cli [EXTRACTED 1.00]
- **Smith slash-command roadmap** — readme_model_command, readme_permission_command, readme_usage_command, readme_plan_command, readme_goal_command, readme_loop_command, readme_kanban_command, readme_ultraplan_command [EXTRACTED 1.00]
- **Goal1 incremental package delivery plan (P1-P8)** — docs_goal1_p1_model, docs_goal1_p2_permission, docs_goal1_p3_usage, docs_goal1_p4_plan, docs_goal1_p5_goal, docs_goal1_p6_loop, docs_goal1_p7_kanban, docs_goal1_p8_ultraplan [EXTRACTED 1.00]

## Communities (44 total, 6 thin omitted)

### Community 0 - "TUI App State & Events"
Cohesion: 0.06
Nodes (78): Action, Activity, activity_label(), ActivityStatus, App, ask_user_does_not_get_a_transcript_tool_line(), assistant_turn_complete_mid_loop_does_not_reset_waiting_flag(), building_phase_survives_thinking_event() (+70 more)

### Community 1 - "CLI Orchestrator (main.rs)"
Cohesion: 0.06
Nodes (55): Connection, approved_plan_prompt_embeds_plan_excerpt(), assistant(), build_approved_plan_prompt(), build_loop_task_prompt(), build_provider(), Cli, Commands (+47 more)

### Community 2 - "MCP Tool Bridge"
Cohesion: 0.08
Nodes (30): AtomicU64, ChildStdout, McpToolAdapter, Arc, CancellationToken, Self, Value, connect_fake() (+22 more)

### Community 3 - "Setup Wizard"
Cohesion: 0.14
Nodes (27): ColorfulTheme, ensure_ollama_running(), ollama_binary_present(), ollama_reachable(), Result, String, run(), select_model() (+19 more)

### Community 4 - "Anthropic Provider"
Cohesion: 0.12
Nodes (22): ContentBlock, Value, AnthropicProvider, build_request_body(), builds_request_body_with_system_and_tools(), content_block_to_wire(), message_to_wire(), parse_sse_payload() (+14 more)

### Community 5 - "OpenAI/Ollama Provider"
Cohesion: 0.15
Nodes (21): build_request_body(), builds_request_body_with_system_and_tools(), messages_to_wire(), OpenAiProvider, parse_chunk(), parses_text_delta_chunk(), parses_tool_call_chunks_and_finish(), BoxStream (+13 more)

### Community 6 - "Permission Classes & Tool Trait"
Cohesion: 0.09
Nodes (8): PermissionClass, Send, Sync, Tool, EditFileTool, ListDirTool, ReadFileTool, WriteFileTool

### Community 7 - "Markdown Rendering"
Cohesion: 0.14
Nodes (15): options(), owned_lines(), plain(), render(), renders_fenced_code_block_verbatim(), renders_inline_code_as_text(), renders_plain_paragraph(), renders_table_as_multiple_bordered_lines() (+7 more)

### Community 8 - "Agent Construction API"
Cohesion: 0.16
Nodes (16): clearing_goal_reverts_to_base_system(), effective_system_folds_goal_into_base_system(), effective_system_uses_base_system_when_no_goal_set(), effective_system_works_with_goal_but_no_base_system(), empty_assistant_turn_is_not_pushed_to_history(), fake_agent(), plan_gate_blocks_mutating_tools_even_under_skip_policy(), plan_gate_lifted_allows_the_tool_to_run() (+8 more)

### Community 9 - "Agent Event Types"
Cohesion: 0.15
Nodes (20): PermissionAsk, AgentEvent, AgentPhase, LoopStopReason, PermissionDecision, PermissionRequest, ResourceStats, Option (+12 more)

### Community 10 - "TUI Rendering (ui.rs)"
Cohesion: 0.26
Nodes (21): activity_widget_height(), center_vertically(), draw(), draw_activity_widget(), draw_footer(), draw_idle(), draw_input(), draw_messages() (+13 more)

### Community 11 - "Shell Tool (run_bash)"
Cohesion: 0.17
Nodes (15): cancellation_kills_a_long_running_command(), ctx(), format_result(), non_zero_exit_is_an_error(), CancellationToken, Child, Result, String (+7 more)

### Community 12 - "CLAUDE.md Architecture Notes"
Cohesion: 0.13
Nodes (19): Agent::run_one_tool, LlmProvider trait, Permission model (PermissionClass/PermissionPolicy/plan_gated), Provider/model switching mid-session, run_bash tool, smith-cli crate, smith-core crate, smith-mcp crate (+11 more)

### Community 13 - "Goal Persistence"
Cohesion: 0.17
Nodes (11): blank_goal_file_counts_as_no_goal(), clear_goal(), goal_path(), load_goal(), round_trips_a_goal(), Option, Path, PathBuf (+3 more)

### Community 14 - "Agent Core State"
Cohesion: 0.15
Nodes (5): Agent, NoTools, Option, Vec, PermissionPolicy

### Community 15 - "Stream Consumption & Test Providers"
Cohesion: 0.24
Nodes (12): consume_stream(), BoxStream, Result, CompletionRequest, Option, String, StopReason, StreamEvent (+4 more)

### Community 16 - "File Staging"
Cohesion: 0.30
Nodes (16): apply_staged(), ctx(), discard_removes_staged_file(), discard_staged(), flattens_absolute_paths_safely(), mirrors_relative_paths_under_session(), normalize_rel(), Path (+8 more)

### Community 17 - "Permission Detail Formatting"
Cohesion: 0.39
Nodes (14): edit_file_is_a_short_summary(), field(), format_bash(), format_edit(), format_generic(), format_path_action(), format_permission_detail(), format_write() (+6 more)

### Community 18 - "Tool Registry"
Cohesion: 0.19
Nodes (8): Arc, CancellationToken, HashMap, Option, Self, String, Vec, ToolRegistry

### Community 19 - "Roadmap Packages (P2-P7)"
Cohesion: 0.19
Nodes (15): P2 - /permission package, P3 - /usage package, P4 - /plan package, P5 - /goal package, P6 - /loop package, P7 - /kanban package, P8 - /ultraplan package, /goal slash command (+7 more)

### Community 20 - "Agent Turn/Tool Dispatch"
Cohesion: 0.36
Nodes (6): parse_tasks(), QuestionAsk, CancellationToken, String, UnboundedSender, Value

### Community 21 - "File Edit Tool Tests"
Cohesion: 0.45
Nodes (11): cancel(), ctx(), edit_file_errors_when_old_str_missing(), edit_file_requires_unique_match(), glob_finds_matching_files(), list_dir_marks_directories(), read_file_missing_is_error(), read_file_respects_offset_and_limit() (+3 more)

### Community 22 - "Tool Interception & Ollama Bug"
Cohesion: 0.17
Nodes (12): ask_user tool, Tool interception pattern (ask_user), messages_to_wire function, Ollama (OpenAI-compatible adapter), P1 - /model package, Anthropic provider, ~/.smith/config.toml, MCP servers configuration (+4 more)

### Community 23 - "Permission Policy Parsing"
Cohesion: 0.25
Nodes (6): Into, Option, Self, String, ToolResult, Value

### Community 24 - "Read-only FS Tools"
Cohesion: 0.44
Nodes (7): ToolContext, field_str(), resolve(), CancellationToken, Option, PathBuf, Value

### Community 25 - "Slash Command Completion"
Cohesion: 0.24
Nodes (7): complete(), Option, String, Vec, SlashSuggestion, suggestions_filter_by_prefix(), suggestions_for()

### Community 26 - "ask_user Tool"
Cohesion: 0.25
Nodes (3): AskUserTool, CancellationToken, Value

### Community 27 - "write_tasks Tool"
Cohesion: 0.25
Nodes (3): CancellationToken, Value, WriteTasksTool

### Community 28 - "Ollama 400 Bug Report"
Cohesion: 0.22
Nodes (9): Anthropic path (serializer), Empty/cancelled turns hardening item, Bug: Ollama 400 invalid message content type, pydantic-ai (cited prior art), ruby_llm (cited prior art), stream_options.include_usage compatibility item, Tool-call IDs hardening item, Tool message shape requirement (+1 more)

### Community 29 - "LlmProvider Trait & Test Fakes"
Cohesion: 0.25
Nodes (6): AtomicBool, EmptyReplyProvider, SingleToolCallProvider, LlmProvider, Send, Sync

### Community 30 - "Action/AgentEvent Pattern Docs"
Cohesion: 0.32
Nodes (8): Action/AgentEvent loop pattern, Agent::run_turn, App::on_agent_event, smith-core/src/event.rs (AgentEvent definitions), smith-tui crate, smith-tui crate, smith-tui crate, tui-markdown library

### Community 31 - "Resource Monitoring"
Cohesion: 0.43
Nodes (6): nvidia_smi_stats(), ollama_model_vram(), poll(), Option, String, UnboundedSender

### Community 33 - "Project Identity & Sessions"
Cohesion: 0.47
Nodes (6): Smith CLI System Platform (v1.0.0), .smith/sessions.db (per-project session history), Smith (terminal AI coding agent), Smith (terminal AI coding agent), .smith/sessions.db, Smith (terminal AI coding agent)

### Community 34 - "smith-persist & Delivery Principles"
Cohesion: 0.40
Nodes (6): smith-persist crate, Acceptance criteria (per package), Delivery principles (small independent packages), run_slash_command dispatcher, smith-persist crate, smith-persist crate

### Community 35 - "Cost Estimation"
Cohesion: 0.53
Nodes (4): estimate_cost_usd(), estimates_known_model_cost(), price_per_million_usd(), Option

### Community 36 - "Terminal Init/Restore"
Cohesion: 0.60
Nodes (5): init(), install_panic_hook(), restore(), Result, Tui

### Community 37 - "Config & Goal File Refs"
Cohesion: 0.40
Nodes (5): ~/.smith/config.toml (global config), .smith/goal.md, Session/goal persistence, SessionStore, .smith/goal.md

## Knowledge Gaps
- **22 isolated node(s):** `SMITH ASCII Banner`, `LlmProvider trait`, `Tool trait`, `ToolExecutor trait`, `ask_user tool` (+17 more)
  These have ≤1 connection - possible missing edges or undocumented components.
- **6 thin communities (<3 nodes) omitted from report** — run `graphify query` to explore isolated nodes.

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **Why does `App` connect `TUI App State & Events` to `Agent Event Types`, `TUI Rendering (ui.rs)`, `Agent Core State`, `Stream Consumption & Test Providers`?**
  _High betweenness centrality (0.195) - this node is a cross-community bridge._
- **Why does `PermissionPolicy` connect `Agent Core State` to `Permission Policy Tests`, `CLI Orchestrator (main.rs)`, `TUI App State & Events`, `Permission Classes & Tool Trait`, `Agent Construction API`, `Agent Event Types`, `Permission Policy Parsing`?**
  _High betweenness centrality (0.136) - this node is a cross-community bridge._
- **Why does `Tool` connect `Permission Classes & Tool Trait` to `Permission Policy Tests`, `MCP Tool Bridge`, `Glob Tool`, `Shell Tool (run_bash)`, `Tool Registry`, `ask_user Tool`, `write_tasks Tool`?**
  _High betweenness centrality (0.122) - this node is a cross-community bridge._
- **What connects `SMITH ASCII Banner`, `LlmProvider trait`, `Tool trait` to the rest of the system?**
  _22 weakly-connected nodes found - possible documentation gaps or missing edges._
- **Should `TUI App State & Events` be split into smaller, more focused modules?**
  _Cohesion score 0.05558728345707215 - nodes in this community are weakly interconnected._
- **Should `CLI Orchestrator (main.rs)` be split into smaller, more focused modules?**
  _Cohesion score 0.06322624743677376 - nodes in this community are weakly interconnected._
- **Should `MCP Tool Bridge` be split into smaller, more focused modules?**
  _Cohesion score 0.07878787878787878 - nodes in this community are weakly interconnected._