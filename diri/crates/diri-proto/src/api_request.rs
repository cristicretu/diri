//! An HTTP request an agent asks the app to open in its session's API tab.
//!
//! The MCP bridge builds an [`ApiRequestDraft`] from `open_api_request`, the
//! Engine relays it as the `session.api_request` event, and the app prefills a
//! right-panel API surface with it. Nothing on this path sends the request:
//! only the app does, and only a `GET` may go out without the person pressing
//! Send ([`ApiRequestDraft::may_auto_send`]).
//!
//! Every hop calls [`ApiRequestDraft::validate`], so a malformed or oversized
//! draft is refused before it reaches the event bus or the UI.

use serde::{Deserialize, Serialize};

use crate::SessionId;

/// Methods the API surface can send.
pub const API_REQUEST_METHODS: [&str; 7] =
    ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];
pub const API_REQUEST_MAX_URL: usize = 8 * 1024;
pub const API_REQUEST_MAX_HEADERS: usize = 64;
pub const API_REQUEST_MAX_FIELD: usize = 16 * 1024;
pub const API_REQUEST_MAX_BODY: usize = 1024 * 1024;
pub const API_REQUEST_MAX_VARIABLES: usize = 64;
pub const API_REQUEST_MAX_NAME: usize = 200;

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ApiRequestDraft {
    pub method: String,
    /// May carry `{{variable}}` placeholders filled from the environment.
    pub url: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<ApiHeaderDraft>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// Title of the tab and of the request if the person saves it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<ApiEnvironmentDraft>,
    /// Send once it opens. Honoured for `GET` only.
    #[serde(default)]
    pub auto_send: bool,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ApiHeaderDraft {
    pub name: String,
    pub value: String,
}

/// Variables merged into the project's environment of this name (created
/// when missing), which becomes the active one.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ApiEnvironmentDraft {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default)]
    pub variables: Vec<ApiVariableDraft>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ApiVariableDraft {
    pub name: String,
    pub value: String,
    /// Shown masked in the app until revealed.
    #[serde(default)]
    pub secret: bool,
}

/// `session.open_api_request` params, and the `session.api_request` event.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SessionOpenApiRequestParams {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
    pub request: ApiRequestDraft,
}

impl ApiRequestDraft {
    /// Only a `GET` may go out without the person pressing Send.
    pub fn may_auto_send(&self) -> bool {
        self.auto_send && self.method == "GET"
    }

    pub fn validate(&self) -> Result<(), String> {
        if !API_REQUEST_METHODS.contains(&self.method.as_str()) {
            return Err(format!(
                "method must be one of {}",
                API_REQUEST_METHODS.join(", ")
            ));
        }
        let url = self.url.trim();
        if url.is_empty() {
            return Err("url is empty".into());
        }
        if url.len() > API_REQUEST_MAX_URL {
            return Err("url is too long".into());
        }
        if url.chars().any(char::is_control) {
            return Err("url contains control characters".into());
        }
        if let Some((scheme, _)) = url.split_once("://")
            && !scheme.starts_with("{{")
            && !matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https")
        {
            return Err("url must be http or https".into());
        }
        if self.headers.len() > API_REQUEST_MAX_HEADERS {
            return Err(format!("at most {API_REQUEST_MAX_HEADERS} headers"));
        }
        for header in &self.headers {
            valid_header_name(&header.name)?;
            if header.value.len() > API_REQUEST_MAX_FIELD
                || header.value.chars().any(|c| c.is_control() && c != '\t')
            {
                return Err(format!("header {} has an invalid value", header.name));
            }
        }
        if self
            .body
            .as_ref()
            .is_some_and(|body| body.len() > API_REQUEST_MAX_BODY)
        {
            return Err("body is larger than 1 MiB".into());
        }
        if self
            .name
            .as_ref()
            .is_some_and(|name| name.len() > API_REQUEST_MAX_NAME || name.contains('\n'))
        {
            return Err("name is too long".into());
        }
        if let Some(environment) = &self.environment {
            if environment
                .name
                .as_ref()
                .is_some_and(|name| name.trim().is_empty() || name.len() > API_REQUEST_MAX_NAME)
            {
                return Err("environment name is invalid".into());
            }
            if environment.variables.len() > API_REQUEST_MAX_VARIABLES {
                return Err(format!("at most {API_REQUEST_MAX_VARIABLES} variables"));
            }
            for variable in &environment.variables {
                if !valid_variable_name(&variable.name) {
                    return Err(format!("variable name {:?} is invalid", variable.name));
                }
                if variable.value.len() > API_REQUEST_MAX_FIELD {
                    return Err(format!("variable {} is too long", variable.name));
                }
            }
        }
        Ok(())
    }
}

/// An RFC 9110 token: what a header name may be.
pub fn valid_header_name(name: &str) -> Result<(), String> {
    let token = !name.is_empty()
        && name.len() <= 256
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte));
    if token {
        Ok(())
    } else {
        Err(format!("header name {name:?} is invalid"))
    }
}

/// `{{name}}` names: letters, digits, `_`, `-` and `.`.
pub fn valid_variable_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft(method: &str, url: &str) -> ApiRequestDraft {
        ApiRequestDraft {
            method: method.into(),
            url: url.into(),
            ..ApiRequestDraft::default()
        }
    }

    #[test]
    fn only_a_get_may_send_itself() {
        let mut get = draft("GET", "http://localhost:3000/health");
        get.auto_send = true;
        assert!(get.may_auto_send());
        let mut post = draft("POST", "http://localhost:3000/items");
        post.auto_send = true;
        assert!(!post.may_auto_send());
        get.auto_send = false;
        assert!(!get.may_auto_send());
    }

    #[test]
    fn validation_refuses_what_the_app_could_not_send_safely() {
        assert!(draft("GET", "http://localhost:3000").validate().is_ok());
        assert!(draft("GET", "{{base}}/users").validate().is_ok());
        assert!(draft("GET", "localhost:8080/x").validate().is_ok());
        assert!(draft("get", "http://x").validate().is_err());
        assert!(draft("GET", "  ").validate().is_err());
        assert!(draft("GET", "file:///etc/passwd").validate().is_err());
        assert!(draft("GET", "http://x/\nHost: y").validate().is_err());
        let mut headers = draft("GET", "http://x");
        headers.headers = vec![ApiHeaderDraft {
            name: "X Bad".into(),
            value: "1".into(),
        }];
        assert!(headers.validate().is_err());
        headers.headers[0].name = "Authorization".into();
        headers.headers[0].value = "Bearer a\r\nInjected: 1".into();
        assert!(headers.validate().is_err());
        let mut variables = draft("GET", "http://x");
        variables.environment = Some(ApiEnvironmentDraft {
            name: Some("Local".into()),
            variables: vec![ApiVariableDraft {
                name: "has space".into(),
                value: String::new(),
                secret: false,
            }],
        });
        assert!(variables.validate().is_err());
    }

    #[test]
    fn params_round_trip_on_the_wire() {
        let params = SessionOpenApiRequestParams {
            session_id: SessionId("s1".into()),
            request: ApiRequestDraft {
                method: "POST".into(),
                url: "{{base}}/items".into(),
                headers: vec![ApiHeaderDraft {
                    name: "Content-Type".into(),
                    value: "application/json".into(),
                }],
                body: Some("{\"a\":1}".into()),
                name: Some("Create item".into()),
                environment: Some(ApiEnvironmentDraft {
                    name: None,
                    variables: vec![ApiVariableDraft {
                        name: "base".into(),
                        value: "http://localhost:3000".into(),
                        secret: false,
                    }],
                }),
                auto_send: false,
            },
        };
        let value = serde_json::to_value(&params).unwrap();
        assert_eq!(value["sessionID"], "s1");
        assert_eq!(value["request"]["autoSend"], false);
        let back: SessionOpenApiRequestParams = serde_json::from_value(value).unwrap();
        assert_eq!(back, params);
    }
}
