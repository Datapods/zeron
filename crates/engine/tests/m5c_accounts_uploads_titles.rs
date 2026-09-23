//! M5c integration: agent-account slot mechanics (claude-swap), uploads
//! chunk→commit→readback + path jail, chat auto-titling with the mock harness,
//! and the RPC dispatch for each new method over the memory transport.
//!
//! Account tests use explicit `AgentAccountsConfig` paths under a tempdir (never
//! the real `~/.claude` / `~/.codex`), so they are hermetic and parallel-safe.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as BASE64_URL;

use zeron_engine::{
    AgentAccounts, AgentAccountsConfig, EngineCore, HarnessRegistry, Repos, Uploads,
    worktree_branch_from_title,
};
use zeron_harness::mock::MockHarness;
use zeron_proto::{
    AgentAccountsSnapshot, AgentEvent, AgentLoginMode, AgentLoginStatus, DoneStatus, HarnessId,
    SandboxLevel,
};
use zeron_rpc::methods;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// AgentAccounts wired to temp claude/codex homes.
fn test_accounts(root: &Path) -> (AgentAccounts, AgentAccountsConfig) {
    let config = AgentAccountsConfig {
        data_dir: root.join("data"),
        claude_config_dir: root.join("claude"),
        claude_config_file: root.join("claude.json"),
        codex_home: root.join("codex"),
        cursor_sdk_auth_file: root.join("cursor-sdk").join("auth.json"),
        claude_keychain_service: "Claude Code-credentials-zeron-test".into(),
        // Refused instantly: ownership is "unverifiable" unless a test serves it.
        claude_profile_url: "http://127.0.0.1:9/api/oauth/profile".into(),
    };
    (AgentAccounts::new(config.clone()), config)
}

fn write_claude_login(config: &AgentAccountsConfig, email: &str, uuid: &str, token: &str) {
    std::fs::create_dir_all(&config.claude_config_dir).expect("claude dir");
    std::fs::write(
        &config.claude_config_file,
        serde_json::json!({
            "oauthAccount": {
                "accountUuid": uuid,
                "emailAddress": email,
                "displayName": "Test User",
                "organizationName": "Test Org",
                "organizationType": "claude_max",
                "organizationRateLimitTier": "default_claude_max_20x",
            },
            "userID": format!("user-{uuid}"),
            "projects": { "/keep/me": { "history": [] } },
        })
        .to_string(),
    )
    .expect("claude config");
    std::fs::write(
        config.claude_config_dir.join(".credentials.json"),
        serde_json::json!({
            "claudeAiOauth": {
                "accessToken": token,
                "refreshToken": format!("refresh-{token}"),
                // Far-future expiry: usage probes must never try to rotate it.
                "expiresAt": 4_102_444_800_000i64,
            }
        })
        .to_string(),
    )
    .expect("claude creds");
}

/// An unsigned JWT with the claims codex mines from `id_token`.
fn fake_id_token(email: &str, account_id: &str, plan: &str) -> String {
    let header = BASE64_URL.encode(br#"{"alg":"none"}"#);
    let payload = BASE64_URL.encode(
        serde_json::json!({
            "email": email,
            "name": "Codex User",
            "https://api.openai.com/auth": {
                "chatgpt_account_id": account_id,
                "chatgpt_plan_type": plan,
            },
        })
        .to_string(),
    );
    format!("{header}.{payload}.x")
}

fn write_codex_login(config: &AgentAccountsConfig, email: &str, account_id: &str) {
    std::fs::create_dir_all(&config.codex_home).expect("codex home");
    std::fs::write(
        config.codex_home.join("auth.json"),
        serde_json::json!({
            "tokens": {
                "id_token": fake_id_token(email, account_id, "plus"),
                "access_token": format!("at-{account_id}"),
                "account_id": account_id,
            }
        })
        .to_string(),
    )
    .expect("codex auth");
}

fn account_emails(snapshot: &AgentAccountsSnapshot, harness: HarnessId) -> Vec<(String, bool)> {
    snapshot
        .accounts
        .iter()
        .filter(|a| a.harness == harness)
        .map(|a| (a.email.clone().unwrap_or_default(), a.active))
        .collect()
}

fn assemble_with_mock(dir: &Path, script: Vec<AgentEvent>) -> EngineCore {
    std::fs::create_dir_all(dir).expect("data dir");
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(MockHarness { script }));
    EngineCore::assemble(dir, Arc::new(registry), HarnessId::Mock, None).expect("engine assembles")
}

async fn git(cwd: &Path, args: &[&str]) {
    let output = tokio::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@test")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@test")
        .output()
        .await
        .expect("git spawns");
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}

async fn init_repo(dir: &Path) {
    std::fs::create_dir_all(dir).expect("repo dir");
    git(dir, &["init", "-b", "main"]).await;
    std::fs::write(dir.join("a.txt"), "one\n").expect("write a.txt");
    git(dir, &["add", "."]).await;
    git(dir, &["commit", "-m", "initial"]).await;
}

/// Poll until `probe` yields Some, or panic at the deadline.
async fn wait_for<T>(what: &str, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(value) = probe() {
            return value;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ---------------------------------------------------------------------------
// Agent accounts — claude slot swap round trip
// ---------------------------------------------------------------------------

#[tokio::test]
async fn claude_slot_swap_round_trip() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (accounts, config) = test_accounts(tmp.path());

    // Live login = Alice. Listing detects + auto-snapshots her into a slot.
    write_claude_login(&config, "alice@example.com", "uuid-alice", "token-alice");
    let snapshot = accounts.list(false).await.expect("list");
    assert_eq!(
        account_emails(&snapshot, HarnessId::ClaudeCode),
        vec![("alice@example.com".to_string(), true)]
    );
    let alice = &snapshot.accounts[0];
    assert_eq!(
        alice.plan_label.as_deref(),
        Some("Max 20×"),
        "plan label parse"
    );
    assert_eq!(alice.display_name.as_deref(), Some("Test User"));
    assert_eq!(alice.organization.as_deref(), Some("Test Org"));
    assert!(alice.switchable);
    assert!(snapshot.warnings.is_empty());
    let alice_id = alice.id.clone();
    assert_eq!(alice_id.len(), 16, "slot id is 16 hex chars");

    // Bob logs in via the CLI (live files replaced) — next list snapshots Bob
    // and shows Alice as a saved, inactive slot.
    write_claude_login(&config, "bob@example.com", "uuid-bob", "token-bob");
    let snapshot = accounts.list(false).await.expect("list bob");
    let mut emails = account_emails(&snapshot, HarnessId::ClaudeCode);
    emails.sort();
    assert_eq!(
        emails,
        vec![
            ("alice@example.com".to_string(), false),
            ("bob@example.com".to_string(), true)
        ]
    );

    // Activate Alice: her slot's tokens land in the live files, Bob's live
    // session is auto-snapshotted first, identity merged into claude.json.
    let snapshot = accounts
        .activate(HarnessId::ClaudeCode, &alice_id)
        .await
        .expect("activate");
    let mut emails = account_emails(&snapshot, HarnessId::ClaudeCode);
    emails.sort();
    assert_eq!(
        emails,
        vec![
            ("alice@example.com".to_string(), true),
            ("bob@example.com".to_string(), false)
        ]
    );
    let creds: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(config.claude_config_dir.join(".credentials.json"))
            .expect("creds readable"),
    )
    .expect("creds json");
    assert_eq!(creds["claudeAiOauth"]["accessToken"], "token-alice");
    let cfg: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&config.claude_config_file).expect("cfg"))
            .expect("cfg json");
    assert_eq!(cfg["oauthAccount"]["emailAddress"], "alice@example.com");
    assert_eq!(cfg["userID"], "user-uuid-alice");
    // The rest of the config survived the merge (only identity fields swapped).
    assert!(
        cfg["projects"]["/keep/me"].is_object(),
        "unrelated config keys preserved"
    );

    // Slot files: exactly two, under data/agent-accounts/claude-code.
    let slots_dir = config.data_dir.join("agent-accounts").join("claude-code");
    let slot_count = std::fs::read_dir(&slots_dir)
        .expect("slots dir")
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .count();
    assert_eq!(slot_count, 2);

    // Corrupt claude.json → activate must refuse rather than wipe it.
    std::fs::write(&config.claude_config_file, "{ definitely not json").expect("corrupt");
    let bob_id = snapshot
        .accounts
        .iter()
        .find(|a| a.email.as_deref() == Some("bob@example.com"))
        .expect("bob listed")
        .id
        .clone();
    let refused = accounts.activate(HarnessId::ClaudeCode, &bob_id).await;
    assert!(refused.is_err(), "parse-failed config must block the swap");
    assert_eq!(
        std::fs::read_to_string(&config.claude_config_file).expect("still there"),
        "{ definitely not json",
        "the unparsable config was left untouched"
    );
}

#[tokio::test]
async fn claude_account_switch_keeps_live_mcp_oauth() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (accounts, config) = test_accounts(tmp.path());
    let creds_file = config.claude_config_dir.join(".credentials.json");

    write_claude_login(&config, "alice@example.com", "uuid-alice", "token-alice");
    // Alice's first snapshot includes a MCP token that will go stale.
    std::fs::write(
        &creds_file,
        serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "token-alice",
                "refreshToken": "refresh-token-alice",
                "expiresAt": 4_102_444_800_000i64,
            },
            "mcpOAuth": { "github": { "accessToken": "stale-github" } },
            "pluginSecrets": { "old": true },
            "trustedDeviceToken": "alice-device",
        })
        .to_string(),
    )
    .expect("alice mcp creds");
    let snapshot = accounts.list(false).await.expect("list alice");
    let alice_id = snapshot
        .accounts
        .iter()
        .find(|a| a.email.as_deref() == Some("alice@example.com"))
        .expect("alice listed")
        .id
        .clone();

    // Bob becomes live; MCP tokens rotate while he is the active login.
    write_claude_login(&config, "bob@example.com", "uuid-bob", "token-bob");
    std::fs::write(
        &creds_file,
        serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "token-bob",
                "refreshToken": "refresh-token-bob",
                "expiresAt": 4_102_444_800_000i64,
            },
            "mcpOAuth": { "github": { "accessToken": "live-github" } },
            "pluginSecrets": { "live": true },
            "trustedDeviceToken": "bob-device",
        })
        .to_string(),
    )
    .expect("bob mcp creds");
    accounts.list(false).await.expect("list bob");

    accounts
        .activate(HarnessId::ClaudeCode, &alice_id)
        .await
        .expect("activate alice");

    let creds: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&creds_file).expect("creds readable"))
            .expect("creds json");
    assert_eq!(creds["claudeAiOauth"]["accessToken"], "token-alice");
    assert_eq!(
        creds["trustedDeviceToken"], "alice-device",
        "account-bound device token stays with the slot"
    );
    assert_eq!(
        creds["mcpOAuth"]["github"]["accessToken"], "live-github",
        "live MCP OAuth must survive the switch, not Alice's stale snapshot"
    );
    assert_eq!(creds["pluginSecrets"]["live"], true);
    assert!(
        creds["pluginSecrets"].get("old").is_none(),
        "slot plugin secrets must not clobber the live generation"
    );
}

#[tokio::test]
async fn claude_account_switch_keeps_mcp_when_target_slot_has_none() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (accounts, config) = test_accounts(tmp.path());
    let creds_file = config.claude_config_dir.join(".credentials.json");

    // Alice saved via the oauth-only shape (new login / usage refresh).
    write_claude_login(&config, "alice@example.com", "uuid-alice", "token-alice");
    let snapshot = accounts.list(false).await.expect("list alice");
    let alice_id = snapshot.accounts[0].id.clone();

    write_claude_login(&config, "bob@example.com", "uuid-bob", "token-bob");
    std::fs::write(
        &creds_file,
        serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "token-bob",
                "refreshToken": "refresh-token-bob",
                "expiresAt": 4_102_444_800_000i64,
            },
            "mcpOAuth": { "linear": { "accessToken": "live-linear" } },
        })
        .to_string(),
    )
    .expect("bob mcp creds");
    accounts.list(false).await.expect("list bob");

    accounts
        .activate(HarnessId::ClaudeCode, &alice_id)
        .await
        .expect("activate alice");

    let creds: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&creds_file).expect("creds readable"))
            .expect("creds json");
    assert_eq!(creds["claudeAiOauth"]["accessToken"], "token-alice");
    assert_eq!(creds["mcpOAuth"]["linear"]["accessToken"], "live-linear");
}

#[tokio::test]
async fn codex_slot_swap_and_api_key_detection() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (accounts, config) = test_accounts(tmp.path());

    write_codex_login(&config, "carol@example.com", "acct-carol");
    let snapshot = accounts.list(false).await.expect("list");
    let carol = snapshot
        .accounts
        .iter()
        .find(|a| a.harness == HarnessId::Codex)
        .expect("codex account");
    assert_eq!(carol.email.as_deref(), Some("carol@example.com"));
    assert_eq!(carol.plan_label.as_deref(), Some("ChatGPT Plus"));
    assert!(carol.active);
    let carol_id = carol.id.clone();

    // Second login (Dave) becomes live; swap back to Carol.
    write_codex_login(&config, "dave@example.com", "acct-dave");
    accounts.list(false).await.expect("list dave");
    let snapshot = accounts
        .activate(HarnessId::Codex, &carol_id)
        .await
        .expect("activate carol");
    let mut emails = account_emails(&snapshot, HarnessId::Codex);
    emails.sort();
    assert_eq!(
        emails,
        vec![
            ("carol@example.com".to_string(), true),
            ("dave@example.com".to_string(), false)
        ]
    );
    let auth: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(config.codex_home.join("auth.json")).expect("auth"),
    )
    .expect("auth json");
    assert_eq!(auth["tokens"]["account_id"], "acct-carol");

    // API-key mode: no tokens, just the key.
    std::fs::write(
        config.codex_home.join("auth.json"),
        serde_json::json!({ "OPENAI_API_KEY": "sk-test-12345678abcd" }).to_string(),
    )
    .expect("api key auth");
    let snapshot = accounts.list(false).await.expect("list api key");
    let key_account = snapshot
        .accounts
        .iter()
        .find(|a| a.harness == HarnessId::Codex && a.active)
        .expect("api key account");
    assert_eq!(key_account.plan_label.as_deref(), Some("API key"));
    assert_eq!(key_account.email.as_deref(), Some("API key ·…abcd"));
}

// ---------------------------------------------------------------------------
// Switch stability: the ways a swap used to cost a saved login
// ---------------------------------------------------------------------------

/// Overwrite only the live Claude credential store (the identity file is left
/// alone — exactly what a still-running Claude Code session does when it
/// refreshes and saves its in-memory login).
fn write_claude_tokens(config: &AgentAccountsConfig, oauth: serde_json::Value) {
    std::fs::write(
        config.claude_config_dir.join(".credentials.json"),
        serde_json::json!({ "claudeAiOauth": oauth }).to_string(),
    )
    .expect("claude creds");
}

fn live_tokens(access: &str, refresh: &str) -> serde_json::Value {
    serde_json::json!({
        "accessToken": access,
        "refreshToken": refresh,
        "expiresAt": 4_102_444_800_000i64,
    })
}

/// The stored slot for `account_key`'s refresh token (read straight off disk).
fn slot_refresh_token(config: &AgentAccountsConfig, email: &str) -> Option<String> {
    let dir = config.data_dir.join("agent-accounts").join("claude-code");
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .filter_map(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .find(|slot| slot["profile"]["email"] == email)
        .and_then(|slot| {
            slot["credentials"]["claudeAiOauth"]["refreshToken"]
                .as_str()
                .map(str::to_string)
        })
}

fn active_email(snapshot: &AgentAccountsSnapshot, harness: HarnessId) -> Option<String> {
    snapshot
        .accounts
        .iter()
        .find(|a| a.harness == harness && a.active)
        .and_then(|a| a.email.clone())
}

fn account_id(snapshot: &AgentAccountsSnapshot, email: &str) -> String {
    snapshot
        .accounts
        .iter()
        .find(|a| a.email.as_deref() == Some(email))
        .unwrap_or_else(|| panic!("{email} listed"))
        .id
        .clone()
}

/// A stand-in for Anthropic's `/api/oauth/profile`: answers with the account
/// each bearer token was registered to, 401 for anything else.
async fn serve_claude_profiles(owners: Vec<(&'static str, &'static str, &'static str)>) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let owners = owners.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 16 * 1024];
                let mut len = 0;
                while len < buf.len() {
                    let read = socket.read(&mut buf[len..]).await.unwrap_or(0);
                    if read == 0 {
                        break;
                    }
                    len += read;
                    if buf[..len].windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let request = String::from_utf8_lossy(&buf[..len]).to_lowercase();
                let owner = owners
                    .iter()
                    .find(|(token, ..)| request.contains(&format!("bearer {token}")));
                let (status, body) = match owner {
                    Some((_, uuid, email)) => (
                        "200 OK",
                        serde_json::json!({ "account": { "uuid": uuid, "email_address": email } })
                            .to_string(),
                    ),
                    None => ("401 Unauthorized", "{}".to_string()),
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    format!("http://{addr}/api/oauth/profile")
}

#[tokio::test]
async fn claude_wiped_login_never_overwrites_its_slot() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (accounts, config) = test_accounts(tmp.path());
    write_claude_login(&config, "alice@example.com", "uuid-alice", "token-alice");
    accounts.list(false).await.expect("list");

    // Claude Code's reaction to a rejected refresh: tokens emptied in place,
    // wrapper and metadata kept.
    write_claude_tokens(
        &config,
        serde_json::json!({
            "accessToken": "",
            "refreshToken": "",
            "expiresAt": 0,
            "scopes": ["user:inference"],
            "subscriptionType": "max",
        }),
    );
    let snapshot = accounts.list(false).await.expect("list wiped");
    assert_eq!(
        slot_refresh_token(&config, "alice@example.com").as_deref(),
        Some("refresh-token-alice"),
        "the emptied login must not be snapshotted over the slot"
    );
    assert!(
        snapshot
            .warnings
            .iter()
            .any(|w| w.harness == HarnessId::ClaudeCode && w.message.contains("signed out")),
        "the sign-out is surfaced: {:?}",
        snapshot.warnings
    );
}

#[tokio::test]
async fn claude_stale_session_write_back_does_not_poison_slots() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (_, mut config) = test_accounts(tmp.path());
    config.claude_profile_url =
        serve_claude_profiles(vec![("token-bob-2", "uuid-bob", "bob@example.com")]).await;
    let accounts = AgentAccounts::new(config.clone());

    write_claude_login(&config, "alice@example.com", "uuid-alice", "token-alice");
    accounts.list(false).await.expect("list alice");
    write_claude_login(&config, "bob@example.com", "uuid-bob", "token-bob");
    let snapshot = accounts.list(false).await.expect("list bob");
    accounts
        .activate(
            HarnessId::ClaudeCode,
            &account_id(&snapshot, "alice@example.com"),
        )
        .await
        .expect("switch to alice");

    // A Claude Code session still running as Bob saves its login back over
    // the switch; ~/.claude.json keeps naming Alice (claude-swap #117).
    write_claude_tokens(&config, live_tokens("token-bob", "refresh-token-bob"));
    let snapshot = accounts.list(false).await.expect("list after write-back");
    assert_eq!(
        slot_refresh_token(&config, "alice@example.com").as_deref(),
        Some("refresh-token-alice"),
        "Alice's slot keeps her own login"
    );
    assert_eq!(
        active_email(&snapshot, HarnessId::ClaudeCode).as_deref(),
        Some("bob@example.com"),
        "the tokens are Bob's, so Bob is what the CLI really runs as"
    );
    assert!(
        snapshot
            .warnings
            .iter()
            .any(|w| w.message.contains("signed in as bob@example.com")),
        "{:?}",
        snapshot.warnings
    );

    // That session then rotates Bob's tokens: a lineage no slot holds. The
    // token itself says it's Bob's — his slot takes the rotation, Alice's
    // stays hers.
    write_claude_tokens(&config, live_tokens("token-bob-2", "refresh-bob-2"));
    accounts.list(false).await.expect("list after rotation");
    assert_eq!(
        slot_refresh_token(&config, "bob@example.com").as_deref(),
        Some("refresh-bob-2")
    );
    assert_eq!(
        slot_refresh_token(&config, "alice@example.com").as_deref(),
        Some("refresh-token-alice")
    );

    // Switching back to Alice straightens the live store out again.
    let snapshot = accounts.list(false).await.expect("list");
    accounts
        .activate(
            HarnessId::ClaudeCode,
            &account_id(&snapshot, "alice@example.com"),
        )
        .await
        .expect("switch back to alice");
    let creds: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(config.claude_config_dir.join(".credentials.json"))
            .expect("creds"),
    )
    .expect("creds json");
    assert_eq!(
        creds["claudeAiOauth"]["refreshToken"],
        "refresh-token-alice"
    );
}

#[tokio::test]
async fn claude_rotation_is_saved_once_verified_or_on_switch() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (_, mut config) = test_accounts(tmp.path());
    config.claude_profile_url =
        serve_claude_profiles(vec![("token-alice-2", "uuid-alice", "alice@example.com")]).await;
    let accounts = AgentAccounts::new(config.clone());

    write_claude_login(&config, "bob@example.com", "uuid-bob", "token-bob");
    accounts.list(false).await.expect("list bob");
    write_claude_login(&config, "alice@example.com", "uuid-alice", "token-alice");
    accounts.list(false).await.expect("list alice");

    // Claude Code's routine rotation, verifiable: saved on the next list.
    write_claude_tokens(&config, live_tokens("token-alice-2", "refresh-alice-2"));
    accounts.list(false).await.expect("list rotated");
    assert_eq!(
        slot_refresh_token(&config, "alice@example.com").as_deref(),
        Some("refresh-alice-2")
    );

    // Unverifiable (the endpoint doesn't know this token): a list leaves the
    // slot alone…
    write_claude_tokens(&config, live_tokens("token-alice-3", "refresh-alice-3"));
    let snapshot = accounts.list(false).await.expect("list unverified");
    assert_eq!(
        slot_refresh_token(&config, "alice@example.com").as_deref(),
        Some("refresh-alice-2")
    );
    // …but a switch away saves it, rather than strand the only live copy.
    accounts
        .activate(
            HarnessId::ClaudeCode,
            &account_id(&snapshot, "bob@example.com"),
        )
        .await
        .expect("switch to bob");
    assert_eq!(
        slot_refresh_token(&config, "alice@example.com").as_deref(),
        Some("refresh-alice-3")
    );
}

#[tokio::test]
async fn claude_expired_login_asks_for_sign_in_instead_of_switching() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (accounts, config) = test_accounts(tmp.path());
    write_claude_login(&config, "alice@example.com", "uuid-alice", "token-alice");
    // Alice's login deadline (stamped at sign-in, never extended) passed.
    write_claude_tokens(
        &config,
        serde_json::json!({
            "accessToken": "token-alice",
            "refreshToken": "refresh-token-alice",
            "expiresAt": 4_102_444_800_000i64,
            "refreshTokenExpiresAt": 1_000i64,
        }),
    );
    accounts.list(false).await.expect("list alice");
    write_claude_login(&config, "bob@example.com", "uuid-bob", "token-bob");
    let snapshot = accounts.list(false).await.expect("list bob");
    let alice = snapshot
        .accounts
        .iter()
        .find(|a| a.email.as_deref() == Some("alice@example.com"))
        .expect("alice");
    assert!(alice.needs_login);
    assert!(!alice.switchable);
    assert_eq!(alice.login_expires_at, Some(1_000));

    let refused = accounts
        .activate(HarnessId::ClaudeCode, &alice.id)
        .await
        .expect_err("a dead login is never written live");
    assert!(refused.to_string().contains("expired"), "{refused}");
    let creds: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(config.claude_config_dir.join(".credentials.json"))
            .expect("creds"),
    )
    .expect("creds json");
    assert_eq!(creds["claudeAiOauth"]["accessToken"], "token-bob");
}

#[tokio::test]
async fn claude_switch_takes_and_releases_claude_codes_refresh_locks() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (accounts, config) = test_accounts(tmp.path());
    write_claude_login(&config, "alice@example.com", "uuid-alice", "token-alice");
    accounts.list(false).await.expect("list alice");
    write_claude_login(&config, "bob@example.com", "uuid-bob", "token-bob");
    let snapshot = accounts.list(false).await.expect("list bob");

    // A lock left behind by a crashed Claude Code (older than its 60s
    // staleness bound) must not wedge the switch.
    let primary = config.claude_config_dir.join(".oauth_refresh.lock");
    std::fs::create_dir(&primary).expect("stale lock");
    std::fs::File::open(&primary)
        .expect("open lock dir")
        .set_modified(std::time::SystemTime::now() - Duration::from_secs(120))
        .expect("age lock");

    accounts
        .activate(
            HarnessId::ClaudeCode,
            &account_id(&snapshot, "alice@example.com"),
        )
        .await
        .expect("switch past a stale lock");
    let mut legacy = config.claude_config_dir.clone().into_os_string();
    legacy.push(".lock");
    assert!(!primary.exists(), "primary refresh lock released");
    assert!(!PathBuf::from(legacy).exists(), "legacy lock released");
}

/// A codex login for one seat: `user_id` inside workspace `account_id`.
fn write_codex_seat(config: &AgentAccountsConfig, email: &str, user_id: &str, account_id: &str) {
    let header = BASE64_URL.encode(br#"{"alg":"none"}"#);
    let payload = BASE64_URL.encode(
        serde_json::json!({
            "email": email,
            "https://api.openai.com/auth": {
                "chatgpt_account_id": account_id,
                "chatgpt_user_id": user_id,
                "chatgpt_plan_type": "team",
            },
        })
        .to_string(),
    );
    std::fs::create_dir_all(&config.codex_home).expect("codex home");
    std::fs::write(
        config.codex_home.join("auth.json"),
        serde_json::json!({
            "tokens": {
                "id_token": format!("{header}.{payload}.x"),
                "access_token": format!("at-{user_id}"),
                "refresh_token": format!("rt-{user_id}"),
                "account_id": account_id,
            }
        })
        .to_string(),
    )
    .expect("codex auth");
}

#[tokio::test]
async fn codex_team_seats_keep_separate_slots() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (accounts, config) = test_accounts(tmp.path());

    // Two teammates in ONE ChatGPT Team workspace share its account id.
    write_codex_seat(&config, "erin@team.com", "user-erin", "ws-team");
    accounts.list(false).await.expect("list erin");
    write_codex_seat(&config, "finn@team.com", "user-finn", "ws-team");
    let snapshot = accounts.list(false).await.expect("list finn");
    let mut emails = account_emails(&snapshot, HarnessId::Codex);
    emails.sort();
    assert_eq!(
        emails,
        vec![
            ("erin@team.com".to_string(), false),
            ("finn@team.com".to_string(), true)
        ]
    );

    accounts
        .activate(HarnessId::Codex, &account_id(&snapshot, "erin@team.com"))
        .await
        .expect("switch to erin");
    let auth: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(config.codex_home.join("auth.json")).expect("auth"),
    )
    .expect("auth json");
    assert_eq!(auth["tokens"]["refresh_token"], "rt-user-erin");
}

#[tokio::test]
async fn codex_workspace_keyed_slots_are_rekeyed_in_place() {
    use sha2::Digest as _;
    let tmp = tempfile::tempdir().expect("tempdir");
    let (accounts, config) = test_accounts(tmp.path());

    // A slot saved before seats were told apart: keyed by workspace alone.
    write_codex_seat(&config, "erin@team.com", "user-erin", "ws-team");
    let auth: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(config.codex_home.join("auth.json")).expect("auth"),
    )
    .expect("auth json");
    let legacy_id = {
        let digest = sha2::Sha256::digest(b"codex:ws-team");
        digest[..8]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    };
    let dir = config.data_dir.join("agent-accounts").join("codex");
    std::fs::create_dir_all(&dir).expect("slots dir");
    std::fs::write(
        dir.join(format!("{legacy_id}.json")),
        serde_json::json!({
            "id": legacy_id,
            "harness": "codex",
            "accountKey": "ws-team",
            "profile": { "email": "erin@team.com", "authKind": "oauth" },
            "credentials": auth,
            "savedAt": 5,
            "createdAt": 5,
        })
        .to_string(),
    )
    .expect("legacy slot");
    // The live login is someone else, so the migration alone must carry Erin.
    write_codex_seat(&config, "finn@team.com", "user-finn", "ws-team");

    let snapshot = accounts.list(false).await.expect("list");
    let erin: Vec<_> = snapshot
        .accounts
        .iter()
        .filter(|a| a.email.as_deref() == Some("erin@team.com"))
        .collect();
    assert_eq!(erin.len(), 1, "exactly one Erin slot");
    assert_ne!(erin[0].id, legacy_id, "re-keyed");
    assert!(!erin[0].active);
    assert!(!dir.join(format!("{legacy_id}.json")).exists());
    assert_eq!(
        snapshot.accounts[0].email.as_deref(),
        Some("erin@team.com"),
        "creation order kept"
    );
}

#[tokio::test]
async fn codex_unreadable_live_login_blocks_the_switch() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (accounts, config) = test_accounts(tmp.path());
    write_codex_login(&config, "carol@example.com", "acct-carol");
    let snapshot = accounts.list(false).await.expect("list carol");
    let carol = account_id(&snapshot, "carol@example.com");

    // A login we can't parse is a login we can't back up.
    std::fs::write(
        config.codex_home.join("auth.json"),
        r#"{"tokens":{"refresh_token":"rt-mystery"}}"#,
    )
    .expect("opaque auth");
    let snapshot = accounts.list(false).await.expect("list opaque");
    assert!(
        snapshot
            .warnings
            .iter()
            .any(|w| w.harness == HarnessId::Codex),
        "{:?}",
        snapshot.warnings
    );
    accounts
        .activate(HarnessId::Codex, &carol)
        .await
        .expect_err("switching would destroy the unsaved login");
    assert_eq!(
        std::fs::read_to_string(config.codex_home.join("auth.json")).expect("auth"),
        r#"{"tokens":{"refresh_token":"rt-mystery"}}"#
    );
}

#[tokio::test]
async fn forget_guards_and_removes_slots() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (accounts, config) = test_accounts(tmp.path());
    write_claude_login(&config, "alice@example.com", "uuid-alice", "token-alice");
    let snapshot = accounts.list(false).await.expect("list");
    let alice_id = snapshot.accounts[0].id.clone();

    // Path-shaped ids never reach the filesystem.
    assert!(
        accounts
            .forget(HarnessId::ClaudeCode, "../../evil")
            .await
            .is_err()
    );
    assert!(
        accounts
            .forget(HarnessId::ClaudeCode, "ABCDEF0123456789")
            .await
            .is_err()
    );
    // The live login can't be forgotten (it would just be re-detected).
    assert!(
        accounts
            .forget(HarnessId::ClaudeCode, &alice_id)
            .await
            .is_err()
    );

    // A non-active slot forgets cleanly.
    write_claude_login(&config, "bob@example.com", "uuid-bob", "token-bob");
    accounts.list(false).await.expect("list bob");
    let snapshot = accounts
        .forget(HarnessId::ClaudeCode, &alice_id)
        .await
        .expect("forget alice");
    assert_eq!(
        account_emails(&snapshot, HarnessId::ClaudeCode),
        vec![("bob@example.com".to_string(), true)]
    );
}

#[test]
fn snapshot_wire_shape() {
    let snapshot = AgentAccountsSnapshot::default();
    let value = serde_json::to_value(&snapshot).expect("serializes");
    assert_eq!(value, serde_json::json!({ "accounts": [], "warnings": [] }));
}

#[tokio::test]
async fn claude_login_flow_is_pkce_paste_code() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (accounts, _) = test_accounts(tmp.path());
    let start = accounts
        .start_login(HarnessId::ClaudeCode)
        .await
        .expect("start");
    assert!(
        start
            .url
            .starts_with("https://claude.ai/oauth/authorize?code=true")
    );
    assert!(start.url.contains("code_challenge_method=S256"));
    assert!(
        start
            .url
            .contains("redirect_uri=https%3A%2F%2Fconsole.anthropic.com")
    );
    let mode = serde_json::to_value(start.mode).expect("mode");
    assert_eq!(mode, serde_json::json!("paste-code"));

    // Claude flows poll as pending (paste-code completes them); cancel drops the
    // flow so the next poll reports it expired.
    let poll = accounts.poll_login(&start.login_id).await.expect("poll");
    assert_eq!(
        serde_json::to_value(poll.status).expect("status"),
        serde_json::json!("pending")
    );
    accounts.cancel_login(&start.login_id);
    assert!(
        accounts.poll_login(&start.login_id).await.is_err(),
        "cancelled flow is gone"
    );
    assert!(
        accounts
            .complete_login(&start.login_id, "code#state")
            .await
            .is_err()
    );
}

// ---------------------------------------------------------------------------
// Uploads
// ---------------------------------------------------------------------------

#[tokio::test]
async fn uploads_chunk_commit_readback_and_jail() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let uploads = Uploads::new(tmp.path());

    // 100KB of pseudo-random bytes, staged as three positional base64 chunks
    // (out of order, with one retried) — chunk boundaries are multiples of 3
    // bytes so independent base64 strings concatenate losslessly.
    let payload: Vec<u8> = (0..100_002u32)
        .map(|i| (i.wrapping_mul(31) % 251) as u8)
        .collect();
    let chunks: Vec<String> = payload.chunks(45_000).map(|c| BASE64.encode(c)).collect();
    assert_eq!(chunks.len(), 3);
    uploads
        .append("up-1", &chunks[2], Some(2))
        .expect("chunk 2");
    uploads
        .append("up-1", &chunks[0], Some(0))
        .expect("chunk 0");
    uploads
        .append("up-1", &chunks[0], Some(0))
        .expect("chunk 0 retry is idempotent");
    uploads
        .append("up-1", &chunks[1], Some(1))
        .expect("chunk 1");
    let path = uploads.commit("up-1", "photo.png").expect("commit");
    assert!(path.ends_with("up-1-photo.png"), "path: {path}");
    assert_eq!(std::fs::read(&path).expect("committed file"), payload);

    // Readback: chunked reassembly round-trips.
    let mut assembled = Vec::new();
    let mut offset = 0u64;
    loop {
        let chunk = uploads.read_chunk(&path, offset, &[]).expect("read chunk");
        assert_eq!(chunk.mime_type, "image/png");
        assert_eq!(chunk.name, "up-1-photo.png");
        assembled.extend(BASE64.decode(&chunk.data).expect("chunk base64"));
        offset = chunk.next_offset;
        if chunk.done {
            break;
        }
    }
    assert_eq!(assembled, payload);

    // Missing chunk → commit fails.
    uploads
        .append("up-2", &chunks[0], Some(0))
        .expect("chunk 0");
    uploads
        .append("up-2", &chunks[2], Some(2))
        .expect("chunk 2 (hole at 1)");
    assert!(
        uploads.commit("up-2", "holey.png").is_err(),
        "hole detected"
    );

    // Path jail: files outside the uploads dir (and outside any allowed cwd
    // root) are rejected, including traversal attempts and the dir itself.
    let outside = tmp.path().join("outside.png");
    std::fs::write(&outside, b"nope").expect("outside file");
    assert!(
        uploads
            .read_chunk(&outside.to_string_lossy(), 0, &[])
            .is_err()
    );
    assert!(uploads.read_chunk("/etc/passwd", 0, &[]).is_err());
    let sneaky = format!("{}/../outside.png", uploads.dir().display());
    assert!(
        uploads.read_chunk(&sneaky, 0, &[]).is_err(),
        "traversal rejected"
    );
    // …but a workspace-known cwd root admits its files.
    let ok = uploads
        .read_chunk(&outside.to_string_lossy(), 0, &[tmp.path().to_path_buf()])
        .expect("cwd-rooted read");
    assert_eq!(BASE64.decode(&ok.data).expect("data"), b"nope");
    // Non-image extensions are refused even inside the jail (zeron parity).
    let text = PathBuf::from(uploads.dir()).join("notes.txt");
    std::fs::create_dir_all(uploads.dir()).expect("uploads dir");
    std::fs::write(&text, b"text").expect("txt");
    assert!(uploads.read_chunk(&text.to_string_lossy(), 0, &[]).is_err());

    // Bogus upload ids never become paths.
    assert!(uploads.append("../evil", "aGk=", None).is_err());
    assert!(uploads.commit("unknown-upload", "x.png").is_err());
}

// ---------------------------------------------------------------------------
// Titling
// ---------------------------------------------------------------------------

#[tokio::test]
async fn titling_e2e_names_chat_and_renames_worktree_branch() {
    let tmp = tempfile::tempdir().expect("tempdir");
    // Worktree root must be inside the tempdir (EngineCore reads the env-less
    // default otherwise) — create the worktree with a dedicated Repos handle.
    let repo_dir = tmp.path().join("repo");
    init_repo(&repo_dir).await;
    let repos = Repos::with_worktrees_root(
        &tmp.path().join("data"),
        "device-test",
        tmp.path().join("worktrees"),
    );
    let worktree = repos
        .create_worktree(&repo_dir, "main")
        .await
        .expect("worktree");

    let core = assemble_with_mock(
        &tmp.path().join("data"),
        vec![
            AgentEvent::TextDelta {
                text: "Fix Login Flow".into(),
            },
            AgentEvent::Done {
                status: DoneStatus::Completed,
                result: None,
                error: None,
                session_id: None,
            },
        ],
    );
    let chat_id = "chat-title-1";
    core.workspace
        .create_space(
            "space-title",
            &core.device_id,
            &repo_dir.to_string_lossy(),
            None,
            true,
        )
        .expect("create space");
    core.workspace
        .create_chat(
            chat_id,
            Some("space-title"),
            None,
            None,
            Some(worktree.path.clone()),
        )
        .expect("create chat");
    core.workspace
        .set_chat_branch(chat_id, &worktree.branch)
        .expect("set branch");

    let request = zeron_proto::RunRequest {
        prompt: "please fix the login flow".into(),
        harness: None,
        model: None,
        reasoning: None,
        model_options: serde_json::Map::new(),
        cwd: worktree.path.clone(),
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: true,
        attachments: Vec::new(),
        worktree: None,
        resume: None,
    };
    core.sessions
        .dispatch(chat_id, HarnessId::Mock, request, None)
        .await
        .expect("dispatch");

    // The mock's scripted reply doubles as the titling model's output.
    let chat = wait_for("chat title", || {
        core.workspace
            .chat(chat_id)
            .ok()
            .flatten()
            .filter(|c| c.title.as_deref().is_some_and(|t| !t.is_empty()))
    })
    .await;
    assert_eq!(chat.title.as_deref(), Some("Fix Login Flow"));
    // Branch renamed from the title, chat row updated to match.
    assert_eq!(chat.branch.as_deref(), Some("zeron/fix-login-flow"));
    let head = tokio::process::Command::new("git")
        .args(["branch", "--show-current"])
        .current_dir(&worktree.path)
        .output()
        .await
        .expect("git");
    assert_eq!(
        String::from_utf8_lossy(&head.stdout).trim(),
        "zeron/fix-login-flow"
    );

    // A titled chat is never re-titled: rename, run again, title sticks.
    core.workspace
        .rename_chat(chat_id, "My Custom Name")
        .expect("rename");
    let request = zeron_proto::RunRequest {
        prompt: "another request".into(),
        harness: None,
        model: None,
        reasoning: None,
        model_options: serde_json::Map::new(),
        cwd: worktree.path.clone(),
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: true,
        attachments: Vec::new(),
        worktree: None,
        resume: None,
    };
    core.sessions
        .dispatch(chat_id, HarnessId::Mock, request, None)
        .await
        .expect("second dispatch");
    tokio::time::sleep(Duration::from_millis(400)).await;
    let chat = core.workspace.chat(chat_id).expect("chat").expect("row");
    assert_eq!(chat.title.as_deref(), Some("My Custom Name"));
    core.shutdown().await;
}

#[tokio::test]
async fn rename_worktree_branch_guards_and_collisions() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_dir = tmp.path().join("repo");
    init_repo(&repo_dir).await;
    let repos = Repos::with_worktrees_root(
        &tmp.path().join("data"),
        "device-test",
        tmp.path().join("worktrees"),
    );
    let wt = repos
        .create_worktree(&repo_dir, "main")
        .await
        .expect("worktree");
    let wt_path = Path::new(&wt.path);

    // Guard: expected branch mismatch → no-op, returns the actual branch.
    let unchanged = repos
        .rename_worktree_branch(wt_path, "zeron/not-this-one", "Some Title")
        .await
        .expect("guarded");
    assert_eq!(unchanged, wt.branch);

    // Happy path: renamed to the title slug.
    let renamed = repos
        .rename_worktree_branch(wt_path, &wt.branch, "Add Dark Mode!")
        .await
        .expect("renamed");
    assert_eq!(renamed, "zeron/add-dark-mode");

    // Already renamed → the guard (branch no longer zeron/<folder>) makes any
    // further title rename a no-op.
    let again = repos
        .rename_worktree_branch(wt_path, "zeron/add-dark-mode", "Different Title")
        .await
        .expect("second rename");
    assert_eq!(again, "zeron/add-dark-mode");

    // Collision: a second worktree whose title slug already exists gets the
    // stable hash suffix.
    let wt2 = repos
        .create_worktree(&repo_dir, "main")
        .await
        .expect("worktree 2");
    let renamed2 = repos
        .rename_worktree_branch(Path::new(&wt2.path), &wt2.branch, "Add Dark Mode!")
        .await
        .expect("suffixed rename");
    assert!(
        renamed2.starts_with("zeron/add-dark-mode-")
            && renamed2.len() == "zeron/add-dark-mode-".len() + 6,
        "suffixed: {renamed2}"
    );

    // Slug edge cases.
    assert_eq!(
        worktree_branch_from_title("  Fix `Login` Flow!  "),
        "zeron/fix-login-flow"
    );
    assert_eq!(worktree_branch_from_title("***"), "zeron/update");
    assert_eq!(
        worktree_branch_from_title("Cafe's Dark Mode"),
        "zeron/cafes-dark-mode"
    );
}

// ---------------------------------------------------------------------------
// RPC dispatch
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rpc_dispatch_for_m5c_methods() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let core = assemble_with_mock(&tmp.path().join("data"), Vec::new());
    let client = zeron_rpc::memory_client(core.rpc_service());

    // Uploads: chunk → commit → readback over the wire.
    let payload = b"fake png bytes".to_vec();
    let ok = client
        .call(
            methods::UPLOAD_CHUNK,
            serde_json::json!({ "uploadId": "rpc-up", "data": BASE64.encode(&payload), "seq": 0 }),
        )
        .await
        .expect("UploadChunk");
    assert_eq!(ok["ok"], true);
    let committed = client
        .call(
            methods::UPLOAD_COMMIT,
            serde_json::json!({ "uploadId": "rpc-up", "fileName": "shot.png" }),
        )
        .await
        .expect("UploadCommit");
    let path = committed["path"].as_str().expect("path").to_string();
    assert!(path.ends_with("rpc-up-shot.png"));
    let chunk = client
        .call(
            methods::READ_ATTACHMENT_CHUNK,
            serde_json::json!({ "path": path, "offset": 0 }),
        )
        .await
        .expect("ReadAttachmentChunk");
    assert_eq!(chunk["mimeType"], "image/png");
    assert_eq!(chunk["done"], true);
    assert_eq!(
        BASE64
            .decode(chunk["data"].as_str().expect("data"))
            .expect("base64"),
        payload
    );
    // Jail holds over RPC too.
    assert!(
        client
            .call(
                methods::READ_ATTACHMENT_CHUNK,
                serde_json::json!({ "path": "/etc/passwd", "offset": 0 })
            )
            .await
            .is_err()
    );

    // Agent accounts: snapshot shape (this machine's real CLI state may or may
    // not include logins — assert the envelope, not the contents).
    let snapshot = client
        .call(methods::LIST_AGENT_ACCOUNTS, serde_json::json!({}))
        .await
        .expect("ListAgentAccounts");
    assert!(snapshot["accounts"].is_array());
    assert!(snapshot["warnings"].is_array());

    // Login lifecycle: start (paste-code) → poll pending → cancel → gone.
    let start = client
        .call(
            methods::START_AGENT_LOGIN,
            serde_json::json!({ "harness": "claude-code" }),
        )
        .await
        .expect("StartAgentLogin");
    assert_eq!(start["mode"], "paste-code");
    assert!(
        start["url"]
            .as_str()
            .expect("url")
            .contains("claude.ai/oauth/authorize")
    );
    let login_id = start["loginId"].as_str().expect("loginId").to_string();
    let poll = client
        .call(
            methods::POLL_AGENT_LOGIN,
            serde_json::json!({ "loginId": login_id }),
        )
        .await
        .expect("PollAgentLogin");
    assert_eq!(poll["status"], "pending");
    let cancelled = client
        .call(
            methods::CANCEL_AGENT_LOGIN,
            serde_json::json!({ "loginId": login_id }),
        )
        .await
        .expect("CancelAgentLogin");
    assert_eq!(cancelled["ok"], true);
    assert!(
        client
            .call(
                methods::POLL_AGENT_LOGIN,
                serde_json::json!({ "loginId": login_id })
            )
            .await
            .is_err(),
        "cancelled login is expired"
    );

    // Error paths: junk account ids and dead logins fail cleanly.
    assert!(
        client
            .call(
                methods::FORGET_AGENT_ACCOUNT,
                serde_json::json!({ "harness": "claude-code", "accountId": "../nope" })
            )
            .await
            .is_err()
    );
    assert!(
        client
            .call(
                methods::ACTIVATE_AGENT_ACCOUNT,
                serde_json::json!({ "harness": "claude-code", "accountId": "0123456789abcdef" })
            )
            .await
            .is_err(),
        "unknown slot cannot be activated"
    );
    assert!(
        client
            .call(
                methods::COMPLETE_AGENT_LOGIN,
                serde_json::json!({ "loginId": "no-such-login", "code": "x#y" })
            )
            .await
            .is_err()
    );
    core.shutdown().await;
}

fn write_cursor_login(config: &AgentAccountsConfig, email: &str, expires_in_ms: i64) {
    let file = &config.cursor_sdk_auth_file;
    std::fs::create_dir_all(file.parent().unwrap()).expect("cursor sdk dir");
    std::fs::write(
        file,
        serde_json::json!({
            "version": 1,
            "backendUrl": "https://api2.cursor.sh",
            "apiKey": format!("key-{email}"),
            "apiKeyExpiresAtMs": now_ms_test() + expires_in_ms,
            "email": email,
            "createdAtMs": now_ms_test(),
        })
        .to_string(),
    )
    .expect("cursor auth");
}

fn now_ms_test() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

#[tokio::test]
async fn cursor_slot_swap_round_trip() {
    let dir = tempfile::tempdir().expect("tmp");
    let (accounts, config) = test_accounts(dir.path());

    // Live SDK login = Erin; listing detects + auto-snapshots her slot.
    write_cursor_login(&config, "erin@example.com", 86_400_000);
    let snapshot = accounts.list(false).await.expect("list");
    assert_eq!(
        account_emails(&snapshot, HarnessId::Cursor),
        vec![("erin@example.com".to_string(), true)]
    );
    assert!(snapshot.warnings.is_empty(), "{:?}", snapshot.warnings);
    let erin_id = snapshot.accounts[snapshot
        .accounts
        .iter()
        .position(|a| a.harness == HarnessId::Cursor)
        .unwrap()]
    .id
    .clone();

    // A second login (Frank) becomes live; both slots exist, Frank active.
    write_cursor_login(&config, "frank@example.com", 86_400_000);
    let snapshot = accounts.list(false).await.expect("list");
    assert_eq!(
        account_emails(&snapshot, HarnessId::Cursor),
        vec![
            ("erin@example.com".to_string(), false),
            ("frank@example.com".to_string(), true),
        ]
    );

    // Swap back to Erin: the SDK store file is rewritten from her slot.
    let snapshot = accounts
        .activate(HarnessId::Cursor, &erin_id)
        .await
        .expect("activate");
    assert_eq!(
        account_emails(&snapshot, HarnessId::Cursor),
        vec![
            ("erin@example.com".to_string(), true),
            ("frank@example.com".to_string(), false),
        ]
    );
    let live: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&config.cursor_sdk_auth_file).unwrap())
            .unwrap();
    assert_eq!(live["email"], "erin@example.com");

    // An expired live key detects (card + slot survive) but warns.
    write_cursor_login(&config, "erin@example.com", -1000);
    let snapshot = accounts.list(false).await.expect("list");
    assert!(
        snapshot
            .warnings
            .iter()
            .any(|w| w.harness == HarnessId::Cursor && w.message.contains("expired")),
        "{:?}",
        snapshot.warnings
    );
}

#[tokio::test]
async fn cursor_login_flow_spawns_shim_and_auto_activates() {
    let dir = tempfile::tempdir().expect("tmp");
    let (accounts, config) = test_accounts(dir.path());

    // Fake shim: in login mode, emit the auth-url frame, write the minted
    // store file where the engine pointed us, exit 0. Mirrors the real shim's
    // `node <shim> login <store-path>` argv contract.
    let shim = dir.path().join("fake-cursor-shim.sh");
    std::fs::write(
        &shim,
        r#"#!/bin/sh
[ "$1" = "login" ] || exit 1
printf '%s\n' '{"ev":"auth-url","url":"https://cursor.com/loginDeepControl?challenge=fake"}'
cat > "$2" <<JSON
{"version":1,"backendUrl":"https://api2.cursor.sh","apiKey":"key-minted","apiKeyExpiresAtMs":99999999999999,"email":"grace@example.com","createdAtMs":1}
JSON
printf '%s\n' '{"ev":"logged-in","email":"grace@example.com"}'
exit 0
"#,
    )
    .expect("fake shim");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    unsafe { std::env::set_var("CURSOR_SDK_SHIM_EXECUTABLE", &shim) };

    let start = accounts
        .start_login(HarnessId::Cursor)
        .await
        .expect("start");
    assert_eq!(start.mode, AgentLoginMode::Browser);
    assert_eq!(
        start.url,
        "https://cursor.com/loginDeepControl?challenge=fake"
    );

    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let poll = accounts.poll_login(&start.login_id).await.expect("poll");
        match poll.status {
            AgentLoginStatus::Done => break,
            AgentLoginStatus::Pending => {}
            AgentLoginStatus::Error => panic!("login errored: {:?}", poll.message),
        }
        assert!(tokio::time::Instant::now() < deadline, "login never landed");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // First connect on a device with no live login: the minted key was
    // auto-activated, so runs work immediately.
    let live: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&config.cursor_sdk_auth_file).unwrap())
            .unwrap();
    assert_eq!(live["email"], "grace@example.com");
    let snapshot = accounts.list(false).await.expect("list");
    assert_eq!(
        account_emails(&snapshot, HarnessId::Cursor),
        vec![("grace@example.com".to_string(), true)]
    );
}
