//! `firewall.rs`'s test module, in a sibling file per this codebase's
//! convention for test blocks that outgrow living next to the code they
//! cover (see `relay.rs`, `window.rs`).
//!
//! Classification tests exercise `classify_facts` directly with fabricated
//! facts, so exact-path matching, managed origin, port/service/package
//! restriction, profile scoping, conflicting-block precedence and the
//! default-inbound/outbound-block distinction are all ordinary, fast unit
//! tests - none of them touch a real Windows Firewall.
//!
//! `mock_script_contract_scenarios_all_pass` runs the actual production
//! PowerShell functions (not a Rust re-implementation of them) against mock
//! `*-NetFirewall*` cmdlets and a mock COM policy object shaped like the real
//! ones - see `firewall/inspect_repair_test_harness.ps1`. This is what
//! caught the real bugs a pure-Rust-DTO test could not have: `Get-
//! NetFirewallRule` objects have no `Protocol` property, `-All` cannot be
//! combined with `-Direction`/`-Enabled`/`-Action`, and returning a
//! single-element array from a PowerShell function silently collapses it to
//! a scalar with no `.Count`.
//!
//! The bounded-child tests exercise `run_bounded` against a controlled
//! fixture (this same test binary, re-invoked in a special mode), never
//! against `powershell.exe` or a real mutation.

use super::*;
use std::time::SystemTime;

#[test]
fn a_live_reused_pid_is_not_accepted_as_the_original_repair_requester() {
    // The launcher can time out while UAC is visible. Opening its PID only
    // after approval must not authorize a different process with that PID.
    assert!(RequesterHandle::open_verified(std::process::id(), 0).is_none());
}

#[test]
fn the_original_process_instance_can_be_verified_after_launch() {
    // The timestamp sent over the elevation boundary must identify the same
    // live kernel process when the elevated helper opens its own handle.
    // SAFETY: this is a non-owning pseudo-handle for the current test process.
    let created = process_creation(unsafe { GetCurrentProcess() }).unwrap();
    assert!(RequesterHandle::open_verified(std::process::id(), created)
        .unwrap()
        .is_alive());
}

#[test]
fn a_policy_child_is_stopped_when_the_requester_exits_during_work() {
    let checks = std::sync::atomic::AtomicUsize::new(0);
    let started = Instant::now();
    let run = run_bounded_while(fixture_command("hang"), b"", Duration::from_secs(5), || {
        checks.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 3
    });
    assert!(run.exit_code.is_some(), "the started child must be reaped");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "requester loss must stop work before the ordinary deadline"
    );
}

#[test]
fn a_cancelled_policy_request_never_starts_a_child() {
    // A closed requester must not launch even a read stage of the elevated
    // operation, irrespective of whether its PID has since been reused.
    let mut command = Command::new(powershell_path(&system32_path().unwrap()));
    command.args(["-NoProfile", "-NonInteractive", "-Command", "exit 0"]);
    let run = run_bounded_while(command, b"", Duration::from_secs(2), || false);
    assert!(
        run.exit_code.is_none(),
        "a child ran after the requester was gone"
    );
}

#[test]
fn application_matching_uses_windows_case_rules_for_unicode_user_paths() {
    // PowerShell had already matched this path using ordinal Unicode rules;
    // an ASCII-only second check incorrectly treated it as another program.
    assert!(paths_match(
        r"C:\Users\ÉLISE\Orange.exe",
        r"c:\users\élise\orange.exe"
    ));
    assert!(!paths_match(
        r"C:\Users\ÉLISE\Orange.exe",
        r"c:\users\élise\other.exe"
    ));
}

#[test]
fn pipe_completion_waits_until_its_reader_and_handle_are_released() {
    // Receiving bytes from a channel is not a thread join: the reader can
    // still own the pipe after sending its result. Hold Drop to expose that gap.
    struct HeldReader {
        entered: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
    }
    impl Read for HeldReader {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Ok(0)
        }
    }
    impl Drop for HeldReader {
        fn drop(&mut self) {
            let _ = self.entered.send(());
            let _ = self.release.recv_timeout(Duration::from_secs(2));
        }
    }
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let pipe = bounded_reader(
        HeldReader {
            entered: entered_tx,
            release: release_rx,
        },
        16,
    );
    let (finished_tx, finished_rx) = mpsc::channel();
    let finishing = thread::spawn(move || {
        let _ = finished_tx.send(pipe.finish());
    });
    entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let premature = finished_rx.recv_timeout(Duration::from_millis(100)).is_ok();
    let _ = release_tx.send(());
    finishing.join().unwrap();
    assert!(
        !premature,
        "the reader still owned its input when completion returned"
    );
}

// ---------------------------------------------------------------------------
// classify_facts
// ---------------------------------------------------------------------------

const EXE: &str = r"C:\Program Files\Orange\orange.exe";
const OTHER_EXE: &str = r"C:\Program Files\Contoso\contoso.exe";

fn profile(
    name: &str,
    enabled: bool,
    block_all_inbound: bool,
    outbound_block: bool,
) -> ProfileFact {
    ProfileFact {
        name: name.to_string(),
        enabled,
        block_all_inbound,
        outbound_block,
    }
}

fn all_profiles_enabled() -> Vec<ProfileFact> {
    vec![
        profile("Domain", true, false, false),
        profile("Private", true, false, false),
        profile("Public", true, false, false),
    ]
}

fn whole_app_block_rule(program: &str) -> AppRuleFact {
    AppRuleFact {
        program: program.to_string(),
        enabled: true,
        action: "Block".to_string(),
        protocol: "TCP".to_string(),
        profiles: vec!["Private".to_string(), "Public".to_string()],
        restricted: false,
    }
}

fn facts(profiles: Vec<ProfileFact>, app_rules: Vec<AppRuleFact>) -> FirewallFacts {
    FirewallFacts {
        active_profiles: vec!["Private".to_string()],
        profiles,
        app_rules,
        managed_origin: false,
    }
}

#[test]
fn an_unrestricted_local_whole_app_block_on_the_exact_exe_is_repairable() {
    let data = facts(all_profiles_enabled(), vec![whole_app_block_rule(EXE)]);
    let result = classify_facts(&data, EXE);
    assert_eq!(result.status, FirewallStatus::Blocked);
    assert!(result.repairable);
    assert!(result.detail.len() <= DETAIL_LIMIT);
}

#[test]
fn a_block_rule_for_a_different_program_path_is_not_ours_even_with_a_similar_name() {
    // The "other app" trap: a rule that looks like it could be Orange's (same
    // kind of whole-app Block, same active profile) but whose Program is a
    // different executable must never be treated as evidence against Orange.
    let data = facts(
        all_profiles_enabled(),
        vec![whole_app_block_rule(OTHER_EXE)],
    );
    let result = classify_facts(&data, EXE);
    assert_eq!(result.status, FirewallStatus::Clear);
    assert!(!result.repairable);
}

#[test]
fn exact_path_matching_is_case_insensitive_but_still_exact() {
    let mut rule = whole_app_block_rule(EXE);
    rule.program = EXE.to_ascii_uppercase();
    let data = facts(all_profiles_enabled(), vec![rule]);
    let result = classify_facts(&data, EXE);
    assert_eq!(result.status, FirewallStatus::Blocked);
    assert!(result.repairable);
}

#[test]
fn a_rule_scoped_to_an_inactive_profile_only_is_not_evidence_of_a_block() {
    let mut rule = whole_app_block_rule(EXE);
    rule.profiles = vec!["Domain".to_string()]; // active profile is "Private"
    let data = facts(all_profiles_enabled(), vec![rule]);
    let result = classify_facts(&data, EXE);
    assert_eq!(result.status, FirewallStatus::Clear);
}

#[test]
fn a_rule_restricted_by_port_is_blocked_but_not_repairable() {
    let mut rule = whole_app_block_rule(EXE);
    rule.restricted = true;
    let data = facts(all_profiles_enabled(), vec![rule]);
    let result = classify_facts(&data, EXE);
    assert_eq!(result.status, FirewallStatus::Blocked);
    assert!(!result.repairable);
}

#[test]
fn managed_origin_is_managed_not_repairable() {
    let mut data = facts(all_profiles_enabled(), vec![whole_app_block_rule(EXE)]);
    data.managed_origin = true;
    let result = classify_facts(&data, EXE);
    assert_eq!(result.status, FirewallStatus::Managed);
    assert!(!result.repairable);
}

#[test]
fn managed_origin_takes_precedence_over_an_otherwise_repairable_local_block() {
    // Conflicting block: never claim a local repair is available just
    // because a matching rule happens to look local, when policy also
    // affects the app - repairing the local rule would not fix anything and
    // would be reported dishonestly as a success.
    let mut data = facts(all_profiles_enabled(), vec![whole_app_block_rule(EXE)]);
    data.managed_origin = true;
    let result = classify_facts(&data, EXE);
    assert_eq!(result.status, FirewallStatus::Managed);
    assert!(!result.repairable);
}

#[test]
fn block_all_inbound_with_no_matching_app_rule_is_managed_not_blocked() {
    // "distinguish a definite matching application block from default
    // inbound policy": Block-all-incoming-connections with nothing naming
    // Orange must never be reported as an Orange-specific Blocked result.
    let mut profiles = all_profiles_enabled();
    profiles[1] = profile("Private", true, true, false); // block-all-inbound on the active profile
    let data = facts(profiles, vec![]);
    let result = classify_facts(&data, EXE);
    assert_eq!(result.status, FirewallStatus::Managed);
    assert!(!result.repairable);
}

#[test]
fn block_all_inbound_takes_precedence_over_a_matching_repairable_local_block() {
    let mut profiles = all_profiles_enabled();
    profiles[1] = profile("Private", true, true, false);
    let data = facts(profiles, vec![whole_app_block_rule(EXE)]);
    let result = classify_facts(&data, EXE);
    assert_eq!(result.status, FirewallStatus::Managed);
    assert!(!result.repairable);
}

#[test]
fn a_default_outbound_block_with_nothing_else_is_managed_not_clear() {
    let mut profiles = all_profiles_enabled();
    profiles[1] = profile("Private", true, false, true);
    let data = facts(profiles, vec![]);
    let result = classify_facts(&data, EXE);
    assert_eq!(result.status, FirewallStatus::Managed);
    assert!(!result.repairable);
}

#[test]
fn ordinary_default_inbound_policy_without_any_block_condition_is_clear() {
    // DefaultInboundAction=Block alone (the normal state of Private/Public,
    // not surfaced as a fact at all here) is not proof of anything; only
    // block-all-inbound, default-outbound-block or a real rule is.
    let data = facts(all_profiles_enabled(), vec![]);
    let result = classify_facts(&data, EXE);
    assert_eq!(result.status, FirewallStatus::Clear);
}

#[test]
fn a_disabled_firewall_on_every_active_profile_is_clear() {
    let profiles = vec![
        profile("Domain", true, false, false),
        profile("Private", false, false, false),
        profile("Public", true, false, false),
    ];
    let data = facts(profiles, vec![whole_app_block_rule(EXE)]);
    let result = classify_facts(&data, EXE);
    assert_eq!(result.status, FirewallStatus::Clear);
}

#[test]
fn no_active_profile_reported_is_unknown_not_clear() {
    let mut data = facts(all_profiles_enabled(), vec![]);
    data.active_profiles.clear();
    let result = classify_facts(&data, EXE);
    assert_eq!(result.status, FirewallStatus::Unknown);
    assert!(!result.repairable);
}

#[test]
fn an_unrecognized_active_profile_name_is_unknown() {
    let mut data = facts(all_profiles_enabled(), vec![]);
    data.active_profiles = vec!["Guest".to_string()];
    let result = classify_facts(&data, EXE);
    assert_eq!(result.status, FirewallStatus::Unknown);
}

#[test]
fn an_active_profile_missing_from_the_profile_facts_is_unknown_not_firewall_off() {
    // Validates DTO coverage: an active profile with no corresponding
    // `profiles[]` entry must never be silently read as "that profile has
    // the firewall off".
    let data = facts(vec![profile("Private", true, false, false)], vec![])
        .tap_active_profiles(vec!["Private".to_string(), "Public".to_string()]);
    let result = classify_facts(&data, EXE);
    assert_eq!(result.status, FirewallStatus::Unknown);
}

trait TapActiveProfiles {
    fn tap_active_profiles(self, active: Vec<String>) -> Self;
}

impl TapActiveProfiles for FirewallFacts {
    fn tap_active_profiles(mut self, active: Vec<String>) -> Self {
        self.active_profiles = active;
        self
    }
}

#[test]
fn a_disabled_block_rule_is_never_treated_as_current_evidence() {
    let mut rule = whole_app_block_rule(EXE);
    rule.enabled = false;
    let data = facts(all_profiles_enabled(), vec![rule]);
    let result = classify_facts(&data, EXE);
    assert_eq!(result.status, FirewallStatus::Clear);
}

// ---------------------------------------------------------------------------
// paths_match / parse_script_outcome
// ---------------------------------------------------------------------------

#[test]
fn path_matching_ignores_case_and_surrounding_whitespace() {
    assert!(paths_match(
        " C:\\Orange\\orange.exe ",
        "c:\\orange\\orange.exe"
    ));
    assert!(!paths_match(
        r"C:\Orange\orange.exe",
        r"C:\Other\orange.exe"
    ));
}

fn facts_json() -> &'static str {
    r#"{"activeProfiles":["Public"],"profiles":[{"name":"Domain","enabled":true,"blockAllInbound":false,"outboundBlock":false},{"name":"Private","enabled":true,"blockAllInbound":false,"outboundBlock":false},{"name":"Public","enabled":true,"blockAllInbound":false,"outboundBlock":false}],"appRules":[],"managedOrigin":false}"#
}

#[test]
fn exit_zero_with_a_well_formed_facts_payload_is_facts() {
    match parse_script_outcome(0, facts_json().as_bytes()) {
        Ok(ScriptOutcome::Facts(_)) => {}
        other => panic!("expected Facts, got {other:?}"),
    }
}

#[test]
fn exit_zero_with_a_cancelled_marker_is_requester_cancelled() {
    let payload = br#"{"schema":2,"requesterCancelled":true}"#;
    match parse_script_outcome(0, payload) {
        Ok(ScriptOutcome::RequesterCancelled) => {}
        other => panic!("expected RequesterCancelled, got {other:?}"),
    }
}

#[test]
fn exit_zero_with_neither_recognized_shape_is_malformed() {
    let result = parse_script_outcome(0, b"not json at all");
    assert_eq!(result, Err("malformed"));
}

#[test]
fn exit_zero_with_truncated_json_is_malformed_rather_than_partially_trusted() {
    let truncated = &facts_json().as_bytes()[..facts_json().len() - 30];
    let result = parse_script_outcome(0, truncated);
    assert_eq!(result, Err("malformed"));
}

#[test]
fn exit_one_with_a_script_error_payload_is_carried_through() {
    let payload = br#"{"schema":2,"error":"exception"}"#;
    match parse_script_outcome(1, payload) {
        Ok(ScriptOutcome::ScriptError(reason)) => assert_eq!(reason, "exception"),
        other => panic!("expected ScriptError, got {other:?}"),
    }
}

#[test]
fn exit_one_with_a_non_error_shaped_payload_is_malformed() {
    let result = parse_script_outcome(1, facts_json().as_bytes());
    assert_eq!(result, Err("malformed"));
}

#[test]
fn any_other_exit_code_is_rejected_without_trusting_its_output() {
    // Even if the body happens to look like well-formed facts JSON, an exit
    // code this module was not designed to see must never be trusted: it
    // could be a PowerShell host crash that coincidentally left something
    // parseable behind.
    let result = parse_script_outcome(2, facts_json().as_bytes());
    assert_eq!(result, Err("unexpected-exit"));
}

impl PartialEq for ScriptOutcome {
    fn eq(&self, other: &Self) -> bool {
        matches!(
            (self, other),
            (ScriptOutcome::Facts(_), ScriptOutcome::Facts(_))
                | (
                    ScriptOutcome::RequesterCancelled,
                    ScriptOutcome::RequesterCancelled
                )
        ) || matches!(
            (self, other),
            (ScriptOutcome::ScriptError(a), ScriptOutcome::ScriptError(b)) if a == b
        )
    }
}

// ---------------------------------------------------------------------------
// Repair planning and postcondition
// ---------------------------------------------------------------------------

fn inspection_of(status: FirewallStatus, repairable: bool) -> Inspection {
    inspection(status, "detail", repairable)
}

#[test]
fn repair_planning_matches_each_inspection_outcome_to_the_right_action() {
    assert_eq!(
        plan_repair(&inspection_of(FirewallStatus::Clear, false)),
        RepairAction::NothingToChange
    );
    assert_eq!(
        plan_repair(&inspection_of(FirewallStatus::Blocked, true)),
        RepairAction::Proceed
    );
    assert_eq!(
        plan_repair(&inspection_of(FirewallStatus::Blocked, false)),
        RepairAction::NotRepairable
    );
    assert_eq!(
        plan_repair(&inspection_of(FirewallStatus::Managed, false)),
        RepairAction::NotRepairable
    );
    assert_eq!(
        plan_repair(&inspection_of(FirewallStatus::Unknown, false)),
        RepairAction::NotRepairable
    );
}

#[test]
fn repair_is_only_reported_successful_when_the_postcondition_shows_clear() {
    assert_eq!(
        repair_outcome_code(&inspection_of(FirewallStatus::Clear, false)),
        REPAIR_CHANGED_OR_VERIFIED
    );
    // A managed block surviving the local repair must never be reported as
    // fixed - the script only ever touches the local rule it was scoped to.
    assert_eq!(
        repair_outcome_code(&inspection_of(FirewallStatus::Managed, false)),
        REPAIR_FAILED
    );
    assert_eq!(
        repair_outcome_code(&inspection_of(FirewallStatus::Blocked, false)),
        REPAIR_FAILED
    );
}

// ---------------------------------------------------------------------------
// Exe path normalization / trusted system paths
// ---------------------------------------------------------------------------

#[test]
fn the_normalized_current_exe_has_no_verbatim_prefix_and_exists() {
    let exe = normalized_current_exe().expect("current_exe should resolve in a test binary");
    assert!(!exe.starts_with(r"\\?\"));
    assert!(std::path::Path::new(&exe).is_file());
}

#[test]
fn the_system32_path_is_resolved_from_the_os_not_an_environment_variable() {
    let system32 = system32_path().expect("GetSystemDirectoryW should resolve on any Windows host");
    assert!(system32.is_dir());
    let powershell = powershell_path(&system32);
    assert!(powershell.is_file(), "expected {powershell:?} to exist");
}

// ---------------------------------------------------------------------------
// Requester liveness, held across a repair flow
// ---------------------------------------------------------------------------

#[test]
fn a_requester_handle_reports_alive_then_not_alive_after_the_process_exits() {
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", FIXTURE_TEST_NAME, "--nocapture"])
        .env(FIXTURE_ENV, "hang")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn fixture");

    let handle = RequesterHandle::open(child.id()).expect("open should succeed for a live child");
    assert!(handle.is_alive());

    child.kill().expect("kill fixture");
    child.wait().expect("reap fixture");
    assert!(!handle.is_alive());
}

#[test]
fn a_requester_handle_cannot_be_opened_for_a_pid_that_does_not_exist() {
    // Not a guarantee any given number is free, but PIDs this large are not
    // valid on a normal desktop session, and `OpenProcess` fails cleanly
    // either way; this only pins that failure to open resolves to `None`
    // rather than panicking or fabricating a handle.
    assert!(RequesterHandle::open(0xFFFF_FFF0).is_none());
}

// ---------------------------------------------------------------------------
// Bounded child execution, against a controlled fixture (never powershell.exe
// and never a real firewall mutation).
// ---------------------------------------------------------------------------

const FIXTURE_ENV: &str = "ORANGE_FIREWALL_FIXTURE";
const FIXTURE_MARKER_ENV: &str = "ORANGE_FIREWALL_FIXTURE_MARKER";
const FIXTURE_TEST_NAME: &str = "firewall::tests::firewall_bounded_child_fixture";

/// Not a real test of `firewall.rs` behaviour: this is the controlled
/// subprocess target the tests below re-invoke through `run_bounded`. It only
/// does anything when its selecting environment variable is set, exactly like
/// `test_support::run_in_bounded_subprocess`'s child half; run normally as
/// part of the suite, it is an immediate no-op pass.
#[test]
fn firewall_bounded_child_fixture() {
    match std::env::var(FIXTURE_ENV).ok().as_deref() {
        Some("echo") => println!("fixture-output"),
        Some("hang") => loop {
            std::thread::sleep(Duration::from_secs(30));
        },
        Some("big-output") => {
            // Comfortably past the 16 KiB retained cap.
            print!("{}", "x".repeat(64 * 1024));
        }
        Some("ignore-stdin-then-exit") => {
            // Never reads stdin at all; exits quickly regardless. Proves a
            // large, unread stdin payload cannot make `run_bounded` itself
            // wait out the whole deadline.
            std::thread::sleep(Duration::from_millis(50));
        }
        Some("spawn-grandchild") => {
            let marker = std::env::var(FIXTURE_MARKER_ENV).expect("marker path");
            let exe = std::env::current_exe().expect("current exe");
            let _ = Command::new(exe)
                .args(["--exact", FIXTURE_TEST_NAME, "--nocapture"])
                .env(FIXTURE_ENV, "heartbeat")
                .env(FIXTURE_MARKER_ENV, marker)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
            loop {
                std::thread::sleep(Duration::from_secs(30));
            }
        }
        Some("heartbeat") => {
            let marker = std::env::var(FIXTURE_MARKER_ENV).expect("marker path");
            loop {
                let now = SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis();
                let _ = std::fs::write(&marker, now.to_string());
                std::thread::sleep(Duration::from_millis(40));
            }
        }
        _ => {}
    }
}

fn fixture_command(mode: &str) -> Command {
    let exe = std::env::current_exe().expect("current exe");
    let mut command = Command::new(exe);
    command
        .args(["--exact", FIXTURE_TEST_NAME, "--nocapture"])
        .env(FIXTURE_ENV, mode);
    command
}

#[test]
fn a_bounded_child_that_finishes_in_time_returns_its_output_exit_code_and_is_not_marked_timed_out()
{
    let run = run_bounded(fixture_command("echo"), b"", Duration::from_secs(10));
    assert!(!run.timed_out);
    assert!(!run.truncated);
    assert_eq!(run.exit_code, Some(0));
    assert!(String::from_utf8_lossy(&run.stdout).contains("fixture-output"));
}

#[test]
fn a_bounded_child_past_its_deadline_is_killed_and_reaped() {
    let run = run_bounded(fixture_command("hang"), b"", Duration::from_millis(300));
    assert!(run.timed_out);
}

#[test]
fn retained_stdout_never_exceeds_the_bounded_cap_and_is_flagged_truncated() {
    let run = run_bounded(fixture_command("big-output"), b"", Duration::from_secs(10));
    assert!(!run.timed_out);
    assert_eq!(run.stdout.len(), OUTPUT_LIMIT);
    assert!(run.truncated);
}

#[test]
fn a_child_that_never_reads_a_large_stdin_payload_does_not_hold_the_deadline_hostage() {
    // Larger than a typical OS pipe buffer, fed to a child that never reads
    // any of it: with the write on its own thread, `run_bounded` must still
    // return promptly once the child exits on its own, rather than blocking
    // on `write_all` for the whole deadline.
    let large_input = vec![b'a'; 4 * 1024 * 1024];
    let started = Instant::now();
    let run = run_bounded(
        fixture_command("ignore-stdin-then-exit"),
        &large_input,
        Duration::from_secs(10),
    );
    assert!(!run.timed_out);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "expected the child's own quick exit to end the run, not the 10s deadline"
    );
}

#[test]
fn a_killed_bounded_child_takes_its_grandchild_down_with_it() {
    let dir = tempfile::tempdir().expect("temp dir");
    let marker = dir.path().join("heartbeat.txt");

    let mut command = fixture_command("spawn-grandchild");
    command.env(FIXTURE_MARKER_ENV, marker.to_str().expect("utf8 path"));

    let run = run_bounded(command, b"", Duration::from_millis(300));
    assert!(run.timed_out);

    // Give the grandchild time to have written at least once before the kill
    // could plausibly have reached it, then confirm the marker stops moving:
    // proof the Job Object's KILL_ON_JOB_CLOSE reached the grandchild, not
    // just the immediate (PowerShell-shaped) child `run_bounded` spawned.
    std::thread::sleep(Duration::from_millis(200));
    let read_marker = || std::fs::read_to_string(&marker).ok();
    let after_kill = read_marker();
    std::thread::sleep(Duration::from_millis(400));
    let later = read_marker();
    assert_eq!(
        after_kill, later,
        "grandchild kept writing after the bounded run ended"
    );
}

// ---------------------------------------------------------------------------
// Native smoke test: read-only, against whatever this machine's real Windows
// Firewall reports. Never mutates anything - only `inspect()` is exercised
// here, never `repair`. Deliberately lenient about *which* status comes back
// (that depends on the machine this happens to run on); it only pins the
// bounded, well-formed shape of the contract every caller depends on.
// ---------------------------------------------------------------------------

#[test]
fn inspect_completes_quickly_and_returns_a_well_formed_result_on_this_machine() {
    let started = Instant::now();
    let result = inspect();
    // Generous relative to the script's own 15-second bound: this only
    // guards against the whole call silently blocking forever.
    assert!(started.elapsed() < Duration::from_secs(30));
    assert!(result.detail.len() <= DETAIL_LIMIT);
    assert!(!result.detail.is_empty());
    if result.repairable {
        assert_eq!(result.status, FirewallStatus::Blocked);
    }
}

// ---------------------------------------------------------------------------
// Real PowerShell contract tests: mock cmdlets, real-shaped objects, the
// production script's own functions. Never touches the real firewall.
// ---------------------------------------------------------------------------

#[test]
fn mock_script_contract_scenarios_all_pass() {
    let harness =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src/firewall/inspect_repair_test_harness.ps1");
    assert!(harness.is_file(), "expected {harness:?} to exist");

    let system32 = system32_path().expect("system32 path");
    let mut command = Command::new(powershell_path(&system32));
    command.args([
        "-NoProfile",
        "-NonInteractive",
        "-ExecutionPolicy",
        "Bypass",
        "-File",
    ]);
    command.arg(&harness);

    let run = run_bounded(command, b"", Duration::from_secs(30));
    let output = String::from_utf8_lossy(&run.stdout);
    assert!(!run.timed_out, "harness timed out:\n{output}");
    assert!(!run.truncated, "harness output was truncated:\n{output}");
    assert_eq!(
        run.exit_code,
        Some(0),
        "one or more mock script scenarios failed:\n{output}"
    );
    assert!(output.contains("all scenarios passed"));
}
