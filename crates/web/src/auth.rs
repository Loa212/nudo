//! Dashboard authentication: session cookies and CSRF.
//!
//! The web tier is a gRPC client of the control plane for everything the API
//! covers, but sessions are not part of that API — the dashboard talks to the
//! same SQLite store directly for login, because a browser session is a property
//! of the dashboard rather than of the deployment API.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Redirect, Response};
use axum_extra::extract::cookie::{Cookie, SameSite};
use nudo_server::crypto::secure_compare;
use nudo_server::store::{Session, Store};

/// The session cookie's name.
pub const COOKIE_NAME: &str = "nudo_session";

/// How long a session cookie is offered for.
const SESSION_COOKIE_DAYS: i64 = 30;

/// Builds the session cookie.
///
/// `HttpOnly` so JavaScript cannot read it, `SameSite=Lax` so it is not sent on
/// cross-site form posts, and `Secure` whenever the deployment is served over
/// HTTPS — set from the configured base URL rather than guessed per request,
/// since a reverse proxy terminates TLS and the request itself looks plain.
pub fn session_cookie(value: String, secure: bool) -> Cookie<'static> {
    let mut cookie = Cookie::new(COOKIE_NAME, value);
    cookie.set_http_only(true);
    cookie.set_same_site(SameSite::Lax);
    cookie.set_secure(secure);
    cookie.set_path("/");
    cookie.set_max_age(Some(cookie::time::Duration::days(SESSION_COOKIE_DAYS)));
    cookie
}

/// Builds the cookie that clears a session on logout.
pub fn clear_cookie(secure: bool) -> Cookie<'static> {
    let mut cookie = session_cookie(String::new(), secure);
    cookie.set_max_age(Some(cookie::time::Duration::seconds(0)));
    cookie
}

/// Reads the session cookie out of the request headers.
pub fn cookie_value(parts: &Parts) -> Option<String> {
    let header = parts
        .headers
        .get(axum::http::header::COOKIE)?
        .to_str()
        .ok()?;

    // A request can carry several Cookie values, and other cookies may share a
    // prefix with ours, so each pair is parsed rather than string-searched.
    header.split(';').find_map(|pair| {
        let cookie = Cookie::parse(pair.trim()).ok()?;
        if cookie.name() == COOKIE_NAME {
            Some(cookie.value().to_string())
        } else {
            None
        }
    })
}

/// An authenticated dashboard user, extracted from the session cookie.
///
/// Every handler that renders or mutates anything takes this, so a route cannot
/// be exposed by forgetting a check — the type is the check.
#[derive(Debug, Clone)]
pub struct CurrentUser {
    pub id: String,
    pub email: String,
    pub display_name: String,
    /// This session's CSRF token, embedded in every form.
    pub csrf_token: String,
}

impl From<Session> for CurrentUser {
    fn from(session: Session) -> Self {
        Self {
            id: session.user.id,
            email: session.user.email,
            display_name: session.user.display_name,
            csrf_token: session.csrf_token,
        }
    }
}

/// What to do when a request has no valid session.
pub struct RequireLogin;

impl IntoResponse for RequireLogin {
    fn into_response(self) -> Response {
        // A redirect rather than a 401, because these are browser navigations.
        Redirect::to("/login").into_response()
    }
}

impl<S> FromRequestParts<S> for CurrentUser
where
    S: Send + Sync,
    Store: axum::extract::FromRef<S>,
{
    type Rejection = RequireLogin;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        use axum::extract::FromRef;

        let store = Store::from_ref(state);
        let cookie = cookie_value(parts).ok_or(RequireLogin)?;

        let session = store
            .lookup_session(&cookie)
            .await
            .map_err(|error| {
                tracing::error!(%error, "looking up the session failed");
                RequireLogin
            })?
            .ok_or(RequireLogin)?;

        Ok(session.into())
    }
}

/// Verifies a form's CSRF token against the session's.
///
/// Compared in constant time. A same-site cookie already blocks the common
/// cross-site post, but a token is what defends against a same-site injection
/// or a subdomain that can set cookies.
pub fn check_csrf(user: &CurrentUser, submitted: &str) -> Result<(), CsrfRejected> {
    if submitted.is_empty() || !secure_compare(&user.csrf_token, submitted) {
        return Err(CsrfRejected);
    }
    Ok(())
}

/// Returned when a form's CSRF token does not match.
#[derive(Debug)]
pub struct CsrfRejected;

impl IntoResponse for CsrfRejected {
    fn into_response(self) -> Response {
        (
            axum::http::StatusCode::FORBIDDEN,
            "This form has expired or came from another site. Reload the page and try again.",
        )
            .into_response()
    }
}

/// Whether the dashboard is served over HTTPS, which decides the cookie's
/// `Secure` attribute.
pub fn is_https(base_url: &str) -> bool {
    base_url.trim().to_lowercase().starts_with("https://")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;

    fn parts_with_cookies(header: &str) -> Parts {
        let request = Request::builder()
            .header(axum::http::header::COOKIE, header)
            .body(())
            .expect("request");
        request.into_parts().0
    }

    fn user(csrf: &str) -> CurrentUser {
        CurrentUser {
            id: "usr_1".to_string(),
            email: "a@b.com".to_string(),
            display_name: "Alice".to_string(),
            csrf_token: csrf.to_string(),
        }
    }

    #[test]
    fn the_session_cookie_cannot_be_read_by_scripts_or_sent_cross_site() {
        let cookie = session_cookie("value".to_string(), true);
        assert!(cookie.http_only().unwrap_or(false));
        assert_eq!(cookie.same_site(), Some(SameSite::Lax));
        assert!(cookie.secure().unwrap_or(false));
        assert_eq!(cookie.path(), Some("/"));
    }

    #[test]
    fn the_secure_attribute_follows_the_configured_scheme() {
        // A reverse proxy terminates TLS, so the request itself looks plain and
        // cannot be used to decide this.
        assert!(is_https("https://nudo.example.com"));
        assert!(is_https("HTTPS://NUDO.EXAMPLE.COM"));
        assert!(!is_https("http://localhost:3000"));

        assert!(
            !session_cookie("v".to_string(), false)
                .secure()
                .unwrap_or(true)
        );
    }

    #[test]
    fn the_clearing_cookie_expires_immediately() {
        let cookie = clear_cookie(true);
        assert_eq!(cookie.name(), COOKIE_NAME);
        assert!(cookie.value().is_empty());
        assert_eq!(cookie.max_age(), Some(cookie::time::Duration::seconds(0)));
    }

    #[test]
    fn the_session_cookie_is_found_among_others() {
        let parts = parts_with_cookies("theme=dark; nudo_session=abc123; other=x");
        assert_eq!(cookie_value(&parts), Some("abc123".to_string()));
    }

    #[test]
    fn a_cookie_whose_name_merely_starts_the_same_is_not_mistaken_for_ours() {
        // A string search for "nudo_session" would match this.
        let parts = parts_with_cookies("nudo_session_backup=wrong");
        assert_eq!(cookie_value(&parts), None);
    }

    #[test]
    fn a_request_with_no_cookies_yields_nothing() {
        let request = Request::builder().body(()).expect("request");
        let parts = request.into_parts().0;
        assert_eq!(cookie_value(&parts), None);
    }

    #[test]
    fn a_matching_csrf_token_is_accepted_and_anything_else_is_not() {
        let user = user("token-value");
        assert!(check_csrf(&user, "token-value").is_ok());

        // A wrong, empty, truncated or padded token must all fail.
        assert!(check_csrf(&user, "other-value").is_err());
        assert!(check_csrf(&user, "").is_err());
        assert!(check_csrf(&user, "token-valu").is_err());
        assert!(check_csrf(&user, "token-value ").is_err());
    }

    #[test]
    fn a_session_with_an_empty_csrf_token_still_rejects_an_empty_submission() {
        // Otherwise a corrupt session row would accept every form.
        assert!(check_csrf(&user(""), "").is_err());
    }

    #[test]
    fn a_csrf_rejection_is_a_forbidden_with_an_actionable_message() {
        let response = CsrfRejected.into_response();
        assert_eq!(response.status(), axum::http::StatusCode::FORBIDDEN);
    }

    #[test]
    fn an_unauthenticated_request_is_redirected_to_the_login_page() {
        let response = RequireLogin.into_response();
        assert!(response.status().is_redirection());
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::LOCATION)
                .and_then(|v| v.to_str().ok()),
            Some("/login")
        );
    }

    #[test]
    fn a_session_maps_onto_the_current_user_including_its_csrf_token() {
        let session = Session {
            user: nudo_server::store::User {
                id: "usr_9".to_string(),
                email: "b@c.com".to_string(),
                display_name: "Bob".to_string(),
                created_at: chrono::Utc::now(),
            },
            csrf_token: "csrf-abc".to_string(),
        };

        let current: CurrentUser = session.into();
        assert_eq!(current.id, "usr_9");
        assert_eq!(current.email, "b@c.com");
        assert_eq!(current.csrf_token, "csrf-abc");
    }
}
