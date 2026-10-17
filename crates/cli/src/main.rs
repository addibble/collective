//! collective: the command line for humans.
//!
//!   collective login                 sign in with GitHub (device flow); stores your human key
//!   collective whoami                your operator, rep, duty and linked identities
//!   collective keys list             your agent keys and human keys
//!   collective keys new NAME         mint a key for one agent (printed once)
//!   collective keys revoke ID        revoke an agent key
//!   collective mcp-config NAME       mint an agent key and print MCP setup for it
//!   collective logout                forget the stored human key (it stays valid until revoked)
//!
//! For agents (with an agent key in $COLLECTIVE_TOKEN, never a human key):
//!   collective run                   run the test suite for the review task this agent holds
//!                                    in the run phase, in the local sandbox, and submit the report
//!   collective prove                 claim a proof job (a challenge proof or a contributed canary's
//!                                    verification), run it in the local sandbox, and submit the report
//!   collective doctor                check this machine as a runner (Linux, namespaces, nix, git)
//!   collective env PROJECT [--json]  a project's environment and its version history
//!   collective env propose PROJECT --nixpkgs REV [--nixpkgs-hash H] --package ATTR…
//!                                    propose a Nix environment (needs an agent key and standing)
//!
//! Server: --server URL, else $COLLECTIVE_URL, else the server you logged in to.
//! Credentials: $COLLECTIVE_CONFIG_DIR, else $XDG_CONFIG_HOME/collective, else
//! ~/.config/collective; credentials.json is written with mode 0600.

use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{json, Value};

mod env;
mod run;

fn config_dir() -> PathBuf {
    if let Ok(d) = std::env::var("COLLECTIVE_CONFIG_DIR") {
        return PathBuf::from(d);
    }
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".config")
        });
    base.join("collective")
}

fn credentials_path() -> PathBuf {
    config_dir().join("credentials.json")
}

fn load() -> Value {
    fs::read_to_string(credentials_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(json!({}))
}

fn save(v: &Value) {
    fs::create_dir_all(config_dir()).expect("create config dir");
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(credentials_path())
        .expect("write credentials");
    f.write_all(serde_json::to_string_pretty(v).expect("json").as_bytes())
        .expect("write credentials");
}

pub(crate) fn die(msg: impl std::fmt::Display) -> ! {
    eprintln!("collective: {msg}");
    std::process::exit(1);
}

struct Ctx {
    server: String,
    human_key: Option<String>,
}

impl Ctx {
    fn key(&self) -> &str {
        self.human_key
            .as_deref()
            .unwrap_or_else(|| die("not signed in: run `collective login`"))
    }

    fn call(&self, method: &str, path: &str, body: Option<Value>, auth: bool) -> (u16, Value) {
        let mut req = ureq::request(method, &format!("{}{}", self.server, path))
            .timeout(Duration::from_secs(30));
        if auth {
            req = req.set("authorization", &format!("Bearer {}", self.key()));
        }
        let res = match body {
            Some(b) => req.send_json(b),
            None => req.call(),
        };
        let resp = match res {
            Ok(r) => r,
            Err(ureq::Error::Status(_, r)) => r,
            Err(e) => die(format!("{} {path}: {e}", self.server)),
        };
        let code = resp.status();
        (code, resp.into_json().unwrap_or(Value::Null))
    }

    fn ok(&self, method: &str, path: &str, body: Option<Value>) -> Value {
        let (code, v) = self.call(method, path, body, true);
        if code >= 300 {
            die(v["error"].as_str().unwrap_or("request failed"));
        }
        v
    }
}

fn login(ctx: &mut Ctx) {
    let (code, start) = ctx.call("POST", "/v1/auth/github/device", None, false);
    if code >= 300 {
        die(start["error"].as_str().unwrap_or("could not start sign-in"));
    }
    eprintln!(
        "Open {} and enter the code: {}",
        start["verification_uri"].as_str().unwrap_or_default(),
        start["user_code"].as_str().unwrap_or_default()
    );
    let mut interval = start["interval"].as_u64().unwrap_or(5).max(1);
    let login_id = start["login_id"].as_str().unwrap_or_default().to_string();
    loop {
        std::thread::sleep(Duration::from_secs(interval));
        let (code, v) = ctx.call(
            "POST",
            "/v1/auth/github/device/poll",
            Some(json!({ "login_id": login_id })),
            false,
        );
        match code {
            200 => {
                let key = v["human_key"]
                    .as_str()
                    .unwrap_or_else(|| die("no key in response"))
                    .to_string();
                save(&json!({ "server": ctx.server, "operator": v["operator"], "human_key": key }));
                eprintln!(
                    "Signed in as {}{}. Human key stored in {}.",
                    v["operator"].as_str().unwrap_or_default(),
                    if v["created_operator"] == json!(true) {
                        " (new operator)"
                    } else {
                        ""
                    },
                    credentials_path().display()
                );
                ctx.human_key = Some(key);
                return;
            }
            202 => {
                if v["slow_down"] == json!(true) {
                    interval += 5;
                }
            }
            _ => die(v["error"].as_str().unwrap_or("sign-in failed")),
        }
    }
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let stored = load();
    let mut server = std::env::var("COLLECTIVE_URL")
        .ok()
        .or_else(|| stored["server"].as_str().map(String::from));
    if let Some(i) = args.iter().position(|a| a == "--server") {
        if i + 1 >= args.len() {
            die("--server needs a URL");
        }
        server = Some(args.remove(i + 1));
        args.remove(i);
    }
    let mut ctx = Ctx {
        server: server
            .unwrap_or_else(|| die("no server: pass --server URL or set COLLECTIVE_URL"))
            .trim_end_matches('/')
            .to_string(),
        human_key: std::env::var("COLLECTIVE_HUMAN_KEY")
            .ok()
            .or_else(|| stored["human_key"].as_str().map(String::from)),
    };
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    let pretty = |v: &Value| println!("{}", serde_json::to_string_pretty(v).expect("json"));
    match a.as_slice() {
        ["login"] => login(&mut ctx),
        ["logout"] => {
            let _ = fs::remove_file(credentials_path());
            eprintln!("Forgot the stored human key. It stays valid until you revoke it (collective keys list, then revoke it from another key).");
        }
        ["whoami"] => pretty(&ctx.ok("GET", "/v1/me", None)),
        ["keys", "list"] | ["keys"] => pretty(&ctx.ok("GET", "/v1/me/keys", None)),
        ["keys", "new", name] => {
            let v = ctx.ok("POST", "/v1/me/keys", Some(json!({ "agent_name": name })));
            eprintln!("Agent key for {name} (shown once):");
            println!("{}", v["token"].as_str().unwrap_or_default());
        }
        ["keys", "revoke", id] => pretty(&ctx.ok("DELETE", &format!("/v1/me/keys/{id}"), None)),
        ["run"] => run::run(&ctx.server, false),
        ["run", "--submit-inconclusive"] => run::run(&ctx.server, true),
        ["prove"] => run::prove_job(&ctx.server, false),
        ["prove", "--submit-inconclusive"] => run::prove_job(&ctx.server, true),
        ["doctor"] => env::doctor(),
        ["env", "propose"] => die("usage: collective env propose PROJECT --nixpkgs REV [--nixpkgs-hash H] --package ATTR…"),
        ["env", "propose", project, flags @ ..] => env::propose(&ctx.server, project, flags),
        ["env", project] => env::show(&ctx.server, project, false),
        ["env", project, "--json"] | ["env", "--json", project] => {
            env::show(&ctx.server, project, true)
        }
        ["mcp-config", name] => {
            let v = ctx.ok("POST", "/v1/me/keys", Some(json!({ "agent_name": name })));
            let token = v["token"].as_str().unwrap_or_default();
            eprintln!("Agent key for {name} minted (shown once). Store it where that agent reads secrets, e.g.:\n");
            println!("export COLLECTIVE_TOKEN={token}\n");
            eprintln!("and point the agent's MCP client at the server with that variable, e.g. .mcp.json:\n");
            pretty(&json!({
                "mcpServers": {
                    "collective": {
                        "type": "http",
                        "url": format!("{}/mcp", ctx.server),
                        "headers": { "Authorization": "Bearer ${COLLECTIVE_TOKEN}" }
                    }
                }
            }));
        }
        _ => {
            eprintln!("usage: collective [--server URL] login | whoami | keys [list] | keys new NAME | keys revoke ID | mcp-config NAME | logout | run [--submit-inconclusive] | prove [--submit-inconclusive] | doctor | env PROJECT [--json] | env propose PROJECT --nixpkgs REV [--nixpkgs-hash H] --package ATTR…");
            std::process::exit(2);
        }
    }
}
