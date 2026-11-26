# Acceptance scenarios

The design brief numbers its acceptance scenarios A01 to A68. This table
says, for each one, what v0.1 has for it and which tests those are.

The **Tests** column is taken from the code, not written by hand. A test
counts for an id when its name carries the id (`a27_...`) or the doc
comment directly above it does (`/// A27: ...`). To list what the code has
now, or to compare it with this table:

```sh
scripts/dev/acceptance.sh            # id, crate, test, file
scripts/dev/acceptance.sh --missing  # ids no test names
scripts/dev/acceptance.sh --check    # this table against the code
```

How to read the columns:

* **Crate level**: `covered` means at least one test in a library crate
  names the id. Such a test exercises that crate's part of the scenario
  against real files, processes or an in-memory database, not the whole
  system. `not covered` means no test names the id.
* **End to end**: the scenario run through a real controller, worker and
  client. `docs/architecture.md` assigns these to `bella-controller`
  (§6.16) and `bella-worker` (§6.17). Both crates are still being written,
  so every assigned row is `pending`; `-` means no end-to-end run is
  assigned.
* **Scenario** is a short description, taken from the tests that cover the
  id or, where no test exists, from the design brief. A row that says
  "Outside v0.1" concerns something listed in `docs/architecture.md` §9;
  a row that is `not covered` without that note is still owed a test.

| Id | Scenario | Crate level | End to end | Tests |
| --- | --- | --- | --- | --- |
| A01 | Concurrent submissions and offers never share the same capacity | covered | pending (controller) | `bella-ledger`: `a01_concurrent_submissions_and_offers_stay_consistent`; `bella-scheduler`: `a01_two_offers_in_one_call_never_share_the_same_capacity` |
| A02 | Two offers race for the last capacity; exactly one wins | covered | pending (controller) | `bella-ledger`: `a02_two_offers_race_for_the_last_memory`; `bella-scheduler`: `a02_offers_in_one_call_respect_attempt_limits_and_tokens` |
| A03 | Native Windows and WSL start together: one physical budget, nothing counted twice. Outside v0.1 (no WSL executor) | not covered | - | none |
| A04 | An accepted reservation that has used nothing still counts in full | covered | - | `bella-ledger`: `a04_an_idle_reservation_counts_in_full`; `bella-protect`: `a04_offered_and_unstarted_grants_count_in_full` |
| A05 | Use beyond a reservation is charged at once, stops admission and is evicted under pressure | covered | pending (worker) | `bella-protect`: `a05_observed_excess_is_charged_stops_admission_and_is_evicted_under_pressure` |
| A06 | Suspending an attempt frees neither memory nor tokens | covered | - | `bella-protect`: `a06_a_suspended_grant_still_holds_its_memory_and_tokens` |
| A07 | The pressure response has hysteresis and does not flap | covered | pending (worker) | `bella-protect`: `a07_pressure_response_has_hysteresis_and_does_not_flap` |
| A08 | The machine owner stays protected with the controller unreachable | covered | pending (worker) | `bella-protect`: `a08_protection_decisions_take_no_controller_input` |
| A09 | A small or late project is still served among the first (fair share) | covered | - | `bella-scheduler`: `a09_a_late_small_project_gets_the_next_free_slots`, `a09_a_small_project_is_served_among_the_first` |
| A10 | A large job starts despite a stream of small ones | covered | - | `bella-scheduler`: `a10_a_large_job_starts_despite_a_stream_of_small_ones`, `a10_without_the_threshold_the_stream_starves_the_large_job` |
| A11 | One scope owns every process of an attempt; a repeated start, event or release does nothing twice | covered | pending (worker) | `bella-contain`: `a11_every_spawn_in_a_scope_dies_with_one_kill`, `a11_one_scope_owns_every_spawn_and_kill_ends_them_all`, `a11_one_scope_owns_every_spawn_and_one_kill_ends_them_all`; `bella-journal`: `a11_retransmitted_start_is_authorised_exactly_once`, `a11_start_permit_survives_a_restart_and_is_not_fresh_again`; `bella-ledger`: `a11_duplicate_events_and_release_are_no_ops` |
| A12 | A committed start counts as running even if it is never acknowledged | covered | pending (controller) | `bella-ledger`: `a12_a_committed_start_is_assumed_to_be_running` |
| A13 | An offer revoked before its start can never start | covered | pending (controller) | `bella-journal`: `a13_revoke_is_refused_once_a_start_permit_exists`, `a13_revoked_offer_can_never_start`; `bella-ledger`: `a13_revoke_before_start_prevents_the_start` |
| A14 | A machine that is out of contact keeps its reservations | covered | pending (controller) | `bella-ledger`: `a14_an_uncertain_attempt_keeps_its_reservation` |
| A15 | Events and results from an old generation change nothing | covered | pending (controller) | `bella-ledger`: `a15_a60_stale_generations_and_epochs_change_nothing` |
| A16 | A stale scope identity or a reused PID is never signalled; attempts from another boot are lost | covered | pending (worker) | `bella-contain`: `a16_a_tag_does_not_license_signalling_the_session_around_it`, `a16_adopt_drops_a_recorded_process_that_has_died`, `a16_adopting_a_vanished_job_returns_none_and_harms_nobody`, `a16_adopting_a_vanished_scope_returns_none_and_signals_nothing`, `a16_keeper_is_restored_before_the_next_spawn`, `a16_live_job_is_adopted_by_name_after_its_first_handle_closed`, `a16_stale_identity_never_signals_the_process_now_holding_the_pid`, `a16_terminate_and_kill_reach_members_only`; `bella-journal`: `a16_a_quarantined_attempt_is_lost_too_once_the_machine_rebooted`, `a16_a_scope_from_this_boot_is_readopted_after_a_restart`, `a16_attempts_cannot_change_before_recovery_has_run`, `a16_attempts_from_another_boot_are_lost_and_their_scope_is_never_trusted`, `a16_cleanup_started_after_a_reboot_is_readopted_within_that_boot_only` |
| A17 | Execution authority runs out after a sleep or a long disconnection | covered | pending (worker) | `bella-journal`: `a17_a_renewal_in_time_keeps_the_attempt_running`, `a17_a_stop_decided_on_wake_stands_even_if_the_clock_is_set_back`, `a17_after_a_sleep_nothing_new_runs_under_the_stale_authority`, `a17_authority_runs_out_at_valid_until_plus_the_disconnected_allowance`, `a17_the_machine_policy_caps_disconnected_running` |
| A18 | Half of a multi-host environment starts: abort and clean up, nothing falsely released. Outside v0.1 (a job runs on one machine) | not covered | - | none |
| A19 | A service and the tests that run against it reserve their combined peak | covered | pending (worker) | `bella-recipe`: `a19_a_service_and_its_tests_reserve_their_combined_peak`, `a19_services_sum_and_other_steps_take_the_maximum`, `a19_the_job_reserves_the_app_and_its_tests_together`, `a19_the_recipe_declaration_is_a_lower_bound`; `bella-scheduler`: `a19_the_combined_peak_must_fit_as_a_whole` |
| A20 | An environment asks for more memory than is free: bounded failure, no silent overcommit. Outside v0.1 (no growth requests) | not covered | - | none |
| A21 | Closing the MCP session or timing out a wait cancels nothing | covered | pending (controller) | `bella-mcp`: `a21_a_broken_output_pipe_cancels_nothing`, `a21_end_of_input_cancels_nothing`, `a21_repeated_wait_timeouts_cancel_nothing` |
| A22 | An interactive keep-alive disappears: idle and maximum lifetime are enforced and cleanup is tracked | not covered | pending (controller) | none |
| A23 | An environment extension fails at the provider: the old deadline stays visible and enforced. Outside v0.1 (no cloud providers) | not covered | - | none |
| A24 | A daemon that outlives its root process still holds the scope | covered | pending (worker) | `bella-contain`: `a24_a_daemon_that_outlives_its_root_keeps_the_scope_alive`, `a24_daemon_outlives_root_and_still_holds_the_scope`, `a24_daemon_that_outlives_its_root_shell_keeps_the_scope_alive`, `a24_untagged_daemon_is_found_in_the_process_group_of_its_dead_root` |
| A25 | Cleanup cannot confirm a process or device stopped: the capacity is quarantined, never reused | not covered | pending (worker) | none |
| A26 | Working-tree bytes are captured, not the git index | covered | - | `bella-capture`: `a26_working_tree_bytes_win_over_the_index` |
| A27 | A file deleted at the origin does not survive in the snapshot or the workspace | covered | - | `bella-capture`: `a27_tracked_file_deleted_on_disk_is_absent`, `a27_tracked_file_replaced_by_a_directory`; `bella-materialize`: `a27_deleted_file_does_not_survive_in_a_fresh_tree`, `a27_destination_that_is_a_symlink_is_refused`, `a27_non_empty_destination_is_refused` |
| A28 | Unusual names survive exactly; collisions, reserved names and over-long paths are precise errors | covered | - | `bella-capture`: `a28_index_records_keep_unusual_names`, `a28_unusual_names_survive`; `bella-materialize`: `a28_case_collisions_are_refused_on_a_case_insensitive_volume`, `a28_collisions_match_find_collisions_and_reach_into_directories`, `a28_destination_path_over_the_os_limit_is_a_precise_error`, `a28_normalization_collisions_are_refused_where_the_volume_folds_them`, `a28_normalization_pairs_are_found_without_case_folding`, `a28_over_long_names_and_paths_are_reported_per_platform`, `a28_probe_matches_the_volume_and_leaves_nothing_behind`, `a28_unicode_spaces_and_newlines_round_trip_exactly`, `a28_windows_problems_are_reported_up_front_by_check_platform`, `a28_windows_refuses_reserved_names_and_never_renames` |
| A29 | Executable bits and symlinks are preserved as they are | covered | - | `bella-capture`: `a29_chmod_only_change_changes_the_snapshot_id`, `a29_symlink_chain_that_leaves_the_root_fails_naming_the_link`, `a29_symlink_only_change_changes_the_snapshot_id`, `a29_symlink_placeholder_is_recorded_as_a_link`, `a29_untracked_executable_bit`; `bella-materialize`: `a29_executable_bit_is_applied_on_unix_and_ignored_on_windows`, `a29_only_a_missing_capability_counts_as_symlinks_unsupported`, `a29_symlinks_are_real_symlinks`, `a29_verify_tree_detects_a_wrong_link_target_and_never_follows_links`, `a29_verify_tree_detects_a_wrong_mode` |
| A30 | A tree edited during capture is retried, then fails; nothing half-read is stored | covered | - | `bella-capture`: `a30_created_and_deleted_files_restart_the_capture`, `a30_edit_after_a_file_was_read_restarts_the_capture`, `a30_file_changing_during_every_read_fails_with_its_path`, `a30_file_deleted_while_it_is_read_is_absent`, `a30_one_change_during_a_read_is_retried`, `a30_tree_that_never_settles_fails` |
| A31 | Concurrent captures and puts do not collide | covered | - | `bella-capture`: `a31_concurrent_captures_do_not_collide`; `bella-store`: `a31_child_process_puts`, `a31_concurrent_puts_from_several_processes`, `a31_concurrent_puts_of_same_and_different_objects_all_read_back`, `a31_temp_names_are_unique_across_threads_and_store_handles` |
| A32 | Unhydrated LFS pointers, uninitialised submodules and sparse checkouts fail precisely | covered | - | `bella-capture`: `a32_initialised_submodule_is_captured_from_its_working_tree`, `a32_lfs_pointer_is_not_a_hydrated_object`, `a32_sparse_checkout_with_missing_paths_fails_unless_excluded`, `a32_uninitialised_submodule_fails` |
| A33 | Untracked secrets are left out and reported; tracked ones fail the capture | covered | - | `bella-capture`: `a33_patterns_match_the_file_name_and_the_relative_path`, `a33_secrets_in_a_plain_directory_are_excluded_and_reported`, `a33_tracked_secret_fails_unless_allowed`, `a33_untracked_secret_is_excluded_and_reported`, `a34_declared_input_that_is_a_secret_needs_allow_secrets` |
| A34 | Ignored files are included only when declared | covered | - | `bella-capture`: `a34_declared_input_that_is_a_secret_needs_allow_secrets`, `a34_ignored_files_are_included_only_when_declared`, `a34_missing_declared_input_fails` |
| A35 | Several roots are one snapshot taken under one barrier | covered | - | `bella-capture`: `a35_multiple_roots_are_captured_under_one_barrier` |
| A36 | A corrupt object or a hostile manifest never yields a tree | covered | - | `bella-materialize`: `a36_corrupt_object_is_rejected_and_leaves_nothing`, `a36_deep_paths_do_not_multiply_the_work_of_the_collision_check`, `a36_directories_implied_by_deep_paths_count_against_the_limit`, `a36_entangled_symlinks_are_refused_instead_of_resolved_forever`, `a36_escaping_and_absolute_symlink_targets_are_rejected`, `a36_file_below_a_symlink_entry_is_refused_in_either_order`, `a36_implied_directories_count_against_the_node_limit`, `a36_missing_object_is_an_error_naming_the_path_and_leaves_nothing`, `a36_object_longer_or_shorter_than_the_manifest_size_is_rejected`, `a36_reading_stops_at_the_declared_size`, `a36_symlink_across_roots_cannot_carry_a_mount`, `a36_symlink_chain_in_a_mounted_root_is_named_with_its_mount`, `a36_the_reported_error_does_not_depend_on_the_thread_count`, `a36_total_bytes_and_entry_count_limits_are_enforced_before_writing`, `a36_traversal_and_absolute_paths_are_rejected_before_anything_is_written`; `bella-store`: `a36_body_corrupt_object_survives_a_put_and_is_removed_then_put_again`, `a36_flipped_truncated_or_extended_object_is_rejected`, `a36_no_damaged_file_is_ever_returned_as_other_content`, `a36_repair_checks_the_supplied_copy_before_touching_the_stored_one`, `a36_repair_replaces_a_body_corrupt_object_and_only_that` |
| A37 | A snapshot is immutable: later edits and deletions do not change it | covered | pending (controller) | `bella-capture`: `a37_snapshot_survives_later_edits_and_deletions` |
| A38 | Build host, target, test runtime and capabilities stay separate; a toolchain mismatch names both versions | covered | - | `bella-recipe`: `a38_a39_a_recipe_without_hosts_is_pinned_to_the_origin`, `a38_a39_cross_compilation_keeps_a_distinct_test_runtime`, `a38_a39_the_platform_fields_arrive_intact_and_separate`; `bella-scheduler`: `a38_a_toolchain_version_mismatch_names_both_versions` |
| A39 | Cross-compiled work is not reported as validated on a host other than its test runtime | covered | - | `bella-recipe`: `a38_a39_a_recipe_without_hosts_is_pinned_to_the_origin`, `a38_a39_cross_compilation_keeps_a_distinct_test_runtime`, `a38_a39_the_platform_fields_arrive_intact_and_separate`; `bella-scheduler`: `a39_cross_compiled_work_is_not_validated_on_the_wrong_host` |
| A40 | A GUI-session requirement excludes headless machines | covered | - | `bella-scheduler`: `a40_a_gui_session_requirement_excludes_headless_machines` |
| A41 | A device is removed mid-test: loss detected, attempt classified accurately. Outside v0.1 (no device runners) | not covered | - | none |
| A42 | Two destructive Redis tests run: separate instances prevent cross-test deletion. Outside v0.1 (no service adapters) | not covered | - | none |
| A43 | Parallel database migrations: isolated databases or an exclusive allocation. Outside v0.1 (no service adapters) | not covered | - | none |
| A44 | Database test load overwhelms a service pool: controller metadata stays protected. Outside v0.1 (no service pools) | not covered | - | none |
| A45 | A cache built for another OS, SDK or CPU target is not reused. Outside v0.1 (no build caches) | not covered | - | none |
| A46 | Referenced and recently stored objects survive garbage collection | covered | - | `bella-store`: `a46_gc_keeps_live_and_young_objects_and_rechecks_liveness_before_delete`, `a46_gc_waits_for_a_publish_in_progress_and_sees_its_result`, `a46_put_racing_gc_never_loses_the_object`, `a46_putting_an_object_again_restarts_its_grace_period` |
| A47 | Disk or log output fills its budget: controlled failure with recovery space preserved | not covered | pending (worker) | none |
| A48 | No cloud capacity while the provider-side janitor is missing or unhealthy | covered | - | `bella-cloud`: `a48_no_capacity_without_a_healthy_janitor` |
| A49 | A lost create response leaves exactly one instance and one budget hold | covered | - | `bella-cloud`: `a49_a_create_that_never_happened_is_retried_with_the_same_token`, `a49_a_lagging_listing_does_not_cause_a_second_create`, `a49_lost_create_response_yields_one_instance_and_one_hold` |
| A50 | An instance left behind by a dead coordinator is cleaned up (modelled with the simulated provider) | covered | - | `bella-cloud`: `a50_a_recorded_instance_removed_by_the_provider_is_settled_too`, `a50_provider_expiry_and_a_new_engine_clean_up_after_a_dead_coordinator` |
| A51 | A worker of a cancelled, expired, unknown or unreconciled intention is not admitted | covered | - | `bella-cloud`: `a51_a_worker_is_not_admitted_before_its_instance_is_reconciled`, `a51_a_worker_of_a_cancelled_expired_or_unknown_intention_is_denied` |
| A52 | Racing authorizations take the last of an allowance exactly once | covered | - | `bella-cloud`: `a52_racing_authorizations_take_the_last_of_an_allowance_once` |
| A53 | A vanished spot instance is an infrastructure interruption; a retry needs time and budget | covered | - | `bella-cloud`: `a53_a_retry_needs_time_and_budget`, `a53_a_vanished_spot_instance_is_an_infrastructure_interruption` |
| A54 | Unavailable spot capacity never silently becomes on-demand | covered | - | `bella-cloud`: `a54_unavailable_spot_fails_instead_of_becoming_on_demand`, `a54_unavailable_spot_stays_blocked_when_only_spot_is_allowed` |
| A55 | LAN-only source never gets cloud capacity and never leaves the LAN | covered | - | `bella-cloud`: `a55_lan_only_demand_never_causes_a_request`, `a55_lan_only_source_is_refused_whatever_the_priority`; `bella-scheduler`: `a55_lan_only_source_never_leaves_the_lan` |
| A56 | A disk or address left behind by a deletion stays visible until it is gone | covered | - | `bella-cloud`: `a56_a_lost_delete_response_is_resolved_by_the_listing`, `a56_leftover_disk_and_address_stay_visible_until_deleted` |
| A57 | A provider pool wants to replace expired workers: the pool expires too. Outside v0.1 (no provider pools) | not covered | - | none |
| A58 | A dedicated host with a minimum commitment keeps its hold until release | covered | - | `bella-cloud`: `a58_a_minimum_commitment_keeps_the_hold_and_the_host_until_release` |
| A59 | A result upload fails: the test outcome stays separate from artifact publication | not covered | pending (controller) | none |
| A60 | Events and results from an old controller epoch change nothing | covered | pending (controller) | `bella-ledger`: `a15_a60_stale_generations_and_epochs_change_nothing` |
| A61 | An ordinary agent cannot change policy, budgets or enrollment | covered | pending (controller) | `bella-ledger`: `a61_policy_changes_require_an_admin`; `bella-mcp`: `a61_policy_denials_are_clear_tool_errors`, `a61_the_registry_is_exactly_the_documented_tools`, `a61_there_is_no_tool_for_policy_budgets_or_enrollment` |
| A62 | A worker update is incompatible with an old OS: blocked or rolled back. Outside v0.1 (no automatic updates) | not covered | - | none |
| A63 | An older controller backup is restored: workers and providers are reconciled before scheduling. Outside v0.1 as a tested procedure | not covered | - | none |
| A64 | Cloud prices or quota change, or a region is unavailable: fresh validation, no invented placement. Outside v0.1 (no cloud providers) | not covered | - | none |
| A65 | Memory a grant already uses is not subtracted from available memory twice | covered | - | `bella-protect`: `a65_memory_in_use_is_not_subtracted_twice_and_promises_are_kept` |
| A66 | Descendants hidden behind nohup, setsid or a double fork are contained | covered | pending (worker) | `bella-contain`: `a66_hidden_descendants_stay_in_the_job_and_die_with_it`, `a66_orphan_in_its_own_session_is_found_by_its_tag_and_killed`, `a66_orphan_that_also_drops_its_tag_between_observations_is_found`, `a66_orphan_that_left_its_process_group_is_found_in_its_session`, `a66_untagged_child_in_its_own_session_is_found_through_its_parent`, `a66_work_hidden_behind_nohup_setsid_and_double_fork_is_contained` |
| A67 | A required external action times out: no exactly-once claim and no automatic replay. Outside v0.1 beyond never retrying external side effects | not covered | - | none |
| A68 | An outbound network or log flood: control and cleanup stay responsive | not covered | - | none |

49 of the 68 ids have at least one crate-level test. 19 have none: A03,
A18, A20, A22, A23, A25, A41, A42, A43, A44, A45, A47, A57, A59, A62, A63,
A64, A67 and A68. Of those, A22 and A59 are assigned to `bella-controller`
and A25 and A47 to `bella-worker`, so their first tests are expected with
those crates. No scenario is covered end to end yet.
