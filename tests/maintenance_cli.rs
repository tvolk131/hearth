//! Command-line boundaries are exercised without a server profile or connection.
use std::process::{Command, Output};

fn cli(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_pve-devctl"))
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn reviewed_maintenance_subcommands_are_discoverable_without_connecting() {
    for (args, expected) in [
        (vec!["vm", "grow", "--help"], "--root-partition-uuid"),
        (vec!["vm", "enroll", "--help"], "--plan"),
        (vec!["operations", "inspect", "--help"], "--operation-id"),
        (vec!["operations", "recover", "--help"], "--inspection"),
        (
            vec!["operations", "recover", "--help"],
            "acknowledge-failed-start",
        ),
        (vec!["migration", "--help"], "authorize-destination"),
        (vec!["migration", "renew", "--help"], "--source-grant"),
        (vec!["migration", "abort", "--help"], "--source-abort"),
    ] {
        let output = cli(&args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains(expected));
    }
}

#[test]
fn unsafe_or_incomplete_maintenance_commands_never_reach_profile_loading() {
    for args in [
        vec![
            "vm",
            "grow",
            "--request-id",
            "one",
            "--vm-id",
            "9001",
            "--expected-disk-mib",
            "4096",
            "--target-disk-mib",
            "8192",
        ],
        vec![
            "vm",
            "enroll",
            "--request-id",
            "one",
            "--plan",
            "/missing-review.json",
        ],
        vec![
            "operations",
            "recover",
            "--request-id",
            "one",
            "--inspection",
            "/missing-review.json",
            "--kind",
            "abort-preparation",
        ],
        vec![
            "operations",
            "recover",
            "--request-id",
            "one",
            "--inspection",
            "/missing-review.json",
            "--kind",
            "acknowledge-failed-start",
        ],
        vec!["migration", "abort", "--migration-id", "one"],
        vec![
            "migration",
            "finish",
            "--migration-id",
            "one",
            "--confirm-guest-verified",
        ],
    ] {
        let output = cli(&args);
        assert_eq!(output.status.code(), Some(1));
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("requires --confirm"), "{error}");
        assert!(!error.contains("--profile is required"));
        assert!(output.stdout.is_empty());
    }
}

#[test]
fn unknown_force_actions_and_invalid_root_selectors_are_rejected_by_the_parser() {
    for args in [
        vec![
            "operations",
            "recover",
            "--request-id",
            "one",
            "--inspection",
            "review.json",
            "--kind",
            "force",
            "--confirm-recovery",
        ],
        vec![
            "vm",
            "grow-plan",
            "--vm-id",
            "9001",
            "--target-disk-mib",
            "8192",
            "--root-partition-uuid",
            "not-a-uuid",
        ],
        vec![
            "migration",
            "renew",
            "--request-id",
            "renew-once",
            "--migration-id",
            "one",
            "--expires-in-seconds",
            "86401",
        ],
    ] {
        let output = cli(&args);
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
    }
}
