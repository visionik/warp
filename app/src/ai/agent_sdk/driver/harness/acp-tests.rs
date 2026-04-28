use std::path::{Path, PathBuf};

use warp_cli::agent::Harness;

use super::super::super::AgentDriverError;
use super::super::{harness_kind, HarnessKind};
use super::{resolve_safe_path, AcpHarness, ThirdPartyHarness};

fn assert_harness_setup_failed(err: &AgentDriverError) -> (&str, &str) {
    match err {
        AgentDriverError::HarnessSetupFailed { harness, reason } => (harness, reason),
        other => panic!("expected HarnessSetupFailed, got: {other}"),
    }
}

#[test]
fn new_returns_error_when_command_is_none() {
    let err = AcpHarness::new(None).unwrap_err();
    let (harness, reason) = assert_harness_setup_failed(&err);
    assert_eq!(harness, "acp");
    assert!(
        reason.contains("requires a `command`"),
        "unexpected reason: {reason}"
    );
}

#[test]
fn new_returns_error_when_command_is_blank() {
    let err = AcpHarness::new(Some("   ".into())).unwrap_err();
    let (_, reason) = assert_harness_setup_failed(&err);
    assert!(reason.contains("requires a `command`"));
}

#[test]
fn new_accepts_a_non_empty_command() {
    let harness = AcpHarness::new(Some("acp-agent --foo".into())).unwrap();
    assert_eq!(harness.harness(), Harness::Acp);
}

#[test]
fn validate_fails_when_binary_is_not_on_path() {
    let harness = AcpHarness::new(Some("__nonexistent_acp_agent_xyz__".into())).unwrap();
    let err = harness.validate().unwrap_err();
    let (harness_label, reason) = assert_harness_setup_failed(&err);
    assert_eq!(harness_label, "acp");
    assert!(
        reason.contains("__nonexistent_acp_agent_xyz__"),
        "expected program name in reason, got: {reason}"
    );
    assert!(reason.contains("not found"));
}

#[cfg(not(windows))]
#[test]
fn validate_succeeds_for_known_binary_with_args() {
    // `ls` is virtually guaranteed to be on PATH on Unix CI; the args are a
    // pure parse-shape check so any flag will do.
    let harness = AcpHarness::new(Some("ls --color=never".into())).unwrap();
    assert!(harness.validate().is_ok());
}

#[test]
fn validate_rejects_unparseable_command() {
    // Unmatched single quote — shlex returns None on parse failure.
    let harness = AcpHarness::new(Some("acp-agent 'unterminated".into())).unwrap();
    let err = harness.validate().unwrap_err();
    let (_, reason) = assert_harness_setup_failed(&err);
    assert!(
        reason.contains("not a valid shell expression"),
        "unexpected reason: {reason}"
    );
}

#[test]
fn harness_kind_acp_requires_command() {
    let err = harness_kind(Harness::Acp, None).unwrap_err();
    let (harness, _) = assert_harness_setup_failed(&err);
    assert_eq!(harness, "acp");
}

#[test]
fn harness_kind_acp_with_command_returns_third_party() {
    let kind = harness_kind(Harness::Acp, Some("my-acp-agent".into())).unwrap();
    match kind {
        HarnessKind::ThirdParty(harness) => {
            assert_eq!(harness.harness(), Harness::Acp);
        }
        HarnessKind::Oz | HarnessKind::Unsupported(_) => {
            panic!("expected ThirdParty for Harness::Acp")
        }
    }
}

#[test]
fn harness_kind_oz_ignores_acp_command() {
    let kind = harness_kind(Harness::Oz, Some("ignored".into())).unwrap();
    assert_eq!(kind.harness(), Harness::Oz);
}

// ── resolve_safe_path tests ──────────────────────────────────────────────────

fn wd() -> PathBuf {
    PathBuf::from("/home/user/project")
}

#[test]
fn safe_path_relative_inside_wd() {
    let p = resolve_safe_path("src/main.rs", &wd()).unwrap();
    assert_eq!(p, Path::new("/home/user/project/src/main.rs"));
}

#[test]
fn safe_path_dotslash_inside_wd() {
    let p = resolve_safe_path("./README.md", &wd()).unwrap();
    assert_eq!(p, Path::new("/home/user/project/README.md"));
}

#[test]
fn safe_path_absolute_inside_wd() {
    let p = resolve_safe_path("/home/user/project/file.txt", &wd()).unwrap();
    assert_eq!(p, Path::new("/home/user/project/file.txt"));
}

#[test]
fn safe_path_dotdot_escape_rejected() {
    let err = resolve_safe_path("../../etc/passwd", &wd()).unwrap_err();
    assert!(
        err.to_string().contains("escapes the working directory"),
        "unexpected error: {err}"
    );
}

#[test]
fn safe_path_absolute_escape_rejected() {
    let err = resolve_safe_path("/etc/passwd", &wd()).unwrap_err();
    assert!(
        err.to_string().contains("escapes the working directory"),
        "unexpected error: {err}"
    );
}

#[test]
fn safe_path_dotdot_then_back_inside_wd() {
    // Goes out and comes back — should resolve to the working dir root.
    let p = resolve_safe_path("subdir/../file.txt", &wd()).unwrap();
    assert_eq!(p, Path::new("/home/user/project/file.txt"));
}

#[test]
fn safe_path_deeply_nested_is_ok() {
    let p = resolve_safe_path("a/b/c/d/e.txt", &wd()).unwrap();
    assert_eq!(p, Path::new("/home/user/project/a/b/c/d/e.txt"));
}
