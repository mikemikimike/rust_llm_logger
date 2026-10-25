//! Access controls for telemetry and, optionally, the model proxy.
use axum::{
    extract::{Request, State},
    http::{header, HeaderMap, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::{net::SocketAddr, sync::Arc};
use subtle::ConstantTimeEq;

pub const PROXY_TOKEN_HEADER: &str = "x-llm-logger-token";

// Deliberately no Debug implementation: this configuration owns a credential.
#[derive(Clone)]
pub struct AccessConfig {
    token: Option<String>,
    auth_proxy: bool,
    pub(crate) show_prompts: bool,
}

impl Default for AccessConfig {
    fn default() -> Self {
        Self {
            token: None,
            auth_proxy: false,
            show_prompts: true,
        }
    }
}

impl AccessConfig {
    pub fn new(
        token: Option<String>,
        auth_proxy: bool,
        show_prompts: bool,
    ) -> Result<Self, String> {
        if let Some(token) = &token {
            if token.is_empty()
                || !token
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-._~+/=".contains(&b))
            {
                return Err("auth token must be a nonempty Bearer token without whitespace".into());
            }
        } else if auth_proxy {
            return Err("--auth-proxy requires --auth-token / LLM_LOGGER_TOKEN".into());
        }
        Ok(Self {
            token,
            auth_proxy,
            show_prompts,
        })
    }

    pub fn validate_bind(
        &self,
        addresses: &[SocketAddr],
        insecure_allow_public: bool,
    ) -> Result<(), String> {
        if addresses.is_empty() {
            return Err("bind address did not resolve to any socket addresses".into());
        }
        if self.token.is_none()
            && !insecure_allow_public
            && addresses.iter().any(|address| !address.ip().is_loopback())
        {
            return Err("non-loopback binds require --auth-token / LLM_LOGGER_TOKEN; use --insecure-allow-public only to explicitly allow unauthenticated access".into());
        }
        Ok(())
    }

    fn accepts(&self, supplied: Option<&str>) -> bool {
        match (&self.token, supplied) {
            (None, _) => true,
            (Some(expected), Some(supplied)) => {
                bool::from(expected.as_bytes().ct_eq(supplied.as_bytes()))
            }
            _ => false,
        }
    }
}

fn single_header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?.to_str().ok()?;
    if values.next().is_some() {
        return None;
    }
    Some(value)
}

pub(crate) async fn authorize(
    State(access): State<Arc<AccessConfig>>,
    mut request: Request,
    next: Next,
) -> Response {
    let proxy = request.uri().path().starts_with("/proxy/");
    let supplied = if proxy {
        single_header(request.headers(), PROXY_TOKEN_HEADER)
    } else {
        single_header(request.headers(), header::AUTHORIZATION.as_str())
            .and_then(|value| value.split_once(' '))
            .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("Bearer"))
            .map(|(_, token)| token)
    };
    if (!proxy || access.auth_proxy) && !access.accepts(supplied) {
        let mut response = (StatusCode::UNAUTHORIZED, "authentication required").into_response();
        if !proxy {
            response.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                "Bearer realm=\"llm-logger\"".parse().unwrap(),
            );
        }
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
        return response;
    }
    // Never forward or persist the logger credential, even with proxy auth off.
    // The client's upstream Authorization header belongs to the upstream.
    request.headers_mut().remove(PROXY_TOKEN_HEADER);
    let mut response = next.run(request).await;
    if !proxy {
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_bind_requires_token_or_explicit_override() {
        let anonymous = AccessConfig::default();
        for bind in ["127.0.0.1:0", "127.5.6.7:3000", "[::1]:3000"] {
            assert!(anonymous
                .validate_bind(&[bind.parse().unwrap()], false)
                .is_ok());
        }
        for bind in ["0.0.0.0:0", "[::]:3000", "192.168.1.1:3000"] {
            let addresses = [bind.parse().unwrap()];
            assert!(anonymous.validate_bind(&addresses, false).is_err());
            assert!(anonymous.validate_bind(&addresses, true).is_ok());
            assert!(AccessConfig::new(Some("secret".into()), true, true)
                .unwrap()
                .validate_bind(&addresses, false)
                .is_ok());
        }
        assert!(anonymous.validate_bind(&[], true).is_err());
        assert!(anonymous
            .validate_bind(
                &["127.0.0.1:0".parse().unwrap(), "0.0.0.0:0".parse().unwrap()],
                false
            )
            .is_err());
    }

    #[test]
    fn rejects_unusable_credentials_and_proxy_without_token() {
        for token in ["", " ", "with space", "line\nbreak", "密钥"] {
            assert!(AccessConfig::new(Some(token.into()), false, true).is_err());
        }
        assert!(AccessConfig::new(None, true, true).is_err());
        let access = AccessConfig::new(Some("test-secret".into()), true, true).unwrap();
        assert!(access.accepts(Some("test-secret")));
        for supplied in [
            None,
            Some("wrong"),
            Some("test-secreu"),
            Some("test-secret "),
        ] {
            assert!(!access.accepts(supplied));
        }
    }
}
