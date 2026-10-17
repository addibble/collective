//! The proof sandbox. Given an upstream source tarball, a patch and a
//! finder's proof spec, it builds two trees (base, base + patch) and runs
//! the proof command on fresh copies of each, several times, with:
//!
//! - **no network** for anything the finder or the patch controls: either
//!   the container has no internet at all, or each run is wrapped in a new
//!   network namespace holding only loopback;
//! - **no privileges**: as root, the prover drops each run to an
//!   unprivileged user, so a run cannot modify the toolchains, the prover,
//!   or anything a later run will use;
//! - **no inherited environment**: runs see only PATH, HOME and a few fixed
//!   variables, never the prover's own;
//! - **a hard timeout** per run, enforced by killing the run's whole
//!   process group;
//! - **a fresh copy of the tree** per run, so one run cannot prepare the
//!   next.
//!
//! Dependency fetches and, for a project with a Nix environment, its
//! realisation are the only steps with network, and they are fixed commands
//! that download without executing project code (see
//! `collective_core::proof::Ecosystem` and `collective_core::environment`).
//!
//! The prover reports what happened; `collective_core::proof::assess`
//! decides what it means, and fails closed.

use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use collective_core::environment::{
    compose_env, Environment, NixEnvironment, StorePath, REALISE_TIMEOUT_SECS,
};
use collective_core::proof::{
    count_tests, Ecosystem, ProofKind, ProofReport, ProofSpec, RunResult, RunStatus, SetupResult,
    Tree, BUILD_TIMEOUT_SECS,
};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const OUTPUT_TAIL_BYTES: u64 = 8 * 1024;
const SETUP_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, Clone)]
pub struct Options {
    /// The host runs this container without internet (set by the Worker for
    /// proofs that need no dependency fetch). Verified, not trusted.
    pub no_internet: bool,
    /// Development only: run even without network isolation. The report
    /// says so, and `assess` will not accept the result.
    pub allow_unisolated: bool,
    /// Wall-clock budget for the whole proof.
    pub budget: Duration,
    /// Extra environment for the networked steps only, the dependency fetch
    /// and the environment's realisation (e.g. an egress proxy and its CA).
    /// Never passed to builds or runs.
    pub setup_env: Vec<(String, String)>,
    /// The `nix` binary that realises Nix environments. `None` looks it up
    /// on the prover's own `PATH`.
    pub nix_binary: Option<PathBuf>,
}

impl Options {
    pub fn from_env() -> Options {
        let flag = |k: &str| std::env::var(k).is_ok_and(|v| v == "1" || v == "true");
        Options {
            no_internet: flag("PROVER_NO_INTERNET"),
            allow_unisolated: flag("PROVER_ALLOW_UNISOLATED"),
            budget: Duration::from_secs(
                std::env::var("PROVER_BUDGET_SECS")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(900),
            ),
            // PROVER_SETUP_PASSTHROUGH=HTTPS_PROXY,CARGO_HTTP_CAINFO names variables
            // copied from the prover's environment into the fetch step.
            setup_env: std::env::var("PROVER_SETUP_PASSTHROUGH")
                .unwrap_or_default()
                .split(',')
                .filter(|k| !k.is_empty())
                .filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v)))
                .collect(),
            nix_binary: std::env::var_os("PROVER_NIX_BINARY").map(PathBuf::from),
        }
    }
}

/// How test commands are kept off the network.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Isolation {
    /// Each run is wrapped in `unshare -n` (as root, before dropping privileges).
    RootNamespace,
    /// Each run is wrapped in `unshare -rn` (unprivileged user namespace).
    Namespace,
    /// The whole container has no route out.
    NoInternet,
    None,
}

impl Isolation {
    fn label(self) -> Option<String> {
        match self {
            Isolation::RootNamespace => Some("network namespace (unshare -n)".into()),
            Isolation::Namespace => Some("network namespace (unshare -rn)".into()),
            Isolation::NoInternet => Some("no-internet container".into()),
            Isolation::None => None,
        }
    }
}

/// Whether the internet is really reachable. A bare TCP connect proves
/// nothing (sandbox egress proxies accept every connection), so this needs
/// a certificate-verified HTTPS response from a public host. Without curl it
/// falls back to TCP, which can only err on the side of "reachable".
fn can_reach_internet() -> bool {
    let curl = |url: &str| {
        Command::new("curl")
            .args(["-s", "-o", "/dev/null", "--max-time", "4", url])
            .stdin(Stdio::null())
            .status()
            .map(|s| s.success())
    };
    match curl("https://1.1.1.1/cdn-cgi/trace") {
        Ok(true) => true,
        Ok(false) => curl("https://dns.google/").unwrap_or(false),
        Err(_) => ["1.1.1.1:443", "8.8.8.8:53"].iter().any(|a| {
            let addr: SocketAddr = a.parse().expect("valid address");
            TcpStream::connect_timeout(&addr, Duration::from_secs(2)).is_ok()
        }),
    }
}

/// A namespace counts only if it really has nothing but loopback.
fn namespace_works(flag: &str) -> bool {
    Command::new("unshare")
        .args([flag, "cat", "/proc/net/dev"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .is_ok_and(|o| {
            o.status.success() && {
                let text = String::from_utf8_lossy(&o.stdout);
                let ifaces: Vec<&str> = text
                    .lines()
                    .filter_map(|l| l.split_once(':').map(|(name, _)| name.trim()))
                    .collect();
                ifaces == ["lo"]
            }
        })
}

pub fn detect_isolation(opts: &Options) -> Isolation {
    if is_root() && namespace_works("-n") {
        Isolation::RootNamespace
    } else if namespace_works("-rn") {
        Isolation::Namespace
    } else if opts.no_internet && !can_reach_internet() {
        Isolation::NoInternet
    } else {
        Isolation::None
    }
}

/// Runs execute as this user when the prover is root, so they cannot
/// modify the toolchains, the prover or each other's state.
pub const RUN_UID: u32 = 65534;

fn is_root() -> bool {
    use std::os::unix::fs::MetadataExt;
    fs::metadata("/proc/self").is_ok_and(|m| m.uid() == 0)
}

fn chown_run(path: &Path) -> Result<(), String> {
    if !is_root() {
        return Ok(());
    }
    let ok = Command::new("chown")
        .args(["-R", &format!("{RUN_UID}:{RUN_UID}")])
        .arg(path)
        .status()
        .map_err(|e| e.to_string())?
        .success();
    if ok {
        Ok(())
    } else {
        Err(format!("could not chown {}", path.display()))
    }
}

/// The argv prefix that isolates one run: network namespace, then (as root)
/// a drop to an unprivileged user with no way to regain privileges.
fn run_prefix(isolation: Isolation) -> Vec<String> {
    let mut p: Vec<String> = match isolation {
        Isolation::RootNamespace => vec!["unshare".into(), "-n".into()],
        Isolation::Namespace => vec!["unshare".into(), "-rn".into()],
        Isolation::NoInternet | Isolation::None => vec![],
    };
    if is_root() && isolation != Isolation::Namespace {
        p.extend(
            [
                "setpriv",
                &format!("--reuid={RUN_UID}"),
                &format!("--regid={RUN_UID}"),
                "--clear-groups",
                "--no-new-privs",
                "--",
            ]
            .iter()
            .map(|s| s.to_string()),
        );
    }
    p
}

/// Dependency caches live inside each tree, so every run gets its own copy
/// and no run can poison another's dependencies.
fn tree_env(tree: &Path) -> Vec<(&'static str, String)> {
    let c = tree.join(".collective");
    vec![
        ("CARGO_HOME", c.join("cargo").display().to_string()),
        ("GOMODCACHE", c.join("gomod").display().to_string()),
        ("GOCACHE", c.join("gocache").display().to_string()),
        ("npm_config_cache", c.join("npm").display().to_string()),
        ("PIP_CACHE_DIR", c.join("pip").display().to_string()),
    ]
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct Workdir(PathBuf);

impl Workdir {
    fn new() -> std::io::Result<Workdir> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "prover-{}-{nanos}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&dir)?;
        Ok(Workdir(dir))
    }
}

impl Drop for Workdir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn setup_error(msg: impl Into<String>, isolation: Isolation) -> ProofReport {
    ProofReport {
        prover_version: VERSION.into(),
        setup: SetupResult {
            ok: false,
            error: Some(msg.into()),
            network_isolated: isolation != Isolation::None,
            isolation: isolation.label(),
        },
        runs: vec![],
        cpu_ms: None,
    }
}

fn clock_ticks_per_sec() -> u64 {
    Command::new("getconf")
        .arg("CLK_TCK")
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
        .filter(|t| *t > 0)
        .unwrap_or(100)
}

/// CPU time (user + system) used so far by this process's finished,
/// waited-for descendants: `cutime` and `cstime` in /proc/self/stat. The
/// difference across a proof is what the proof cost, provided the process
/// runs one proof at a time (the container and `collective run` do).
fn children_cpu_ms(ticks_per_sec: u64) -> Option<u64> {
    let stat = fs::read_to_string("/proc/self/stat").ok()?;
    // Fields after the command name, which is in parentheses and may hold spaces.
    let rest = stat.get(stat.rfind(')')? + 2..)?;
    let f: Vec<&str> = rest.split_whitespace().collect();
    // `rest` starts at field 3 (state); cutime is field 16, cstime field 17.
    let ticks = f.get(13)?.parse::<u64>().ok()? + f.get(14)?.parse::<u64>().ok()?;
    Some(ticks * 1000 / ticks_per_sec)
}

fn tail(path: &Path) -> String {
    let Ok(mut f) = fs::File::open(path) else {
        return String::new();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let _ = f.seek(SeekFrom::Start(len.saturating_sub(OUTPUT_TAIL_BYTES)));
    let mut buf = Vec::new();
    let _ = f.read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

/// Runs a command with a clean environment in its own process group,
/// killing the whole group on timeout. Returns (status, exit code, output tail).
fn run_command<S: AsRef<std::ffi::OsStr>>(
    argv: &[S],
    cwd: &Path,
    env: &[(&str, String)],
    timeout: Duration,
    log: &Path,
) -> std::io::Result<(RunStatus, Option<i32>, String)> {
    run_command_to(argv, cwd, env, timeout, log, None)
}

/// As [`run_command`], but with stdout written to `stdout` when given (the
/// log, and the returned tail, then hold stderr only).
fn run_command_to<S: AsRef<std::ffi::OsStr>>(
    argv: &[S],
    cwd: &Path,
    env: &[(&str, String)],
    timeout: Duration,
    log: &Path,
    stdout: Option<&Path>,
) -> std::io::Result<(RunStatus, Option<i32>, String)> {
    let out = fs::File::create(log)?;
    let stdout = match stdout {
        Some(p) => fs::File::create(p)?,
        None => out.try_clone()?,
    };
    let mut cmd = Command::new("setsid");
    cmd.args(argv)
        .current_dir(cwd)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(out);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn()?;
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            let code = status.code();
            let st = if status.success() {
                RunStatus::Passed
            } else {
                RunStatus::Failed
            };
            return Ok((st, code, tail(log)));
        }
        if start.elapsed() >= timeout {
            // setsid made the child a session and group leader: kill the group.
            let _ = Command::new("kill")
                .args(["-KILL", "--", &format!("-{}", child.id())])
                .status();
            let _ = child.kill();
            let _ = child.wait();
            return Ok((RunStatus::TimedOut, None, tail(log)));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn base_env(home: &Path) -> Vec<(&'static str, String)> {
    vec![
        ("PATH", "/usr/local/cargo/bin:/usr/local/go/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string()),
        ("RUSTUP_HOME", std::env::var("RUSTUP_HOME").unwrap_or_else(|_| "/usr/local/rustup".into())),
        ("HOME", home.display().to_string()),
        ("LANG", "C.UTF-8".to_string()),
        ("CI", "true".to_string()),
        // Never let git discover a repository above the tree.
        ("GIT_CEILING_DIRECTORIES", home.parent().unwrap_or(home).display().to_string()),
    ]
}

/// Variables passed from the prover's own environment to `nix`, so it finds
/// its CA bundle and configuration (the image sets `NIX_SSL_CERT_FILE`).
const NIX_PASSTHROUGH: &[&str] = &["NIX_SSL_CERT_FILE", "SSL_CERT_FILE", "NIX_CONF_DIR"];

/// The `nix` binary: the one the options name, or the first on the
/// prover's own `PATH`.
fn find_nix(opts: &Options) -> Result<PathBuf, String> {
    if let Some(p) = &opts.nix_binary {
        return if p.is_file() {
            Ok(p.clone())
        } else {
            Err(format!(
                "nix is not installed: {} does not exist",
                p.display()
            ))
        };
    }
    std::env::var_os("PATH")
        .iter()
        .flat_map(std::env::split_paths)
        .map(|d| d.join("nix"))
        .find(|p| p.is_file())
        .ok_or_else(|| "nix is not installed: no `nix` on PATH".to_string())
}

/// Whether this prover can realise Nix environments (`nix --version` runs).
pub fn nix_available(opts: &Options) -> bool {
    find_nix(opts).is_ok_and(|nix| {
        Command::new(nix)
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    })
}

/// A path `nix build --print-out-paths` may print: a top-level entry of
/// `/nix/store`, nothing else.
fn is_store_path(p: &str) -> bool {
    p.strip_prefix("/nix/store/").is_some_and(|name| {
        !name.is_empty()
            && !name.starts_with('.')
            && !name.contains('/')
            && !name.chars().any(char::is_whitespace)
    })
}

/// Realises a Nix environment by substitution only and returns the
/// variables it adds to every later step (`collective_core::environment::compose_env`).
/// Runs as the prover's own user, with network, with `HOME` inside the
/// workdir so Nix's per-user caches are discarded with it.
fn realise(
    nix_env: &NixEnvironment,
    opts: &Options,
    work: &Path,
) -> Result<Vec<(String, String)>, String> {
    let nix = find_nix(opts)?;
    let refs = nix_env.flake_refs();
    let mut argv: Vec<std::ffi::OsString> = vec![nix.into()];
    argv.extend(nix_env.realise_argv().into_iter().skip(1).map(Into::into));
    let home = work.join("home-nix");
    fs::create_dir_all(&home).map_err(|e| format!("home: {e}"))?;
    let passthrough: Vec<(String, String)> = NIX_PASSTHROUGH
        .iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v)))
        .collect();
    let mut env: Vec<(&str, String)> = base_env(&home);
    env.extend(passthrough.iter().map(|(k, v)| (k.as_str(), v.clone())));
    env.extend(opts.setup_env.iter().map(|(k, v)| (k.as_str(), v.clone())));
    let (log, out) = (work.join("environment.log"), work.join("environment.out"));
    let timeout = Duration::from_secs(REALISE_TIMEOUT_SECS as u64);
    match run_command_to(&argv, work, &env, timeout, &log, Some(&out)) {
        Ok((RunStatus::Passed, ..)) => {}
        Ok((st, _, tail)) => return Err(format!("nix build {st:?}: {}", tail.trim())),
        Err(e) => return Err(format!("could not run nix: {e}")),
    }
    let printed = fs::read_to_string(&out).map_err(|e| format!("nix output: {e}"))?;
    let paths: Vec<&str> = printed
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    // A package prints one path per output it installs (`openssl` prints its
    // `bin` and `man`), so there may be more paths than packages, never fewer.
    if paths.len() < refs.len() {
        return Err(format!(
            "nix build printed {} store paths for {} packages: {}",
            paths.len(),
            refs.len(),
            printed.trim()
        ));
    }
    let mut store = Vec::with_capacity(paths.len());
    for p in paths {
        if !is_store_path(p) {
            return Err(format!("nix build printed a path outside /nix/store/: {p}"));
        }
        let dir = Path::new(p);
        if fs::symlink_metadata(dir).is_err() {
            return Err(format!("{p} does not exist after realisation"));
        }
        let has = |sub: &str| dir.join(sub).is_dir();
        store.push(StorePath {
            path: p.to_string(),
            has_bin: has("bin"),
            has_lib: has("lib"),
            has_include: has("include"),
            has_lib_pkgconfig: has("lib/pkgconfig"),
            has_share_pkgconfig: has("share/pkgconfig"),
        });
    }
    Ok(compose_env(&store))
}

/// Adds the realised environment's variables to a step's: `PATH` gets the
/// environment's directories ahead of its own, the rest are added as given.
fn with_environment<'a>(
    mut env: Vec<(&'a str, String)>,
    extra: &'a [(String, String)],
) -> Vec<(&'a str, String)> {
    for (k, v) in extra {
        match env.iter_mut().find(|(ek, _)| *ek == k.as_str()) {
            Some((_, ev)) if k == "PATH" => *ev = format!("{v}:{ev}"),
            Some((_, ev)) => *ev = v.clone(),
            None => env.push((k.as_str(), v.clone())),
        }
    }
    env
}

fn copy_tree(from: &Path, to: &Path) -> Result<(), String> {
    let ok = Command::new("cp")
        .arg("-a")
        .arg(from)
        .arg(to)
        .status()
        .map_err(|e| e.to_string())?
        .success();
    if ok {
        Ok(())
    } else {
        Err(format!("could not copy {}", from.display()))
    }
}

/// Writes a proof file, refusing anything that resolves outside the tree
/// (e.g. through a symlink the patch added).
fn write_inside(root: &Path, rel: &str, content: &str) -> Result<(), String> {
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    let target = root.join(rel);
    let parent = target.parent().ok_or("bad path")?;
    fs::create_dir_all(parent).map_err(|e| format!("{rel}: {e}"))?;
    let real_parent = parent.canonicalize().map_err(|e| e.to_string())?;
    if !real_parent.starts_with(&root) {
        return Err(format!("{rel} escapes the source tree"));
    }
    if fs::symlink_metadata(&target).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(format!("{rel} is a symlink in the tree"));
    }
    fs::write(&target, content).map_err(|e| format!("{rel}: {e}"))
}

/// Builds both trees and runs the proof. Never panics on bad input: every
/// failure is reported in `setup`.
pub fn prove(spec: &ProofSpec, source_tgz: &[u8], patch: &str, opts: &Options) -> ProofReport {
    let ticks = clock_ticks_per_sec();
    let cpu_before = children_cpu_ms(ticks);
    let mut report = prove_inner(spec, source_tgz, patch, opts);
    if let (Some(a), Some(b)) = (cpu_before, children_cpu_ms(ticks)) {
        report.cpu_ms = Some(b.saturating_sub(a));
    }
    report
}

fn prove_inner(spec: &ProofSpec, source_tgz: &[u8], patch: &str, opts: &Options) -> ProofReport {
    let started = Instant::now();
    let isolation = detect_isolation(opts);
    if let Err(e) = spec.validate() {
        return setup_error(format!("invalid spec: {e}"), isolation);
    }
    if isolation == Isolation::None && !opts.allow_unisolated {
        let why = if opts.no_internet {
            "the host says this container has no internet, but it can reach the internet"
        } else {
            "network namespaces are unavailable and the host did not disable internet access"
        };
        return setup_error(
            format!("network isolation unavailable ({why}): refusing to run untrusted code"),
            isolation,
        );
    }
    let work = match Workdir::new() {
        Ok(w) => w,
        Err(e) => return setup_error(format!("workdir: {e}"), isolation),
    };
    let (base, patched) = (work.0.join("base"), work.0.join("patched"));

    // 1. Source.
    if let Err(e) = fs::create_dir_all(&base) {
        return setup_error(e.to_string(), isolation);
    }
    let extracted = Command::new("tar")
        .args([
            "-xz",
            "--strip-components=1",
            "--no-same-owner",
            "--no-same-permissions",
            "-C",
        ])
        .arg(&base)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .and_then(|mut c| {
            c.stdin.take().expect("piped").write_all(source_tgz)?;
            c.wait_with_output()
        });
    match extracted {
        Ok(o) if o.status.success() => {}
        Ok(o) => {
            return setup_error(
                format!(
                    "source archive: {}",
                    String::from_utf8_lossy(&o.stderr).trim()
                ),
                isolation,
            )
        }
        Err(e) => return setup_error(format!("source archive: {e}"), isolation),
    }

    // 2. Patched tree.
    if let Err(e) = copy_tree(&base, &patched) {
        return setup_error(e, isolation);
    }
    let patch_file = work.0.join("patch.diff");
    if let Err(e) = fs::write(&patch_file, patch) {
        return setup_error(e.to_string(), isolation);
    }
    let home = work.0.join("home");
    let _ = fs::create_dir_all(&home);
    let log = work.0.join("setup.log");
    // An empty patch means both trees are the base: an environment
    // proposal's job runs the suite that way (docs/ENVIRONMENTS.md). `git
    // apply` refuses empty input, so it is skipped rather than asked.
    if !patch.trim().is_empty() {
        match run_command(
            &[
                "git",
                "apply",
                "--whitespace=nowarn",
                patch_file.to_str().unwrap_or_default(),
            ],
            &patched,
            &base_env(&home),
            Duration::from_secs(60),
            &log,
        ) {
            Ok((RunStatus::Passed, ..)) => {}
            Ok((_, _, out)) => {
                return setup_error(
                    format!("patch does not apply to the base commit: {}", out.trim()),
                    isolation,
                )
            }
            Err(e) => return setup_error(format!("git apply: {e}"), isolation),
        }
    }

    // 3. The finder's files go into both trees.
    for tree in [&base, &patched] {
        for f in &spec.files {
            if let Err(e) = write_inside(tree, &f.path, &f.content) {
                return setup_error(e, isolation);
            }
        }
    }

    // A regression or suite run must run one of the project's own scripts or tests.
    if matches!(spec.kind, ProofKind::Regression | ProofKind::Suite)
        && spec.ecosystem == Ecosystem::None
        && !base.join(&spec.tests[0]).is_file()
    {
        return setup_error(
            format!("{} is not a file in the base commit", spec.tests[0]),
            isolation,
        );
    }

    // 4. Environment: realise the project's Nix package set by substitution
    //    only, with network, as the prover's user. Its variables apply to
    //    every later step. Anything that fails makes the run inconclusive,
    //    never "the stock environment instead".
    let env_extra = match &spec.environment {
        Environment::Stock => vec![],
        Environment::Nix(n) => match realise(n, opts, &work.0) {
            Ok(v) => v,
            Err(e) => return setup_error(format!("environment: {e}"), isolation),
        },
    };

    // 5. Dependency fetch: fixed commands, with network, no project code.
    if let Some(setup) = spec.ecosystem.setup_command() {
        for tree in [&base, &patched] {
            let mut env = base_env(&home);
            env.extend(tree_env(tree));
            let mut env = with_environment(env, &env_extra);
            env.extend(opts.setup_env.iter().map(|(k, v)| (k.as_str(), v.clone())));
            match run_command(&["sh", "-c", setup], tree, &env, SETUP_TIMEOUT, &log) {
                Ok((RunStatus::Passed, ..)) => {}
                Ok((st, _, out)) => {
                    return setup_error(
                        format!("dependency fetch ({setup}) {st:?}: {}", out.trim()),
                        isolation,
                    )
                }
                Err(e) => return setup_error(format!("dependency fetch: {e}"), isolation),
            }
        }
    }

    // 6. Build once per tree: offline, isolated and unprivileged, because
    //    build scripts and macros are project code too. A base that does
    //    not build makes the proof inconclusive; a patched tree that does
    //    not build simply fails its runs.
    if let Some(build) = spec.ecosystem.build_argv() {
        for (tree, dir) in [(Tree::Base, &base), (Tree::Patched, &patched)] {
            let build_home = work.0.join(format!("home-build-{tree:?}"));
            let _ = fs::create_dir_all(&build_home);
            if let Err(e) = chown_run(dir).and_then(|_| chown_run(&build_home)) {
                return setup_error(e, isolation);
            }
            let mut env = base_env(&build_home);
            env.extend(tree_env(dir));
            let mut env = with_environment(env, &env_extra);
            env.extend(
                spec.ecosystem
                    .offline_env()
                    .iter()
                    .map(|(k, v)| (*k, v.to_string())),
            );
            let mut argv = run_prefix(isolation);
            argv.extend(build.iter().map(|s| s.to_string()));
            let build_log = work.0.join(format!("build-{tree:?}.log"));
            let timeout = Duration::from_secs(BUILD_TIMEOUT_SECS as u64);
            match run_command(&argv, dir, &env, timeout, &build_log) {
                Ok((RunStatus::Passed, ..)) => {}
                Ok((st, _, out)) if tree == Tree::Base => {
                    return setup_error(
                        format!("the base commit does not build ({st:?}): {}", out.trim()),
                        isolation,
                    )
                }
                Ok(_) => {}
                Err(e) => return setup_error(format!("build: {e}"), isolation),
            }
        }
    }

    // 7. Runs, interleaved so a slow or flaky moment hits both trees.
    let timeout = Duration::from_secs(spec.timeout_secs as u64);
    let mut runs = Vec::new();
    for attempt in 0..spec.repeats {
        for (tree, dir) in [(Tree::Base, &base), (Tree::Patched, &patched)] {
            if started.elapsed() + timeout > opts.budget {
                runs.push(RunResult {
                    tree,
                    attempt,
                    status: RunStatus::Skipped,
                    exit_code: None,
                    duration_ms: 0,
                    output_tail: String::new(),
                    tests_run: None,
                });
                continue;
            }
            let run_dir = work.0.join(format!("run-{attempt}-{tree:?}"));
            let run_home = work.0.join(format!("home-{attempt}-{tree:?}"));
            let _ = fs::create_dir_all(&run_home);
            if let Err(e) = copy_tree(dir, &run_dir)
                .and_then(|_| chown_run(&run_dir))
                .and_then(|_| chown_run(&run_home))
            {
                return setup_error(e, isolation);
            }
            let mut env = base_env(&run_home);
            env.extend(tree_env(&run_dir));
            let mut env = with_environment(env, &env_extra);
            env.extend(
                spec.ecosystem
                    .offline_env()
                    .iter()
                    .map(|(k, v)| (*k, v.to_string())),
            );
            let run_log = work.0.join(format!("run-{attempt}-{tree:?}.log"));
            let mut argv = run_prefix(isolation);
            argv.extend(spec.argv());
            let t0 = Instant::now();
            let result = run_command(&argv, &run_dir, &env, timeout, &run_log);
            let (status, exit_code, output_tail) = match result {
                Ok(r) => r,
                Err(e) => return setup_error(format!("could not start run: {e}"), isolation),
            };
            runs.push(RunResult {
                tree,
                attempt,
                status,
                exit_code,
                duration_ms: t0.elapsed().as_millis() as u64,
                output_tail,
                tests_run: fs::read(&run_log)
                    .ok()
                    .and_then(|b| count_tests(spec.ecosystem, &String::from_utf8_lossy(&b))),
            });
            let _ = fs::remove_dir_all(&run_dir);
            let _ = fs::remove_dir_all(&run_home);
        }
    }

    ProofReport {
        prover_version: VERSION.into(),
        setup: SetupResult {
            ok: true,
            error: None,
            network_isolated: isolation != Isolation::None,
            isolation: isolation.label(),
        },
        runs,
        cpu_ms: None,
    }
}

pub use collective_core::proof::{
    decode_prove_request as decode_request, encode_prove_request as encode_request, ProveRequest,
};
