use super::*;

fn config(hostname: &str) -> PalantirAipConfig {
    PalantirAipConfig {
        hostname: Some(hostname.to_owned()),
        client_id: Some("abc123".to_owned()),
    }
}

fn credential(hostname: &str, expires_in_secs: i64, refresh: Option<&str>) -> StoredCredential {
    StoredCredential {
        hostname: hostname.to_owned(),
        client_id: "abc123".to_owned(),
        access_token: "access-1".to_owned(),
        refresh_token: refresh.map(str::to_owned),
        expires_at: chrono::Utc::now() + chrono::Duration::seconds(expires_in_secs),
    }
}

#[test]
fn normalize_hostname_accepts_hosts_and_urls() {
    for input in [
        "acme.palantirfoundry.com",
        "  ACME.palantirfoundry.com/ ",
        "https://acme.palantirfoundry.com",
        "https://acme.palantirfoundry.com/",
    ] {
        assert_eq!(
            normalize_hostname(input).unwrap(),
            "acme.palantirfoundry.com",
            "{input}"
        );
    }
    assert_eq!(
        normalize_hostname("acme.example:8443").unwrap(),
        "acme.example:8443"
    );
}

#[test]
fn normalize_hostname_rejects_non_hosts() {
    for input in [
        "http://acme.palantirfoundry.com",
        "https://acme.palantirfoundry.com/workspace",
        "https://user:pw@acme.palantirfoundry.com",
        "https://acme.palantirfoundry.com/?x=1",
        "https://10.0.0.1",
        "",
    ] {
        assert!(
            normalize_hostname(input).is_err(),
            "{input:?} should be rejected"
        );
    }
}

#[test]
fn config_reads_table_and_requires_fields() {
    let raw: toml::Value = toml::from_str(
        "[palantir_aip]\nhostname = \"acme.palantirfoundry.com\"\nclient_id = \"c1\"\n",
    )
    .unwrap();
    let cfg = PalantirAipConfig::from_raw(&raw);
    assert_eq!(cfg.require_hostname().unwrap(), "acme.palantirfoundry.com");
    assert_eq!(cfg.require_client_id().unwrap(), "c1");

    let empty = PalantirAipConfig::from_raw_with_env(&toml::Value::Table(Default::default()), None);
    assert!(empty.require_hostname().is_err());
    assert!(empty.require_client_id().is_err());

    let dotted = PalantirAipConfig {
        client_id: Some("ri.foundry.main.deployed-app.a1b2".to_owned()),
        ..Default::default()
    };
    assert_eq!(
        dotted.require_client_id().unwrap(),
        "ri.foundry.main.deployed-app.a1b2"
    );
    for bad in ["two words", "tab\there", "é"] {
        let bad = PalantirAipConfig {
            client_id: Some(bad.to_owned()),
            ..Default::default()
        };
        assert!(bad.require_client_id().is_err(), "{:?}", bad.client_id);
    }
}

#[test]
fn foundry_hostname_env_fills_in_only_a_missing_hostname() {
    let empty = toml::Value::Table(Default::default());
    let from_env =
        PalantirAipConfig::from_raw_with_env(&empty, Some("env.palantirfoundry.com".to_owned()));
    assert_eq!(
        from_env.require_hostname().unwrap(),
        "env.palantirfoundry.com"
    );

    let raw: toml::Value =
        toml::from_str("[palantir_aip]\nhostname = \"config.palantirfoundry.com\"\n").unwrap();
    let configured =
        PalantirAipConfig::from_raw_with_env(&raw, Some("env.palantirfoundry.com".to_owned()));
    assert_eq!(
        configured.require_hostname().unwrap(),
        "config.palantirfoundry.com"
    );
}

#[test]
fn authorize_url_requests_only_model_scopes_with_pkce() {
    let url = build_authorize_url(
        "acme.palantirfoundry.com",
        "c1",
        "http://127.0.0.1:5555/",
        "challenge",
        "state1",
    );
    let url = url::Url::parse(&url).unwrap();
    assert_eq!(url.host_str(), Some("acme.palantirfoundry.com"));
    assert_eq!(url.path(), "/multipass/api/oauth2/authorize");
    let params: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(params["response_type"], "code");
    assert_eq!(params["client_id"], "c1");
    assert_eq!(params["redirect_uri"], "http://127.0.0.1:5555/");
    assert_eq!(
        params["scope"],
        "api:use-language-models-execute offline_access"
    );
    assert_eq!(params["code_challenge"], "challenge");
    assert_eq!(params["code_challenge_method"], "S256");
    assert_eq!(params["state"], "state1");
}

#[test]
fn oauth_error_code_never_echoes_the_body() {
    assert_eq!(
        oauth_error_code(r#"{"error":"invalid_grant","error_description":"secret-ish"}"#),
        "invalid_grant"
    );
    assert_eq!(
        oauth_error_code("<html>refresh=abc</html>"),
        "no error code"
    );
}

#[test]
fn credential_debug_hides_tokens() {
    let debug = format!("{:?}", credential("acme.example", 60, Some("refresh-1")));
    assert!(!debug.contains("access-1"));
    assert!(!debug.contains("refresh-1"));
}

#[tokio::test]
async fn helper_serves_a_fresh_oauth_token_without_network() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("oauth.json");
    write_credential(&path, &credential("acme.example", 3600, Some("r1"))).unwrap();

    let token = helper_token_at(
        &config("acme.example"),
        &path,
        &dir.path().join("token"),
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(token.access_token, "access-1");
    assert_eq!(
        token.expires_in,
        Some(RECHECK_SECS),
        "the agent re-runs the helper well before the hour-long token expires"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}

#[tokio::test]
async fn helper_refuses_a_credential_from_another_hostname() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("oauth.json");
    write_credential(&path, &credential("old.example", 3600, Some("r1"))).unwrap();

    let error = helper_token_at(
        &config("new.example"),
        &path,
        &dir.path().join("token"),
        None,
        None,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("old.example") && error.contains("new.example"),
        "{error}"
    );
}

#[tokio::test]
async fn helper_requires_sign_in_when_expired_without_refresh_token() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("oauth.json");
    write_credential(&path, &credential("acme.example", 10, None)).unwrap();

    let error = helper_token_at(
        &config("acme.example"),
        &path,
        &dir.path().join("token"),
        None,
        None,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("login --palantir-aip"), "{error}");
}

#[tokio::test]
async fn helper_serves_a_short_lived_token_that_cannot_refresh() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("oauth.json");
    write_credential(&path, &credential("acme.example", 240, None)).unwrap();

    let token = helper_token_at(
        &config("acme.example"),
        &path,
        &dir.path().join("token"),
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(token.access_token, "access-1");
    assert!(token.expires_in.is_some_and(|secs| secs <= 240));
}

#[tokio::test]
async fn helper_never_sends_a_token_saved_without_a_hostname() {
    let dir = tempfile::tempdir().unwrap();
    let token_file = dir.path().join("saved.token");
    // An older Bladerunner saved the bare token, with no record of its Foundry.
    std::fs::write(&token_file, "old-personal-token\n").unwrap();
    let oauth = dir.path().join("none.json");

    let token = helper_token_at(
        &config("acme.example"),
        &oauth,
        &token_file,
        Some("env-token".to_owned()),
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        token.access_token, "env-token",
        "FOUNDRY_TOKEN is used instead"
    );

    let error = helper_token_at(&config("acme.example"), &oauth, &token_file, None, None)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("enter it again"), "{error}");
}

#[tokio::test]
async fn helper_prefers_a_saved_token_over_foundry_token_env() {
    let dir = tempfile::tempdir().unwrap();
    let token_file = dir.path().join("saved.token");
    std::fs::write(
        &token_file,
        r#"{"hostname":"acme.example","token":"saved-token"}"#,
    )
    .unwrap();

    let token = helper_token_at(
        &config("acme.example"),
        &dir.path().join("none.json"),
        &token_file,
        Some("env-token".to_owned()),
        None,
    )
    .await
    .unwrap();
    assert_eq!(token.access_token, "saved-token");
    assert_eq!(
        token.expires_in,
        Some(RECHECK_SECS),
        "a logout or replaced token in another process is noticed"
    );
}

#[tokio::test]
async fn helper_refreshes_instead_of_reserving_a_rejected_token() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("oauth.json");
    // Fresh by its expiry, but the server just returned 401 for it. With no
    // refresh token the sign-in is spent, and with no other token the user is
    // asked to sign in again.
    write_credential(&path, &credential("acme.example", 3600, None)).unwrap();
    let token_file = dir.path().join("none.token");

    let error = helper_token_at(
        &config("acme.example"),
        &path,
        &token_file,
        None,
        Some("access-1"),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("Not signed in"), "{error}");
    assert!(
        !path.exists(),
        "a rejected sign-in that can't refresh is removed"
    );

    // A different rejected token means another process already refreshed.
    write_credential(&path, &credential("acme.example", 3600, None)).unwrap();
    let token = helper_token_at(
        &config("acme.example"),
        &path,
        &token_file,
        None,
        Some("older"),
    )
    .await
    .unwrap();
    assert_eq!(token.access_token, "access-1");
}

#[test]
fn auth_provider_passes_the_configured_hostname_to_the_helper() {
    let provider = || crate::auth::AuthProviderConfig {
        command: "bladerunner".into(),
        args: Some(vec!["auth".into(), "palantir-aip-token".into()]),
        ..Default::default()
    };
    let mut providers = indexmap::IndexMap::new();
    providers.insert(PROVIDER_ID.to_owned(), provider());
    let raw: toml::Value =
        toml::from_str("[palantir_aip]\nhostname = \"Acme.PalantirFoundry.com\"\n").unwrap();
    bind_auth_provider_hostname(&mut providers, &raw);
    assert_eq!(
        providers[PROVIDER_ID].args.as_deref().unwrap(),
        [
            "auth",
            "palantir-aip-token",
            "--hostname",
            "acme.palantirfoundry.com"
        ],
        "a hostname change changes the command, so the cached token is dropped"
    );

    // A hand-written `--hostname` for another Foundry is replaced.
    let mut stale = indexmap::IndexMap::new();
    stale.insert(
        PROVIDER_ID.to_owned(),
        crate::auth::AuthProviderConfig {
            args: Some(vec![
                "auth".into(),
                "palantir-aip-token".into(),
                "--hostname".into(),
                "old.palantirfoundry.com".into(),
                "--hostname=older.palantirfoundry.com".into(),
            ]),
            ..provider()
        },
    );
    bind_auth_provider_hostname(&mut stale, &raw);
    assert_eq!(
        stale[PROVIDER_ID].args.as_deref().unwrap(),
        [
            "auth",
            "palantir-aip-token",
            "--hostname",
            "acme.palantirfoundry.com"
        ]
    );

    let mut unconfigured = indexmap::IndexMap::new();
    unconfigured.insert(PROVIDER_ID.to_owned(), provider());
    bind_auth_provider_hostname(&mut unconfigured, &toml::Value::Table(Default::default()));
    // (FOUNDRY_HOSTNAME in the test environment would also bind; it is unset here.)
    if std::env::var_os(HOSTNAME_ENV).is_none() {
        assert_eq!(unconfigured[PROVIDER_ID].args.as_deref().unwrap().len(), 2);
    }
}

#[tokio::test]
async fn helper_uses_foundry_token_env_when_nothing_is_saved() {
    let dir = tempfile::tempdir().unwrap();
    let token = helper_token_at(
        &config("acme.example"),
        &dir.path().join("none.json"),
        &dir.path().join("none.token"),
        Some(" env-token\n".to_owned()),
        None,
    )
    .await
    .unwrap();
    assert_eq!(token.access_token, "env-token");
}

#[tokio::test]
async fn helper_explains_how_to_sign_in() {
    let dir = tempfile::tempdir().unwrap();
    let error = helper_token_at(
        &config("acme.example"),
        &dir.path().join("none.json"),
        &dir.path().join("none.token"),
        None,
        None,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("Not signed in"), "{error}");
}

#[test]
fn helper_output_omits_missing_expiry() {
    let json = serde_json::to_string(&HelperToken {
        access_token: "t".to_owned(),
        expires_in: None,
    })
    .unwrap();
    assert_eq!(json, r#"{"access_token":"t"}"#);
}

#[tokio::test]
async fn helper_hands_out_no_token_without_a_hostname() {
    let dir = tempfile::tempdir().unwrap();
    let token_file = dir.path().join("saved.token");
    std::fs::write(&token_file, "personal-token\n").unwrap();

    let error = helper_token_at(
        &PalantirAipConfig::default(),
        &dir.path().join("none.json"),
        &token_file,
        Some("env-token".to_owned()),
        None,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("No Foundry hostname"), "{error}");
}

#[tokio::test]
async fn helper_serves_a_saved_token_only_for_its_hostname() {
    let dir = tempfile::tempdir().unwrap();
    let token_file = dir.path().join("saved.token");
    std::fs::write(
        &token_file,
        r#"{"hostname":"old.palantirfoundry.com","token":"old-token"}"#,
    )
    .unwrap();
    let oauth = dir.path().join("none.json");

    let token = helper_token_at(
        &config("old.palantirfoundry.com"),
        &oauth,
        &token_file,
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(token.access_token, "old-token");

    let error = helper_token_at(
        &config("new.palantirfoundry.com"),
        &oauth,
        &token_file,
        None,
        None,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("old.palantirfoundry.com") && error.contains("new.palantirfoundry.com"),
        "{error}"
    );
}

#[test]
fn browser_callbacks_must_return_the_issued_state() {
    use PalantirAipLoginMethod::{Browser, Paste};
    assert!(check_state(Browser, Some("s1"), "s1").is_ok());
    assert!(
        check_state(Browser, None, "s1").is_err(),
        "a stateless redirect is rejected"
    );
    assert!(check_state(Browser, Some("other"), "s1").is_err());
    assert!(
        check_state(Paste, None, "s1").is_ok(),
        "the hosted page shows a bare code"
    );
    assert!(check_state(Paste, Some("other"), "s1").is_err());
}

#[tokio::test]
async fn refresh_rechecks_the_hostname_after_waiting_for_the_lock() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("oauth.json");
    // While this helper waited, a sign-in to another Foundry replaced the file.
    write_credential(&path, &credential("b.example", 3600, Some("r1"))).unwrap();

    let token = refresh_locked(&path, "a.example", None).await.unwrap();
    assert!(
        token.is_none(),
        "another Foundry's credential is never served"
    );
}

#[test]
fn a_saved_token_replaces_the_oauth_sign_in() {
    let dir = tempfile::tempdir().unwrap();
    let token_file = dir.path().join("saved.token");
    let oauth = dir.path().join("oauth.json");
    write_credential(&oauth, &credential("acme.example", 3600, Some("r1"))).unwrap();

    assert!(write_token_credential(&token_file, &oauth, "acme.example", "two words").is_err());
    assert!(oauth.exists(), "a malformed token changes nothing");

    write_token_credential(&token_file, &oauth, "acme.example", "tok").unwrap();
    assert!(token_file.exists());
    assert!(!oauth.exists(), "a saved token replaces the OAuth sign-in");
}

#[test]
fn a_token_save_that_cant_drop_the_oauth_sign_in_keeps_the_old_token() {
    let dir = tempfile::tempdir().unwrap();
    let token_file = dir.path().join("saved.token");
    // A non-empty directory where the OAuth file goes can't be removed.
    let oauth = dir.path().join("oauth.json");
    std::fs::create_dir_all(oauth.join("busy")).unwrap();

    assert!(write_token_credential(&token_file, &oauth, "acme.example", "new").is_err());
    assert!(!token_file.exists(), "no token is left behind");

    std::fs::write(&token_file, b"previous").unwrap();
    assert!(write_token_credential(&token_file, &oauth, "acme.example", "new").is_err());
    assert_eq!(std::fs::read(&token_file).unwrap(), b"previous");
}

#[test]
fn the_credential_lock_wait_is_bounded() {
    // A second handle in this process stands in for another process.
    use fs2::FileExt as _;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("oauth.json");
    let held = lock_credential(&path).unwrap();
    let other = std::fs::OpenOptions::new()
        .write(true)
        .open(path.with_extension("json.lock"))
        .unwrap();
    assert!(other.try_lock_exclusive().is_err(), "the lock is exclusive");
    drop(held);
    assert!(other.try_lock_exclusive().is_ok(), "released on drop");
}

#[test]
fn foundry_token_is_ignored_when_foundry_hostname_names_another_foundry() {
    let in_use = config("b.palantirfoundry.com");
    let token = || Some("env-token".to_owned());
    assert_eq!(
        env_token_for(&in_use, token(), None).as_deref(),
        Some("env-token")
    );
    assert_eq!(
        env_token_for(&in_use, token(), Some("B.PalantirFoundry.com".into())).as_deref(),
        Some("env-token")
    );
    assert_eq!(
        env_token_for(&in_use, token(), Some("a.palantirfoundry.com".into())),
        None,
        "A's token is never sent to B"
    );
    assert_eq!(
        env_token_for(&in_use, None, Some("b.palantirfoundry.com".into())),
        None
    );
}

#[tokio::test]
async fn an_oauth_sign_in_without_refresh_falls_back_to_other_tokens() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("oauth.json");
    // Near expiry and no refresh token: the OAuth sign-in is spent.
    write_credential(&path, &credential("acme.example", 10, None)).unwrap();

    let token = helper_token_at(
        &config("acme.example"),
        &path,
        &dir.path().join("none.token"),
        Some("env-token".to_owned()),
        None,
    )
    .await
    .unwrap();
    assert_eq!(token.access_token, "env-token");
}

#[tokio::test]
async fn credentials_for_another_foundry_dont_block_foundry_token() {
    let dir = tempfile::tempdir().unwrap();
    let oauth = dir.path().join("oauth.json");
    let token_file = dir.path().join("saved.token");
    write_credential(
        &oauth,
        &credential("a.palantirfoundry.com", 3600, Some("r1")),
    )
    .unwrap();
    std::fs::write(
        &token_file,
        r#"{"hostname":"a.palantirfoundry.com","token":"a-token"}"#,
    )
    .unwrap();

    let token = helper_token_at(
        &config("b.palantirfoundry.com"),
        &oauth,
        &token_file,
        Some("b-env-token".to_owned()),
        None,
    )
    .await
    .unwrap();
    assert_eq!(token.access_token, "b-env-token");
}

#[test]
fn pasted_tokens_are_checked_before_anything_is_saved() {
    assert_eq!(validate_token("  Bearer abc.def  ").unwrap(), "abc.def");
    assert!(validate_token("two words").is_err());
    assert!(validate_token("").is_err());
}

#[tokio::test]
async fn a_saved_token_foundry_rejected_is_never_resent() {
    let dir = tempfile::tempdir().unwrap();
    let token_file = dir.path().join("saved.token");
    std::fs::write(
        &token_file,
        r#"{"hostname":"acme.example","token":"bad-token"}"#,
    )
    .unwrap();
    let oauth = dir.path().join("none.json");

    let token = helper_token_at(
        &config("acme.example"),
        &oauth,
        &token_file,
        Some("env-token".to_owned()),
        Some("bad-token"),
    )
    .await
    .unwrap();
    assert_eq!(
        token.access_token, "env-token",
        "falls back to FOUNDRY_TOKEN"
    );

    let error = helper_token_at(
        &config("acme.example"),
        &oauth,
        &token_file,
        None,
        Some("bad-token"),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("rejected"), "{error}");
}

#[test]
fn a_config_change_that_cant_take_effect_is_undone() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    // The rejected change was already written over the previous contents.
    std::fs::write(&path, "changed").unwrap();
    ConfigBackup {
        path: path.clone(),
        previous: Some("[ui]\ntheme = \"dark\"\n".to_owned()),
        mode: 0o600,
    }
    .restore()
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "[ui]\ntheme = \"dark\"\n"
    );

    // A config.toml that didn't exist before is removed again.
    ConfigBackup {
        path: path.clone(),
        previous: None,
        mode: 0o600,
    }
    .restore()
    .unwrap();
    assert!(!path.exists());
}

#[test]
fn only_terminal_oauth_errors_end_a_sign_in() {
    assert!(is_terminal_oauth_error(400, "invalid_grant"));
    assert!(is_terminal_oauth_error(401, "invalid_client"));
    assert!(is_terminal_oauth_error(401, NO_ERROR_CODE));
    assert!(!is_terminal_oauth_error(400, "temporarily_unavailable"));
    assert!(!is_terminal_oauth_error(400, "server_error"));
    assert!(!is_terminal_oauth_error(400, NO_ERROR_CODE));
}

#[tokio::test]
async fn a_rejected_saved_token_stays_skipped_on_later_turns() {
    let dir = tempfile::tempdir().unwrap();
    let token_file = dir.path().join("saved.token");
    std::fs::write(
        &token_file,
        r#"{"hostname":"acme.example","token":"bad-token"}"#,
    )
    .unwrap();
    let oauth = dir.path().join("none.json");
    let env = || Some("env-token".to_owned());

    // The 401 retry names the rejected token...
    let retry = helper_token_at(
        &config("acme.example"),
        &oauth,
        &token_file,
        env(),
        Some("bad-token"),
    )
    .await
    .unwrap();
    assert_eq!(retry.access_token, "env-token");
    // ...and a routine re-mint later (no rejection signal) still skips it.
    let later = helper_token_at(&config("acme.example"), &oauth, &token_file, env(), None)
        .await
        .unwrap();
    assert_eq!(later.access_token, "env-token");
    let stored = std::fs::read_to_string(dir.path().join(REJECTED_TOKENS_FILE)).unwrap();
    assert!(
        !stored.contains("bad-token"),
        "only a fingerprint is stored"
    );
}

#[tokio::test]
async fn not_signed_in_is_a_no_credential_error() {
    let dir = tempfile::tempdir().unwrap();
    let error = helper_token_at(
        &config("acme.example"),
        &dir.path().join("none.json"),
        &dir.path().join("none.token"),
        None,
        None,
    )
    .await
    .unwrap_err();
    assert!(error.downcast_ref::<NoCredential>().is_some(), "{error}");
}

#[test]
fn the_newest_rejected_fingerprints_are_kept() {
    let mut fingerprints = Vec::new();
    for n in 0..REJECTED_TOKENS_MAX + 5 {
        push_rejected_token(&mut fingerprints, format!("{n:04}"));
    }
    // A fingerprint that sorts first is still kept when it's the newest.
    push_rejected_token(&mut fingerprints, "0000-newest".to_owned());
    assert_eq!(fingerprints.len(), REJECTED_TOKENS_MAX);
    assert_eq!(fingerprints.last().map(String::as_str), Some("0000-newest"));
    assert!(
        !fingerprints.contains(&"0000".to_owned()),
        "the oldest are dropped"
    );
}
