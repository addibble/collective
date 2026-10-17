//! `collective env` and `collective doctor`: project environments
//! (docs/ENVIRONMENTS.md) and a check of this machine as a runner.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use collective_core::environment::{Environment, NixEnvironment};
use collective_prover::{detect_isolation, Isolation, Options};
use serde_json::Value;

use crate::die;

/// The URL path segment for a project: `github.com/owner/repo` (a leading
/// `https://` is accepted) with the slashes percent-encoded, as the API takes it.
fn project_segment(project: &str) -> String {
    project
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .replace('/', "%2F")
}

/// A request that hands back the status and body, errors included.
fn call(
    server: &str,
    key: Option<&str>,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (u16, Value) {
    let mut req =
        ureq::request(method, &format!("{server}{path}")).timeout(Duration::from_secs(30));
    if let Some(k) = key {
        req = req.set("authorization", &format!("Bearer {k}"));
    }
    let res = match body {
        Some(b) => req.send_json(b),
        None => req.call(),
    };
    match res {
        Ok(r) => (r.status(), r.into_json().unwrap_or(Value::Null)),
        Err(ureq::Error::Status(code, r)) => (code, r.into_json().unwrap_or(Value::Null)),
        Err(e) => die(format!("{server}{path}: {e}")),
    }
}

fn describe(env: &Value) -> String {
    match env.get("nix") {
        Some(n) => {
            let packages: Vec<&str> = n["packages"]
                .as_array()
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            format!(
                "nixpkgs {}{}; packages: {}",
                n["nixpkgs"].as_str().unwrap_or("?"),
                n["nixpkgs_hash"]
                    .as_str()
                    .map(|h| format!(" ({h})"))
                    .unwrap_or_default(),
                packages.join(", ")
            )
        }
        None => "stock (the prover image as it is)".into(),
    }
}

/// `collective env PROJECT [--json]`: the current environment and the
/// version history. Public: needs no key.
pub fn show(server: &str, project: &str, raw: bool) {
    let (code, v) = call(
        server,
        None,
        "GET",
        &format!("/v1/projects/{}/environments", project_segment(project)),
        None,
    );
    if code >= 300 {
        die(format!(
            "HTTP {code}: {}",
            v["error"].as_str().unwrap_or("request failed")
        ));
    }
    if raw {
        println!("{}", serde_json::to_string_pretty(&v).expect("json"));
        return;
    }
    let cur = &v["current"];
    println!("Project {}", v["project_id"].as_str().unwrap_or(project));
    println!(
        "Current: {}{}",
        describe(&cur["environment"]),
        match cur["id"].as_str() {
            Some(id) => format!(" [{id}, {}]", cur["state"].as_str().unwrap_or("?")),
            None => String::new(),
        }
    );
    let versions = v["versions"].as_array().cloned().unwrap_or_default();
    if versions.is_empty() {
        println!("No versions recorded: new jobs run in the stock image.");
    }
    for ver in versions {
        println!(
            "\nVersion {}  {}  {}",
            ver["version"],
            ver["state"].as_str().unwrap_or("?"),
            ver["id"].as_str().unwrap_or("?")
        );
        match ver["environment"].get("nix") {
            Some(n) => {
                println!("  nixpkgs:  {}", n["nixpkgs"].as_str().unwrap_or("?"));
                if let Some(h) = n["nixpkgs_hash"].as_str() {
                    println!("  hash:     {h}");
                }
                let pk: Vec<&str> = n["packages"]
                    .as_array()
                    .map(|a| a.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default();
                println!("  packages: {}", pk.join(", "));
            }
            None => println!("  stock"),
        }
        if let Some(r) = ver["reason"].as_str() {
            println!("  reason:   {r}");
        }
    }
}

/// `collective env propose PROJECT --nixpkgs REV [--nixpkgs-hash H] --package ATTR…`
pub fn propose(server: &str, project: &str, flags: &[&str]) {
    let (mut rev, mut hash, mut packages) = (None, None, Vec::new());
    let mut it = flags.iter();
    while let Some(f) = it.next() {
        let mut value = |name: &str| {
            it.next()
                .map(|s| s.to_string())
                .unwrap_or_else(|| die(format!("{name} needs a value")))
        };
        match *f {
            "--nixpkgs" => rev = Some(value("--nixpkgs")),
            "--nixpkgs-hash" => hash = Some(value("--nixpkgs-hash")),
            "--package" => packages.push(value("--package")),
            other => die(format!(
                "unknown option {other} (see `collective env propose` in the README)"
            )),
        }
    }
    let nixpkgs = rev.unwrap_or_else(|| die("--nixpkgs <40-hex commit> is required"));
    let env = Environment::Nix(NixEnvironment {
        nixpkgs,
        nixpkgs_hash: hash,
        packages,
    });
    if let Err(e) = env.validate() {
        die(e);
    }
    let env = env.canonical();
    let record = serde_json::to_value(&env).expect("json");
    println!("Proposing {} for {project}:", env.id());
    println!("{}", serde_json::to_string_pretty(&record).expect("json"));
    let key = crate::run::agent_key();
    let (code, v) = call(
        server,
        Some(&key),
        "POST",
        &format!("/v1/projects/{}/environments", project_segment(project)),
        Some(record),
    );
    if code >= 300 {
        // The server's message, verbatim.
        eprintln!(
            "HTTP {code}: {}",
            v["error"].as_str().unwrap_or("request failed")
        );
        std::process::exit(1);
    }
    println!("HTTP {code}");
    println!("{}", serde_json::to_string_pretty(&v).expect("json"));
}

fn nix_conf_files() -> Vec<PathBuf> {
    let user = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
    let mut files = vec![PathBuf::from("/etc/nix/nix.conf")];
    files.extend(user.map(|d| d.join("nix/nix.conf")));
    files
}

/// Whether a nix.conf sets `max-jobs` to 0 (the last setting wins).
fn disables_builds(conf: &str) -> bool {
    let mut off = false;
    for line in conf.lines() {
        let line = line.split('#').next().unwrap_or("");
        if let Some((k, v)) = line.split_once('=') {
            if k.trim() == "max-jobs" {
                off = v.trim() == "0";
            }
        }
    }
    off
}

/// `collective doctor`: can this machine run proofs? Exits non-zero when
/// network namespaces or git are missing.
pub fn doctor() {
    let mut hard_fail = false;
    let line = |mark: &str, what: String, remedy: &str| {
        if remedy.is_empty() {
            println!("[{mark}] {what}");
        } else {
            println!("[{mark}] {what}\n       {remedy}");
        }
    };

    if cfg!(target_os = "linux") {
        line("ok", "OS is Linux".into(), "");
    } else {
        line(
            "FAIL",
            format!("OS is {}, not Linux", std::env::consts::OS),
            "The sandbox needs Linux namespaces: run `collective run` and `collective prove` in a Linux VM.",
        );
    }

    let opts = Options::from_env();
    match detect_isolation(&opts) {
        Isolation::RootNamespace => line(
            "ok",
            "network isolation: network namespace (unshare -n, as root)".into(),
            "",
        ),
        Isolation::Namespace => line(
            "ok",
            "network isolation: network namespace (unshare -rn, unprivileged)".into(),
            "",
        ),
        Isolation::NoInternet => line(
            "ok",
            "network isolation: no-internet container (PROVER_NO_INTERNET)".into(),
            "",
        ),
        Isolation::None => {
            hard_fail = true;
            line(
                "FAIL",
                "network isolation: none available (network namespaces do not work here)".into(),
                "Install util-linux (`unshare`) and allow user namespaces (sysctl kernel.unprivileged_userns_clone=1), or run as root in a VM; runs without isolation are inconclusive.",
            );
        }
    }

    let nix = opts
        .nix_binary
        .clone()
        .unwrap_or_else(|| PathBuf::from("nix"));
    match Command::new(&nix)
        .arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    {
        Ok(o) if o.status.success() => line(
            "ok",
            format!("nix: {}", String::from_utf8_lossy(&o.stdout).trim()),
            "",
        ),
        _ => line(
            "FAIL",
            "nix: `nix --version` does not work".into(),
            "Optional: install Nix (single-user is enough) to be offered work in projects that declare an environment; without it only stock-environment work is offered.",
        ),
    }

    let readable: Vec<(PathBuf, String)> = nix_conf_files()
        .into_iter()
        .filter_map(|p| std::fs::read_to_string(&p).ok().map(|s| (p, s)))
        .collect();
    if readable.is_empty() {
        line(
            "--",
            "nix.conf: not checked (no readable /etc/nix/nix.conf or user nix.conf)".into(),
            "Advisory only: the prover passes `--max-jobs 0` on the command line anyway.",
        );
    } else if let Some((p, _)) = readable.iter().find(|(_, s)| disables_builds(s)) {
        line(
            "ok",
            format!(
                "nix.conf: {} sets max-jobs = 0 (local builds disabled)",
                p.display()
            ),
            "Advisory only: the prover passes `--max-jobs 0` on the command line anyway.",
        );
    } else {
        line(
            "ok",
            "nix.conf: no max-jobs = 0 setting found".into(),
            "Advisory only: the prover passes `--max-jobs 0` on the command line anyway, so builds are disabled for environments regardless.",
        );
    }

    match Command::new("git")
        .arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    {
        Ok(o) if o.status.success() => line(
            "ok",
            format!("git: {}", String::from_utf8_lossy(&o.stdout).trim()),
            "",
        ),
        _ => {
            hard_fail = true;
            line(
                "FAIL",
                "git: not found on PATH".into(),
                "Install git; the prover needs it to apply patches.",
            );
        }
    }

    if hard_fail {
        std::process::exit(1);
    }
}
