//! The records-backed `mb` grammar, tested on the host.
//!
//! These exist so the wasm binary is not the only place the command surface is
//! written down: a flag the runner accepts but the grammar rejects (or the
//! reverse) shows up here instead of at the shell.

use std::collections::BTreeMap;

use marbles::records_cli::{self, Invocation, Request};
use marbles::types::{ActorKind, Evidence};

fn argv(line: &[&str]) -> Vec<String> {
    line.iter().map(|arg| (*arg).to_string()).collect()
}

fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
        .collect()
}

fn parse(line: &[&str]) -> Invocation {
    records_cli::parse(&argv(line), &env(&[])).expect("parses")
}

fn parse_error(line: &[&str]) -> String {
    records_cli::parse(&argv(line), &env(&[])).expect_err("refused")
}

#[test]
fn the_documented_agent_workflow_parses() {
    // Every command the project's own AGENTS.md block names, in the spelling it
    // uses: `mb ready --json`, `mb claim <id> --as <who>`, `mb review <id> --pr URL`.
    assert!(matches!(
        parse(&["ready", "--json"]).request,
        Request::Ready
    ));
    assert!(matches!(
        parse(&["claim", "marbles-1", "--as", "worker-1"]).request,
        Request::Claim {
            assignee: Some(_),
            ..
        }
    ));
    assert!(matches!(
        parse(&["touch", "marbles-1"]).request,
        Request::Touch { .. }
    ));
    assert!(matches!(
        parse(&["review", "marbles-1", "--pr", "https://example.test/pr/1"]).request,
        Request::Review { pr: Some(_), .. }
    ));
    assert!(matches!(
        parse(&[
            "close",
            "marbles-1",
            "--pr",
            "https://example.test/pr/1",
            "--commit",
            "abc123",
        ])
        .request,
        Request::Close { .. }
    ));
    assert!(matches!(
        parse(&["close", "marbles-1", "--ack", "no delivery expected",]).request,
        Request::Close { .. }
    ));
    assert!(matches!(
        parse(&["show", "marbles-1"]).request,
        Request::Show { .. }
    ));
}

#[test]
fn json_is_accepted_anywhere_because_it_is_a_global_flag() {
    // `mb ready --json` is the documented contract, so `--json` cannot be a
    // leading-only flag: clap makes it global in the native CLI.
    for line in [
        vec!["--json", "ready"],
        vec!["ready", "--json"],
        vec!["--project", "soil", "list", "--all", "--json"],
        vec!["close", "marbles-1", "--json", "--ack", "done"],
    ] {
        assert!(
            records_cli::parse(&argv(&line), &env(&[])).is_ok(),
            "{line:?} should parse"
        );
    }
}

#[test]
fn a_missing_project_is_every_project_not_a_project_called_default() {
    // The store trait reads `None` as "all projects". A backend that quietly
    // invented `default` would answer an empty list and look like an empty store.
    assert_eq!(parse(&["list"]).project, None);
    assert_eq!(parse(&["ready"]).project, None);
}

#[test]
fn project_and_prefix_come_from_the_host_environment() {
    let invocation = records_cli::parse(
        &argv(&["create", "fix the thing"]),
        &env(&[
            ("MARBLES_PROJECT", "soil"),
            ("MARBLES_PREFIX", "SOIL"),
            ("MARBLES_ACTOR", "codex-1"),
        ]),
    )
    .expect("parses");
    assert_eq!(invocation.project.as_deref(), Some("soil"));
    assert_eq!(invocation.prefix.as_deref(), Some("SOIL"));
    assert_eq!(invocation.actor, "codex-1");
}

#[test]
fn flags_after_the_command_still_win_over_the_environment() {
    let invocation = records_cli::parse(
        &argv(&[
            "--actor",
            "codex-1",
            "claim",
            "marbles-1",
            "--as",
            "worker-2",
            "--ttl",
            "60",
        ]),
        &env(&[("MARBLES_ACTOR", "codex-9")]),
    )
    .expect("parses");
    assert_eq!(invocation.actor, "codex-1");
    match invocation.request {
        Request::Claim {
            assignee,
            ttl_seconds,
            kind,
            ..
        } => {
            assert_eq!(assignee.as_deref(), Some("worker-2"));
            assert_eq!(ttl_seconds, Some(60));
            assert_eq!(kind, ActorKind::Agent);
        }
        other => panic!("expected a claim, got {other:?}"),
    }
}

#[test]
fn human_claims_and_ttl_overrides_survive() {
    match parse(&["claim", "marbles-1", "--human"]).request {
        Request::Claim { kind, .. } => assert_eq!(kind, ActorKind::Human),
        other => panic!("expected a claim, got {other:?}"),
    }
}

#[test]
fn create_collects_the_flags_the_native_cli_documents() {
    let invocation = parse(&[
        "--project",
        "soil",
        "create",
        "ship it",
        "--description",
        "a b",
        "--priority",
        "1",
        "--type",
        "task",
        "--parent",
        "soil-1",
        "--labels",
        "feature,pm",
    ]);
    match invocation.request {
        Request::Create(args) => {
            assert_eq!(args.title, "ship it");
            assert_eq!(args.description, "a b");
            assert_eq!(args.priority, 1);
            assert_eq!(args.issue_type, "task");
            assert_eq!(args.parent.as_deref(), Some("soil-1"));
            assert_eq!(args.labels, vec!["feature".to_string(), "pm".to_string()]);
        }
        other => panic!("expected a create, got {other:?}"),
    }
}

#[test]
fn close_turns_each_flag_into_named_evidence() {
    match parse(&[
        "close",
        "marbles-1",
        "--pr",
        "p",
        "--commit",
        "c",
        "--ack",
        "a",
    ])
    .request
    {
        Request::Close { evidence, .. } => {
            assert_eq!(
                evidence,
                vec![Evidence::pr("p"), Evidence::commit("c"), Evidence::ack("a")]
            );
        }
        other => panic!("expected a close, got {other:?}"),
    }
}

#[test]
fn dep_and_label_take_an_action_word() {
    assert!(matches!(
        parse(&["dep", "add", "child", "parent"]).request,
        Request::Dep { add: true, .. }
    ));
    assert!(matches!(
        parse(&["label", "rm", "marbles-1", "pm"]).request,
        Request::Label { add: false, .. }
    ));
    assert!(parse_error(&["dep", "attach", "a", "b"]).contains("add or rm"));
    assert!(parse_error(&["label", "marbles-1", "pm"]).contains("add or rm"));
}

#[test]
fn transport_flags_are_refused_by_name() {
    // Accepting `--url` and talking to the local store anyway would silently put
    // writes somewhere the caller did not point at.
    for flag in ["--url", "--token", "--token-file"] {
        let error = parse_error(&[flag, "value", "list"]);
        assert!(error.contains(flag), "{error}");
        assert!(error.contains("soil:records"), "{error}");
    }
}

#[test]
fn an_unknown_command_lists_what_works() {
    let error = parse_error(&["portfolio"]);
    assert!(error.contains("portfolio"), "{error}");
    for command in ["ready", "review", "touch", "stats"] {
        assert!(error.contains(command), "{error} should name {command}");
    }
}

#[test]
fn every_documented_command_reaches_its_own_parser() {
    // The list is the contract the error message quotes. A command added to the
    // list but not to the parser would fall through to "unknown command".
    for command in records_cli::COMMANDS {
        let error = match records_cli::parse(&argv(&[command]), &env(&[])) {
            Ok(_) => continue,
            Err(error) => error,
        };
        assert!(
            !error.contains("unknown command"),
            "{command} is listed but not parsed: {error}"
        );
    }
}

#[test]
fn list_defaults_are_the_open_set_and_unlimited() {
    match parse(&["list"]).request {
        Request::List { all, limit, id } => {
            assert!(!all);
            assert_eq!(limit, 0);
            assert!(id.is_none());
        }
        other => panic!("expected a list, got {other:?}"),
    }
}

#[test]
fn missing_operands_name_their_usage() {
    for line in ["show", "claim", "close", "review", "history", "supersede"] {
        let error = parse_error(&[line]);
        assert!(error.contains("usage"), "{line}: {error}");
    }
}

#[test]
fn supersede_requires_the_replacement() {
    let error = parse_error(&["supersede", "marbles-1"]);
    assert!(error.contains("--with"), "{error}");
    assert!(matches!(
        parse(&["supersede", "marbles-1", "--with", "marbles-2"]).request,
        Request::Supersede { .. }
    ));
}

#[test]
fn a_bare_invocation_prints_usage_instead_of_guessing() {
    let error = parse_error(&["--project", "soil"]);
    assert!(error.contains("usage: mb"), "{error}");
}

#[test]
fn an_empty_variable_is_treated_as_unset() {
    // A host that found no project passes the variable empty rather than
    // omitting it; scoping to "" would answer nothing and look like an empty
    // store.
    let invocation = records_cli::parse(
        &argv(&["list"]),
        &env(&[
            ("MARBLES_PROJECT", ""),
            ("MARBLES_PREFIX", ""),
            ("MARBLES_ACTOR", ""),
        ]),
    )
    .expect("parses");
    assert_eq!(invocation.project, None);
    assert_eq!(invocation.prefix, None);
    assert_eq!(invocation.actor, "agent");
}

#[test]
fn create_without_a_project_leaves_the_decision_to_the_runner() {
    // The grammar does not invent a project for a write; the runner owns the
    // store and answers with the flag/env it actually needs.
    let invocation = parse(&["create", "ship it"]);
    assert_eq!(invocation.project, None);
    assert!(matches!(
        invocation.request,
        Request::Create(args) if args.title == "ship it"
    ));
}
