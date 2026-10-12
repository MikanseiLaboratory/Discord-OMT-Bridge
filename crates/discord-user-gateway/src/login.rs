use std::time::Duration;

use serde_json::{json, Value};

use crate::GatewayError;

const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) discord/1.0.9163 Chrome/128.0.6613.186 Electron/32.2.2 Safari/537.36";

#[derive(Clone)]
pub struct CaptchaAnswer {
    pub key: String,
    pub rqtoken: Option<String>,
    pub session_id: Option<String>,
}

#[derive(Clone)]
pub struct CaptchaChallenge {
    pub service: String,
    pub sitekey: String,
    pub rqdata: String,
    pub rqtoken: String,
    pub session_id: String,
}

pub enum AuthStep {
    Token(String),
    Mfa {
        ticket: String,
        totp: bool,
        sms: bool,
        backup: bool,
    },
    Captcha(CaptchaChallenge),
    SmsSent {
        phone: String,
    },
}

impl std::fmt::Debug for AuthStep {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Token(_) => formatter.write_str("Token(<redacted>)"),
            Self::Mfa {
                totp, sms, backup, ..
            } => formatter
                .debug_struct("Mfa")
                .field("ticket", &"<redacted>")
                .field("totp", totp)
                .field("sms", sms)
                .field("backup", backup)
                .finish(),
            Self::Captcha(challenge) => formatter
                .debug_struct("Captcha")
                .field("service", &challenge.service)
                .finish_non_exhaustive(),
            Self::SmsSent { phone } => formatter
                .debug_struct("SmsSent")
                .field("phone", phone)
                .finish(),
        }
    }
}

pub async fn login_password(
    login: &str,
    password: &str,
    build: u32,
    captcha: Option<CaptchaAnswer>,
) -> Result<AuthStep, GatewayError> {
    let login = login.trim();
    if login.is_empty() || password.is_empty() {
        return Err(GatewayError::Message(
            "メールアドレスとパスワードを入力してください".into(),
        ));
    }
    let mut body = json!({
        "login": login,
        "password": password,
        "undelete": false,
        "login_source": Value::Null,
        "gift_code_sku_id": Value::Null
    });
    attach_captcha(&mut body, captcha.as_ref());
    let (status, value) = post_auth("/auth/login", &body, build, captcha.as_ref()).await?;
    read_step(status, &value, false)
}

pub async fn submit_mfa(
    ticket: &str,
    code: &str,
    method: &str,
    build: u32,
    captcha: Option<CaptchaAnswer>,
) -> Result<AuthStep, GatewayError> {
    let ticket = ticket.trim();
    let code = code.trim();
    if ticket.is_empty() || code.is_empty() {
        return Err(GatewayError::Message("認証コードを入力してください".into()));
    }
    let path = if method == "sms" {
        "/auth/mfa/sms"
    } else {
        "/auth/mfa/totp"
    };
    let mut body = json!({
        "code": code,
        "ticket": ticket,
        "login_source": Value::Null,
        "gift_code_sku_id": Value::Null
    });
    attach_captcha(&mut body, captcha.as_ref());
    let (status, value) = post_auth(path, &body, build, captcha.as_ref()).await?;
    read_step(status, &value, false)
}

pub async fn send_login_sms(
    ticket: &str,
    build: u32,
    captcha: Option<CaptchaAnswer>,
) -> Result<AuthStep, GatewayError> {
    let ticket = ticket.trim();
    if ticket.is_empty() {
        return Err(GatewayError::Message(
            "ログインからやり直してください".into(),
        ));
    }
    let mut body = json!({ "ticket": ticket });
    attach_captcha(&mut body, captcha.as_ref());
    let (status, value) = post_auth("/auth/mfa/sms/send", &body, build, captcha.as_ref()).await?;
    read_step(status, &value, true)
}

fn attach_captcha(body: &mut Value, captcha: Option<&CaptchaAnswer>) {
    let Some(captcha) = captcha else {
        return;
    };
    if captcha.key.is_empty() {
        return;
    }
    body["captcha_key"] = json!(captcha.key);
    if let Some(token) = captcha.rqtoken.as_ref().filter(|token| !token.is_empty()) {
        body["captcha_rqtoken"] = json!(token);
    }
}

async fn post_auth(
    path: &str,
    body: &Value,
    build: u32,
    captcha: Option<&CaptchaAnswer>,
) -> Result<(u16, Value), GatewayError> {
    let build = if build == 0 {
        fetch_build_number().await.unwrap_or(366_000)
    } else {
        build
    };
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(20))
        .build()?;
    let experiments = client
        .get("https://discord.com/api/v9/experiments")
        .headers(common_headers(build, None, None))
        .send()
        .await?;
    let cookies = cookie_header(&experiments);
    let experiments_body: Value = experiments.json().await.unwrap_or(Value::Null);
    let fingerprint = experiments_body
        .get("fingerprint")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let mut request = client
        .post(format!("https://discord.com/api/v9{path}"))
        .headers(common_headers(build, Some(fingerprint), captcha));
    if !cookies.is_empty() {
        request = request.header("Cookie", cookies);
    }
    let response = request.json(body).send().await?;
    let status = response.status().as_u16();
    let value = response.json().await.unwrap_or(Value::Null);
    Ok((status, value))
}

fn common_headers(
    build: u32,
    fingerprint: Option<&str>,
    captcha: Option<&CaptchaAnswer>,
) -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        "X-Super-Properties",
        super_properties(build)
            .parse()
            .unwrap_or(reqwest::header::HeaderValue::from_static("")),
    );
    headers.insert(
        "X-Discord-Locale",
        reqwest::header::HeaderValue::from_static("ja"),
    );
    if let Some(fingerprint) = fingerprint.filter(|value| !value.is_empty()) {
        if let Ok(value) = fingerprint.parse() {
            headers.insert("X-Fingerprint", value);
        }
    }
    if let Some(session) = captcha
        .and_then(|captcha| captcha.session_id.as_deref())
        .filter(|value| !value.is_empty())
    {
        if let Ok(value) = session.parse() {
            headers.insert("X-Captcha-Session-Id", value);
        }
    }
    headers
}

fn super_properties(build: u32) -> String {
    let payload = json!({
        "os": "Windows",
        "browser": "Discord Client",
        "release_channel": "stable",
        "client_version": "1.0.9163",
        "os_version": "10.0.22631",
        "os_arch": "x64",
        "app_arch": "x64",
        "system_locale": "ja",
        "has_client_mods": false,
        "browser_user_agent": USER_AGENT,
        "browser_version": "32.2.2",
        "client_build_number": build,
        "native_build_number": build,
        "client_event_source": Value::Null
    });
    standard_b64(&serde_json::to_vec(&payload).unwrap_or_default())
}

fn cookie_header(response: &reqwest::Response) -> String {
    response
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .filter_map(|value| value.split(';').next())
        .collect::<Vec<_>>()
        .join("; ")
}

async fn fetch_build_number() -> Result<u32, GatewayError> {
    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(10))
        .build()?;
    let response = client
        .get("https://discord.com/api/updates/distributions/app/manifests/latest?channel=stable&platform=win&arch=x64")
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(GatewayError::Message("build lookup failed".into()));
    }
    let body: Value = response.json().await?;
    body.get("build_number")
        .or_else(|| body.get("buildNumber"))
        .and_then(|value| value.as_u64())
        .map(|value| value as u32)
        .ok_or_else(|| GatewayError::Message("build number missing".into()))
}

fn read_step(status: u16, body: &Value, sms_send: bool) -> Result<AuthStep, GatewayError> {
    if let Some(token) = body.get("token").and_then(|value| value.as_str()) {
        if !token.is_empty() {
            return Ok(AuthStep::Token(token.to_owned()));
        }
    }
    if let Some(challenge) = captcha_challenge(body) {
        return Ok(AuthStep::Captcha(challenge));
    }
    if body.get("mfa").and_then(|value| value.as_bool()) == Some(true) {
        let ticket = body
            .get("ticket")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .to_owned();
        if ticket.is_empty() {
            return Err(GatewayError::Message(
                "二段階認証を開始できませんでした".into(),
            ));
        }
        return Ok(AuthStep::Mfa {
            ticket,
            totp: body
                .get("totp")
                .and_then(|value| value.as_bool())
                .unwrap_or(true),
            sms: body
                .get("sms")
                .and_then(|value| value.as_bool())
                .unwrap_or(false),
            backup: body
                .get("backup")
                .and_then(|value| value.as_bool())
                .unwrap_or(true),
        });
    }
    if sms_send && (200..300).contains(&status) {
        let phone = body
            .get("phone")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .to_owned();
        return Ok(AuthStep::SmsSent { phone });
    }
    Err(GatewayError::Message(failure_message(status, body)))
}

fn captcha_challenge(body: &Value) -> Option<CaptchaChallenge> {
    let sitekey = body
        .get("captcha_sitekey")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    if sitekey.is_empty() {
        return None;
    }
    Some(CaptchaChallenge {
        service: body
            .get("captcha_service")
            .and_then(|value| value.as_str())
            .unwrap_or("hcaptcha")
            .to_owned(),
        sitekey: sitekey.to_owned(),
        rqdata: body
            .get("captcha_rqdata")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .to_owned(),
        rqtoken: body
            .get("captcha_rqtoken")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .to_owned(),
        session_id: body
            .get("captcha_session_id")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .to_owned(),
    })
}

fn failure_message(status: u16, body: &Value) -> String {
    let code = body
        .get("code")
        .and_then(|value| value.as_u64())
        .unwrap_or(0);
    let blob = body.to_string();
    if status == 429 {
        return "ログインの試行が多すぎます。しばらく待ってからもう一度試してください".into();
    }
    if code == 60008 || blob.contains("two-factor") || blob.contains("two factor") {
        return "認証コードが違います".into();
    }
    if blob.contains("INVALID_LOGIN") || blob.contains("Login or password is invalid") {
        return "メールアドレスまたはパスワードが違います".into();
    }
    if blob.contains("ACCOUNT_LOGIN_VERIFICATION") || blob.contains("verify") {
        return "メールで本人確認が必要です。確認してから、もう一度ログインしてください".into();
    }
    "ログインできませんでした。入力を見直して、もう一度試してください".into()
}

fn standard_b64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    let mut index = 0;
    while index + 3 <= bytes.len() {
        let value = ((bytes[index] as u32) << 16)
            | ((bytes[index + 1] as u32) << 8)
            | (bytes[index + 2] as u32);
        out.push(TABLE[((value >> 18) & 63) as usize] as char);
        out.push(TABLE[((value >> 12) & 63) as usize] as char);
        out.push(TABLE[((value >> 6) & 63) as usize] as char);
        out.push(TABLE[(value & 63) as usize] as char);
        index += 3;
    }
    let rest = bytes.len() - index;
    if rest == 1 {
        let value = (bytes[index] as u32) << 16;
        out.push(TABLE[((value >> 18) & 63) as usize] as char);
        out.push(TABLE[((value >> 12) & 63) as usize] as char);
        out.push('=');
        out.push('=');
    } else if rest == 2 {
        let value = ((bytes[index] as u32) << 16) | ((bytes[index + 1] as u32) << 8);
        out.push(TABLE[((value >> 18) & 63) as usize] as char);
        out.push(TABLE[((value >> 12) & 63) as usize] as char);
        out.push(TABLE[((value >> 6) & 63) as usize] as char);
        out.push('=');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_token_mfa_and_captcha() {
        let token = read_step(
            200,
            &json!({"token": "secret-token", "user_id": "1"}),
            false,
        )
        .unwrap();
        assert!(matches!(token, AuthStep::Token(value) if value == "secret-token"));
        let mfa = read_step(
            200,
            &json!({"token": null, "mfa": true, "ticket": "ticket-1", "totp": true, "sms": true, "backup": true}),
            false,
        )
        .unwrap();
        assert!(matches!(
            mfa,
            AuthStep::Mfa {
                sms: true,
                backup: true,
                ..
            }
        ));
        let captcha = read_step(
            400,
            &json!({"captcha_key": ["captcha-required"], "captcha_sitekey": "site", "captcha_service": "hcaptcha", "captcha_rqdata": "rq"}),
            false,
        )
        .unwrap();
        assert!(
            matches!(captcha, AuthStep::Captcha(challenge) if challenge.sitekey == "site" && challenge.rqdata == "rq")
        );
    }

    #[test]
    fn invalid_login_does_not_echo_the_password() {
        let err = read_step(
            400,
            &json!({"code": 50035, "errors": {"login": {"_errors": [{"code": "INVALID_LOGIN", "message": "Login or password is invalid."}]}}}),
            false,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("メールアドレスまたはパスワードが違います"));
        assert!(!err.contains("hunter2"));
    }

    #[test]
    fn base64_encodes_without_newlines() {
        assert_eq!(standard_b64(b"hello"), "aGVsbG8=");
    }
}
