//! The one mapping from `Error` to a response.
//!
//! A failing handler returns JSON (`{"error": "..."}`) with the right status. The outermost layer,
//! `pages`, turns that into an HTML page for every route that is not under `/api/`; the path
//! decides, never the `Accept` header. Not being signed in is a redirect to the sign-in form on a
//! page and a 401 under `/api/`.
use askama::Template;
use axum::Json;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};

use crate::Error;

impl Error {
    pub(crate) fn status(&self) -> StatusCode {
        match self {
            Error::Validation(_) => StatusCode::UNPROCESSABLE_ENTITY,
            Error::NotFound(_) => StatusCode::NOT_FOUND,
            Error::Unauthorized => StatusCode::UNAUTHORIZED,
            Error::Forbidden(_) => StatusCode::FORBIDDEN,
            Error::TooMany(_) => StatusCode::TOO_MANY_REQUESTS,
            Error::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            Error::Db(_) | Error::Migrate(_) | Error::Json(_) | Error::Io(_) | Error::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// What the person may be told. Internal failures are logged here and reduced to "internal error".
    pub(crate) fn public_message(&self) -> String {
        if self.status() == StatusCode::INTERNAL_SERVER_ERROR {
            eprintln!("mealier: {self}");
            return "internal error".into();
        }
        self.to_string()
    }
}

/// Put on the response so `pages` can rebuild it as a page.
#[derive(Clone)]
struct Failure {
    status: StatusCode,
    message: String,
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let failure = Failure { status: self.status(), message: self.public_message() };
        let mut resp = (failure.status, Json(serde_json::json!({ "error": failure.message }))).into_response();
        resp.extensions_mut().insert(failure);
        resp
    }
}

#[derive(Template)]
#[template(path = "error.html")]
struct ErrorPage {
    status: u16,
    reason: &'static str,
    message: String,
}

/// Outermost layer: an error on a page is a page.
pub(crate) async fn pages(req: Request, next: Next) -> Response {
    let api = req.uri().path().starts_with("/api/");
    let resp = next.run(req).await;
    if api {
        return resp;
    }
    let Some(Failure { status, message }) = resp.extensions().get::<Failure>().cloned() else { return resp };
    if status == StatusCode::UNAUTHORIZED {
        return Redirect::to("/login").into_response();
    }
    let page = ErrorPage { status: status.as_u16(), reason: status.canonical_reason().unwrap_or(""), message };
    match page.render() {
        Ok(html) => (status, axum::response::Html(html)).into_response(),
        // The page itself failed to render: keep the JSON body the handler gave.
        Err(e) => {
            eprintln!("mealier: error page: {e}");
            resp
        }
    }
}
