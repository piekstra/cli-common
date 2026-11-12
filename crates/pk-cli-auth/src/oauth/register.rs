//! Dynamic client registration (RFC 7591).

use std::fmt;

use pk_cli_secrets::Secret;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use zeroize::Zeroize;

/// The metadata a client registers with (RFC 7591 §2). Fields a provider adds
/// go in `extra`, serialized beside the rest.
#[derive(Debug, Clone, Serialize)]
pub struct ClientMetadata {
    pub redirect_uris: Vec<String>,
    pub token_endpoint_auth_method: String,
    pub grant_types: Vec<String>,
    pub response_types: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub software_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub software_version: Option<String>,
    /// The client's public keys as a JWK Set, for `private_key_jwt` or a
    /// provider that binds the registration to a key it can verify.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jwks: Option<Value>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl ClientMetadata {
    /// A native app's public client (RFC 8252): no secret
    /// (`token_endpoint_auth_method: none`), the authorization-code and
    /// refresh grants, `code` responses.
    pub fn native_public(client_name: impl Into<String>, redirect_uris: Vec<String>) -> Self {
        ClientMetadata {
            redirect_uris,
            token_endpoint_auth_method: "none".into(),
            grant_types: vec!["authorization_code".into(), "refresh_token".into()],
            response_types: vec!["code".into()],
            client_name: Some(client_name.into()),
            scope: None,
            software_id: None,
            software_version: None,
            jwks: None,
            extra: Map::new(),
        }
    }

    pub fn scope(mut self, scope: impl Into<String>) -> Self {
        self.scope = Some(scope.into());
        self
    }

    pub fn software(mut self, id: impl Into<String>, version: impl Into<String>) -> Self {
        self.software_id = Some(id.into());
        self.software_version = Some(version.into());
        self
    }

    pub fn jwks(mut self, jwks: Value) -> Self {
        self.jwks = Some(jwks);
        self
    }

    /// Any other member, such as a provider's own.
    pub fn field(mut self, name: impl Into<String>, value: Value) -> Self {
        self.extra.insert(name.into(), value);
        self
    }
}

/// What the registration endpoint issued (RFC 7591 §3.2.1). Serializable, so
/// a CLI keeps it in the same keychain item as its tokens. `Debug` shows no
/// secret; the secrets are zeroed on drop.
#[derive(Clone, Serialize, Deserialize)]
pub struct RegisteredClient {
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_secret: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id_issued_at: Option<u64>,
    /// `0` means the secret does not expire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret_expires_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    registration_access_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registration_client_uri: Option<String>,
    /// The registered metadata the server echoed, and its own members.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl RegisteredClient {
    pub fn client_secret(&self) -> Option<Secret> {
        self.client_secret.as_deref().map(Secret::new)
    }

    /// RFC 7592's token for reading or updating the registration.
    pub fn registration_access_token(&self) -> Option<Secret> {
        self.registration_access_token.as_deref().map(Secret::new)
    }
}

impl fmt::Debug for RegisteredClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RegisteredClient")
            .field("client_id", &self.client_id)
            .field("client_secret", &self.client_secret.is_some())
            .field("client_secret_expires_at", &self.client_secret_expires_at)
            .field(
                "registration_access_token",
                &self.registration_access_token.is_some(),
            )
            .finish_non_exhaustive()
    }
}

impl Drop for RegisteredClient {
    fn drop(&mut self) {
        if let Some(s) = self.client_secret.as_mut() {
            s.zeroize();
        }
        if let Some(s) = self.registration_access_token.as_mut() {
            s.zeroize();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_native_public_client_registers_with_no_secret() {
        let metadata =
            ClientMetadata::native_public("pk-test", vec!["http://127.0.0.1/callback".into()])
                .scope("openid offline_access")
                .software("pk-test", "1.2.3")
                .field("contacts", Value::from(Vec::<String>::new()));
        let json = serde_json::to_value(&metadata).unwrap();
        assert_eq!(json["token_endpoint_auth_method"], "none");
        assert_eq!(
            json["grant_types"],
            serde_json::json!(["authorization_code", "refresh_token"])
        );
        assert_eq!(json["response_types"], serde_json::json!(["code"]));
        assert_eq!(json["redirect_uris"][0], "http://127.0.0.1/callback");
        assert_eq!(json["software_version"], "1.2.3");
        assert!(json["contacts"].is_array());
        assert!(json.get("jwks").is_none());
    }

    #[test]
    fn debug_hides_the_secrets() {
        let client: RegisteredClient = serde_json::from_str(
            r#"{"client_id":"id-1","client_secret":"sec-1","registration_access_token":"rat-1"}"#,
        )
        .unwrap();
        let shown = format!("{client:?}");
        assert!(shown.contains("id-1"));
        assert!(
            !shown.contains("sec-1") && !shown.contains("rat-1"),
            "{shown}"
        );
        assert_eq!(client.client_secret().unwrap().expose(), "sec-1");
    }
}
