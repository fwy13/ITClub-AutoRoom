use axum::{http::StatusCode, response::{IntoResponse, Response}, Json};
use serde_json::json;

#[derive(Debug)]
pub struct ApiError(pub StatusCode, pub String);
impl ApiError {
    pub fn bad(message: impl Into<String>) -> Self { Self(StatusCode::BAD_REQUEST, message.into()) }
    pub fn unauthorized() -> Self { Self(StatusCode::UNAUTHORIZED, "Please sign in".into()) }
    pub fn conflict(message: impl Into<String>) -> Self { Self(StatusCode::CONFLICT, message.into()) }
    pub fn unavailable(message: impl Into<String>) -> Self { Self(StatusCode::SERVICE_UNAVAILABLE, message.into()) }
    pub fn internal() -> Self { Self(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error".into()) }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response { (self.0, Json(json!({"error":self.1}))).into_response() }
}
impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self { tracing::error!(error=%e, "database error"); Self::internal() }
}
