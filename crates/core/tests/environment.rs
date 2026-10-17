use collective_core::environment::*;
use collective_core::proof::{Ecosystem, ProofKind, ProofSpec};

const REV: &str = "0123456789abcdef0123456789abcdef01234567";
const HASH: &str = "sha256-ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmn+/0=";

fn nix(packages: &[&str]) -> NixEnvironment {
    NixEnvironment {
        nixpkgs: REV.into(),
        nixpkgs_hash: Some(HASH.into()),
        packages: packages.iter().map(|p| p.to_string()).collect(),
    }
}

fn env(packages: &[&str]) -> Environment {
    Environment::Nix(nix(packages))
}

#[test]
fn validation_accepts_well_formed_records() {
    assert!(Environment::Stock.validate().is_ok());
    assert!(
        env(&["rustc", "cargo", "openssl.dev", "python3Packages.numpy"])
            .validate()
            .is_ok()
    );
    assert!(env(&["_private", "gcc-unwrapped", "a.b-c_d.e9"])
        .validate()
        .is_ok());
    let mut n = nix(&["hello"]);
    n.nixpkgs_hash = None;
    assert!(Environment::Nix(n).validate().is_ok());
    let max: Vec<String> = (0..MAX_PACKAGES).map(|i| format!("p{i}")).collect();
    let max: Vec<&str> = max.iter().map(String::as_str).collect();
    assert!(env(&max).validate().is_ok());
    assert!(env(&[&"a".repeat(MAX_PACKAGE_LEN)]).validate().is_ok());
}

#[test]
fn validation_rejects_malformed_records() {
    let with_rev = |rev: &str| {
        let mut n = nix(&["hello"]);
        n.nixpkgs = rev.into();
        Environment::Nix(n)
    };
    for rev in [
        "nixos-25.05",
        "nixos-unstable",
        "master",
        "0123456789ABCDEF0123456789ABCDEF01234567", // uppercase
        "0123456789abcdef0123456789abcdef0123456",  // 39
        "0123456789abcdef0123456789abcdef012345678", // 41
        "g123456789abcdef0123456789abcdef01234567", // not hex
        "",
    ] {
        assert!(with_rev(rev).validate().is_err(), "{rev}");
    }

    let with_hash = |h: &str| {
        let mut n = nix(&["hello"]);
        n.nixpkgs_hash = Some(h.into());
        Environment::Nix(n)
    };
    for h in [
        "",
        "sha256-",
        "sha512-ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmn+/0=",
        "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmn+/0=",
        "sha256-ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmn+/0", // 43
        "sha256-ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmn+/0==", // 45
        "sha256-ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijk#mn+/0=", // bad char
        "sha256-ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmn+/01", // no padding
    ] {
        assert!(with_hash(h).validate().is_err(), "{h}");
    }

    for p in [
        "",
        "-option",
        "--impure",
        ".hidden",
        "openssl..dev",
        "openssl.",
        "a b",
        "pkgs/hello",
        "hello#x",
        "hello?y",
        "nixpkgs:hello",
        "héllo",
        &"a".repeat(MAX_PACKAGE_LEN + 1),
    ] {
        assert!(env(&["hello", p]).validate().is_err(), "{p:?}");
    }

    assert!(env(&[]).validate().is_err());
    let many: Vec<String> = (0..=MAX_PACKAGES).map(|i| format!("p{i}")).collect();
    let many: Vec<&str> = many.iter().map(String::as_str).collect();
    assert_eq!(many.len(), 65);
    assert!(env(&many).validate().is_err());
}

#[test]
fn records_have_the_documented_shape() {
    let stock: Environment = serde_json::from_str(r#"{"stock": {}}"#).unwrap();
    assert_eq!(stock, Environment::Stock);
    assert_eq!(serde_json::to_string(&stock).unwrap(), r#"{"stock":{}}"#);
    assert_eq!(Environment::default(), Environment::Stock);

    let json =
        format!(r#"{{"nix":{{"nixpkgs":"{REV}","nixpkgs_hash":null,"packages":["hello"]}}}}"#);
    let e: Environment = serde_json::from_str(&json).unwrap();
    assert_eq!(
        e,
        Environment::Nix(NixEnvironment {
            nixpkgs: REV.into(),
            nixpkgs_hash: None,
            packages: vec!["hello".into()],
        })
    );
    assert_eq!(serde_json::to_string(&e).unwrap(), json);
    // The hash may be left out.
    let short = format!(r#"{{"nix":{{"nixpkgs":"{REV}","packages":["hello"]}}}}"#);
    assert_eq!(serde_json::from_str::<Environment>(&short).unwrap(), e);

    // Fields this prover doesn't understand are refused, not ignored.
    let extra = format!(r#"{{"nix":{{"nixpkgs":"{REV}","packages":["hello"],"overlays":["x"]}}}}"#);
    assert!(serde_json::from_str::<Environment>(&extra).is_err());
    assert!(serde_json::from_str::<Environment>(r#"{"stock":{"x":1}}"#).is_err());
    assert!(serde_json::from_str::<Environment>(r#"{"docker":{}}"#).is_err());
}

#[test]
fn canonical_form_sorts_and_dedups_and_fixes_the_id() {
    let a = env(&["rustc", "cargo", "openssl.dev", "cargo"]);
    let b = env(&["openssl.dev", "rustc", "cargo"]);
    assert_eq!(
        a.clone().canonical(),
        env(&["cargo", "openssl.dev", "rustc"])
    );
    assert_eq!(a.clone().canonical(), b.clone().canonical());
    assert_eq!(a.id(), b.id());
    assert_eq!(a.id(), a.clone().canonical().id());

    let id = a.id();
    assert!(id.starts_with("env_"));
    assert_eq!(id.len(), 4 + 32);
    assert!(id[4..]
        .bytes()
        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    // Stable: the hash of the canonical JSON, not of memory layout.
    let json = serde_json::to_string(&a.clone().canonical()).unwrap();
    assert_eq!(
        id,
        format!("env_{}", &collective_core::hash::sha256_hex(json)[..32])
    );

    // Anything that changes the meaning changes the id.
    assert_ne!(a.id(), env(&["cargo", "rustc"]).id());
    let mut no_hash = nix(&["cargo", "openssl.dev", "rustc"]);
    no_hash.nixpkgs_hash = None;
    assert_ne!(a.id(), Environment::Nix(no_hash).id());
    assert_ne!(a.id(), Environment::Stock.id());
    assert_eq!(Environment::Stock.canonical(), Environment::Stock);
}

#[test]
fn flake_refs_pin_the_commit_and_hash() {
    assert_eq!(
        nix(&["hello", "openssl.dev"]).flake_refs(),
        [
            format!("github:NixOS/nixpkgs/{REV}?narHash=sha256-ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmn%2B%2F0%3D#hello"),
            format!("github:NixOS/nixpkgs/{REV}?narHash=sha256-ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmn%2B%2F0%3D#openssl.dev"),
        ]
    );
    let mut n = nix(&["hello"]);
    n.nixpkgs_hash = None;
    assert_eq!(
        n.flake_refs(),
        [format!("github:NixOS/nixpkgs/{REV}#hello")]
    );
}

#[test]
fn realisation_substitutes_only() {
    let mut n = nix(&["hello", "cargo"]);
    n.nixpkgs_hash = None;
    assert_eq!(
        n.realise_argv(),
        [
            "nix",
            "build",
            "--max-jobs",
            "0",
            "--no-link",
            "--print-out-paths",
            "--option",
            "allow-import-from-derivation",
            "false",
            &format!("github:NixOS/nixpkgs/{REV}#hello"),
            &format!("github:NixOS/nixpkgs/{REV}#cargo"),
        ]
    );
}

fn store(
    path: &str,
    bin: bool,
    lib: bool,
    include: bool,
    lib_pc: bool,
    share_pc: bool,
) -> StorePath {
    StorePath {
        path: path.into(),
        has_bin: bin,
        has_lib: lib,
        has_include: include,
        has_lib_pkgconfig: lib_pc,
        has_share_pkgconfig: share_pc,
    }
}

#[test]
fn compose_env_builds_variables_in_input_order() {
    let paths = [
        store("/nix/store/aaa-rustc", true, true, false, false, false),
        store(
            "/nix/store/bbb-openssl-dev",
            false,
            false,
            true,
            true,
            false,
        ),
        store("/nix/store/ccc-openssl", false, true, false, false, false),
        store("/nix/store/ddd-pkg-config", true, false, false, false, true),
        store("/nix/store/eee-empty", false, false, false, false, false),
        // Listed twice: counts once.
        store("/nix/store/aaa-rustc", true, true, false, false, false),
    ];
    let env = compose_env(&paths);
    let pairs: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    assert_eq!(
        pairs,
        [
            ("PATH", "/nix/store/aaa-rustc/bin:/nix/store/ddd-pkg-config/bin"),
            (
                "PKG_CONFIG_PATH",
                "/nix/store/bbb-openssl-dev/lib/pkgconfig:/nix/store/ddd-pkg-config/share/pkgconfig"
            ),
            ("NIX_CFLAGS_COMPILE", "-isystem /nix/store/bbb-openssl-dev/include"),
            ("NIX_LDFLAGS", "-L/nix/store/aaa-rustc/lib -L/nix/store/ccc-openssl/lib"),
            ("LIBRARY_PATH", "/nix/store/aaa-rustc/lib:/nix/store/ccc-openssl/lib"),
            ("CPATH", "/nix/store/bbb-openssl-dev/include"),
        ]
    );
    // Deterministic.
    assert_eq!(compose_env(&paths), env);

    // Both pkg-config dirs of one path, lib first.
    let both = compose_env(&[store("/nix/store/x", false, false, false, true, true)]);
    assert_eq!(
        both,
        [(
            "PKG_CONFIG_PATH".to_string(),
            "/nix/store/x/lib/pkgconfig:/nix/store/x/share/pkgconfig".to_string()
        )]
    );
}

#[test]
fn compose_env_with_nothing_sets_nothing() {
    assert!(compose_env(&[]).is_empty());
    assert!(compose_env(&[store("/nix/store/e", false, false, false, false, false)]).is_empty());
}

#[test]
fn proposing_needs_standing() {
    assert!(!may_propose(0.25), "a newcomer can't propose");
    assert!(!may_propose(0.49));
    assert!(may_propose(MIN_PROPOSER_TRUST));
    assert!(may_propose(1.0));
    assert!(!may_propose(f64::NAN));
    // Standing is something to lose: a refusal costs more than an adoption earns.
    const { assert!(PROPOSAL_ADOPTED_REP > 0.0 && PROPOSAL_REFUSED_REP < -PROPOSAL_ADOPTED_REP) };
}

fn suite(ecosystem: Ecosystem, environment: Environment) -> ProofSpec {
    ProofSpec {
        kind: ProofKind::Suite,
        ecosystem,
        tests: if ecosystem == Ecosystem::None {
            vec!["test.sh".into()]
        } else {
            vec![]
        },
        command: None,
        files: vec![],
        repeats: 1,
        timeout_secs: 900,
        environment,
    }
}

#[test]
fn needs_network_when_there_is_setup_to_fetch() {
    assert!(!suite(Ecosystem::None, Environment::Stock).needs_network());
    assert!(suite(Ecosystem::Cargo, Environment::Stock).needs_network());
    assert!(suite(Ecosystem::None, env(&["hello"])).needs_network());
    assert!(suite(Ecosystem::Cargo, env(&["cargo"])).needs_network());
}

#[test]
fn specs_carry_and_validate_the_environment() {
    // Absent means stock: specs from before environments still parse.
    let old: ProofSpec =
        serde_json::from_str(r#"{"kind":"suite","tests":["test.sh"],"repeats":1}"#).unwrap();
    assert_eq!(old.environment, Environment::Stock);
    assert!(old.validate().is_ok());

    let good = suite(Ecosystem::None, env(&["hello"]));
    assert!(good.validate().is_ok());
    let round: ProofSpec = serde_json::from_str(&serde_json::to_string(&good).unwrap()).unwrap();
    assert_eq!(round, good);

    let bad = suite(Ecosystem::None, env(&["-rf"]));
    let err = bad.validate().unwrap_err();
    assert!(err.starts_with("environment:"), "{err}");
}

#[test]
fn a_nix_environment_extends_the_budget() {
    let stock = suite(Ecosystem::Cargo, Environment::Stock);
    let with_nix = suite(Ecosystem::Cargo, env(&["cargo"]));
    assert_eq!(
        with_nix.budget_secs(),
        stock.budget_secs() + REALISE_TIMEOUT_SECS
    );
}

// -- proposals ----------------------------------------------------------------

use collective_core::proof::{
    grade_outcome_runs, outcome_quorum, OutcomeClaim, Quorum, RunOutcome, MAX_RUNNERS,
    PROOF_RUN_AGREED, PROOF_RUN_DISAGREED,
};

fn oc(runner: &str, trust: f64, outcome: RunOutcome) -> OutcomeClaim {
    OutcomeClaim {
        runner: runner.into(),
        trust,
        outcome,
    }
}

#[test]
fn a_proposal_is_adopted_only_by_a_trusted_quorum_passing_its_suite() {
    use RunOutcome::*;
    let trusted = [oc("a", 1.0, Passed), oc("b", 1.0, Passed)];
    assert_eq!(decide_proposal(outcome_quorum(&trusted)), None);
    let mut three = trusted.to_vec();
    three.push(oc("c", 1.0, Passed));
    assert_eq!(outcome_quorum(&three), Quorum::Settled(Passed));
    assert_eq!(
        decide_proposal(outcome_quorum(&three)),
        Some(ProposalDecision::Adopt)
    );
    // Newcomers alone settle nothing, however many agree.
    let newcomers: Vec<_> = (0..6).map(|i| oc(&format!("n{i}"), 0.25, Passed)).collect();
    assert_eq!(outcome_quorum(&newcomers), Quorum::Pending);
    // One operator counts once.
    let dup = [
        oc("a", 1.0, Passed),
        oc("a", 1.0, Passed),
        oc("a", 1.0, Passed),
    ];
    assert_eq!(outcome_quorum(&dup), Quorum::Pending);
}

#[test]
fn a_quorum_for_anything_but_passed_refuses() {
    use RunOutcome::*;
    for o in [BaseFails, Regressed, Flaky, Inconclusive] {
        let claims = [oc("a", 1.0, o), oc("b", 1.0, o), oc("c", 1.0, o)];
        assert_eq!(
            decide_proposal(outcome_quorum(&claims)),
            Some(ProposalDecision::Refuse(o)),
            "{o:?}"
        );
    }
    // An environment nobody can realise is refused, not left waiting: an
    // inconclusive run is a vote here.
    let mut claims = vec![
        oc("a", 1.0, Inconclusive),
        oc("b", 1.0, Inconclusive),
        oc("c", 1.0, Inconclusive),
    ];
    assert_eq!(
        decide_proposal(outcome_quorum(&claims)).map(|d| d.state()),
        Some("refused")
    );
    // A dissent outweighed still settles; one that isn't keeps it pending.
    claims[2] = oc("c", 1.0, Passed);
    assert_eq!(outcome_quorum(&claims), Quorum::Pending);
}

#[test]
fn runs_without_a_quorum_lapse_and_nobody_is_graded_for_them() {
    use RunOutcome::*;
    let split: Vec<_> = (0..MAX_RUNNERS)
        .map(|i| {
            oc(
                &format!("r{i}"),
                0.3,
                if i % 2 == 0 { Passed } else { BaseFails },
            )
        })
        .collect();
    assert_eq!(outcome_quorum(&split), Quorum::Split);
    let d = decide_proposal(outcome_quorum(&split)).unwrap();
    assert_eq!(d, ProposalDecision::Lapse);
    assert_eq!(d.state(), "refused");
    assert_eq!(d.outcome(), Inconclusive);
    assert_eq!(d.proposer_entry("p"), None);
}

#[test]
fn proposers_and_runners_are_graded() {
    use RunOutcome::*;
    let e = ProposalDecision::Adopt.proposer_entry("p").unwrap();
    assert_eq!(
        (e.operator_id.as_str(), e.delta, e.reason.as_str()),
        ("p", PROPOSAL_ADOPTED_REP, "environment_adopted")
    );
    let e = ProposalDecision::Refuse(BaseFails)
        .proposer_entry("p")
        .unwrap();
    assert_eq!(
        (e.delta, e.reason.as_str()),
        (PROPOSAL_REFUSED_REP, "environment_refused")
    );

    let claims = [
        oc("a", 1.0, Passed),
        oc("b", 1.0, BaseFails),
        oc("c", 1.0, Inconclusive),
    ];
    let g = grade_outcome_runs(Passed, &claims);
    assert_eq!(g.len(), 2, "inconclusive runs earn and cost nothing");
    assert_eq!(
        (g[0].operator_id.as_str(), g[0].delta),
        ("a", PROOF_RUN_AGREED)
    );
    assert_eq!(
        (g[1].operator_id.as_str(), g[1].delta),
        ("b", PROOF_RUN_DISAGREED)
    );
    assert!(
        grade_outcome_runs(Inconclusive, &claims).is_empty(),
        "an inconclusive settlement grades nobody"
    );
}

#[test]
fn proposal_limits_are_small() {
    assert_eq!(MAX_OPEN_PROPOSALS_PER_PROJECT, 3);
    assert_eq!(MAX_PROPOSALS_PER_OPERATOR_PER_DAY, 3);
}
