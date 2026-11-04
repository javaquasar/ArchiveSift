use super::*;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Mutex,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Default)]
struct MemoryStore {
    entries: Mutex<BTreeMap<String, Secret>>,
    fail_next_password: std::sync::atomic::AtomicBool,
}
impl SecretStore for MemoryStore {
    fn get(&self, target: &str) -> Result<Option<Secret>, StoreError> {
        Ok(self.entries.lock().unwrap().get(target).cloned())
    }
    fn set(&self, target: &str, value: &Secret) -> Result<(), StoreError> {
        if target.ends_with("/password") && self.fail_next_password.swap(false, Ordering::SeqCst) {
            return Err(StoreError);
        }
        self.entries
            .lock()
            .unwrap()
            .insert(target.into(), value.clone());
        Ok(())
    }
    fn delete(&self, target: &str) -> Result<(), StoreError> {
        self.entries.lock().unwrap().remove(target);
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum Scenario {
    Success,
    WrongPassword,
    Challenge,
    ExternalRedirect,
    ExternalForm,
    Oversized,
}
struct Site {
    origin: String,
    posts: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Site {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Site {
    async fn start(scenario: Scenario) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let posts = Arc::new(AtomicUsize::new(0));
        let count = posts.clone();
        let task = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let count = count.clone();
                tokio::spawn(async move {
                    let mut bytes = Vec::new();
                    loop {
                        let mut chunk = [0; 4096];
                        let n = stream.read(&mut chunk).await.unwrap();
                        if n == 0 {
                            return;
                        }
                        bytes.extend_from_slice(&chunk[..n]);
                        if let Some(end) = bytes.windows(4).position(|s| s == b"\r\n\r\n") {
                            let header = String::from_utf8_lossy(&bytes[..end]).to_lowercase();
                            let length = header
                                .lines()
                                .find_map(|line| {
                                    line.strip_prefix("content-length:")
                                        .and_then(|n| n.trim().parse::<usize>().ok())
                                })
                                .unwrap_or(0);
                            if bytes.len() >= end + 4 + length {
                                break;
                            }
                        }
                    }
                    let request = String::from_utf8(bytes).unwrap();
                    let mut headers = String::new();
                    let mut status = "200 OK";
                    let body = if request.starts_with("POST /login ") {
                        count.fetch_add(1, Ordering::SeqCst);
                        let lower = request.to_lowercase();
                        assert!(lower.contains("cookie: preflight=nonce"));
                        assert!(request.contains("csrf=fixture-token"));
                        assert!(
                            request.contains("username=fixture-user")
                                && request.contains("password=fixture-password")
                        );
                        if matches!(scenario, Scenario::ExternalRedirect) {
                            status = "307 Temporary Redirect";
                            headers.push_str("Location: http://127.0.0.1:1/forbidden\r\n");
                        } else if !matches!(scenario, Scenario::WrongPassword) {
                            status = "303 See Other";
                            headers.push_str("Set-Cookie: session=fixture-session; Path=/; HttpOnly\r\nLocation: /home\r\n");
                        }
                        "<p>response</p>".to_string()
                    } else if request.starts_with("GET /home ") {
                        if request.contains("session=fixture-session") {
                            "<a id='logout'>Sign out</a>".into()
                        } else {
                            "<p>Guest</p>".into()
                        }
                    } else {
                        headers.push_str("Set-Cookie: preflight=nonce; Path=/; HttpOnly\r\n");
                        if matches!(scenario, Scenario::Challenge) {
                            "<input name='captcha'>".into()
                        } else if matches!(scenario, Scenario::ExternalForm) {
                            "<form method='post' action='http://127.0.0.1:1/forbidden'><input name='username'><input type='password' name='password'></form>".into()
                        } else if matches!(scenario, Scenario::Oversized) {
                            "x".repeat(MAX_HTML + 1)
                        } else {
                            "<form method='post' action='/login'><input type='hidden' name='csrf' value='fixture-token'><input name='username'><input name='password' type='password'><input type='submit' name='submit' value='Sign in'></form>".into()
                        }
                    };
                    let header = format!("HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                    let _ = stream.write_all(header.as_bytes()).await;
                    let _ = stream.write_all(body.as_bytes()).await;
                });
            }
        });
        Self {
            origin,
            posts,
            task,
        }
    }
    fn provider(&self) -> AccountProvider {
        provider(&self.origin)
    }
}

fn provider(origin: &str) -> AccountProvider {
    let mut manifest = crate::scraper::extensions::tests::manifest();
    manifest.network_origins = vec![origin.into()];
    manifest.allow_http = origin.starts_with("http://");
    AccountProvider {
        id: manifest.id.clone(),
        display_name: "Example extension".into(),
        form: FormLogin {
            schema: 1,
            login_url: format!("{origin}/login"),
            check_url: format!("{origin}/home"),
            form_selector: "form".into(),
            username_field: "username".into(),
            password_field: "password".into(),
            session_cookie: "session".into(),
            session_paths: vec!["/home".into(), "/download".into()],
            success_selector: "#logout".into(),
            extra_fields: BTreeMap::new(),
        },
        manifest,
    }
}

async fn login(
    provider: &AccountProvider,
    store: &dyn SecretStore,
    remember: bool,
) -> Result<AccountOutcome, AccountError> {
    provider
        .sign_in(
            "fixture-user",
            Some(Secret::new(b"fixture-password".to_vec())),
            remember,
            store,
        )
        .await
}

#[tokio::test]
async fn real_form_flow_preserves_csrf_cookie_and_session_across_new_clients() {
    let site = Site::start(Scenario::Success).await;
    let provider = site.provider();
    let store = MemoryStore::default();
    let result = login(&provider, &store, true).await.unwrap();
    assert!(result.password_saved);
    assert_eq!(result.account.unwrap().username, "fixture-user");
    assert_eq!(
        store
            .get(&provider.session_target())
            .unwrap()
            .unwrap()
            .bytes(),
        b"session=fixture-session"
    );
    provider.check_session(&store).await.unwrap();
    provider
        .sign_in("fixture-user", None, true, &store)
        .await
        .unwrap();
    assert_eq!(site.posts.load(Ordering::SeqCst), 2);
    AccountCommand::Forget(provider.clone())
        .execute(&store)
        .await
        .unwrap();
    assert!(store.entries.lock().unwrap().is_empty());
    assert_eq!(
        provider.check_session(&store).await.unwrap_err(),
        AccountError::SessionExpired
    );
}

#[tokio::test]
async fn failed_login_and_storage_failure_preserve_previous_secrets() {
    let site = Site::start(Scenario::WrongPassword).await;
    let provider = site.provider();
    let store = MemoryStore::default();
    store
        .set(
            &provider.session_target(),
            &Secret::new(b"old-session".to_vec()),
        )
        .unwrap();
    store
        .set(
            &provider.password_target(),
            &Secret::new(b"old-password".to_vec()),
        )
        .unwrap();
    assert_eq!(
        login(&provider, &store, true).await.unwrap_err(),
        AccountError::SignInFailed
    );
    assert_eq!(
        store
            .get(&provider.password_target())
            .unwrap()
            .unwrap()
            .bytes(),
        b"old-password"
    );
    let good = Site::start(Scenario::Success).await;
    let provider = good.provider();
    store.fail_next_password.store(true, Ordering::SeqCst);
    assert_eq!(
        login(&provider, &store, true).await.unwrap_err(),
        AccountError::Storage
    );
    assert_eq!(
        store
            .get(&provider.session_target())
            .unwrap()
            .unwrap()
            .bytes(),
        b"old-session"
    );
    assert_eq!(
        store
            .get(&provider.password_target())
            .unwrap()
            .unwrap()
            .bytes(),
        b"old-password"
    );
    login(&provider, &store, false).await.unwrap();
    assert!(store.get(&provider.password_target()).unwrap().is_none());
}

#[tokio::test]
async fn browser_challenges_external_redirects_and_large_responses_are_blocked() {
    for (scenario, error, posts) in [
        (Scenario::Challenge, AccountError::BrowserRequired, 0),
        (Scenario::ExternalForm, AccountError::RedirectBlocked, 0),
        (Scenario::ExternalRedirect, AccountError::RedirectBlocked, 1),
        (Scenario::Oversized, AccountError::ResponseTooLarge, 0),
    ] {
        let site = Site::start(scenario).await;
        let store = MemoryStore::default();
        assert_eq!(
            login(&site.provider(), &store, true).await.unwrap_err(),
            error
        );
        assert_eq!(site.posts.load(Ordering::SeqCst), posts);
        assert!(store.entries.lock().unwrap().is_empty());
    }
}

#[test]
fn legacy_form_encoding_handles_unicode_and_hidden_fields() {
    let provider = provider("https://example.test");
    let html = "<form method='post' action='/login'><input name='username'><input name='password' type='password'><input type='hidden' name='csrf' value='token'><input type='submit' name='submit' value='Вход'></form>";
    let body = login_form(&provider, html, encoding_rs::WINDOWS_1251, "Иван", "пароль").unwrap();
    let encoded = std::str::from_utf8(body.bytes()).unwrap();
    assert!(encoded.contains("submit=%C2%F5%EE%E4"));
    assert!(encoded.contains("username=%C8%E2%E0%ED"));
    assert!(encoded.contains("password=%EF%E0%F0%EE%EB%FC"));
    assert!(!format!("{body:?}").contains("password"));
    assert_eq!(
        login_form(&provider, html, encoding_rs::WINDOWS_1251, "😀", "password").unwrap_err(),
        AccountError::InvalidInput
    );
}

#[test]
fn profiles_validate_permissions_paths_fields_and_selectors() {
    let provider = provider("https://example.test");
    provider.validate().unwrap();
    let mut changed = provider.clone();
    changed.form.check_url = "https://other.test/".into();
    assert_eq!(changed.validate(), Err(AccountError::InvalidProfile));
    for path in [
        "//other.test/home",
        "/home?x=1",
        "/home#fragment",
        "/../home",
    ] {
        let mut changed = provider.clone();
        changed.form.session_paths = vec![path.into()];
        assert_eq!(changed.validate(), Err(AccountError::InvalidProfile));
    }
    let mut changed = provider.clone();
    changed
        .form
        .extra_fields
        .insert("password".into(), "fixed".into());
    assert_eq!(changed.validate(), Err(AccountError::InvalidProfile));
    let mut changed = provider.clone();
    changed.form.form_selector = "[".into();
    assert_eq!(changed.validate(), Err(AccountError::InvalidProfile));
    assert_eq!(
        self::provider("http://example.test").validate(),
        Err(AccountError::InvalidProfile)
    );
}

#[test]
fn configuration_and_debug_output_never_serialize_passwords() {
    let mut config = crate::app::AppConfig::default();
    config.extension_accounts.insert(
        "example.parser".into(),
        ExtensionAccount {
            username: "fixture-user".into(),
            remember_password: true,
        },
    );
    let json = serde_json::to_string(&config).unwrap();
    assert!(json.contains("fixture-user"));
    assert!(!json.contains("fixture-password"));
    let provider = provider("https://example.test");
    let request = SignInRequest {
        provider,
        username: "fixture-user".into(),
        password: Some(Secret::new(b"fixture-password".to_vec())),
        remember_password: true,
    };
    assert!(!format!("{request:?}").contains("fixture-password"));
}
