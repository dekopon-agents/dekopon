# Mutation baseline: dekopon-policy, dekopon-broker, dekopon-broker-protocol

Base `9089a6e1808cbac4d5ab4fb9969765c0394b2fa3` (typed-events-pr3, #837). cargo-mutants 27.1.0, rustc 1.98.1.
Study only: no source, test, manifest, lockfile or CI change; no PR. Supersedes the earlier network-stop note on this branch
(the tracing-core git dependency fetched fine through this environment's proxy).

## Results

| Crate | Mutants | Tested | Caught | Missed | Timeout | Unviable | Score caught/(caught+missed) | Wall |
|---|---:|---:|---:|---:|---:|---:|---:|---|
| dekopon-policy | 153 | 153 | 97 | 20 | 0 | 36 | 82.9% | 8m27s (-j3) |
| dekopon-broker-protocol | 576 | 576 | 381 | 87 | 27 | 81 | 81.4% | 41m (-j3) |
| dekopon-broker | 825 | **209** (source order, lib.rs lines 83-853) | 142 | 47 | 0 | 20 | 75.1% (partial; reduced test set) | 78m (-j3), plus a 35m partial rerun of the misses |

**dekopon-broker is incomplete.** 616 mutants were NOT tested: every mutant in `crates/dekopon-broker/src/lib.rs` from line 853
(`CredentialRefreshError::permanent -> bool with false`) through line 5643 (`storage_scope_permits`). The exact list is
`mutants-study/dekopon-broker/untested.txt`. The 4 h study cap ended the run; at ~3-4 mutants/minute (4 cores, 60 s rebuild of the 6k-line crate per mutant) the rest needs about 3 more hours.
Rerun the remainder with `cargo mutants -p dekopon-broker --file crates/dekopon-broker/src/lib.rs --shard k/n` or `--re` on `untested.txt`.

The 47 broker misses were measured with `--lib --test integration` only (plus broker-host built for its `wat` feature). A rerun of the misses with the full
test set (all eight broker test targets) reached 16 of 55 mutants before the cap: 15 became TIMEOUT (a hang under the full set, still no assertion; flagged below) and 1 stayed missed (`lib.rs:83:37`); 39 were not rerun, so those misses are not confirmed against the full set. Outputs: `mutants-study/dekopon-broker-missed-rerun/`.

Triage tags (missed mutants only; timeouts are counted as detected by cargo-mutants and are not triaged, except the 15 rerun timeouts noted):

| Tag | policy | broker | protocol | total |
|---|---:|---:|---:|---:|
| kill | 13 | 28 | 47 | 88 |
| equivalent | 0 | 1 | 6 | 7 |
| covered-elsewhere | 0 | 1 | 9 | 10 |
| skip | 7 | 17 | 25 | 49 |

Covered-elsewhere was assigned only where a dependent test was found by reading it (named inline); a mutant not so named was not proven killed by dependents. Dependents were searched by grep and reading, not rerun, except the broker full-set rerun above. Kill entries state the test name as the invariant; names are proposals for kill-survivors.

## Ten highest-value kills, in order

1. protocol `lib.rs:208` `decode_hex`: `||`→`&&` lets a wrong-width traceparent field pass the guard; the next lines are documented as unreachable. Peer-controlled text; check whether it panics (P1 if so). Test: `a_traceparent_field_of_the_wrong_width_is_refused`.
2. protocol `lib.rs:953,970-973` generation digest validation (`valid_generation_digest`, the `validate` guard): `a_generation_request_with_a_malformed_digest_is_refused` plus a 63/65-char/uppercase/non-hex table.
3. protocol `lib.rs:2104` `BrokerClient::generation`: `a_generation_reply_for_another_digest_is_an_unexpected_response` and the matching-digest success case.
4. protocol `lib.rs:1832,1925` descriptor-count refusal before send for `run` and `invoke`: `a_run_with_more_asset_descriptors_than_the_limit_is_refused_before_send`.
5. policy `lib.rs:889` `@id` annotation: `an_empty_policy_id_annotation_is_refused`, `an_overlong_policy_id_annotation_is_refused`.
6. policy `lib.rs:644` `a_policy_set_over_the_policy_count_ceiling_is_refused` (1025 policies).
7. policy `lib.rs:1259` `the_policy_digest_changes_when_the_world_gains_a_secret`.
8. protocol `lib.rs:77,383` `an_unmapped_subject_error_decodes_to_unmapped_subject`; `a_string_over_its_bound_is_refused_by_deserialize`.
9. broker `lib.rs:135-177` one table test, `chat_memory_config_refuses_<each invalid limit>`: 24 of the 47 broker misses sit in `ChatMemoryConfig::validate`; under the full set 15 of them only hang rather than fail an assertion.
10. broker `lib.rs:803-804` `a_chat_route_needs_each_storage_attribute_to_match` (MissingChatStorage per attribute) and protocol `lib.rs:2172,2224,2232` connect/refusal-before-credentials error classification.

## Missed mutants

One line per missed mutant: location, mutation, tag, reason.

## dekopon-policy

- `lib.rs:123` replace * with + — **skip** — ceiling constant arithmetic; a test could only assert the constant (AGENTS: no ceiling twins)
- `lib.rs:189` replace PolicyWorld::providers -> impl Iterator<Item =&ProviderId> with ::std::iter::empty() — **kill** — a_world_lists_its_providers: providers() returns every registered provider
- `lib.rs:193` replace PolicyWorld::capabilities -> impl Iterator<Item =(&CapabilityId, &ProviderId)> with ::std::iter::empty() — **kill** — a_world_lists_its_capabilities_with_their_providers: capabilities() returns the registered pairs (callers in broker lib.rs:616,688)
- `lib.rs:203` replace PolicyWorld::secrets -> impl Iterator<Item =&SecretDrn> with ::std::iter::empty() — **kill** — a_world_lists_its_secrets: secrets() returns every added secret
- `lib.rs:185` replace PolicyWorld::principals -> impl Iterator<Item =&PrincipalId> with ::std::iter::empty() — **kill** — a_world_lists_its_principals: PolicyWorld::principals returns every registered principal (a test may need dependents; broker uses it at lib.rs:2564)
- `lib.rs:539` replace UnresolvedKind::label -> &'static str with "" — **kill** — unresolved_kinds_have_distinct_labels: label text is part of the policy digest input (lib.rs:1280)
- `lib.rs:539` replace UnresolvedKind::label -> &'static str with "xyzzy" — **kill** — unresolved_kinds_have_distinct_labels: label text is part of the policy digest input (lib.rs:1280)
- `lib.rs:628` replace > with >= in PolicyEngine::build — **skip** — boundary at exactly MAX_POLICY_BYTES; ceiling twin banned by AGENTS Tests
- `lib.rs:644` replace > with == in PolicyEngine::build — **kill** — a_policy_set_over_the_policy_count_ceiling_is_refused: 1025 policies must give TooManyPolicies
- `lib.rs:644` replace > with >= in PolicyEngine::build — **skip** — boundary at exactly MAX_POLICIES; ceiling twin banned
- `lib.rs:889` replace || with && in apply_annotated_ids — **kill** — an_empty_policy_id_annotation_is_refused: @id("") must give InvalidPolicyId
- `lib.rs:889` replace > with == in apply_annotated_ids — **kill** — an_overlong_policy_id_annotation_is_refused: a 129-byte @id must give InvalidPolicyId
- `lib.rs:889` replace > with >= in apply_annotated_ids — **skip** — boundary at exactly MAX_POLICY_ID_BYTES; ceiling twin banned
- `lib.rs:1259` delete ! in policy_digest — **kill** — the_policy_digest_changes_when_the_world_gains_a_secret: secret.use action joins the digest only with secrets
- `request.rs:7` replace * with + — **skip** — ceiling constant arithmetic; a test could only assert the constant
- `request.rs:39` replace OpaqueBody::label -> &'static str with "xyzzy" — **kill** — opaque_body_reasons_have_distinct_labels: label is recorded as http.request.body.reason (broker lib.rs:3797)
- `request.rs:39` replace OpaqueBody::label -> &'static str with "" — **kill** — opaque_body_reasons_have_distinct_labels: label is recorded as http.request.body.reason (broker lib.rs:3797)
- `request.rs:126` replace > with >= in json_record — **skip** — boundary at exactly MAX_BODY_BYTES; ceiling twin banned
- `request.rs:162` replace <impl Deserialize<'de> for Unique>::deserialize::<impl Visitor<'de> for Walk>::expecting -> fmt::Result with Ok(Default::default()) — **skip** — serde expecting() message text only; asserting it asserts internals
- `request.rs:189` replace match guard fraction.len() <= 4 with true in <impl Deserialize<'de> for Unique>::deserialize::<impl Visitor<'de> for Walk>::visit_f64 — **kill** — a_json_number_with_more_than_four_fraction_digits_is_not_cedar_representable: 0.12345 must give an opaque-body refusal

## dekopon-broker

- `lib.rs:141` replace || with && in ChatMemoryConfig::validate — **kill** — chat_memory_config_refuses_a_compaction_threshold_above_the_storage_file_ceiling [full test set: TIMEOUT, a hang rather than an assertion]
- `lib.rs:140` replace || with && in ChatMemoryConfig::validate — **kill** — chat_memory_config_refuses_a_compaction_target_not_below_its_threshold [full test set: TIMEOUT, a hang rather than an assertion]
- `lib.rs:139` replace || with && in ChatMemoryConfig::validate — **kill** — chat_memory_config_refuses_more_search_results_than_lookback_turns [full test set: TIMEOUT, a hang rather than an assertion]
- `lib.rs:138` replace || with && in ChatMemoryConfig::validate — **kill** — chat_memory_config_refuses_more_recent_turns_than_lookback_turns [full test set: TIMEOUT, a hang rather than an assertion]
- `lib.rs:137` replace || with && in ChatMemoryConfig::validate — **kill** — chat_memory_config_refuses_a_zero_limit: each positive limit set to 0 gives InvalidChatMemory [full test set: TIMEOUT, a hang rather than an assertion]
- `lib.rs:83` replace * with + — **skip** — POLICY_STACK_BYTES arithmetic; a test could only assert the constant
- `lib.rs:136` replace || with && in ChatMemoryConfig::validate — **kill** — chat_memory_config_refuses_duplicate_enabled_agents [full test set: TIMEOUT, a hang rather than an assertion]
- `lib.rs:135` replace || with && in ChatMemoryConfig::validate — **kill** — chat_memory_config_refuses_an_empty_enabled_agent_list: an empty enabled_agents gives InvalidChatMemory [full test set: TIMEOUT, a hang rather than an assertion]
- `lib.rs:140` replace > with == in ChatMemoryConfig::validate — **skip** — boundary on target/threshold equality; ceiling-style twin banned [full test set: TIMEOUT, a hang rather than an assertion]
- `lib.rs:140` replace > with >= in ChatMemoryConfig::validate — **skip** — boundary on target/threshold equality; ceiling-style twin banned [full test set: TIMEOUT, a hang rather than an assertion]
- `lib.rs:142` replace < with <= in ChatMemoryConfig::validate — **skip** — boundary at exactly MEMORY_MIN_TURN_LINE_BYTES; ceiling twin banned [full test set: TIMEOUT, a hang rather than an assertion]
- `lib.rs:177` replace || with && in ChatMemoryConfig::validate — **kill** — chat_memory_config_refuses_a_query_ceiling_above_256_kib_and_a_result_ceiling_above_1_mib [full test set: TIMEOUT, a hang rather than an assertion]
- `lib.rs:168` replace || with && in ChatMemoryConfig::validate — **kill** — chat_memory_config_refuses_a_read_chunk_above_the_storage_read_limit [full test set: TIMEOUT, a hang rather than an assertion]
- `lib.rs:167` replace || with && in ChatMemoryConfig::validate — **kill** — chat_memory_config_refuses_a_lookback_that_cannot_fit_the_compaction_target [full test set: TIMEOUT, a hang rather than an assertion]
- `lib.rs:167` replace > with < in ChatMemoryConfig::validate — **kill** — chat_memory_config_refuses_a_lookback_that_cannot_fit_the_compaction_target (retained > target must refuse; < inverts it) [full test set: TIMEOUT, a hang rather than an assertion]
- `lib.rs:168` replace > with == in ChatMemoryConfig::validate — **skip** — boundary at exactly storage.max_read_bytes_per_call [full test set: TIMEOUT, a hang rather than an assertion]
- `lib.rs:168` replace > with >= in ChatMemoryConfig::validate — **skip** — boundary at exactly storage.max_read_bytes_per_call
- `lib.rs:171` replace > with >= in ChatMemoryConfig::validate — **skip** — boundary at exactly storage.max_write_bytes_per_call
- `lib.rs:172` replace > with == in ChatMemoryConfig::validate — **kill** — chat_memory_config_refuses_a_compaction_target_above_the_storage_write_limit
- `lib.rs:172` replace > with >= in ChatMemoryConfig::validate — **skip** — boundary at exactly storage.max_write_bytes_per_call
- `lib.rs:176` replace > with == in ChatMemoryConfig::validate — **skip** — boundary at exactly storage.max_file_bytes
- `lib.rs:176` replace > with >= in ChatMemoryConfig::validate — **skip** — boundary at exactly storage.max_file_bytes
- `lib.rs:176` replace * with + in ChatMemoryConfig::validate — **kill** — chat_memory_config_refuses_a_namespace_that_cannot_hold_the_headroom: namespace_headroom 32*4096 arithmetic must reach the max_namespace_bytes check
- `lib.rs:177` replace > with == in ChatMemoryConfig::validate — **skip** — boundary at exactly the query/result ceiling
- `lib.rs:177` replace > with >= in ChatMemoryConfig::validate — **skip** — boundary at exactly the query/result ceiling
- `lib.rs:231` replace > with >= in ChatMemoryConfig::validate_host_limits — **skip** — boundary at exactly host.max_input_bytes; ceiling twin banned
- `lib.rs:232` replace > with >= in ChatMemoryConfig::validate_host_limits — **skip** — boundary at exactly host.max_memory_bytes; ceiling twin banned
- `lib.rs:233` replace > with >= in ChatMemoryConfig::validate_host_limits — **covered-elsewhere** — dekopon-brokerd src/tests.rs chat_memory_rejects_a_host_fuel_ceiling_that_cannot_reach_compaction covers refusal; the exact-fuel boundary is skip
- `lib.rs:242` replace ChatMemoryConfig::enabled_for -> bool with true — **kill** — chat_memory_is_enabled_only_for_its_listed_agents: enabled_for is false for an unlisted agent
- `lib.rs:249` replace * with + in round_up — **kill** — round_up_rounds_a_threshold_to_the_read_chunk: read budget uses value/multiple*multiple, not a sum
- `lib.rs:434` replace CapabilityRoute::as_str -> &'static str with "" — **kill** — capability_routes_have_distinct_stable_names: as_str of each route (Generic, ChatMemoryRecord, ...) is the wire name
- `lib.rs:434` replace CapabilityRoute::as_str -> &'static str with "xyzzy" — **kill** — capability_routes_have_distinct_stable_names: as_str of each route (Generic, ChatMemoryRecord, ...) is the wire name
- `lib.rs:493` replace deserialize_default_secret_use -> Result<Option<DefaultSecretUse>, D::Error> with Ok(None) — **kill** — a_default_secret_use_field_deserializes_to_some: present value gives Some(DefaultSecretUse), absent gives None
- `lib.rs:535` replace <impl fmt::Display for StartupWarning>::fmt -> fmt::Result with Ok(Default::default()) — **skip** — Display text of StartupWarning only
- `lib.rs:558` replace ConstraintCatalog::empty -> Self with Default::default() — **equivalent** — ConstraintCatalog::empty is Self::default(); Default::default() is the same value
- `lib.rs:653` replace ConstraintCatalog::len -> usize with 0 — **kill** — a_constraint_catalog_reports_its_length: len equals the number of sets (0 and 1 both missed, so only empty and one-set catalogs were exercised)
- `lib.rs:653` replace ConstraintCatalog::len -> usize with 1 — **kill** — a_constraint_catalog_reports_its_length: len equals the number of sets (0 and 1 both missed, so only empty and one-set catalogs were exercised)
- `lib.rs:658` replace ConstraintCatalog::is_empty -> bool with true — **kill** — a_constraint_catalog_is_empty_only_without_sets
- `lib.rs:658` replace ConstraintCatalog::is_empty -> bool with false — **kill** — a_constraint_catalog_is_empty_only_without_sets
- `lib.rs:667` replace > with == in ConstraintCatalog::validate — **kill** — a_catalog_over_the_constraint_set_ceiling_is_refused: count > maximum gives TooManyConstraintSets
- `lib.rs:667` replace > with >= in ConstraintCatalog::validate — **skip** — boundary at exactly the constraint-set maximum
- `lib.rs:705` replace > with == in ConstraintCatalog::retention_policies — **kill** — an_idle_ttl_beyond_u64_millis_is_refused: ttl above u64::MAX ms gives InvalidStorageRetention
- `lib.rs:705` replace > with >= in ConstraintCatalog::retention_policies — **skip** — boundary at exactly u64::MAX milliseconds; unreachable in practice
- `lib.rs:804` replace && with || in ConstraintCatalog::validate_routes — **kill** — a_chat_route_needs_each_storage_attribute_to_match (same test)
- `lib.rs:803` replace && with || in ConstraintCatalog::validate_routes — **kill** — a_chat_route_needs_each_storage_attribute_to_match: private-conversation scope, JSONL interface and access each independently produce MissingChatStorage
- `lib.rs:846` replace CredentialRefreshError::category -> &'static str with "" — **kill** — credential_refresh_errors_have_distinct_categories: category() is recorded on the span error attribute (lib.rs:3595,3945)
- `lib.rs:846` replace CredentialRefreshError::category -> &'static str with "xyzzy" — **kill** — credential_refresh_errors_have_distinct_categories: category() is recorded on the span error attribute (lib.rs:3595,3945)

## dekopon-broker-protocol

- `lib.rs:60` replace * with + — **skip** — frame-size constant arithmetic; test could only assert the constant
- `lib.rs:77` delete match arm ERROR_UNMAPPED_SUBJECT in SessionRefusal::from_error — **kill** — an_unmapped_subject_error_decodes_to_unmapped_subject: from_error(ERROR_UNMAPPED_SUBJECT) is not Other
- `lib.rs:60` replace * with + — **skip** — frame-size constant arithmetic; test could only assert the constant
- `lib.rs:157` replace TraceParent::flags -> u8 with 1 — **kill** — trace_flags_survive_a_traceparent_round_trip: flags() returns the parsed byte (use 00 and 01)
- `lib.rs:208` replace || with && in decode_hex — **kill** — a_traceparent_field_of_the_wrong_width_is_refused: peer text must give Malformed; the && form lets a wrong width reach the digit-pair parse (P1 if it can panic; verify)
- `lib.rs:369` replace deserialize_bounded_string::<impl serde::de::Visitor<'de> for Visitor<MAXIMUM>>::expecting -> fmt::Result with Ok(Default::default()) — **skip** — serde expecting() message text only
- `lib.rs:383` replace > with == in deserialize_bounded_string::<impl serde::de::Visitor<'de> for Visitor<MAXIMUM>>::visit_str — **kill** — a_string_over_its_bound_is_refused_by_deserialize: MAXIMUM+1 bytes must error
- `lib.rs:383` replace > with >= in deserialize_bounded_string::<impl serde::de::Visitor<'de> for Visitor<MAXIMUM>>::visit_str — **skip** — boundary at exactly MAXIMUM; ceiling twin banned
- `lib.rs:393` replace > with >= in deserialize_bounded_string::<impl serde::de::Visitor<'de> for Visitor<MAXIMUM>>::visit_string — **skip** — boundary at exactly MAXIMUM; ceiling twin banned
- `lib.rs:525` replace <impl fmt::Debug for DeliveryIdentity>::fmt -> fmt::Result with Ok(Default::default()) — **kill** — delivery_identity_debug_is_redacted: format!("{:?}") equals DeliveryIdentity([REDACTED])
- `lib.rs:641` replace deserialize_signed_service_decimal -> Result<String, D::Error> with Ok("xyzzy".into()) — **kill** — a_signed_service_decimal_field_keeps_its_canonical_text: deserialize returns the input string, and rejects non-canonical
- `lib.rs:641` replace deserialize_signed_service_decimal -> Result<String, D::Error> with Ok(String::new()) — **kill** — a_signed_service_decimal_field_keeps_its_canonical_text: deserialize returns the input string, and rejects non-canonical
- `lib.rs:659` replace deserialize_optional_positive_service_decimal::<impl serde::de::Visitor<'de> for OptionalServiceDecimal>::expecting -> fmt::Result with Ok(Default::default()) — **skip** — serde expecting() message text only
- `lib.rs:673` replace deserialize_optional_positive_service_decimal::<impl serde::de::Visitor<'de> for OptionalServiceDecimal>::visit_unit -> Result<Self::Value, E> with Ok(Default::default()) — **equivalent** — visit_unit returns Ok(None); Default of the Option value is None
- `lib.rs:666` replace deserialize_optional_positive_service_decimal::<impl serde::de::Visitor<'de> for OptionalServiceDecimal>::visit_none -> Result<Self::Value, E> with Ok(Default::default()) — **equivalent** — visit_none returns Ok(None); Default of the Option value is None
- `lib.rs:687` replace * with + — **skip** — turn-text ceiling constant arithmetic; a test could only assert the constant
- `lib.rs:701` replace DeliveredAnswer::as_str -> &str with "" — **kill** — a_delivered_answer_exposes_its_text: as_str returns the answer (used by broker/tui; no protocol-crate assertion)
- `lib.rs:701` replace DeliveredAnswer::as_str -> &str with "xyzzy" — **kill** — a_delivered_answer_exposes_its_text: as_str returns the answer (used by broker/tui; no protocol-crate assertion)
- `lib.rs:749` replace DeliveredTurnRequest::user -> &str with "" — **kill** — a_delivered_turn_request_exposes_the_fitted_user_text: user() returns the stored text
- `lib.rs:749` replace DeliveredTurnRequest::user -> &str with "xyzzy" — **kill** — a_delivered_turn_request_exposes_the_fitted_user_text: user() returns the stored text
- `lib.rs:953` replace match guard !valid_generation_digest(digest) with true in BrokerRequest::validate — **kill** — a_generation_request_with_a_malformed_digest_is_refused: Prepare/Commit/AbortGeneration with a 63-char or uppercase digest give InvalidGenerationDigest (tests.rs:45 sends "ABC" but only via serde)
- `lib.rs:920` replace > with == in BrokerRequest::validate — **kill** — a_run_with_more_than_the_asset_descriptor_limit_is_refused: MAX+1 rows give TooManyAssetRows
- `lib.rs:920` replace > with >= in BrokerRequest::validate — **skip** — boundary at exactly MAX_ASSET_DESCRIPTORS; ceiling twin banned
- `lib.rs:947` replace > with >= in BrokerRequest::validate — **skip** — boundary at exactly MAX_ASSET_ROWS; ceiling twin banned
- `lib.rs:970` replace valid_generation_digest -> bool with false — **kill** — valid_generation_digest accepts exactly 64 lowercase hex digits: assert 64 hex ok, 63/65 chars, uppercase, non-hex all refused
- `lib.rs:971` replace && with || in valid_generation_digest — **kill** — valid_generation_digest accepts exactly 64 lowercase hex digits (same test)
- `lib.rs:970` replace == with != in valid_generation_digest — **kill** — valid_generation_digest accepts exactly 64 lowercase hex digits: assert 64 hex ok, 63/65 chars, uppercase, non-hex all refused
- `lib.rs:973` replace || with && in valid_generation_digest — **kill** — valid_generation_digest refuses non-hex bytes (same test)
- `lib.rs:1003` replace AssetIndex::get -> u8 with 0 — **kill** — an_asset_index_reports_its_value: AssetIndex::try_from(2).get() == 2
- `lib.rs:1003` replace AssetIndex::get -> u8 with 1 — **kill** — an_asset_index_reports_its_value: AssetIndex::try_from(2).get() == 2
- `lib.rs:1010` replace < with <= in <impl TryFrom<u8> for AssetIndex>::try_from — **skip** — boundary at exactly MAX_ASSET_DESCRIPTORS; ceiling twin banned
- `lib.rs:1454` replace || with && in FrameLimits::validate — **kill** — a_zero_frame_limit_is_refused: FrameLimits{max_frame_bytes:0} gives InvalidFrameLimit
- `lib.rs:1454` replace > with == in FrameLimits::validate — **skip** — boundary at exactly HARD_MAX_FRAME_BYTES; ceiling twin banned (== form: a limit above the hard max is accepted; a test with HARD_MAX+1 kills it)
- `lib.rs:1454` replace > with >= in FrameLimits::validate — **skip** — boundary at exactly HARD_MAX_FRAME_BYTES; ceiling twin banned (== form: a limit above the hard max is accepted; a test with HARD_MAX+1 kills it)
- `lib.rs:1467` replace * with + — **equivalent** — READ_CHUNK_BYTES only sizes read chunks; observable behaviour is unchanged
- `lib.rs:1832` replace > with == in BrokerClient::run — **kill** — a_run_with_more_asset_descriptors_than_the_limit_is_refused_before_send: MAX+1 descriptors give ClientError::Protocol Request
- `lib.rs:1832` replace > with < in BrokerClient::run — **kill** — a_run_with_more_asset_descriptors_than_the_limit_is_refused_before_send: MAX+1 descriptors give ClientError::Protocol Request
- `lib.rs:1832` replace > with >= in BrokerClient::run — **kill** — a_run_with_more_asset_descriptors_than_the_limit_is_refused_before_send: MAX+1 descriptors give ClientError::Protocol Request
- `lib.rs:1925` replace > with == in BrokerClient::invoke — **kill** — an_invoke_with_more_asset_descriptors_than_the_limit_is_refused_before_send: same for invoke (> with >= is boundary)
- `lib.rs:1925` replace > with >= in BrokerClient::invoke — **kill** — an_invoke_with_more_asset_descriptors_than_the_limit_is_refused_before_send: same for invoke (> with >= is boundary)
- `lib.rs:2104` replace match guard digest == expected with true in BrokerClient::generation — **kill** — a_generation_reply_for_another_digest_is_an_unexpected_response
- `lib.rs:2104` replace match guard digest == expected with false in BrokerClient::generation — **kill** — a_generation_reply_for_the_expected_digest_returns_its_outcome
- `lib.rs:2104` replace == with != in BrokerClient::generation — **kill** — a_generation_reply_for_the_expected_digest_returns_its_outcome (and mismatch refused)
- `lib.rs:2172` replace match guard error.is_absent_broker() with true in BrokerClient::connect — **kill** — a_broker_refusal_during_connect_is_returned_not_retried: only absent-broker errors are retried within the connect window
- `lib.rs:2224` replace == with != in refusal_before_credentials — **kill** — a_capacity_refusal_before_credentials_is_reported_as_remote: NotConnected plus capacity-exhausted frame gives ClientError::Remote
- `lib.rs:2232` replace == with != in refusal_before_credentials — **kill** — a_non_capacity_error_frame_before_credentials_is_a_peer_credentials_error
- `lib.rs:2377` replace <impl fmt::Display for ExchangePhase>::fmt -> fmt::Result with Ok(Default::default()) — **skip** — Display text of ExchangePhase only
- `lib.rs:2392` replace ClientError::is_absent_broker -> bool with true — **kill** — a_connect_failure_other_than_absent_broker_is_not_retried: is_absent_broker is false for PeerCredentials
- `lib.rs:2411` delete match arm Self::UnexpectedResponse in ClientError::may_have_executed — **kill** — an_unexpected_response_may_have_executed: may_have_executed() is true (agent crate matches on it)
- `lib.rs:2471` replace ResolvedBrokerSocket::into_path -> PathBuf with Default::default() — **kill** — a_resolved_socket_converts_into_its_path: into_path returns the discovered path
- `lib.rs:2516` replace ipc_socket_mode -> u32 with 0 — **covered-elsewhere** — dekopon-brokerd tests/integration/ipc_process.rs ipc_process_boundary checks the 0o660 socket mode (needs root); the 0o600 owner-only branch is unasserted (kill: a_private_parent_gives_an_owner_only_socket_mode)
- `lib.rs:2516` replace ipc_socket_mode -> u32 with 1 — **covered-elsewhere** — dekopon-brokerd tests/integration/ipc_process.rs ipc_process_boundary checks the 0o660 socket mode (needs root); the 0o600 owner-only branch is unasserted (kill: a_private_parent_gives_an_owner_only_socket_mode)
- `lib.rs:2516` replace == with != in ipc_socket_mode — **covered-elsewhere** — dekopon-brokerd tests/integration/ipc_process.rs ipc_process_boundary checks the 0o660 socket mode (needs root); the 0o600 owner-only branch is unasserted (kill: a_private_parent_gives_an_owner_only_socket_mode)
- `lib.rs:2516` replace & with | in ipc_socket_mode — **covered-elsewhere** — dekopon-brokerd tests/integration/ipc_process.rs ipc_process_boundary checks the 0o660 socket mode (needs root); the 0o600 owner-only branch is unasserted (kill: a_private_parent_gives_an_owner_only_socket_mode)
- `lib.rs:2516` replace & with ^ in ipc_socket_mode — **covered-elsewhere** — dekopon-brokerd tests/integration/ipc_process.rs ipc_process_boundary checks the 0o660 socket mode (needs root); the 0o600 owner-only branch is unasserted (kill: a_private_parent_gives_an_owner_only_socket_mode)
- `lib.rs:2536` replace BrokerSocketDiscovery::from_process -> Self with Default::default() — **kill** — socket_discovery_from_process_reads_its_environment: from_process captures DEKOPON_BROKER_SOCKET, XDG_RUNTIME_DIR and HOME (mutates process env; weigh against AGENTS test rules)
- `lib.rs:2539` delete ! in BrokerSocketDiscovery::from_process — **kill** — an_empty_broker_socket_variable_is_ignored: empty env values filter to None
- `lib.rs:2542` delete ! in BrokerSocketDiscovery::from_process — **kill** — an_empty_xdg_runtime_dir_is_ignored
- `lib.rs:2545` delete ! in BrokerSocketDiscovery::from_process — **kill** — an_empty_home_is_ignored
- `conversation.rs:160` replace && with || in Conversation::is_canonical_for — **kill** — a_thread_conversation_requires_a_thread_part: is_canonical_for rejects Thread kind with a negative id but no thread
- `conversation.rs:283` replace <impl Deserialize<'de> for ConversationKindMatch>::deserialize::<impl serde::de::Visitor<'de> for KindMatch>::expecting -> fmt::Result with Ok(Default::default()) — **skip** — serde expecting() message text only
- `conversation.rs:417` replace && with || in thread_form — **kill** — a_local_transport_has_no_thread_form: thread_form refuses Local transport even with a parent:thread value
- `conversation.rs:517` replace < with <= in canonical_negative_decimal — **skip** — boundary: i64 0 is not negative; parse already makes 0 round-trip, test would assert "0" refused as negative (cheap kill if wanted)
- `conversation.rs:521` replace canonical_signed_decimal -> bool with true — **kill** — canonical_signed_decimal refuses non-canonical text ("+1", "01", "0")
- `conversation.rs:523` replace && with || in canonical_signed_decimal — **kill** — canonical_signed_decimal refuses zero and non-canonical text
- `conversation.rs:527` replace canonical_meta_decimal -> bool with true — **kill** — canonical_meta_decimal refuses empty, leading-zero, non-digit and >64-byte text
- `conversation.rs:530` replace && with || in canonical_meta_decimal — **kill** — canonical_meta_decimal refuses non-digit text
- `conversation.rs:528` replace && with || in canonical_meta_decimal — **kill** — canonical_meta_decimal refuses empty text
- `conversation.rs:529` replace && with || in canonical_meta_decimal — **kill** — canonical_meta_decimal refuses a leading zero
- `conversation.rs:553` replace deserialize_optional_part::<impl serde::de::Visitor<'de> for OptionalPart>::expecting -> fmt::Result with Ok(Default::default()) — **skip** — serde expecting() message text only
- `conversation.rs:560` replace deserialize_optional_part::<impl serde::de::Visitor<'de> for OptionalPart>::visit_none -> Result<Self::Value, E> with Ok(Default::default()) — **equivalent** — visit_none returns Ok(None); Default of the Option value is None
- `conversation.rs:567` replace deserialize_optional_part::<impl serde::de::Visitor<'de> for OptionalPart>::visit_unit -> Result<Self::Value, E> with Ok(Default::default()) — **equivalent** — visit_unit returns Ok(None); Default of the Option value is None
- `descriptor.rs:48` replace DescriptorStream::peer_disconnected with () — **covered-elsewhere** — dekopon-brokerd tests/integration/server.rs a_peer_hangup_during_parked_invoke_still_audits_and_closes_streams (peer-disconnected) and gateway_hangup_mid_upcall; protocol tests.rs:376 also calls it
- `descriptor.rs:55` replace match guard error.kind() == io::ErrorKind::WouldBlock with true in DescriptorStream::peer_disconnected — **covered-elsewhere** — WouldBlock arm of peer_disconnected: same brokerd hangup tests; the loop would spin or return early
- `descriptor.rs:55` replace match guard error.kind() == io::ErrorKind::WouldBlock with false in DescriptorStream::peer_disconnected — **covered-elsewhere** — WouldBlock arm of peer_disconnected: same brokerd hangup tests; the loop would spin or return early
- `descriptor.rs:55` replace == with != in DescriptorStream::peer_disconnected — **covered-elsewhere** — WouldBlock arm of peer_disconnected: same brokerd hangup tests; the loop would spin or return early
- `descriptor.rs:78` replace match guard error.kind() == io::ErrorKind::WouldBlock with true in DescriptorStream::read_frame_when_ready — **skip** — WouldBlock retry arm of read_frame_when_ready; kill needs a delayed-writer test (race, internal)
- `descriptor.rs:78` replace match guard error.kind() == io::ErrorKind::WouldBlock with false in DescriptorStream::read_frame_when_ready — **skip** — WouldBlock retry arm of read_frame_when_ready; kill needs a delayed-writer test (race, internal)
- `descriptor.rs:78` replace == with != in DescriptorStream::read_frame_when_ready — **skip** — WouldBlock retry arm of read_frame_when_ready; kill needs a delayed-writer test (race, internal)
- `descriptor.rs:194` replace match guard error.kind() == io::ErrorKind::WouldBlock with false in <impl AsyncRead for DescriptorStream>::poll_read — **skip** — WouldBlock arm of poll_read; async readiness internals
- `descriptor.rs:194` replace == with != in <impl AsyncRead for DescriptorStream>::poll_read — **skip** — WouldBlock arm of poll_read; async readiness internals
- `descriptor.rs:194` replace match guard error.kind() == io::ErrorKind::WouldBlock with true in <impl AsyncRead for DescriptorStream>::poll_read — **skip** — WouldBlock arm of poll_read; async readiness internals
- `descriptor.rs:237` replace match guard error.kind() == io::ErrorKind::WouldBlock with true in <impl AsyncWrite for DescriptorStream>::poll_write — **skip** — WouldBlock arm of poll_write; async readiness internals
- `descriptor.rs:237` replace match guard error.kind() == io::ErrorKind::WouldBlock with false in <impl AsyncWrite for DescriptorStream>::poll_write — **skip** — WouldBlock arm of poll_write; async readiness internals
- `descriptor.rs:237` replace == with != in <impl AsyncWrite for DescriptorStream>::poll_write — **skip** — WouldBlock arm of poll_write; async readiness internals
- `descriptor.rs:244` replace <impl AsyncWrite for DescriptorStream>::poll_flush -> Poll<io::Result<()>> with Poll::from(Ok(())) — **equivalent** — poll_flush on a unix stream is already immediate; Ready(Ok) is the same result
- `descriptor.rs:248` replace <impl AsyncWrite for DescriptorStream>::poll_shutdown -> Poll<io::Result<()>> with Poll::from(Ok(())) — **kill** — a_shutdown_stream_delivers_eof_to_the_peer: poll_shutdown must half-close (peer read returns EOF)
