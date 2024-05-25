use axum::{response::IntoResponse, routing::post, Json, Router};
use eyre::Result;
use serde_json::Value;

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    let webhook_uri = std::env::var("autopr_webhook")?;
    let secret = std::env::var("autopr_secret")?;
    let app = Router::new().route("/", post(handler));

    let listener = tokio::net::TcpListener::bind(webhook_uri).await?;
    axum::serve(listener, app).await?;

    Ok(())
}


async fn handler(Json(json): Json<Value>) -> impl IntoResponse {
    dbg!(json);
}
