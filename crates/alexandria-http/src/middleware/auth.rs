use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;

pub async fn auth_stub(req: axum::extract::Request, next: Next) -> Result<Response, StatusCode> {
    Ok(next.run(req).await)
}

pub fn reject_unauthenticated() -> StatusCode {
    StatusCode::UNAUTHORIZED
}
