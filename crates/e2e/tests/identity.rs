//! Human identity end to end: GitHub sign-in (web and device flows, against a
//! fake GitHub), human keys versus agent keys, linking, and the CLI.
//!
//! Needs `wrangler dev` with these in .dev.vars, and `cargo build -p collective-cli`:
//!   GITHUB_CLIENT_ID=test-client  GITHUB_CLIENT_SECRET=test-secret
//!   GITHUB_OAUTH_BASE=http://127.0.0.1:18081  GITHUB_API_BASE=http://127.0.0.1:18081
//!   REGISTER_LIMIT_PER_HOUR=50

use std::collections::HashMap;
use std::process::Command;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

const FAKE_GITHUB: &str = "127.0.0.1:18081";

/// A stand-in for GitHub's OAuth endpoints. `code-<login>` exchanges for a
/// token for <login>; a device login is pending once, then approved for the
/// login queued with `approve_device`.
struct FakeGitHub {
    device_user: Arc<Mutex<String>>,
}

impl FakeGitHub {
    fn start() -> FakeGitHub {
        let device_user = Arc::new(Mutex::new(String::new()));
        let server = tiny_http::Server::http(FAKE_GITHUB).expect("bind fake GitHub");
        let du = device_user.clone();
        std::thread::spawn(move || {
            let mut polls: HashMap<String, u32> = HashMap::new();
            for mut req in server.incoming_requests() {
                let mut body = String::new();
                let _ = req.as_reader().read_to_string(&mut body);
                let b: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                let url = req.url().to_string();
                let auth = req
                    .headers()
                    .iter()
                    .find(|h| h.field.equiv("authorization"))
                    .map(|h| h.value.to_string())
                    .unwrap_or_default();
                let reply = match (req.method().as_str(), url.split('?').next().unwrap_or("")) {
                    ("POST", "/login/device/code") => {
                        let user = du.lock().unwrap().clone();
                        json!({ "device_code": format!("dev-{user}"), "user_code": "WDJB-MJHT", "verification_uri": "https://github.com/login/device", "interval": 1, "expires_in": 900 })
                    }
                    ("POST", "/login/oauth/access_token") => {
                        if let Some(code) = b["code"].as_str() {
                            if b["client_secret"] != "test-secret" {
                                json!({ "error": "incorrect_client_credentials" })
                            } else if let Some(user) = code.strip_prefix("code-") {
                                json!({ "access_token": format!("tok-{user}"), "token_type": "bearer" })
                            } else {
                                json!({ "error": "bad_verification_code" })
                            }
                        } else if let Some(dev) = b["device_code"].as_str() {
                            let n = polls.entry(dev.to_string()).or_insert(0);
                            *n += 1;
                            if *n == 1 {
                                json!({ "error": "authorization_pending" })
                            } else {
                                json!({ "access_token": format!("tok-{}", dev.trim_start_matches("dev-")) })
                            }
                        } else {
                            json!({ "error": "unsupported_grant_type" })
                        }
                    }
                    ("GET", "/user") => match auth.strip_prefix("Bearer tok-") {
                        Some(user) => {
                            json!({ "id": user.bytes().fold(7u64, |h, c| h.wrapping_mul(31).wrapping_add(c as u64)) % 1_000_000_000, "login": user })
                        }
                        None => json!({ "message": "Bad credentials" }),
                    },
                    _ => json!({ "message": "Not Found" }),
                };
                let _ = req.respond(
                    tiny_http::Response::from_string(reply.to_string()).with_header(
                        tiny_http::Header::from_bytes("content-type", "application/json").unwrap(),
                    ),
                );
            }
        });
        FakeGitHub { device_user }
    }

    fn approve_device(&self, login: &str) {
        *self.device_user.lock().unwrap() = login.to_string();
    }
}

fn base() -> String {
    std::env::var("COLLECTIVE_URL").unwrap_or_else(|_| "http://localhost:8787".into())
}

fn call(
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (u16, Value, String) {
    let agent = ureq::AgentBuilder::new().redirects(0).build();
    let mut req = agent.request(method, &format!("{}{}", base(), path));
    if let Some(t) = token {
        req = req.set("authorization", &format!("Bearer {t}"));
    }
    let res = match body {
        Some(b) => req.send_json(b),
        None => req.call(),
    };
    let resp = match res {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(e) => panic!("{method} {path}: {e}"),
    };
    let status = resp.status();
    let location = resp.header("location").unwrap_or_default().to_string();
    let text = resp.into_string().unwrap();
    let v = serde_json::from_str(&text).unwrap_or(Value::String(text));
    (status, v, location)
}

fn cli(config: &std::path::Path, args: &[&str]) -> (bool, String, String) {
    let bin =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/collective");
    let out = Command::new(&bin)
        .args(["--server", &base()])
        .args(args)
        .env("COLLECTIVE_CONFIG_DIR", config)
        .env_remove("COLLECTIVE_URL")
        .output()
        .unwrap_or_else(|e| {
            panic!(
                "run {} (cargo build -p collective-cli first): {e}",
                bin.display()
            )
        });
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn query_param(url: &str, key: &str) -> String {
    let query = url.split_once('?').map(|(_, q)| q).unwrap_or_default();
    query
        .split('&')
        .find_map(|kv| kv.strip_prefix(&format!("{key}=")))
        .unwrap_or_else(|| panic!("no {key} in {url}"))
        .to_string()
}

fn human_key_in(html: &str) -> String {
    let i = html.find("clv_human_").expect("a human key on the page");
    html[i..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect()
}

/// The GitHub web flow: start, (the browser approves), callback.
fn web_sign_in(login: &str, link_with: Option<&str>) -> (u16, Value) {
    let authorize = match link_with {
        None => {
            let (s, _, location) = call("GET", "/v1/auth/github/start", None, None);
            assert_eq!(s, 302);
            location
        }
        Some(key) => {
            let (s, v, _) = call("POST", "/v1/auth/github/start", Some(key), None);
            assert_eq!(s, 200, "{v}");
            v["authorize_url"].as_str().unwrap().to_string()
        }
    };
    assert!(
        authorize.starts_with(&format!("http://{FAKE_GITHUB}/login/oauth/authorize?")),
        "{authorize}"
    );
    let state = query_param(&authorize, "state");
    let (s, page, _) = call(
        "GET",
        &format!("/v1/auth/github/callback?code=code-{login}&state={state}"),
        None,
        None,
    );
    // State is single use.
    let (again, _, _) = call(
        "GET",
        &format!("/v1/auth/github/callback?code=code-{login}&state={state}"),
        None,
        None,
    );
    assert_eq!(again, 400);
    (s, page)
}

#[test]
#[ignore = "needs a running Worker configured for the fake GitHub; see module docs"]
fn humans_sign_in_and_manage_their_agents_keys() {
    let _gh = FakeGitHub::start();
    let run = format!(
        "{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
            % 0xffffff
    );
    let config = std::env::temp_dir().join(format!("collective-cli-{run}"));

    // 1. The CLI signs in with the device flow and stores a human key, 0600.
    _gh.approve_device(&format!("octo{run}"));
    let (ok, _, err) = cli(&config, &["login"]);
    assert!(ok, "{err}");
    assert!(
        err.contains(&format!("Signed in as octo{run} (new operator)")),
        "{err}"
    );
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(config.join("credentials.json"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    let creds: Value =
        serde_json::from_str(&std::fs::read_to_string(config.join("credentials.json")).unwrap())
            .unwrap();
    let human = creds["human_key"].as_str().unwrap().to_string();
    assert!(human.starts_with("clv_human_"));

    let (ok, out, _) = cli(&config, &["whoami"]);
    assert!(ok);
    let me: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(me["identities"][0]["provider"], "github");
    assert_eq!(me["identities"][0]["login"], format!("octo{run}"));

    // 2. One key per agent, all one operator.
    let (ok, laptop, _) = cli(&config, &["keys", "new", "laptop-claude"]);
    assert!(ok);
    let laptop = laptop.trim().to_string();
    let (ok, ci_out, _) = cli(&config, &["mcp-config", "ci-agent"]);
    assert!(ok);
    assert!(
        ci_out.contains("\"Authorization\": \"Bearer ${COLLECTIVE_TOKEN}\""),
        "{ci_out}"
    );
    let ci = ci_out
        .lines()
        .find_map(|l| l.strip_prefix("export COLLECTIVE_TOKEN="))
        .unwrap()
        .to_string();
    for t in [&laptop, &ci] {
        let (s, v, _) = call("GET", "/v1/me", Some(t), None);
        assert_eq!(s, 200, "{v}");
        assert_eq!(v["operator_id"], me["operator_id"]);
    }

    // 3. Human keys manage; agent keys act. Neither does the other's job.
    let (s, _, _) = call(
        "POST",
        "/v1/patches",
        Some(&human),
        Some(json!({ "project": "https://github.com/x/y", "base_commit": "abcdef1", "diff": "" })),
    );
    assert_eq!(s, 401, "a human key cannot act as an agent");
    let (s, _, _) = call("GET", "/v1/me/keys", Some(&laptop), None);
    assert_eq!(s, 401, "an agent key cannot manage keys");

    // 4. Revoking one agent leaves the others working.
    let (ok, list, _) = cli(&config, &["keys", "list"]);
    assert!(ok);
    let list: Value = serde_json::from_str(&list).unwrap();
    let laptop_id = list["agent_keys"]
        .as_array()
        .unwrap()
        .iter()
        .find(|k| k["name"] == "laptop-claude")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let (ok, _, _) = cli(&config, &["keys", "revoke", &laptop_id]);
    assert!(ok);
    assert_eq!(call("GET", "/v1/me", Some(&laptop), None).0, 401);
    assert_eq!(call("GET", "/v1/me", Some(&ci), None).0, 200);

    // 5. Signing in again on the web finds the same operator and issues a new human key.
    let (s, page) = web_sign_in(&format!("octo{run}"), None);
    assert_eq!(s, 200, "{page}");
    let second = human_key_in(page.as_str().unwrap());
    assert_ne!(second, human);
    assert!(page
        .as_str()
        .unwrap()
        .contains(&format!("Signed in as octo{run}")));
    assert_eq!(
        call("GET", "/v1/me", Some(&second), None).1["operator_id"],
        me["operator_id"]
    );

    // 6. A hand-registered operator links GitHub with its human key.
    let (s, reg, _) = call(
        "POST",
        "/v1/register",
        None,
        Some(json!({ "operator": format!("linkme{run}"), "agent_name": "bot" })),
    );
    assert_eq!(s, 201, "{reg}");
    let reg_human = reg["human_key"]
        .as_str()
        .expect("new operators get a human key")
        .to_string();
    // Its only human key cannot be revoked while no identity is linked.
    let (_, keys, _) = call("GET", "/v1/me/keys", Some(&reg_human), None);
    let only = keys["human_keys"][0]["id"].as_str().unwrap().to_string();
    assert_eq!(
        call(
            "DELETE",
            &format!("/v1/me/human-keys/{only}"),
            Some(&reg_human),
            None
        )
        .0,
        409
    );
    let (s, page) = web_sign_in(&format!("linker{run}"), Some(&reg_human));
    assert_eq!(s, 200, "{page}");
    assert!(page.as_str().unwrap().contains("GitHub is now linked"));
    let (_, profile, _) = call("GET", &format!("/v1/operators/linkme{run}"), None, None);
    assert_eq!(
        profile["verified_identities"][0]["login"],
        format!("linker{run}"),
        "{profile}"
    );
    assert_eq!(
        call(
            "DELETE",
            &format!("/v1/me/human-keys/{only}"),
            Some(&reg_human),
            None
        )
        .0,
        200
    );

    // 7. That GitHub account cannot be linked to a second operator.
    let (_, other, _) = call(
        "POST",
        "/v1/register",
        None,
        Some(json!({ "operator": format!("other{run}"), "agent_name": "bot" })),
    );
    let (s, _) = web_sign_in(
        &format!("linker{run}"),
        Some(other["human_key"].as_str().unwrap()),
    );
    assert_eq!(s, 409);

    // 8. A squatted handle is never taken over: the GitHub user gets a suffixed one.
    call(
        "POST",
        "/v1/register",
        None,
        Some(json!({ "operator": format!("squat{run}"), "agent_name": "bot" })),
    );
    let (s, page) = web_sign_in(&format!("squat{run}"), None);
    assert_eq!(s, 200);
    assert!(
        page.as_str()
            .unwrap()
            .contains(&format!("Signed in as squat{run}-github")),
        "{page}"
    );

    // 9. A bad code is refused.
    let (s, _, location) = call("GET", "/v1/auth/github/start", None, None);
    assert_eq!(s, 302);
    let state = query_param(&location, "state");
    assert_eq!(
        call(
            "GET",
            &format!("/v1/auth/github/callback?code=nope&state={state}"),
            None,
            None
        )
        .0,
        400
    );

    // 10. Genesis influence: an admin grant shows publicly and is capped.
    let admin = std::env::var("COLLECTIVE_ADMIN_TOKEN").unwrap_or_else(|_| "dev-admin".into());
    let grant = |amount: f64| {
        ureq::post(&format!("{}/v1/admin/genesis", base()))
            .set("x-admin-token", &admin)
            .send_json(json!({ "operator": format!("octo{run}"), "amount": amount, "note": "founding operator" }))
            .map(|r| r.status())
            .unwrap_or_else(|e| match e {
                ureq::Error::Status(c, _) => c,
                e => panic!("{e}"),
            })
    };
    assert_eq!(grant(1000.0), 400, "grants are capped");
    assert_eq!(grant(50.0), 200);
    let (_, profile, _) = call("GET", &format!("/v1/operators/octo{run}"), None, None);
    assert!(
        (profile["genesis_influence"].as_f64().unwrap() - 50.0).abs() < 0.01,
        "{profile}"
    );
    let (_, me_now, _) = call("GET", "/v1/me", Some(&human), None);
    assert!(me_now["genesis_influence"].as_f64().unwrap() > 49.9);

    let _ = std::fs::remove_dir_all(config);
}

/// Anonymous registration is rate limited per client address.
#[test]
#[ignore = "needs a running Worker with REGISTER_LIMIT_PER_HOUR=50; see module docs"]
fn anonymous_registration_is_rate_limited() {
    let run = format!(
        "{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
            % 0xffffff
    );
    // A documentation-range address unique to this run (Cloudflare sets this
    // header at the edge in production; local dev honors it).
    let ip = format!(
        "198.51.100.{}",
        u32::from_str_radix(&run[run.len() - 2..], 16).unwrap() % 250 + 1
    );
    let mut codes = Vec::new();
    for i in 0..51 {
        let r = ureq::post(&format!("{}/v1/register", base()))
            .set("cf-connecting-ip", &ip)
            .send_json(json!({ "operator": format!("rl{run}-{i}"), "agent_name": "bot" }));
        codes.push(match r {
            Ok(r) => r.status(),
            Err(ureq::Error::Status(c, _)) => c,
            Err(e) => panic!("{e}"),
        });
    }
    assert!(codes[..50].iter().all(|c| *c == 201), "{codes:?}");
    assert_eq!(codes[50], 429);
}
