use std::collections::HashSet;

use collective_core::challenge::*;
use collective_core::jury::*;
use collective_core::net;
use collective_core::work::{solve_work, verify_work};

const T0: i64 = 1_000_000;
const DAY: i64 = PARAMS.round_ms;

fn open(bond: f64) -> Challenge {
    Challenge::open(OpenChallenge {
        id: "c1",
        patch_id: "p1",
        flagged_by: "finder",
        severity: Severity::High,
        evidence_commitment: "abc",
        bond,
        now: T0,
    })
    .unwrap()
}

fn ctx() -> SettlementContext {
    SettlementContext {
        author_operator: "author".into(),
        author_balance: 100.0,
        adopters: vec![
            Exposure {
                operator_id: "a1".into(),
                amount: 1.0,
            },
            Exposure {
                operator_id: "a2".into(),
                amount: 2.0,
            },
        ],
        attestations: vec![Exposure {
            operator_id: "rubberstamp".into(),
            amount: 50.0,
        }],
        pool: 10_000.0,
        found_by_assigned_review: false,
    }
}

fn resolution(verdict: Verdict, intent: Intent) -> Resolution {
    Resolution {
        verdict,
        via: ResolvedVia::Proof,
        severity: Severity::High,
        intent,
        at: T0 + 5,
    }
}

#[test]
fn bonded_flags_quarantine_bare_reports_do_not() {
    assert!(open(10.0).quarantined());
    assert!(!open(0.0).quarantined());
    assert!(Challenge::open(OpenChallenge {
        bond: 5.0,
        ..OpenChallenge {
            id: "c",
            patch_id: "p",
            flagged_by: "f",
            severity: Severity::Low,
            evidence_commitment: "",
            bond: 0.0,
            now: 0
        }
    })
    .is_err());
}

#[test]
fn escalates_with_augur_bonds_and_crowdsourcing() {
    let mut c = open(10.0);
    assert_eq!(c.bond_to_flip(), 20.0); // 2·10 − 3·0
    assert_eq!(
        c.contribute("author", Verdict::Clean, 15.0, T0 + 1)
            .unwrap(),
        15.0
    );
    assert_eq!(c.tentative, Verdict::Problem);
    assert_eq!(
        c.contribute("friend", Verdict::Clean, 100.0, T0 + 2)
            .unwrap(),
        5.0
    );
    assert_eq!(c.tentative, Verdict::Clean);
    assert!(!c.quarantined());
    assert_eq!(c.bond_to_flip(), 30.0); // 2·30 − 3·10
    assert!(c.contribute("x", Verdict::Clean, 10.0, T0 + 3).is_err());
}

#[test]
fn timeout_settles_a_clean_verdict_against_the_flagger() {
    let mut c = open(10.0);
    c.contribute("author", Verdict::Clean, 20.0, T0 + 1)
        .unwrap();
    c.contribute("finder", Verdict::Problem, 7.0, T0 + 2)
        .unwrap(); // partial, never filled
    c.advance(T0 + 2 + DAY);
    assert_eq!(c.resolution.as_ref().unwrap().via, ResolvedVia::Timeout);
    let s = settle(&c, &ctx()).unwrap();
    assert_eq!(s.bounty, 0.0);
    assert_eq!(net(&s.entries, "author"), 29.0);
    assert_eq!(net(&s.entries, "finder"), 7.0);
    assert_eq!(s.pool_delta, 1.0);
    assert!(s.bans.is_empty());
}

#[test]
fn large_stakes_go_to_a_jury() {
    let mut c = open(1000.0);
    let need = c.bond_to_flip();
    c.contribute("author", Verdict::Clean, need, T0 + 1)
        .unwrap();
    assert_eq!(c.phase, Phase::Jury);
    assert!(c.contribute("x", Verdict::Problem, 10.0, T0 + 2).is_err());
}

#[test]
fn proven_backdoor_slashes_vouchers_bans_author_and_pays_finder() {
    let mut c = open(10.0);
    c.contribute("author", Verdict::Clean, 20.0, T0 + 1)
        .unwrap();
    c.resolve(resolution(Verdict::Problem, Intent::Malicious))
        .unwrap();
    let s = settle(&c, &ctx()).unwrap();
    assert_eq!(net(&s.entries, "author"), -100.0);
    assert_eq!(net(&s.entries, "rubberstamp"), -50.0);
    assert_eq!(net(&s.entries, "a1"), -2.0);
    assert_eq!(net(&s.entries, "a2"), -4.0);
    let from_pool = severity_bounty(Severity::High) * 2f64.ln_1p();
    assert!((s.bounty - (156.0 + from_pool)).abs() < 0.01);
    assert!((net(&s.entries, "finder") - (28.0 + s.bounty)).abs() < 0.01);
    assert_eq!(s.bans, ["author"]);
    assert!(s.rereview_author);
}

#[test]
fn honest_mistakes_cost_by_severity_without_a_ban() {
    assert_eq!(author_slash(Intent::Accidental, Severity::Low), 0.025);
    assert_eq!(author_slash(Intent::Accidental, Severity::Critical), 0.2);
    assert_eq!(author_slash(Intent::Malicious, Severity::Low), 1.0);
    let mut c = open(10.0);
    c.resolve(resolution(Verdict::Problem, Intent::Accidental))
        .unwrap();
    let s = settle(&c, &ctx()).unwrap();
    assert_eq!(net(&s.entries, "author"), -10.0);
    assert!(s.bans.is_empty());
}

#[test]
fn no_pool_bounty_for_a_patch_nobody_adopted() {
    let mut c = open(10.0);
    c.resolve(Resolution {
        severity: Severity::Critical,
        ..resolution(Verdict::Problem, Intent::Malicious)
    })
    .unwrap();
    let s = settle(
        &c,
        &SettlementContext {
            adopters: vec![],
            attestations: vec![],
            author_balance: 30.0,
            ..ctx()
        },
    )
    .unwrap();
    assert_eq!(s.bounty, 30.0);
    assert_eq!(s.pool_delta, 0.0);
}

#[test]
fn assigned_review_findings_earn_the_base_bounty_before_any_adoption() {
    let mut c = open(0.0);
    let need = c.bond_to_flip();
    c.contribute(PLATFORM_POOL, Verdict::Problem, need, T0 + 1)
        .unwrap();
    c.resolve(Resolution {
        severity: Severity::Critical,
        ..resolution(Verdict::Problem, Intent::Malicious)
    })
    .unwrap();
    let s = settle(
        &c,
        &SettlementContext {
            adopters: vec![],
            attestations: vec![],
            author_balance: 0.0,
            found_by_assigned_review: true,
            ..ctx()
        },
    )
    .unwrap();
    assert_eq!(s.bounty, severity_bounty(Severity::Critical));
    assert_eq!(net(&s.entries, "finder"), s.bounty); // the pool takes no backer's share
    assert_eq!(net(&s.entries, PLATFORM_POOL), need);
}

#[test]
fn backers_of_a_bare_report_share_the_bounty() {
    let mut c = open(0.0);
    let need = c.bond_to_flip();
    c.contribute("backer", Verdict::Problem, need, T0 + 1)
        .unwrap();
    assert!(c.quarantined());
    c.advance(T0 + 1 + DAY);
    let s = settle(
        &c,
        &SettlementContext {
            adopters: vec![],
            attestations: vec![],
            author_balance: 0.0,
            ..ctx()
        },
    )
    .unwrap();
    assert!((net(&s.entries, "finder") - s.bounty * PARAMS.finder_share).abs() < 0.01);
    assert!(
        (net(&s.entries, "backer") - (10.0 + s.bounty * (1.0 - PARAMS.finder_share))).abs() < 0.01
    );
}

#[test]
fn jury_draws_are_deterministic_weighted_and_exclusive() {
    let cands: Vec<JuryCandidate> = (0..30)
        .map(|i| JuryCandidate {
            operator_id: format!("j{i}"),
            stake: 20.0 + i as f64 * 10.0,
        })
        .collect();
    let ex: HashSet<String> = ["j29".to_string()].into();
    let a = draw_jury(&cands, 7, "seed", &ex).unwrap();
    assert_eq!(a, draw_jury(&cands, 7, "seed", &ex).unwrap());
    assert_eq!(a.iter().collect::<HashSet<_>>().len(), 7);
    assert!(!a.contains(&"j29".to_string()));
    let (mut big, mut small) = (0, 0);
    for i in 0..400 {
        let j = draw_jury(&cands, 1, &format!("s{i}"), &HashSet::new()).unwrap();
        big += (j[0] == "j28") as u32;
        small += (j[0] == "j0") as u32;
    }
    assert!(big > small, "{big} vs {small}");
    assert_eq!([0, 1, 2].map(jury_size), [3, 7, 15]);
}

#[test]
fn commit_reveal_and_coherence_payouts() {
    let c = commit_vote("j1", Verdict::Problem, "salt");
    assert!(verify_reveal(&c, "j1", Verdict::Problem, "salt"));
    assert!(!verify_reveal(&c, "j1", Verdict::Clean, "salt"));
    let b = |op: &str, vote| Ballot {
        operator_id: op.into(),
        vote,
    };
    let (v, e) = tally(&[
        b("j1", Some(Verdict::Problem)),
        b("j2", Some(Verdict::Problem)),
        b("j3", Some(Verdict::Clean)),
        b("j4", None),
    ]);
    assert_eq!(v, Some(Verdict::Problem));
    assert_eq!(net(&e, "j1"), 4.0);
    assert_eq!(net(&e, "j3"), -4.0);
    assert_eq!(net(&e, "j4"), -4.0);
    assert_eq!(
        tally(&[b("a", Some(Verdict::Clean)), b("b", Some(Verdict::Problem))]).0,
        None
    );
}

#[test]
fn proof_of_work_binds_to_its_resource() {
    let nonce = solve_work("op:patch:commitment", 10);
    assert!(verify_work("op:patch:commitment", &nonce, 10));
    assert!(!verify_work("op:other-patch:commitment", &nonce, 10));
}
