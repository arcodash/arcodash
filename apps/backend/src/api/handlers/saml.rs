//! SAML 2.0 SP-initiated login — `GET /api/saml/login` (redirect to the `IdP`) and `POST
//! /api/saml/acs` (assertion consumer service).
//!
//! Enabled only when `SAML_IDP_METADATA_XML` is configured. Auto-provisions users; maps the
//! `FirstName`, `LastName`, and `ArcodashGroups` SAML attributes to this app's own name + group
//! model.
//!
//! A Redis-backed one-time state token guards against replay/CSRF (carried as SAML's `RelayState`,
//! checked against the `AuthnRequest` id we generated), and login issues the same session cookie
//! via `crate::auth::session`.

use axum::extract::{Form, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect};
use redis::AsyncCommands;
use samael::metadata::{EntityDescriptor, HTTP_REDIRECT_BINDING};
use samael::schema::Assertion;
use samael::service_provider::{ServiceProvider, ServiceProviderBuilder};
use serde::Deserialize;

use arcodash_core::events::record_event;

use crate::api::handlers::auth::{record_event_or_warn, session_jar, sso_display_name};
use crate::auth::CurrentOrg;
use crate::db::Db;
use crate::error::ApiError;
use crate::services::user_service;
use crate::state::AppState;

const SAML_STATE_TTL: u64 = 300; // 5 minutes.

fn saml_state_key(request_id: &str) -> String {
    format!("arcodash:saml:req:{request_id}")
}

fn sp_entity_id(state: &AppState) -> String {
    state
        .config
        .saml_sp_entity_id
        .clone()
        .unwrap_or_else(|| format!("{}/api/saml/metadata", state.config.base_url))
}

/// Build the `ServiceProvider` from config, or a clear disabled error if `SAML_IDP_METADATA_XML`
/// isn't set — fails closed.
fn build_service_provider(state: &AppState) -> Result<ServiceProvider, ApiError> {
    let xml =
        state.config.saml_idp_metadata_xml.as_ref().ok_or_else(|| {
            ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "SAML is not configured")
        })?;
    let acs_url = format!("{}/api/saml/acs", state.config.base_url);
    build_service_provider_from_metadata(xml, sp_entity_id(state), acs_url)
}

/// Parses `IdP` metadata XML into a `ServiceProvider`, rejecting metadata with no `IdP` signing
/// certificate at configuration time — samael skips signature verification entirely when there's
/// no cert to check against, so accepting such metadata would silently let every assertion
/// through unverified.
fn build_service_provider_from_metadata(
    xml: &str,
    entity_id: String,
    acs_url: String,
) -> Result<ServiceProvider, ApiError> {
    let idp_metadata: EntityDescriptor = xml
        .parse()
        .map_err(|e| ApiError::internal(format!("invalid SAML_IDP_METADATA_XML: {e}")))?;

    // No `GET /api/saml/metadata` endpoint: IdPs generally let an admin paste the ACS URL +
    // an arbitrary entity ID directly, so no SP metadata document exchange is required.
    let sp = ServiceProviderBuilder::default()
        .entity_id(entity_id)
        .acs_url(acs_url)
        .idp_metadata(idp_metadata)
        // SP-initiated only: an assertion is only accepted if its InResponseTo matches an
        // AuthnRequest id we generated and stashed in Redis (checked in `acs` below) — this
        // flag is belt-and-suspenders on top of that check, not a substitute for it.
        .allow_idp_initiated(false)
        .build()
        .map_err(|e| ApiError::internal(format!("failed to build SAML service provider: {e}")))?;

    let has_signing_cert = sp
        .idp_signing_certs()
        .map_err(|e| ApiError::internal(format!("failed to read IdP signing certs: {e}")))?
        .is_some_and(|certs| !certs.is_empty());
    if !has_signing_cert {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "SAML_IDP_METADATA_XML has no IdP signing certificate; assertions cannot be verified",
        ));
    }

    Ok(sp)
}

/// `GET /api/saml/login` — build an `AuthnRequest` and redirect to the `IdP`'s SSO endpoint.
pub async fn login(
    State(state): State<AppState>,
    CurrentOrg(_org): CurrentOrg,
) -> Result<impl IntoResponse, ApiError> {
    let sp = build_service_provider(&state)?;

    let idp_sso_url = sp
        .sso_binding_location(HTTP_REDIRECT_BINDING)
        .ok_or_else(|| {
            ApiError::internal("IdP metadata has no HTTP-Redirect SingleSignOnService endpoint")
        })?;

    let authn_request = sp
        .make_authentication_request(&idp_sso_url)
        .map_err(|e| ApiError::internal(format!("failed to build SAML AuthnRequest: {e}")))?;
    let request_id = authn_request.id.clone();

    let mut redis = state.redis.clone();
    let _: () = redis
        .set_ex(saml_state_key(&request_id), "1", SAML_STATE_TTL)
        .await
        .map_err(|e| ApiError::internal(format!("redis error storing SAML request state: {e}")))?;

    // RelayState round-trips the request id through the IdP so `acs` can look it back up.
    let url = authn_request
        .redirect(&request_id)
        .map_err(|e| ApiError::internal(format!("failed to build SAML redirect URL: {e}")))?
        .ok_or_else(|| ApiError::internal("SAML AuthnRequest is missing a destination"))?;

    Ok(Redirect::to(url.as_str()))
}

#[derive(Debug, Deserialize)]
pub struct AcsForm {
    #[serde(rename = "SAMLResponse")]
    pub saml_response: String,
    #[serde(rename = "RelayState")]
    pub relay_state: Option<String>,
}

/// `POST /api/saml/acs` — the `IdP` posts here (`application/x-www-form-urlencoded`, per the SAML
/// HTTP-POST binding) with `SAMLResponse` + `RelayState`.
///
/// Verifies the response is signed by the `IdP`, unexpired, and actually answers a request we
/// issued, then provisions/logs the user in.
pub async fn acs(
    State(state): State<AppState>,
    db: Db,
    CurrentOrg(org): CurrentOrg,
    headers: HeaderMap,
    Form(form): Form<AcsForm>,
) -> Result<impl IntoResponse, ApiError> {
    let relay_state = form
        .relay_state
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ApiError::bad_request("missing SAML RelayState"))?;

    // One-time use: GET+DEL so a captured response can't be replayed against this endpoint.
    let mut redis = state.redis.clone();
    let stored: Option<String> = redis
        .get_del(saml_state_key(&relay_state))
        .await
        .map_err(|e| ApiError::internal(format!("redis error fetching SAML request state: {e}")))?;
    if stored.is_none() {
        crate::metrics::record_auth_outcome("saml", "bad_credentials");
        tracing::warn!(
            org_id = org.id,
            via = "saml",
            reason = "replay_or_unknown_state",
            "rejecting login: SAML response replay or unknown relay state"
        );
        record_event_or_warn(
            &db,
            org.id,
            None,
            "user.login_failed",
            None,
            serde_json::json!({ "via": "saml", "reason": "replay_or_unknown_state" }),
        )
        .await;
        return Err(ApiError::bad_request(
            "invalid, expired, or already-used SAML login attempt",
        ));
    }

    let sp = build_service_provider(&state)?;

    // Validates the XML signature against the IdP's metadata certificate, assertion expiry,
    // audience, and that `InResponseTo` equals `relay_state` — any failure here is a hard
    // reject, never a fallback to trusting the assertion unchecked.
    // XML canonicalization + RSA signature verification over caller-supplied, unauthenticated
    // input — run on the blocking pool so it can't hold a tokio worker thread for an
    // attacker-controlled duration. `.map_err` converts the non-`Send` `Box<dyn StdError>` into
    // an owned `String` inside the closure, before it ever needs to cross back over an `.await`.
    let parsed = tokio::task::spawn_blocking(move || {
        sp.parse_base64_response(&form.saml_response, Some(&[relay_state.as_str()]))
            .map_err(|e| format!("invalid SAML response: {e}"))
    })
    .await
    .map_err(|e| ApiError::internal(format!("SAML response parsing task panicked: {e}")))?;

    let assertion = match parsed {
        Ok(a) => a,
        Err(message) => {
            crate::metrics::record_auth_outcome("saml", "bad_credentials");
            // The verification internals (`message`) are logged server-side only — returning
            // them to the caller would hand an unauthenticated party detail about *why* its
            // forged/tampered assertion failed, which is exactly backwards for a 403.
            tracing::warn!(
                org_id = org.id,
                via = "saml",
                reason = "invalid_response",
                detail = %message,
                "rejecting login: SAML assertion failed verification"
            );
            record_event_or_warn(
                &db,
                org.id,
                None,
                "user.login_failed",
                None,
                serde_json::json!({ "via": "saml", "reason": "invalid_response" }),
            )
            .await;
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "SAML assertion failed verification",
            ));
        }
    };

    let email = assertion
        .subject
        .as_ref()
        .and_then(|s| s.name_id.as_ref())
        .map(|n| n.value.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ApiError::bad_request("SAML assertion has no NameID (email)"))?;

    let (first_name, last_name, group_names) = extract_saml_attributes(&assertion);
    let name = sso_display_name(first_name, last_name, &email);

    let user = match user_service::find_or_create_sso_user_with_groups(
        &db,
        org.id,
        &email,
        &name,
        None,
        group_names,
    )
    .await
    {
        Ok(u) => u,
        Err(e) => {
            crate::metrics::record_auth_outcome("saml", "provisioning_failed");
            record_event_or_warn(
                &db,
                org.id,
                None,
                "user.login_failed",
                None,
                serde_json::json!({ "attempted_email": email, "via": "saml" }),
            )
            .await;
            return Err(e);
        }
    };
    crate::metrics::record_auth_outcome("saml", "success");

    record_event(
        &db,
        org.id,
        Some(user.id),
        "user.login",
        Some("user"),
        Some(user.id),
        serde_json::json!({ "via": "saml" }),
    )
    .await?;

    let jar = session_jar(&state, &user, org.id, &headers)?;
    Ok((jar, Redirect::to(&format!("{}/", state.config.base_url))))
}

/// Extract the `FirstName`/`LastName`/`ArcodashGroups` attributes, matching by `Name` or
/// `FriendlyName` since `IdPs` vary in which they populate. `ArcodashGroups` may repeat as multiple
/// `AttributeValue`s (multi-valued attribute) — all are collected.
fn extract_saml_attributes(
    assertion: &Assertion,
) -> (Option<String>, Option<String>, Option<Vec<String>>) {
    let mut first_name = None;
    let mut last_name = None;
    let mut groups: Vec<String> = vec![];

    for attr in assertion
        .attribute_statements
        .iter()
        .flatten()
        .flat_map(|s| &s.attributes)
    {
        let attr_name = attr
            .name
            .as_deref()
            .or(attr.friendly_name.as_deref())
            .unwrap_or_default();
        let values: Vec<String> = attr.values.iter().filter_map(|v| v.value.clone()).collect();

        match attr_name {
            "FirstName" => first_name = values.into_iter().next(),
            "LastName" => last_name = values.into_iter().next(),
            "ArcodashGroups" => groups.extend(values),
            _ => {}
        }
    }

    (
        first_name,
        last_name,
        (!groups.is_empty()).then_some(groups),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use samael::attribute::{Attribute, AttributeValue};
    use samael::schema::{AttributeStatement, Issuer};

    fn attr(name: &str, values: &[&str]) -> Attribute {
        Attribute {
            friendly_name: None,
            name: Some(name.to_string()),
            name_format: None,
            values: values
                .iter()
                .map(|v| AttributeValue {
                    attribute_type: None,
                    value: Some(v.to_string()),
                })
                .collect(),
        }
    }

    fn assertion_with_attributes(attributes: Vec<Attribute>) -> Assertion {
        Assertion {
            id: "test-id".to_string(),
            issue_instant: Utc::now(),
            version: "2.0".to_string(),
            issuer: Issuer::default(),
            signature: None,
            subject: None,
            conditions: None,
            authn_statements: None,
            attribute_statements: Some(vec![AttributeStatement { attributes }]),
        }
    }

    /// Self-check for the attribute-mapping logic this whole handler hinges on: a SAML
    /// assertion carrying `FirstName`/`LastName`/`ArcodashGroups` must map to the expected
    /// name + group list, and unrelated/unknown attributes must be ignored rather than
    /// erroring the login.
    #[test]
    fn maps_first_last_and_arcodash_groups_attributes() {
        let assertion = assertion_with_attributes(vec![
            attr("FirstName", &["Ada"]),
            attr("LastName", &["Lovelace"]),
            attr("ArcodashGroups", &["analysts", "admins"]),
            attr("SomeOtherAttribute", &["ignored"]),
        ]);

        let (first, last, groups) = extract_saml_attributes(&assertion);
        assert_eq!(first.as_deref(), Some("Ada"));
        assert_eq!(last.as_deref(), Some("Lovelace"));
        assert_eq!(
            groups,
            Some(vec!["analysts".to_string(), "admins".to_string()])
        );
    }

    #[test]
    fn missing_attributes_yield_none_rather_than_erroring() {
        let assertion = assertion_with_attributes(vec![]);
        let (first, last, groups) = extract_saml_attributes(&assertion);
        assert_eq!(first, None);
        assert_eq!(last, None);
        assert_eq!(groups, None);
    }

    const METADATA_WITH_SIGNING_CERT: &str =
        include_str!("../../../tests/fixtures/saml_idp_metadata.xml");

    const METADATA_WITHOUT_SIGNING_CERT: &str = r#"
<md:EntityDescriptor entityID="https://sso.example.org/idp"
    xmlns:md="urn:oasis:names:tc:SAML:2.0:metadata">
    <md:IDPSSODescriptor protocolSupportEnumeration="urn:oasis:names:tc:SAML:2.0:protocol">
        <md:SingleSignOnService Binding="urn:oasis:names:tc:SAML:2.0:bindings:HTTP-Redirect" Location="https://idp.example.org/SAML2/SSO/Redirect"/>
    </md:IDPSSODescriptor>
</md:EntityDescriptor>
"#;

    #[test]
    fn rejects_idp_metadata_with_no_signing_cert() {
        let result = build_service_provider_from_metadata(
            METADATA_WITHOUT_SIGNING_CERT,
            "https://sp.example.org".to_string(),
            "https://sp.example.org/api/saml/acs".to_string(),
        );
        assert!(
            result.is_err(),
            "metadata with no signing cert must be rejected"
        );
    }

    #[test]
    fn accepts_idp_metadata_with_a_signing_cert() {
        let result = build_service_provider_from_metadata(
            METADATA_WITH_SIGNING_CERT,
            "https://sp.example.org".to_string(),
            "https://sp.example.org/api/saml/acs".to_string(),
        );
        assert!(
            result.is_ok(),
            "metadata with a signing cert must be accepted"
        );
    }
}
