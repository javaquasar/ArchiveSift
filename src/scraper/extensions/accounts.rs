//! Declarative form sign-in executed by the host, independently of WASM code.
use std::collections::BTreeMap;
use std::fmt;
use std::io::Read;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use reqwest::cookie::{CookieStore, Jar};
use reqwest::{header, Client, Method, Url};
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use super::manifest::PluginManifest;
use crate::app::ExtensionAccount;
use crate::secure_store::{Secret, SecretStore, StoreError};

const MAX_HTML: usize = 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FormLogin {
    pub schema: u32,
    pub login_url: String,
    pub check_url: String,
    pub form_selector: String,
    pub username_field: String,
    pub password_field: String,
    pub session_cookie: String,
    pub session_paths: Vec<String>,
    pub success_selector: String,
    #[serde(default)]
    pub extra_fields: BTreeMap<String, String>,
}

#[derive(Clone, Debug)]
pub struct AccountProvider {
    pub id: String,
    pub display_name: String,
    pub form: FormLogin,
    manifest: PluginManifest,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccountError {
    InvalidProfile,
    InvalidInput,
    MissingPassword,
    Storage,
    Network,
    RedirectBlocked,
    ResponseTooLarge,
    SignInFailed,
    BrowserRequired,
    SessionExpired,
}

impl fmt::Display for AccountError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidProfile => "The extension sign-in configuration is invalid.",
            Self::InvalidInput => "Enter a username and password of a supported length.",
            Self::MissingPassword => "Enter a password. No saved password is available for this account.",
            Self::Storage => "Cannot access the system password store. On Linux, install secret-tool and unlock the desktop keyring.",
            Self::Network => "The sign-in server could not be reached or returned an error. Try again later.",
            Self::RedirectBlocked => "Sign-in was stopped because the server redirected outside the permitted origin.",
            Self::ResponseTooLarge => "The sign-in response exceeds the allowed size.",
            Self::SignInFailed => "Sign-in failed. Check your username and password.",
            Self::BrowserRequired => "The site requires a CAPTCHA or browser verification. Complete sign-in in your browser.",
            Self::SessionExpired => "No active session. Sign in to continue.",
        })
    }
}
impl std::error::Error for AccountError {}
impl From<StoreError> for AccountError {
    fn from(_: StoreError) -> Self {
        Self::Storage
    }
}

#[derive(Clone, Debug)]
pub struct SignInRequest {
    pub provider: AccountProvider,
    pub username: String,
    pub password: Option<Secret>,
    pub remember_password: bool,
}

#[derive(Clone, Debug)]
pub enum AccountCommand {
    SignIn(SignInRequest),
    Check(AccountProvider),
    Forget(AccountProvider),
}

impl AccountCommand {
    pub fn id(&self) -> &str {
        match self {
            Self::SignIn(request) => &request.provider.id,
            Self::Check(provider) | Self::Forget(provider) => &provider.id,
        }
    }

    pub async fn execute(self, store: &dyn SecretStore) -> Result<AccountOutcome, AccountError> {
        match self {
            Self::SignIn(request) => {
                request
                    .provider
                    .sign_in(
                        &request.username,
                        request.password,
                        request.remember_password,
                        store,
                    )
                    .await
            }
            Self::Check(provider) => {
                provider.check_session(store).await?;
                Ok(AccountOutcome {
                    account: None,
                    forgotten: false,
                    password_saved: store.get(&provider.password_target())?.is_some(),
                })
            }
            Self::Forget(provider) => {
                store.delete(&provider.session_target())?;
                store.delete(&provider.password_target())?;
                Ok(AccountOutcome {
                    account: None,
                    forgotten: true,
                    password_saved: false,
                })
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct AccountOutcome {
    pub account: Option<ExtensionAccount>,
    pub password_saved: bool,
    pub forgotten: bool,
}

impl AccountProvider {
    pub fn from_directory(directory: &Path) -> Result<Option<Self>, AccountError> {
        let manifest = super::read_manifest(directory).map_err(|_| AccountError::InvalidProfile)?;
        Self::for_manifest(directory, &manifest)
    }

    pub(super) fn for_manifest(
        directory: &Path,
        manifest: &PluginManifest,
    ) -> Result<Option<Self>, AccountError> {
        let file = match std::fs::File::open(directory.join("account.json")) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(AccountError::InvalidProfile),
        };
        let mut bytes = Vec::new();
        file.take(65537)
            .read_to_end(&mut bytes)
            .map_err(|_| AccountError::InvalidProfile)?;
        if bytes.len() > 65536 {
            return Err(AccountError::InvalidProfile);
        }
        let form: FormLogin =
            serde_json::from_slice(&bytes).map_err(|_| AccountError::InvalidProfile)?;
        let provider = Self {
            id: manifest.id.clone(),
            display_name: manifest.display_name.clone(),
            form,
            manifest: manifest.clone(),
        };
        provider.validate()?;
        Ok(Some(provider))
    }

    fn validate(&self) -> Result<(), AccountError> {
        let form = &self.form;
        let login = self.checked_url(&form.login_url)?;
        let check = self.checked_url(&form.check_url)?;
        if self.id != self.manifest.id
            || form.schema != 1
            || login.origin() != check.origin()
            || login.fragment().is_some()
            || check.fragment().is_some()
            || form.session_paths.is_empty()
            || form.session_paths.len() > 32
            || form.extra_fields.len() > 32
            || !field_name(&form.username_field)
            || !field_name(&form.password_field)
            || !field_name(&form.session_cookie)
            || form.username_field == form.password_field
            || form.extra_fields.contains_key(&form.username_field)
            || form.extra_fields.contains_key(&form.password_field)
            || form
                .extra_fields
                .iter()
                .any(|(key, value)| !field_name(key) || value.len() > 8192)
            || Selector::parse(&form.form_selector).is_err()
            || Selector::parse(&form.success_selector).is_err()
        {
            return Err(AccountError::InvalidProfile);
        }
        for path in &form.session_paths {
            let url =
                self.checked_url(&format!("{}{}", login.origin().ascii_serialization(), path))?;
            if !path.starts_with('/')
                || path.starts_with("//")
                || url.path() != *path
                || url.query().is_some()
                || url.fragment().is_some()
                || url.origin() != login.origin()
            {
                return Err(AccountError::InvalidProfile);
            }
        }
        Ok(())
    }

    fn checked_url(&self, value: &str) -> Result<Url, AccountError> {
        let url = self
            .manifest
            .allowed_url(value)
            .map_err(|_| AccountError::InvalidProfile)?;
        let loopback = url
            .host_str()
            .and_then(|host| host.parse::<std::net::IpAddr>().ok())
            .is_some_and(|ip| ip.is_loopback());
        if url.scheme() != "https"
            && !(self.manifest.allow_http && loopback && url.scheme() == "http")
        {
            return Err(AccountError::InvalidProfile);
        }
        Ok(url)
    }

    pub fn origin(&self) -> String {
        Url::parse(&self.form.login_url)
            .expect("validated profile")
            .origin()
            .ascii_serialization()
    }
    pub fn password_target(&self) -> String {
        format!("ArchiveSift/{}/password", self.id)
    }
    pub fn session_target(&self) -> String {
        format!("ArchiveSift/{}/session", self.id)
    }

    async fn sign_in(
        &self,
        username: &str,
        supplied: Option<Secret>,
        remember: bool,
        store: &dyn SecretStore,
    ) -> Result<AccountOutcome, AccountError> {
        self.validate()?;
        let username = username.trim();
        let password = match supplied {
            Some(password) => password,
            None => store
                .get(&self.password_target())?
                .ok_or(AccountError::MissingPassword)?,
        };
        if username.is_empty()
            || username.len() > 512
            || username.chars().any(char::is_control)
            || password.bytes().is_empty()
            || password.bytes().len() > 1024
        {
            return Err(AccountError::InvalidInput);
        }
        let password_text =
            std::str::from_utf8(password.bytes()).map_err(|_| AccountError::InvalidInput)?;
        let flow = LoginFlow::new(self)?;
        let page = flow
            .request(Method::GET, self.checked_url(&self.form.login_url)?, None)
            .await?;
        let form = login_form(self, &page.body, page.encoding, username, password_text)?;
        let response = flow
            .request(
                Method::POST,
                self.checked_url(&self.form.login_url)?,
                Some(form),
            )
            .await?;
        if challenged(&response.body) {
            return Err(AccountError::BrowserRequired);
        }
        let checked = flow
            .request(Method::GET, self.checked_url(&self.form.check_url)?, None)
            .await?;
        if challenged(&checked.body) {
            return Err(AccountError::BrowserRequired);
        }
        if !signed_in(self, &checked.body) {
            return Err(AccountError::SignInFailed);
        }
        let cookie = flow
            .jar
            .cookies(&self.checked_url(&self.form.check_url)?)
            .ok_or(AccountError::SignInFailed)?;
        let prefix = format!("{}=", self.form.session_cookie);
        let session = cookie
            .to_str()
            .map_err(|_| AccountError::SignInFailed)?
            .split(';')
            .map(str::trim)
            .find(|part| part.starts_with(&prefix) && part.len() > prefix.len())
            .ok_or(AccountError::SignInFailed)?;
        // Authentication is verified before replacing a working session or password.
        let previous_session = store.get(&self.session_target())?;
        let previous_password = store.get(&self.password_target())?;
        let saved = (|| -> Result<(), StoreError> {
            store.set(
                &self.session_target(),
                &Secret::new(session.as_bytes().to_vec()),
            )?;
            if remember {
                store.set(&self.password_target(), &password)?;
            } else {
                store.delete(&self.password_target())?;
            }
            Ok(())
        })();
        if saved.is_err() {
            for (target, previous) in [
                (self.session_target(), previous_session),
                (self.password_target(), previous_password),
            ] {
                let _ = match previous {
                    Some(secret) => store.set(&target, &secret),
                    None => store.delete(&target),
                };
            }
            return Err(AccountError::Storage);
        }
        Ok(AccountOutcome {
            account: Some(ExtensionAccount {
                username: username.into(),
                remember_password: remember,
            }),
            password_saved: remember,
            forgotten: false,
        })
    }

    async fn check_session(&self, store: &dyn SecretStore) -> Result<(), AccountError> {
        self.validate()?;
        let secret = store
            .get(&self.session_target())?
            .ok_or(AccountError::SessionExpired)?;
        let value =
            std::str::from_utf8(secret.bytes()).map_err(|_| AccountError::SessionExpired)?;
        let flow = LoginFlow::new(self)?;
        flow.jar
            .add_cookie_str(value, &self.checked_url(&self.form.check_url)?);
        let checked = flow
            .request(Method::GET, self.checked_url(&self.form.check_url)?, None)
            .await?;
        if challenged(&checked.body) {
            return Err(AccountError::BrowserRequired);
        }
        if signed_in(self, &checked.body) {
            Ok(())
        } else {
            Err(AccountError::SessionExpired)
        }
    }
}

fn field_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-.[]".contains(&b))
}

struct LoginPage {
    body: Zeroizing<String>,
    encoding: &'static encoding_rs::Encoding,
}
struct LoginFlow<'a> {
    provider: &'a AccountProvider,
    client: Client,
    jar: Arc<Jar>,
}

impl<'a> LoginFlow<'a> {
    fn new(provider: &'a AccountProvider) -> Result<Self, AccountError> {
        let jar = Arc::new(Jar::default());
        let client = Client::builder()
            .user_agent(concat!("ArchiveSift/", env!("CARGO_PKG_VERSION")))
            .cookie_provider(jar.clone())
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|_| AccountError::Network)?;
        Ok(Self {
            provider,
            client,
            jar,
        })
    }

    async fn request(
        &self,
        mut method: Method,
        mut url: Url,
        mut body: Option<Secret>,
    ) -> Result<LoginPage, AccountError> {
        let _permit = crate::http::shared_request_limiter().acquire().await;
        for _ in 0..=5 {
            if url.origin().ascii_serialization() != self.provider.origin() {
                return Err(AccountError::RedirectBlocked);
            }
            self.provider.checked_url(url.as_str())?;
            let mut request = self.client.request(method.clone(), url.clone());
            if let Some(body) = &body {
                request = request
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .header(header::ORIGIN, self.provider.origin())
                    .header(header::REFERER, &self.provider.form.login_url)
                    .body(body.bytes().to_vec());
            }
            let response = request.send().await.map_err(|_| AccountError::Network)?;
            if response.status().is_redirection() {
                let target = response
                    .headers()
                    .get(header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| url.join(v).ok())
                    .ok_or(AccountError::RedirectBlocked)?;
                if target.origin() != url.origin() {
                    return Err(AccountError::RedirectBlocked);
                }
                if response.status().as_u16() == 303
                    || (method == Method::POST && matches!(response.status().as_u16(), 301 | 302))
                {
                    method = Method::GET;
                    body = None;
                }
                url = target;
                continue;
            }
            if !response.status().is_success() {
                return Err(if matches!(response.status().as_u16(), 401 | 403) {
                    AccountError::SignInFailed
                } else {
                    AccountError::Network
                });
            }
            let content_type = response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            if response
                .content_length()
                .is_some_and(|n| n > MAX_HTML as u64)
            {
                return Err(AccountError::ResponseTooLarge);
            }
            let encoding = content_type
                .split(';')
                .skip(1)
                .filter_map(|p| p.trim().split_once('='))
                .find_map(|(name, value)| {
                    name.eq_ignore_ascii_case("charset")
                        .then(|| {
                            encoding_rs::Encoding::for_label(
                                value.trim().trim_matches(['\'', '"']).as_bytes(),
                            )
                        })
                        .flatten()
                })
                .unwrap_or(encoding_rs::UTF_8);
            let mut bytes = Zeroizing::new(Vec::new());
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|_| AccountError::Network)?;
                if bytes.len() + chunk.len() > MAX_HTML {
                    return Err(AccountError::ResponseTooLarge);
                }
                bytes.extend_from_slice(&chunk);
            }
            return Ok(LoginPage {
                body: Zeroizing::new(encoding.decode(&bytes).0.into_owned()),
                encoding,
            });
        }
        Err(AccountError::RedirectBlocked)
    }
}

fn login_form(
    provider: &AccountProvider,
    body: &str,
    encoding: &'static encoding_rs::Encoding,
    username: &str,
    password: &str,
) -> Result<Secret, AccountError> {
    if challenged(body) {
        return Err(AccountError::BrowserRequired);
    }
    let document = Html::parse_document(body);
    let selector =
        Selector::parse(&provider.form.form_selector).map_err(|_| AccountError::InvalidProfile)?;
    let form = document
        .select(&selector)
        .next()
        .ok_or(AccountError::SignInFailed)?;
    let base = provider.checked_url(&provider.form.login_url)?;
    let action = base
        .join(form.value().attr("action").unwrap_or(""))
        .map_err(|_| AccountError::RedirectBlocked)?;
    if form.value().name() != "form"
        || !form
            .value()
            .attr("method")
            .is_some_and(|method| method.eq_ignore_ascii_case("post"))
        || action != base
    {
        return Err(AccountError::RedirectBlocked);
    }
    let inputs = Selector::parse("input[name]").unwrap();
    let mut fields = BTreeMap::new();
    let mut has_username = false;
    let mut has_password = false;
    for input in form.select(&inputs) {
        let node = input.value();
        let name = node.attr("name").unwrap();
        let kind = node.attr("type").unwrap_or("text");
        if node.attr("disabled").is_some() {
            continue;
        }
        has_username |= name == provider.form.username_field;
        has_password |=
            name == provider.form.password_field && kind.eq_ignore_ascii_case("password");
        if kind.eq_ignore_ascii_case("hidden") || kind.eq_ignore_ascii_case("submit") {
            let value = node.attr("value").unwrap_or("");
            if !field_name(name) || value.len() > 8192 || fields.len() >= 64 {
                return Err(AccountError::InvalidProfile);
            }
            fields.insert(name.to_string(), Zeroizing::new(value.to_string()));
        }
    }
    if !has_username || !has_password {
        return Err(AccountError::InvalidProfile);
    }
    for (name, value) in &provider.form.extra_fields {
        fields.insert(name.clone(), Zeroizing::new(value.clone()));
    }
    fields.insert(
        provider.form.username_field.clone(),
        Zeroizing::new(username.into()),
    );
    fields.insert(
        provider.form.password_field.clone(),
        Zeroizing::new(password.into()),
    );
    let encoding = form
        .value()
        .attr("accept-charset")
        .and_then(|s| encoding_rs::Encoding::for_label(s.as_bytes()))
        .unwrap_or(encoding);
    let mut encoded = Zeroizing::new(String::new());
    for (key, value) in fields {
        let (bytes, _, errors) = encoding.encode(&value);
        if errors {
            return Err(AccountError::InvalidInput);
        }
        let value_bytes = Zeroizing::new(bytes.into_owned());
        if !encoded.is_empty() {
            encoded.push('&');
        }
        encoded.extend(url::form_urlencoded::byte_serialize(key.as_bytes()));
        encoded.push('=');
        encoded.extend(url::form_urlencoded::byte_serialize(&value_bytes));
    }
    Ok(Secret::new(std::mem::take(&mut *encoded).into_bytes()))
}

fn signed_in(provider: &AccountProvider, body: &str) -> bool {
    Selector::parse(&provider.form.success_selector)
        .ok()
        .is_some_and(|selector| {
            Html::parse_document(body)
                .select(&selector)
                .next()
                .is_some()
        })
}
fn challenged(body: &str) -> bool {
    let selector = Selector::parse(
        "input[name*='captcha'], img[src*='captcha'], [data-sitekey], iframe[src*='recaptcha']",
    )
    .unwrap();
    Html::parse_document(body)
        .select(&selector)
        .next()
        .is_some()
}

#[cfg(test)]
mod tests;
