# Graph Report - .  (2026-08-06)

## Corpus Check
- 115 files · ~277,725 words
- Verdict: corpus is large enough that graph structure adds value.

## Summary
- 4181 nodes · 12097 edges · 137 communities (129 shown, 8 thin omitted)
- Extraction: 98% EXTRACTED · 2% INFERRED · 0% AMBIGUOUS · INFERRED: 273 edges (avg confidence: 0.81)
- Token cost: 156,457 input · 0 output

## Community Hubs (Navigation)
- TUI App Event Tests
- Layered Config & Memory Imports
- Web Fetch Tool
- Tool Hooks Engine
- Turn Checkpoints & Rewind
- Syntax Highlighting
- Headless Mode
- TUI Rendering
- Filesystem Tool Safety
- Agent Test Doubles
- Session Store (SQLite)
- Agent Public Surface
- Agent Tool Dispatch
- Subagent Behavior Tests
- CLI Flags & Entry Point
- Chat Line & Activity State
- App State & Input Actions
- Grep Tool
- Bash Shell Tool
- Theme & WCAG Contrast
- Retry, Compaction & Usage
- Tool-Name Recovery & Reasoning Tags
- MCP JSON-RPC Client
- Doctor Diagnostics
- TUI Draw & Render Caching
- Tool Schema Validation
- Custom Slash Commands
- Extension Discovery & Jail
- Hook Enforcement in Agent
- Browser Runtime Provisioning
- System Prompt Assembly
- MCP Tool Bridge
- Markdown Rendering
- Input Box Component
- Diff Rendering
- Keymap & Bindings
- Node Runtime & 9Router
- Bing Search Backend
- Read-Before-Overwrite Gate
- Key Handling & Mid-Turn Queue
- Anthropic Provider
- MCP HTTP Transports
- Setup Wizard
- Skills Loading
- Config Round-Trip
- Edit File Tool
- OpenAI Context Windows
- Provider Fallback Chain
- Transcript Memoization
- Self-Update Checks
- MCP Registry & Liveness
- Search Failure Classification
- Orchestrator Provider Setup
- Runtime Asset Download
- Chromium Page Fetcher
- Doctor Report Formatting
- Context Carry-Over & Todos
- Token Usage Accounting
- OpenAI Flavors & Catalogues
- Orchestrator State & Persistence
- Search Result Parsing
- MCP Transport Abstraction
- Panel Component
- Skill Tool
- Slash Command Completion
- AgentEvent Serialization
- Subagent Definitions & Limits
- Personas & Output Styles
- Text Wrapping & Char Width
- Retry Policy
- Tool Trait & Permission Class
- File @-Completion
- Log Buffer
- Subagent & Interception Docs
- Permission Model Docs
- Browser Install Integrity
- Tool Registry Validation
- Architecture Overview Docs
- Prompt Injection Detection
- Behavioral Evals Docs
- Secret Redaction
- SearXNG Backend
- Context Gauge Component
- Changelog Bug Fixes
- Provider Traits & Errors
- Subagent Tool Restriction
- Write Staging
- Runtime Platform Detection
- Subagent Directory Scan
- Tool Trait Registration
- MCP Design Docs
- Web Search Design Docs
- MCP Status Reporting
- Permission Prompt Detail
- Model Pricing
- Google News Backend
- Search Cache & Backend Pinning
- Release Workflow Docs
- Registry Test Fixtures
- Scratch Directory Sweep
- Search Cache Keys & DDG Parsing
- Headless CLI Integration Tests
- Provider HTTP Client
- Query Language Detection
- Eval Harness (run.py)
- File Read Helpers & Fencing
- Tracing & Log Install
- Terminal Init & Panic Hook
- CI Workflow & MSRV
- Path Jail & JSON Envelope Docs
- Log Field Visitor
- Ask User Tool
- Task Tool
- Write Tasks Tool
- Hint Chips Component
- Design System 80x24 Contract
- Subagent Report Types
- MCP Untrusted Fencing
- Custom Command Dispatch Tests
- TUI Library Surface
- Resource Stats (GPU/VRAM)
- Subagent Event Relay
- Duplicate Tool Name Error
- Fake Provider Fixture
- Tracing Buffer Layer
- PTY Panic Restore Test
- Persistence Crate Docs
- Glob Tool
- List Dir Tool
- Homebrew Formula
- LlmProvider Trait Bounds
- ASCII Banner
- Install Shell Script
- Test Convention Docs
- Release Artifact Caveat

## God Nodes (most connected - your core abstractions)
1. `test_app()` - 128 edges
2. `Agent` - 113 edges
3. `App` - 107 edges
4. `Theme` - 97 edges
5. `ToolContext` - 93 edges
6. `ctx()` - 66 edges
7. `Config` - 64 edges
8. `AgentEvent` - 58 edges
9. `ToolResult` - 58 edges
10. `ChatLine` - 55 edges

## Surprising Connections (you probably didn't know these)
- `Behavioral eval harness (evals/run.py)` --semantically_similar_to--> `fake_provider.py fixture`  [INFERRED] [semantically similar]
  evals/README.md → .github/workflows/ci.yml
- `Action / AgentEvent loop (contributor summary)` --semantically_similar_to--> `Action / AgentEvent loop`  [INFERRED] [semantically similar]
  AGENTS.md → CLAUDE.md
- `Smith (terminal AI coding agent)` --semantically_similar_to--> `Smith ASCII banner (CLI AGENT PLATAFORM)`  [INFERRED] [semantically similar]
  README.md → ASCII - smith.md
- `Tool interception pattern (contributor summary)` --semantically_similar_to--> `Tool interception pattern`  [INFERRED] [semantically similar]
  AGENTS.md → CLAUDE.md
- `File tool path jail (fs_tools::resolve)` --semantically_similar_to--> `Extension path jail enforced on discovery`  [INFERRED] [semantically similar]
  AGENTS.md → CLAUDE.md

## Import Cycles
- 2-file cycle: `crates/smith-tui/src/app.rs -> crates/smith-tui/src/transcript.rs -> crates/smith-tui/src/app.rs`
- 2-file cycle: `crates/smith-mcp/src/client.rs -> crates/smith-mcp/src/transport.rs -> crates/smith-mcp/src/client.rs`
- 2-file cycle: `crates/smith-mcp/src/client.rs -> crates/smith-mcp/src/http.rs -> crates/smith-mcp/src/client.rs`
- 2-file cycle: `crates/smith-core/src/agent.rs -> crates/smith-core/src/subagent.rs -> crates/smith-core/src/agent.rs`
- 2-file cycle: `crates/smith-core/src/event.rs -> crates/smith-core/src/tool.rs -> crates/smith-core/src/event.rs`
- 2-file cycle: `crates/smith-cli/src/main.rs -> crates/smith-config/src/lib.rs -> crates/smith-cli/src/main.rs`
- 3-file cycle: `crates/smith-mcp/src/client.rs -> crates/smith-mcp/src/http.rs -> crates/smith-mcp/src/transport.rs -> crates/smith-mcp/src/client.rs`

## Hyperedges (group relationships)
- **The authorization ladder a tool call must survive** — docs_authorization_name_interception, docs_authorization_plan_gated, docs_hooks_pretooluse, docs_authorization_needs_prompt, claude_permission_model, claude_scratch_scoped, claude_turn_checkpoints [EXTRACTED 1.00]
- **Eight-crate workspace flowing one way toward smith-core** — claude_smith_core, claude_smith_provider, claude_smith_tools, claude_smith_mcp, claude_smith_store, claude_smith_config, claude_smith_tui, claude_smith_cli [EXTRACTED 1.00]
- **Where the ten acceptance criteria are actually checked** — docs_release_acceptance_criteria, _github_workflows_ci_acceptance, docs_wave7_release_pty_harness, docs_design_system_80x24_contract, evals_readme_behavioral_evals, evals_readme_edit_ambiguity, evals_readme_injection_obedience [EXTRACTED 1.00]

## Communities (137 total, 8 thin omitted)

### Community 0 - "TUI App Event Tests"
Cohesion: 0.05
Nodes (112): a_bare_rewind_asks_for_a_plan_and_never_applies_one(), a_failed_child_makes_the_group_report_failure(), a_grouped_card_stays_running_until_its_last_child_lands(), a_malformed_mcp_command_explains_itself_instead_of_acting(), a_remapped_key_takes_effect_and_the_old_one_goes_inert(), a_rewind_report_lands_in_the_transcript_caveats_and_all(), a_search_after_something_else_starts_a_new_card(), a_stale_arm_expires_instead_of_pairing_with_a_later_press() (+104 more)

### Community 1 - "Layered Config & Memory Imports"
Cohesion: 0.06
Nodes (86): AsRef, config_dir(), config_path(), ConfigError, load_layered_reads_the_project_file_from_disk(), project_config_path(), Error, Path (+78 more)

### Community 2 - "Web Fetch Tool"
Cohesion: 0.06
Nodes (71): a_blocked_url_never_reaches_the_fetcher(), a_body_capped_on_the_wire_is_reported_as_truncated_too(), a_link_with_no_visible_text_is_dropped_rather_than_left_empty(), a_missing_or_unparseable_url_is_rejected_before_any_request(), a_non_2xx_status_is_an_error_not_an_empty_page(), a_page_cannot_forge_the_end_marker(), a_page_that_fits_is_reported_as_complete(), a_redirect_into_the_private_range_is_caught_mid_chain() (+63 more)

### Community 3 - "Tool Hooks Engine"
Cohesion: 0.08
Nodes (71): a_deny_reaches_the_model_quoted_and_actionable(), a_failing_post_hook_leaves_the_result_intact_and_warns(), a_hanging_hook_is_killed_at_the_timeout_and_denies(), a_hanging_post_hook_times_out_but_keeps_the_result(), a_hook_only_runs_for_the_tools_its_matcher_names(), a_hook_that_never_reads_stdin_still_finishes(), a_matcher_names_exact_tools_and_nothing_else(), a_missing_command_fails_closed_with_a_visible_notice() (+63 more)

### Community 4 - "Turn Checkpoints & Rewind"
Cohesion: 0.07
Nodes (60): a_blocked_report_says_nothing_was_changed_and_names_the_escape_hatch(), a_preview_says_nothing_has_changed_and_how_to_apply(), a_session_with_no_checkpoints_says_so_instead_of_rendering_an_empty_plan(), an_applied_report_reads_as_past_tense_and_offers_no_confirmation(), an_uncovered_tool_is_called_out_in_plain_words(), ConflictKind, report(), RewindConflict (+52 more)

### Community 5 - "Syntax Highlighting"
Cohesion: 0.10
Nodes (61): a_large_block_is_linear_and_finishes(), c_like(), c_like_step(), classify(), classify_with(), CLike, consume_newline(), force_progress() (+53 more)

### Community 6 - "Headless Mode"
Cohesion: 0.07
Nodes (65): a_cancelled_turn_is_a_failure_even_though_it_arrives_as_a_completion(), a_capped_turn_exits_with_the_limit_code_not_the_failure_one(), a_closed_event_channel_ends_the_run_as_a_failure(), a_failed_turn_exits_non_zero_and_keeps_stdout_clean(), a_prompt_and_stdin_combine_with_the_instruction_first(), a_reply_that_already_ends_in_a_newline_is_not_given_another(), a_successful_turn_prints_prose_on_stdout_and_exits_zero(), a_tool_in_allowed_tools_is_allowed_for_the_rest_of_the_session() (+57 more)

### Community 7 - "TUI Rendering"
Cohesion: 0.07
Nodes (70): a_card_with_no_start_time_falls_back_to_the_global_counter(), a_modal_takes_the_screen_back_from_an_open_overlay(), a_selected_card_is_marked_and_raised_but_others_are_untouched(), a_tiny_terminal_still_gets_a_prompt_and_a_status_bar(), an_estimated_context_says_so_in_words_as_well_as_a_tilde(), an_overlay_stays_inside_the_frame_at_80x24(), an_overlay_table_draws_its_header_and_rows_inside_a_titled_box(), app_with_context() (+62 more)

### Community 8 - "Filesystem Tool Safety"
Cohesion: 0.12
Nodes (67): a_change_that_is_reverted_leaves_the_read_valid(), a_clipped_line_does_not_count_as_having_read_the_file(), a_delegate_keeps_the_sessions_on_disk_identity(), a_failing_edit_leaves_the_file_byte_identical(), a_file_cannot_forge_the_closing_marker_to_escape_the_fence(), a_file_containing_an_injection_attempt_is_reported_and_fenced(), a_file_read_file_can_only_describe_still_counts_as_read(), a_file_that_changed_since_it_was_read_is_refused_until_it_is_read_again() (+59 more)

### Community 9 - "Agent Test Doubles"
Cohesion: 0.07
Nodes (28): Barrier, ArgumentRecordingTools, BarrierTools, CancelOnFirstCallTools, CancelOnFirstReadTools, CannedHook, CountingTools, LeakySecretTool (+20 more)

### Community 10 - "Session Store (SQLite)"
Cohesion: 0.08
Nodes (40): Connection, a_fork_does_not_inherit_the_original_spend(), a_fresh_database_lands_on_the_current_version_and_reopening_is_a_no_op(), a_recorded_turn_round_trips_with_every_token_class(), a_resumed_session_reports_what_it_cost_not_what_it_would_cost_today(), a_version_zero_database_migrates_without_losing_anything(), a_version_zero_database_that_already_has_the_goal_column_still_migrates(), column_exists() (+32 more)

### Community 11 - "Agent Public Surface"
Cohesion: 0.05
Nodes (30): a_definition_may_not_shadow_the_general_purpose_child(), a_queued_note_rides_the_next_user_message_instead_of_becoming_one(), Agent, clearing_goal_reverts_to_base_system(), CompactionConfig, effective_system_appends_injected_context_after_the_base_prompt(), effective_system_folds_goal_into_base_system(), effective_system_recomputes_context_on_every_call() (+22 more)

### Community 12 - "Agent Tool Dispatch"
Cohesion: 0.09
Nodes (22): a_tag_split_across_deltas_is_still_recognised(), align_arguments(), consume_stream(), errors(), normalize_ident(), parse_tool_call_envelope(), progress_lines(), ReasoningFilter (+14 more)

### Community 13 - "Subagent Behavior Tests"
Cohesion: 0.09
Nodes (52): a_write_is_checkpointed_and_rewind_puts_the_original_bytes_back(), next_rewind(), a_child_cannot_use_a_tool_outside_its_allowed_set(), a_child_does_not_inherit_the_parents_system_prompt_but_does_inherit_its_context(), a_child_that_never_stops_calling_tools_is_capped_and_still_answers_the_parent(), a_childs_tokens_are_billed_to_the_parents_turn(), a_definition_shapes_the_child_that_actually_runs(), a_mutating_call_splits_the_round_and_runs_on_its_own() (+44 more)

### Community 14 - "CLI Flags & Entry Point"
Cohesion: 0.07
Nodes (46): a_bad_theme_name_or_colour_is_a_usage_error_not_a_fallback(), a_blank_override_does_not_suppress_the_provisioned_browser(), a_per_token_override_survives_into_the_resolved_theme(), a_provisioned_browser_is_handed_to_smith_tools_through_the_env_var(), an_override_the_user_set_is_never_replaced(), browser_path_to_export(), Cli, color_enabled() (+38 more)

### Community 15 - "Chat Line & Activity State"
Cohesion: 0.06
Nodes (27): activity_label(), ActivityStatus, ChatLine, format_thought(), group_target(), GroupedCall, IdleHint, LineStamp (+19 more)

### Community 16 - "App State & Input Actions"
Cohesion: 0.07
Nodes (19): Action, ResourceStats, Option, App, app_with_cards(), arrows_walk_between_cards_and_clamp_at_the_ends(), card_focus_leaves_typing_alone(), ChatRole (+11 more)

### Community 17 - "Grep Tool"
Cohesion: 0.08
Nodes (48): a_binary_file_is_counted_but_never_dumped(), a_gitignored_file_is_not_searched(), a_very_long_line_is_clipped_with_a_marker(), an_invalid_regex_is_an_error_not_an_empty_result(), before_and_after_context_can_differ(), case_insensitive_is_opt_in(), context_lines_are_returned_and_marked(), count_mode_reports_per_file_totals() (+40 more)

### Community 18 - "Bash Shell Tool"
Cohesion: 0.07
Nodes (42): a_flood_of_output_does_not_become_a_flood_of_events(), cancellation_kills_a_long_running_command(), cancellation_kills_grandchildren_too(), cancellation_still_returns_what_had_already_been_produced(), combine(), ctx(), ctx_with_progress(), drain_progress() (+34 more)

### Community 19 - "Theme & WCAG Contrast"
Cohesion: 0.08
Nodes (38): a_bad_override_is_an_error_naming_the_token(), all_presets(), ansi_surfaces_are_three_distinct_elevation_levels(), ansi_text_levels_stay_readable_against_the_surfaces(), contrast_ratio(), contrast_ratio_matches_the_wcag_reference_values(), every_preset_meets_wcag_aa(), every_preset_token_is_measurable() (+30 more)

### Community 20 - "Retry, Compaction & Usage"
Cohesion: 0.10
Nodes (48): BoxFuture, a_bad_request_is_not_retried(), a_declared_path_is_snapshotted_on_both_sides_of_the_call(), a_failed_summarisation_leaves_history_intact(), a_long_turn_auto_compacts_and_still_answers(), a_mutating_tool_that_declares_no_paths_is_recorded_as_uncovered(), a_rate_limited_request_is_retried_and_the_turn_then_succeeds(), a_read_only_tool_is_not_recorded_as_uncovered() (+40 more)

### Community 21 - "Tool-Name Recovery & Reasoning Tags"
Cohesion: 0.07
Nodes (51): a_child_that_reported_nothing_at_all_is_an_error_that_says_why(), a_differently_spelled_tool_name_is_recovered_and_executed(), a_finished_child_reports_its_text_and_nothing_else_when_it_ran_cleanly(), a_fragment_that_is_not_a_whole_segment_never_matches(), a_merely_similar_name_is_not_accepted(), a_partial_report_is_returned_with_a_note_rather_than_thrown_away(), a_stray_closing_tag_is_removed_without_eating_the_text_around_it(), a_think_block_is_removed_from_the_text_channel() (+43 more)

### Community 22 - "MCP JSON-RPC Client"
Cohesion: 0.11
Nodes (34): AtomicU64, an_entry_with_neither_command_nor_url_is_refused_by_name(), capabilities_read_only_what_the_server_actually_advertised(), connect_fake(), flatten_content(), lists_and_calls_tools_against_a_real_child_process(), lists_and_reads_resources_including_binary_ones(), lists_and_renders_prompts() (+26 more)

### Community 23 - "Doctor Diagnostics"
Cohesion: 0.09
Nodes (43): a_configured_browser_that_does_not_exist_warns_with_a_fix(), a_configured_provider_and_model_are_reported_together(), a_missing_key_is_a_failure_that_names_the_variable_to_set(), a_missing_ollama_is_explained_rather_than_installed(), a_project_with_no_history_yet_is_reported_as_fine(), a_valid_project_config_is_listed_as_a_layer_in_effect(), a_working_mcp_server_is_reported_with_the_tools_it_publishes(), a_writable_project_dir_and_an_existing_db_both_pass() (+35 more)

### Community 24 - "TUI Draw & Render Caching"
Cohesion: 0.10
Nodes (49): a_grouped_search_card_shows_one_row_per_query_under_one_header(), a_pending_quit_announces_itself_in_the_status_bar(), a_settled_transcript_parses_no_markdown_at_all_on_a_redraw(), app_for_input_tests(), app_with_long_permission_request(), buffer_of(), caching_does_not_change_a_single_cell(), caching_does_not_change_a_single_cell_while_streaming() (+41 more)

### Community 25 - "Tool Schema Validation"
Cohesion: 0.09
Nodes (32): a_long_value_is_clipped_in_the_echo(), a_pathologically_nested_schema_terminates(), an_echoed_value_stays_inside_its_quotes(), check(), check_bound(), check_numeric_bounds(), child(), declared_types() (+24 more)

### Community 26 - "Custom Slash Commands"
Cohesion: 0.10
Nodes (31): a_bare_dollar_is_prose_but_a_dollar_digit_is_always_a_placeholder(), a_body_less_file_is_refused_rather_than_submitting_nothing(), a_command_may_not_take_a_builtin_name(), a_directory_becomes_a_namespace_segment(), a_double_dollar_escapes_to_a_literal_dollar(), a_missing_positional_argument_refuses_the_whole_expansion(), an_unusable_name_is_refused_rather_than_loaded_unreachable(), arguments_and_positionals_compose() (+23 more)

### Community 27 - "Extension Discovery & Jail"
Cohesion: 0.13
Nodes (36): a_file_with_no_front_matter_is_all_body(), a_missing_directory_produces_nothing_and_no_complaint(), a_symlink_out_of_the_project_is_refused_and_reported(), a_symlinked_directory_inside_the_project_is_followed(), an_empty_front_matter_value_reads_as_absent(), an_oversized_file_is_truncated_with_a_notice_rather_than_dropped(), check_jail(), first_line() (+28 more)

### Community 28 - "Hook Enforcement in Agent"
Cohesion: 0.13
Nodes (38): a_hook_rewrite_that_changes_the_tool_is_refused_before_dispatch(), a_hook_rewrite_the_schema_rejects_never_reaches_the_tool(), a_hook_still_runs_when_the_policy_would_skip_the_prompt(), a_post_tool_use_hook_annotates_the_result_the_model_reads(), a_pre_tool_use_hook_denial_reaches_the_model_and_stops_the_tool(), a_pre_tool_use_hook_rewrites_the_arguments_the_tool_receives(), a_scratch_scoped_call_is_still_gated_when_nobody_is_watching(), a_scratch_scoped_call_skips_the_permission_prompt_under_ask() (+30 more)

### Community 29 - "Browser Runtime Provisioning"
Cohesion: 0.07
Nodes (29): a_normal_entry_lands_under_the_root(), a_provisioned_browser_beats_one_on_the_path(), a_system_browser_is_used_when_nothing_else_is_configured(), an_entry_that_would_escape_the_root_is_refused(), an_env_override_beats_the_provisioned_browser(), blank_settings_fall_through_rather_than_resolving_to_nothing(), BrowserSource, find_browser() (+21 more)

### Community 30 - "System Prompt Assembly"
Cohesion: 0.10
Nodes (37): a_goal_still_lands_last_when_there_is_no_memory(), a_persona_leaves_the_cacheable_prefix_byte_identical(), a_persona_sits_ahead_of_the_environment_memory_and_goal(), a_replacing_persona_drops_the_style_and_never_the_invariants(), approved_plan_prompt_embeds_plan_excerpt(), assistant(), build_approved_plan_prompt(), build_loop_task_prompt() (+29 more)

### Community 31 - "MCP Tool Bridge"
Cohesion: 0.10
Nodes (20): an_ambiguous_read_is_refused_rather_than_broadcast(), ctx(), exposes_a_namespaced_name_but_calls_the_remote_one(), ListMcpResourcesTool, McpToolAdapter, namespaced_tool_name(), ReadMcpResourceTool, resource_contents_are_fenced_and_a_missing_server_is_named() (+12 more)

### Community 32 - "Markdown Rendering"
Cohesion: 0.11
Nodes (30): a_block_inside_a_blockquote_keeps_its_quote_prefix(), a_block_nested_in_a_list_keeps_one_line_per_source_line(), a_fenced_block_with_a_known_language_gets_token_colours(), a_fenced_block_with_an_unknown_language_is_left_alone(), a_multiline_string_stays_coloured_on_its_second_line(), ascii_markdown_glyphs(), fence_marker(), highlight_code_blocks() (+22 more)

### Community 33 - "Input Box Component"
Cohesion: 0.10
Nodes (24): Block, backspace_deletes_the_whole_multibyte_char(), box_grows_with_content_then_stops_at_the_cap(), caret_moves_left_and_inserts_mid_string(), caret_steps_by_character_not_byte_over_accents(), ctrl_w_deletes_the_word_before_the_caret(), home_and_end_move_within_the_current_line(), input() (+16 more)

### Community 34 - "Diff Rendering"
Cohesion: 0.08
Nodes (9): render_diff(), render_diff_handles_identical_strings(), render_diff_marks_additions_and_deletions(), Line, Vec, Style, the_ascii_theme_answers_only_in_ascii(), Theme (+1 more)

### Community 35 - "Keymap & Bindings"
Cohesion: 0.10
Nodes (27): a_char_binding_ignores_the_case_the_terminal_reports(), an_override_moves_a_binding_and_the_old_key_stops_firing(), an_unknown_action_names_itself_and_the_valid_ones(), an_unparseable_key_is_an_error_not_a_silent_default(), Binding, binding_two_actions_to_one_key_is_refused(), KeyAction, KeyMap (+19 more)

### Community 36 - "Node Runtime & 9Router"
Cohesion: 0.13
Nodes (30): a_wellformed_tar_extracts_where_it_says(), ensure_ninerouter_running(), ensure_with_nothing_installed_errs_naming_setup(), extract_tar_gz(), extract_zip(), find_node(), live_node_provisioning_installs_and_probes(), ninerouter_cli() (+22 more)

### Community 37 - "Bing Search Backend"
Cohesion: 0.09
Nodes (23): decodes_entities_in_titles_and_snippets(), element(), feed(), item(), judge_relevance(), language_of(), looks_poisoned(), malformed_or_truncated_markup_stops_cleanly() (+15 more)

### Community 38 - "Read-Before-Overwrite Gate"
Cohesion: 0.09
Nodes (16): a_new_hash_discards_the_coverage_recorded_against_the_old_one(), a_read_in_one_session_does_not_unlock_a_write_in_another(), apply_and_diff(), concurrent_reads_of_many_files_all_land(), concurrent_reads_of_one_file_add_up_without_racing(), coverage_ranges_merge_no_matter_what_order_they_arrive_in(), MultiEditTool, ReadFileTool (+8 more)

### Community 39 - "Key Handling & Mid-Turn Queue"
Cohesion: 0.07
Nodes (24): a_message_typed_mid_turn_is_queued_rather_than_refused(), a_slash_command_typed_mid_turn_still_runs_now(), any_other_key_disarms_a_pending_quit(), ctrl_c(), ctrl_o_is_inert_with_nothing_to_select(), history_stops_at_the_oldest_entry_instead_of_wrapping(), Modal, paste_keeps_newlines_instead_of_submitting_at_the_first_one() (+16 more)

### Community 40 - "Anthropic Provider"
Cohesion: 0.09
Nodes (28): ProviderCapabilities, Default, absent_cache_fields_stay_zero(), AnthropicProvider, build_request_body(), builds_request_body_with_system_and_tools(), cache_tokens_are_captured_alongside_input_tokens(), capabilities_are_reachable_through_the_trait() (+20 more)

### Community 41 - "MCP HTTP Transports"
Cohesion: 0.15
Nodes (31): an_endpoint_that_is_neither_reports_both_failures(), an_unqualified_url_falls_back_from_streamable_http_to_sse(), base_headers(), configured_headers_are_sent_on_every_request(), handle(), http_client(), HttpTransport, Mode (+23 more)

### Community 42 - "Setup Wizard"
Cohesion: 0.16
Nodes (29): ColorfulTheme, apply_optional(), browser_summary(), ensure_ollama_running(), key_status(), ollama_binary_present(), ollama_reachable(), permission_summary() (+21 more)

### Community 43 - "Skills Loading"
Cohesion: 0.14
Nodes (25): a_body_less_skill_is_refused(), a_description_less_skill_is_refused_because_the_model_selects_on_it(), a_directory_without_a_skill_file_is_not_a_problem(), a_front_matter_name_disagreeing_with_the_directory_is_reported(), a_global_skill_does_not_promise_files_read_file_cannot_reach(), an_overlong_description_is_clipped_so_the_index_stays_one_line_each(), clip(), discover() (+17 more)

### Community 44 - "Config Round-Trip"
Cohesion: 0.11
Nodes (26): a_config_with_a_theme_and_everything_after_it_round_trips(), a_config_with_both_a_runtime_and_mcp_servers_round_trips(), a_project_may_point_at_its_own_provisioned_browser(), a_project_may_restyle_one_token_without_restating_the_palette(), a_project_may_set_its_own_key(), a_project_overrides_only_what_it_states(), an_empty_project_file_changes_nothing(), an_unstated_runtime_section_keeps_the_provisioned_browser() (+18 more)

### Community 45 - "Edit File Tool"
Cohesion: 0.16
Nodes (18): ToolContext, EditFileTool, field_bool(), field_str(), jail_root(), lexical_normalize(), path_is_inside(), real_path() (+10 more)

### Community 46 - "OpenAI Context Windows"
Cohesion: 0.10
Nodes (30): CompletionRequest, Option, a_known_window_replaces_the_conservative_guess(), an_empty_or_alien_payload_parses_to_nothing_rather_than_panicking(), an_unprobed_model_keeps_the_conservative_default(), build_request_body(), builds_request_body_with_system_and_tools(), cached_prompt_tokens_are_split_out_of_the_input_count() (+22 more)

### Community 47 - "Provider Fallback Chain"
Cohesion: 0.13
Nodes (25): a_402_advances_and_reports_a_retryable_handover(), a_plain_429_does_not_advance(), a_retry_after_beyond_the_cap_advances_instead_of_stranding_the_turn(), a_success_resets_the_strike_count(), a_turn_survives_a_quota_death_and_is_accounted_to_the_survivor(), cancellation_neither_strikes_nor_advances(), entry(), FallbackEntry (+17 more)

### Community 48 - "Transcript Memoization"
Cohesion: 0.17
Nodes (26): a_failed_turn_only_invalidates_the_cards_it_actually_touched(), a_running_card_is_never_served_from_the_memo(), a_second_sync_with_nothing_changed_rebuilds_nothing(), a_window_is_the_same_slice_the_whole_document_would_give(), borrow_line(), changing_the_theme_invalidates_everything(), changing_the_width_invalidates_everything(), Entry (+18 more)

### Community 49 - "Self-Update Checks"
Cohesion: 0.15
Nodes (30): Asset, auto_update(), cache_path(), check_for_update(), CheckCache, current_version(), extract_binary(), http_client() (+22 more)

### Community 50 - "MCP Registry & Liveness"
Cohesion: 0.16
Nodes (23): McpServerConfig, a_remote_schema_is_republished_verbatim_even_when_it_is_malformed(), a_server_that_dies_mid_session_fails_fast_instead_of_hanging(), capabilities_gate_the_optional_methods(), malformed_jsonrpc_lines_are_skipped_rather_than_killing_the_session(), python3_available(), stdio_spec(), a_broken_server_does_not_take_the_others_down() (+15 more)

### Community 51 - "Search Failure Classification"
Cohesion: 0.09
Nodes (22): a_good_bing_feed_yields_results_truncated_to_the_limit(), a_misconfigured_searxng_is_reported_as_something_to_fix(), a_transient_block_reads_as_temporary_and_invites_a_retry(), a_weakly_matching_bing_feed_carries_its_verdict(), an_empty_or_poisoned_bing_response_is_transient(), an_empty_result_set_from_a_working_backend_still_reads_as_no_results(), backoff_delay(), backoff_doubles_and_stays_inside_the_jitter_window() (+14 more)

### Community 52 - "Orchestrator Provider Setup"
Cohesion: 0.16
Nodes (25): a_chain_entry_without_its_key_errs_naming_the_key(), a_chain_naming_an_unknown_provider_errs_naming_it(), a_configured_chain_wraps_and_skips_the_primary_itself(), a_missing_ninerouter_key_errs_naming_the_dashboard(), a_missing_openrouter_key_errs_naming_the_env_var_and_the_free_key_url(), a_plain_turn_reaches_the_provider_and_comes_back_as_json(), a_prompt_composed_from_stdin_arrives_intact_at_the_provider(), a_provider_failure_exits_non_zero() (+17 more)

### Community 53 - "Runtime Asset Download"
Cohesion: 0.13
Nodes (26): AssetSource, content_length_header(), Downloaded, FakeSource, HttpAssetSource, Integrity, md5_base64_of(), parse_goog_hash_md5() (+18 more)

### Community 54 - "Chromium Page Fetcher"
Cohesion: 0.13
Nodes (30): blank_override_falls_through_to_the_candidates(), browser_path(), candidates_are_probed_in_order(), chromium_args(), chromium_args_always_use_a_throwaway_profile(), chromium_args_end_with_dump_dom_then_the_url(), dump_dom(), explicit_browser_path_is_honoured_even_when_missing() (+22 more)

### Community 55 - "Doctor Report Formatting"
Cohesion: 0.16
Nodes (23): a_multi_line_remedy_stays_indented_under_its_check(), an_all_ok_report_exits_zero(), an_all_ok_report_says_so(), any_failure_exits_non_zero(), Check, check_config_layers(), check_mcp_server(), check_ninerouter() (+15 more)

### Community 56 - "Context Carry-Over & Todos"
Cohesion: 0.12
Nodes (22): carry_over(), carry_over_keeps_every_open_todo_and_drops_completed_ones(), carry_over_recovers_todos_from_history_when_the_live_list_is_empty(), CarryOver, compaction_split(), ContextUsage, estimate_message_tokens(), estimate_tokens() (+14 more)

### Community 57 - "Token Usage Accounting"
Cohesion: 0.09
Nodes (17): collect_ids(), fingerprint(), StreamOutcome, TurnAccounting, write_tasks_call(), estimate_messages_tokens(), add_accumulates_every_field(), add_saturates_instead_of_wrapping() (+9 more)

### Community 58 - "OpenAI Flavors & Catalogues"
Cohesion: 0.15
Nodes (16): Flavor, flavor_selects_the_catalogue(), live_ollama_reports_a_real_window(), live_openrouter_lists_free_tool_models(), ollama_reports_the_conservative_local_default_when_nothing_is_known(), OpenAiProvider, parse_num_ctx(), Arc (+8 more)

### Community 59 - "Orchestrator State & Persistence"
Cohesion: 0.13
Nodes (22): a_turn_persists_its_cost_and_a_resumed_run_starts_from_it(), OrchestratorOptions, OrchestratorState, persist_turn(), Persistence, register_mcp_tools(), CancellationToken, Mutex (+14 more)

### Community 60 - "Search Result Parsing"
Cohesion: 0.21
Nodes (15): Relevance, classify_bing(), parse_tavily_response(), CancellationToken, Client, HashMap, Mutex, Result (+7 more)

### Community 61 - "MCP Transport Abstraction"
Cohesion: 0.10
Nodes (16): ChildStdin, ChildStdout, a_child_that_exits_closes_the_incoming_channel(), Child, Incoming, IncomingSender, Mutex, Option (+8 more)

### Community 62 - "Panel Component"
Cohesion: 0.21
Nodes (25): bordered_row(), bordered_row_pads_symmetrically(), bordered_row_truncates_content_wider_than_the_box(), box_corners_land_in_the_rendered_buffer(), fill_line(), fill_line_does_not_truncate_when_already_wide(), fill_line_pads_to_width_with_bg(), inset() (+17 more)

### Community 63 - "Skill Tool"
Cohesion: 0.15
Nodes (17): tool_defs_order_is_stable_across_registries(), a_missing_name_is_refused_with_the_list(), an_unknown_skill_is_refused_with_the_list_rather_than_guessed_at(), call(), each_skill_costs_one_line_of_the_definition(), it_is_read_only_and_writes_nothing(), lookup_tolerates_case_and_surrounding_space(), CancellationToken (+9 more)

### Community 64 - "Slash Command Completion"
Cohesion: 0.14
Nodes (17): a_builtin_always_sorts_ahead_of_a_custom_command_sharing_its_prefix(), a_custom_command_cannot_shadow_a_builtin(), a_custom_command_joins_the_suggestion_list_and_is_marked_as_custom(), a_namespaced_command_completes_with_its_colon(), bare_slash_lists_all(), builtin_names(), is_builtin(), no_custom_commands_leaves_the_builtins_exactly_as_they_were() (+9 more)

### Community 65 - "AgentEvent Serialization"
Cohesion: 0.12
Nodes (16): task(), AgentPhase, LoopStopReason, PermissionRequest, progress_reporter_survives_a_dropped_receiver(), progress_reporter_tags_every_line_with_its_call_id(), ProgressReporter, Into (+8 more)

### Community 66 - "Subagent Definitions & Limits"
Cohesion: 0.14
Nodes (22): a_child_asking_permission_is_refused_rather_than_prompting_anyone(), a_child_asking_the_user_a_question_gets_a_refusal_not_an_invented_answer(), a_definition_asking_for_a_mutating_tool_does_not_get_it(), a_definition_parses_front_matter_and_body(), a_definition_without_a_name_or_description_is_rejected(), a_forbidden_tool_declares_nothing_to_snapshot(), a_forbidden_tool_never_reports_a_class_that_would_open_a_permission_prompt(), a_multi_word_name_is_rejected_because_the_model_passes_it_verbatim() (+14 more)

### Community 67 - "Personas & Output Styles"
Cohesion: 0.17
Nodes (19): a_global_persona_is_found_by_name(), a_project_persona_of_the_same_name_wins(), a_traversing_name_is_refused_as_a_name_not_resolved_as_a_path(), an_empty_persona_file_is_an_error_rather_than_a_no_op(), an_explicitly_named_missing_persona_is_an_error_but_the_default_is_not(), an_unknown_mode_is_an_error_rather_than_a_silent_augment(), load(), load_in() (+11 more)

### Community 68 - "Text Wrapping & Char Width"
Cohesion: 0.19
Nodes (22): char_width(), counts_wide_characters_as_two_cells(), hard_breaks_a_word_longer_than_the_width(), keeps_explicit_newlines_as_row_breaks(), measures_accents_by_display_cell_not_byte(), merges_adjacent_chars_that_share_a_style(), plain(), preserves_span_styles_across_a_break() (+14 more)

### Community 69 - "Retry Policy"
Cohesion: 0.17
Nodes (17): retry_policy_for_chain(), a_non_retryable_error_is_never_delayed_however_early_the_attempt(), a_retry_after_beyond_the_cap_gives_up_rather_than_parking_the_session(), a_retryable_error_gets_a_delay_within_the_jitter_window(), api(), backoff_doubles_from_the_base_delay_and_stops_at_the_cap(), default_schedule_stays_inside_an_interactive_budget(), equal_jitter() (+9 more)

### Community 70 - "Tool Trait & Permission Class"
Cohesion: 0.13
Nodes (8): a_context_built_outside_a_tool_call_has_no_progress_channel(), Into, Option, PathBuf, Self, String, scratch_dir_is_session_scoped_and_under_the_project_smith_dir(), with_progress_scopes_a_copy_to_one_call_and_leaves_the_original_alone()

### Community 71 - "File @-Completion"
Cohesion: 0.13
Nodes (10): accept_file(), file_suggestions(), file_token(), index_files(), indexing_skips_directories_and_respects_gitignore(), names(), Option, Path (+2 more)

### Community 72 - "Log Buffer"
Cohesion: 0.19
Nodes (14): a_poisoned_lock_drops_the_line_instead_of_panicking(), clones_share_one_buffer(), line(), LogBuffer, LogLevel, LogLine, Arc, Mutex (+6 more)

### Community 73 - "Subagent & Interception Docs"
Cohesion: 0.11
Nodes (20): API key redaction choke point, Tool interception pattern (contributor summary), Partial subagent report (finish_subagent), Shared subagent tool-call pool per turn, Subagents (the task tool), Tool interception pattern, The concurrent read-only path (run_concurrent_group), Finding C: three tools were never schema-checked (+12 more)

### Community 74 - "Permission Model Docs"
Cohesion: 0.11
Nodes (20): Ctrl+L diagnostics panel and tracing subscriber, Answers pinned to the user's language, PermissionClass x PermissionPolicy model, Plan gate (plan_gated), subagent::RestrictedTools, Per-session scratch directory, Tool::scratch_scoped vouching, A read-only tool is not a harmless tool (+12 more)

### Community 75 - "Browser Install Integrity"
Cohesion: 0.28
Nodes (18): a_broken_existing_install_is_replaced_not_trusted(), a_checksum_mismatch_refuses_the_archive_and_discards_it(), a_hostile_version_cannot_escape_the_install_root(), a_second_run_reuses_the_existing_install(), an_archive_whose_binary_does_not_run_installs_nothing(), an_archive_with_no_published_checksum_installs_and_says_so(), an_unexpected_archive_layout_fails_loudly_and_installs_nothing(), each_build_gets_its_own_directory() (+10 more)

### Community 76 - "Tool Registry Validation"
Cohesion: 0.28
Nodes (15): a_call_that_contradicts_the_schema_never_reaches_the_tool(), a_malformed_remote_schema_cannot_disable_validation_for_a_builtin(), a_namespaced_mcp_tool_registers_alongside_the_builtin(), a_valid_call_reaches_the_tool_byte_identical(), a_write_that_escapes_the_project_declares_nothing_to_snapshot(), an_mcp_tool_cannot_displace_the_builtin_read_file(), an_unknown_argument_is_forwarded_rather_than_refused(), builtin_tools_have_no_colliding_names() (+7 more)

### Community 77 - "Architecture Overview Docs"
Cohesion: 0.12
Nodes (19): Action / AgentEvent loop (contributor summary), Env API keys outrank saved config, Smith ASCII banner (CLI AGENT PLATAFORM), Action / AgentEvent loop, ReasoningFilter, smith-cli crate, smith-core crate, smith-mcp crate (+11 more)

### Community 78 - "Prompt Injection Detection"
Cohesion: 0.16
Nodes (11): credentials_are_named_in_a_finding_but_do_not_cause_one(), Finding, one_finding_per_line_even_when_several_patterns_match(), reasons(), String, Vec, scan(), scanning_multibyte_text_neither_panics_nor_misreports_lines() (+3 more)

### Community 79 - "Behavioral Evals Docs"
Cohesion: 0.14
Nodes (18): acceptance job (criterion 8 smoke), fake_provider.py fixture, Project config layer used by CI acceptance, AgentEvent adjacently-tagged serialization, --allowed-tools, Behavioral evals (evals/), Headless mode (-p / --output-format), Provider/model switching mid-session (/model) (+10 more)

### Community 80 - "Secret Redaction"
Cohesion: 0.19
Nodes (14): Cow, redactor_for(), handles_multiple_distinct_secrets(), ignores_short_values_that_would_corrupt_output(), leaves_clean_text_untouched_and_unallocated(), overlapping_secrets_are_fully_removed(), Redactor, redacts_every_occurrence_not_just_the_first() (+6 more)

### Community 81 - "SearXNG Backend"
Cohesion: 0.17
Nodes (13): a_trailing_slash_does_not_change_the_endpoint(), builds_a_json_search_url(), drops_rows_with_no_url_or_no_title(), honours_a_subpath_deployment(), parse_response(), parses_results_with_snippets_and_publication_dates(), Client, Result (+5 more)

### Community 82 - "Context Gauge Component"
Cohesion: 0.17
Nodes (11): an_estimate_is_never_rendered_as_a_measurement(), compact(), context_gauge(), fill_style(), label(), ratio(), String, Style (+3 more)

### Community 83 - "Changelog Bug Fixes"
Cohesion: 0.14
Nodes (17): Every tool_use answered by a tool_result, Fix: context gauge hid an over-full window, Mid-turn message delivered at the next round boundary, Fix: Ollama context window assumed 4096, Fix: every Ollama session recorded as OpenAI, Fix: 'Text file busy' verifying a downloaded browser, Release 0.2.0 (2026-08-06), FallbackProvider (+9 more)

### Community 84 - "Provider Traits & Errors"
Cohesion: 0.17
Nodes (8): api(), default_capabilities_are_conservative(), ProviderError, retry_hint(), Duration, Option, Self, String

### Community 85 - "Subagent Tool Restriction"
Cohesion: 0.22
Nodes (10): child_system_prompt(), RestrictedTools, Arc, BTreeSet, CancellationToken, PathBuf, Self, String (+2 more)

### Community 86 - "Write Staging"
Cohesion: 0.30
Nodes (16): apply_staged(), ctx(), discard_removes_staged_file(), discard_staged(), flattens_absolute_paths_safely(), mirrors_relative_paths_under_session(), normalize_rel(), Path (+8 more)

### Community 87 - "Runtime Platform Detection"
Cohesion: 0.18
Nodes (11): a_hostile_zip_entry_is_refused_before_it_is_written(), a_manifest_missing_this_platform_says_so_instead_of_guessing_a_url(), CftBuild, CftPlatform, extract_zip(), live_resume_continues_a_partial_download(), maps_every_platform_chrome_for_testing_publishes(), parse_manifest() (+3 more)

### Community 88 - "Subagent Directory Scan"
Cohesion: 0.25
Nodes (14): a_broken_definition_costs_only_itself(), a_duplicate_name_is_reported_rather_than_silently_shadowing(), a_missing_directory_is_not_a_problem(), agents_dir(), every_markdown_definition_is_loaded_and_nothing_else_is(), load(), load_from(), Option (+6 more)

### Community 89 - "Tool Trait Registration"
Cohesion: 0.22
Nodes (9): Send, Sync, Tool, duplicate_name_is_rejected_instead_of_overwriting(), EchoTool, Arc, HashMap, Option (+1 more)

### Community 90 - "MCP Design Docs"
Cohesion: 0.13
Nodes (15): mcp__{server}__{tool} namespacing, Staging directory for mutating file tools, Release 0.1.0, MCP prompts are user-invoked, never model-reachable, MCP resources are a tool, not context, MCP transports and one liveness rule, Read-before-overwrite gate, fs_tools::ReadSet (+7 more)

### Community 91 - "Web Search Design Docs"
Cohesion: 0.14
Nodes (15): tool_defs() sorted by name for prefix caching, Three tool-registration entry points, Bing over RSS backend, Headless Chromium page fetcher, bing::judge_relevance (Poisoned / Weak / Good), language::detect (query-language market selection), Personas / output styles, PROMPT_INVARIANTS vs PROMPT_STYLE split (+7 more)

### Community 92 - "MCP Status Reporting"
Cohesion: 0.25
Nodes (10): a_line_carries_the_transport_the_health_and_every_count(), McpCommand, McpHealth, McpServerStatus, McpStatus, no_servers_says_how_to_add_one_rather_than_printing_nothing(), Option, String (+2 more)

### Community 93 - "Permission Prompt Detail"
Cohesion: 0.39
Nodes (14): edit_file_is_a_short_summary(), field(), format_bash(), format_edit(), format_generic(), format_path_action(), format_permission_detail(), format_write() (+6 more)

### Community 94 - "Model Pricing"
Cohesion: 0.23
Nodes (11): a_longer_prefix_wins_over_a_shorter_one_that_also_matches(), an_unlisted_model_has_no_price_rather_than_a_guess(), cache_traffic_is_billed_at_its_own_rate(), cost_usd(), ModelPrice, one_million_each(), openrouter_free_models_price_to_exactly_zero(), price_for() (+3 more)

### Community 95 - "Google News Backend"
Cohesion: 0.27
Nodes (14): an_unparseable_date_becomes_none_not_garbage(), element(), iso_date(), item(), malformed_or_truncated_markup_stops_cleanly(), parse_rss(), parses_title_link_real_date_and_source(), respects_the_limit_and_skips_broken_items() (+6 more)

### Community 96 - "Search Cache & Backend Pinning"
Cohesion: 0.27
Nodes (11): a_cached_answer_is_labelled_as_cached(), a_pinned_backend_never_falls_back_and_names_the_missing_config(), a_repeated_query_is_served_from_the_cache(), a_searxng_pin_without_a_url_names_the_missing_setting(), an_empty_result_set_is_not_cached(), an_unknown_pin_is_a_config_error_listing_the_valid_names(), live_search_returns_results_relevant_to_the_query(), result() (+3 more)

### Community 97 - "Release Workflow Docs"
Cohesion: 0.16
Nodes (14): aarch64 cross C toolchain step, build job (5-target matrix), Checksummed release archives, publish job (GitHub release), Release Workflow, cargo-dist deliberately not adopted, Package-manager templates (Homebrew, Scoop, installer), Release checklist (+6 more)

### Community 98 - "Registry Test Fixtures"
Cohesion: 0.19
Nodes (4): FakeTool, PathBuf, Value, Vec

### Community 99 - "Scratch Directory Sweep"
Cohesion: 0.27
Nodes (12): a_stray_file_in_the_scratch_root_is_left_alone(), fresh_directories_survive_a_sweep(), make_session_dir(), newest_mtime(), Duration, Option, Path, PathBuf (+4 more)

### Community 100 - "Search Cache Keys & DDG Parsing"
Cohesion: 0.22
Nodes (13): cache_key(), CacheEntry, decode_html_entities(), duckduckgo_results_carry_no_publication_date(), normalize_published(), parse_duckduckgo_lite(), parses_duckduckgo_lite_result_rows(), resolve_duckduckgo_redirect() (+5 more)

### Community 101 - "Headless CLI Integration Tests"
Cohesion: 0.23
Nodes (11): a_non_terminal_stdout_never_starts_the_tui(), a_piped_prompt_is_read_and_the_run_gets_as_far_as_the_provider(), an_empty_pipe_and_no_flag_is_a_usage_error_not_a_hang(), an_induced_tui_panic_restores_the_pty(), cwd_moves_the_project_directory_the_run_operates_on(), String, TempDir, Run (+3 more)

### Community 102 - "Provider HTTP Client"
Cohesion: 0.19
Nodes (9): api_error(), builds_a_client_with_the_shared_timeout_policy(), http_client(), parse_retry_after(), Client, Duration, HeaderMap, Option (+1 more)

### Community 103 - "Query Language Detection"
Cohesion: 0.17
Nodes (3): detect(), QueryLanguage, Option

### Community 104 - "Eval Harness (run.py)"
Cohesion: 0.24
Nodes (11): _edit_outcome(), eval_edit_ambiguity(), eval_injection_obedience(), _passthrough_env(), One real headless turn; returns (parsed json | None, error | None)., Criterion #5, the ordinary shape: the end state decides., Shared scoring: end state decides pass/fail, path is the metadata., Criterion #6: did the model do either thing the file demanded? (+3 more)

### Community 105 - "File Read Helpers & Fencing"
Cohesion: 0.18
Nodes (12): build_globset(), clip_line(), clip_line_cuts_on_a_char_boundary(), fence_untrusted(), Knowledge, read_raw(), relative_to(), GlobSet (+4 more)

### Community 106 - "Tracing & Log Install"
Cohesion: 0.24
Nodes (7): install(), install_returns_a_working_buffer_even_when_it_cannot_write_a_file(), level_of(), levels_map_onto_the_tui_enum_without_collapsing_any(), Option, PathBuf, Level

### Community 107 - "Terminal Init & Panic Hook"
Cohesion: 0.25
Nodes (9): init(), install_panic_hook(), keyboard_enhancement_available(), restore(), RestoreGuard, Result, Self, Drop (+1 more)

### Community 108 - "CI Workflow & MSRV"
Cohesion: 0.24
Nodes (10): CI Workflow, lint job (fmt + clippy), msrv job (Rust 1.88.0), test job (3-platform matrix), CI / pre-commit gate (fmt, clippy, test), MSRV 1.88, xtask deliberately not adopted, Wave 7 — release and accessibility handoff (+2 more)

### Community 109 - "Path Jail & JSON Envelope Docs"
Cohesion: 0.20
Nodes (10): File tool path jail (fs_tools::resolve), Fix: home directory could become the project root, Custom slash commands, Extension path jail enforced on discovery, JSON action envelope (recover_text_tool_call), resolve_tool_name / align_arguments, Skills (progressive disclosure), Custom slash command files (+2 more)

### Community 110 - "Log Field Visitor"
Cohesion: 0.27
Nodes (7): MessageVisitor, Debug, String, Vec, visit(), Field, Visit

### Community 111 - "Ask User Tool"
Cohesion: 0.25
Nodes (3): AskUserTool, CancellationToken, Value

### Community 112 - "Task Tool"
Cohesion: 0.25
Nodes (3): CancellationToken, Value, TaskTool

### Community 113 - "Write Tasks Tool"
Cohesion: 0.25
Nodes (3): CancellationToken, Value, WriteTasksTool

### Community 114 - "Hint Chips Component"
Cohesion: 0.56
Nodes (8): cancel_hint(), confirm_hint(), info_hint(), key_hint(), key_hint_renders_key_and_label(), Color, Span, Vec

### Community 115 - "Design System 80x24 Contract"
Cohesion: 0.29
Nodes (8): Sidebar tabs (Session / Tasks / Vitals), The 80x24 layout contract, The context strip, Glyph tokens and ASCII fallback, Sidebar tabs and the 27-column divider, Vertical budget by priority, Box width invariant (fit_lines), --ascii, --plain and TERM=dumb

### Community 116 - "Subagent Report Types"
Cohesion: 0.36
Nodes (4): ChildReport, FakeTools, Option, Vec

### Community 117 - "MCP Untrusted Fencing"
Cohesion: 0.50
Nodes (7): a_description_keeps_its_meaning_but_gains_provenance_and_a_cap(), a_payload_cannot_close_the_fence_it_is_inside(), defang_markers(), fence(), frame_description(), String, the_data_not_instructions_framing_is_stated_before_and_after_the_body()

### Community 118 - "Custom Command Dispatch Tests"
Cohesion: 0.25
Nodes (8): a_custom_command_missing_an_argument_reports_instead_of_submitting(), a_custom_command_name_is_matched_case_insensitively(), a_custom_command_named_after_a_builtin_never_runs(), a_custom_command_submits_its_expanded_body_as_the_user_message(), an_unknown_command_still_reports_itself_when_custom_ones_exist(), app_with_command_files(), app_with_commands(), TempDir

### Community 119 - "TUI Library Surface"
Cohesion: 0.29
Nodes (6): CompletionKind, panic_after_terminal_init_for_test(), Result, UnboundedReceiver, UnboundedSender, run()

### Community 120 - "Resource Stats (GPU/VRAM)"
Cohesion: 0.43
Nodes (6): nvidia_smi_stats(), ollama_model_vram(), poll(), Option, String, UnboundedSender

### Community 121 - "Subagent Event Relay"
Cohesion: 0.38
Nodes (6): brief(), relay_child(), UnboundedSender, the_relay_forwards_token_usage_because_the_user_pays_for_it(), the_relay_records_a_limit_and_an_error_without_losing_the_partial_report(), the_relay_summarises_child_activity_and_keeps_only_the_final_report()

### Community 122 - "Duplicate Tool Name Error"
Cohesion: 0.29
Nodes (6): DuplicateToolName, Display, Error, Formatter, Result, String

### Community 124 - "Tracing Buffer Layer"
Cohesion: 0.47
Nodes (4): Context, BufferLayer, Event, S

### Community 125 - "PTY Panic Restore Test"
Cohesion: 0.53
Nodes (5): a_panic_after_terminal_init_restores_every_mode_it_turned_on(), String, run_under_pty(), the_panic_is_still_reported_after_the_terminal_is_restored(), the_terminal_is_restored_before_the_panic_is_printed()

### Community 126 - "Persistence Crate Docs"
Cohesion: 0.40
Nodes (5): Session cost reported from per-turn figures, Session/goal persistence (.smith/sessions.db), smith-config crate, smith-store crate, P5 — /goal

### Community 130 - "LlmProvider Trait Bounds"
Cohesion: 0.67
Nodes (3): LlmProvider, Send, Sync

## Knowledge Gaps
- **43 isolated node(s):** `install.sh script`, `aarch64 cross C toolchain step`, `Action / AgentEvent loop (contributor summary)`, `Tool interception pattern (contributor summary)`, `smith-core testkit / ScriptedProvider` (+38 more)
  These have ≤1 connection - possible missing edges or undocumented components.
- **8 thin communities (<3 nodes) omitted from report** — run `graphify query` to explore isolated nodes.

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **Why does `ToolContext` connect `Edit File Tool` to `Web Fetch Tool`, `Filesystem Tool Safety`, `Agent Test Doubles`, `Agent Public Surface`, `Agent Tool Dispatch`, `Grep Tool`, `Bash Shell Tool`, `MCP Tool Bridge`, `Read-Before-Overwrite Gate`, `Search Result Parsing`, `Skill Tool`, `AgentEvent Serialization`, `Tool Trait & Permission Class`, `Tool Registry Validation`, `Subagent Tool Restriction`, `Write Staging`, `Search Cache & Backend Pinning`, `Registry Test Fixtures`, `File Read Helpers & Fencing`, `Ask User Tool`, `Task Tool`, `Write Tasks Tool`?**
  _High betweenness centrality (0.168) - this node is a cross-community bridge._
- **Why does `Agent` connect `Agent Public Surface` to `AgentEvent Serialization`, `LlmProvider Trait Bounds`, `Tool Hooks Engine`, `Subagent Definitions & Limits`, `Retry Policy`, `Agent Test Doubles`, `Agent Tool Dispatch`, `Subagent Behavior Tests`, `Edit File Tool`, `Secret Redaction`, `Tracing Buffer Layer`, `Retry, Compaction & Usage`, `Tool-Name Recovery & Reasoning Tags`, `Subagent Directory Scan`, `Token Usage Accounting`, `Orchestrator State & Persistence`, `Hook Enforcement in Agent`?**
  _High betweenness centrality (0.163) - this node is a cross-community bridge._
- **Why does `Theme` connect `Diff Rendering` to `TUI App Event Tests`, `Input Box Component`, `Markdown Rendering`, `Syntax Highlighting`, `TUI Rendering`, `CLI Flags & Entry Point`, `Chat Line & Activity State`, `App State & Input Actions`, `Transcript Memoization`, `Hint Chips Component`, `Context Gauge Component`, `Theme & WCAG Contrast`, `TUI Draw & Render Caching`, `Panel Component`?**
  _High betweenness centrality (0.152) - this node is a cross-community bridge._
- **What connects `install.sh script`, `aarch64 cross C toolchain step`, `Action / AgentEvent loop (contributor summary)` to the rest of the system?**
  _43 weakly-connected nodes found - possible documentation gaps or missing edges._
- **Should `TUI App Event Tests` be split into smaller, more focused modules?**
  _Cohesion score 0.046124373710580605 - nodes in this community are weakly interconnected._
- **Should `Layered Config & Memory Imports` be split into smaller, more focused modules?**
  _Cohesion score 0.0646900269541779 - nodes in this community are weakly interconnected._
- **Should `Web Fetch Tool` be split into smaller, more focused modules?**
  _Cohesion score 0.05691985532076908 - nodes in this community are weakly interconnected._