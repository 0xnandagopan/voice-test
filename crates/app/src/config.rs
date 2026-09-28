use argon2::PasswordHash;
use std::env;
#[derive(Clone)]
pub struct Config {
    pub origin: String,
    pub agency_name: String,
    pub operator_username: String,
    pub operator_password_hash: String,
    pub invitation_signing_key: String,
    pub secure_cookie: bool,
    pub voice_api_key: Option<String>,
}
impl Config {
    pub fn from_env() -> Result<Self, String> {
        let required = |name: &str| {
            env::var(name)
                .ok()
                .filter(|v| !v.trim().is_empty())
                .ok_or_else(|| format!("{name} must be configured"))
        };
        let origin = required("APP_ORIGIN")?;
        let parsed =
            url::Url::parse(&origin).map_err(|_| "APP_ORIGIN must be an absolute origin")?;
        if !matches!(parsed.scheme(), "http" | "https")
            || parsed.path() != "/"
            || parsed.query().is_some()
            || parsed.fragment().is_some()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
        {
            return Err("APP_ORIGIN must contain only scheme, host and optional port".into());
        }
        let secure_cookie = match env::var("COOKIE_SECURE")
            .unwrap_or_else(|_| "true".into())
            .as_str()
        {
            "true" => true,
            "false" => false,
            _ => return Err("COOKIE_SECURE must be true or false".into()),
        };
        if !secure_cookie
            && !matches!(
                parsed.host_str(),
                Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
            )
        {
            return Err("Insecure cookies are restricted to loopback development".into());
        }
        if secure_cookie && parsed.scheme() != "https" {
            return Err("Secure cookies require an HTTPS APP_ORIGIN".into());
        }
        let operator_password_hash = required("OPERATOR_PASSWORD_HASH")?;
        PasswordHash::new(&operator_password_hash)
            .map_err(|_| "OPERATOR_PASSWORD_HASH must be a valid Argon2 password hash")?;
        let invitation_signing_key = required("INVITATION_SIGNING_KEY")?;
        if invitation_signing_key.len() < 32 {
            return Err(
                "INVITATION_SIGNING_KEY needs at least 32 random bytes encoded as text".into(),
            );
        }
        Ok(Self {
            origin: origin.trim_end_matches('/').to_owned(),
            agency_name: required("AGENCY_NAME")?,
            operator_username: required("OPERATOR_USERNAME")?,
            operator_password_hash,
            invitation_signing_key,
            secure_cookie,
            voice_api_key: env::var("VOICE_AGENT_API_KEY")
                .ok()
                .filter(|v| !v.trim().is_empty()),
        })
    }
}

/// Fail with a fixed message: dotenv parse errors may include credential values.
pub fn load_dotenv() -> Result<(), &'static str> {
    match dotenvy::dotenv() {
        Ok(_) => Ok(()),
        Err(dotenvy::Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(
            "Could not load .env. Check assignment syntax and quote values containing spaces; no values were logged.",
        ),
    }
}
