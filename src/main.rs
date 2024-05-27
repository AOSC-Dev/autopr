mod worker;

use std::{env::current_dir, path::Path};

use axum::{extract::State, response::IntoResponse, routing::post, Json, Router};
use eyre::{bail, Result};

use octocrab::{params, Octocrab};
use once_cell::sync::Lazy;
use reqwest::{Client, ClientBuilder, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{
    fs,
    io::{AsyncBufReadExt, BufReader},
    process::Command,
};
use tracing::{info, level_filters::LevelFilter, warn};
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer};
use worker::{find_update_and_update_checksum, open_pr, OpenPRRequest};

#[derive(Clone)]
struct AppState {
    lines: Vec<String>,
    client: Client,
    github_client: octocrab::Octocrab,
    bot_name: String,
    repo_url: String,
}

pub static ABBS_REPO_LOCK: Lazy<tokio::sync::Mutex<()>> = Lazy::new(|| tokio::sync::Mutex::new(()));
const NEW_PR_URL: &str = "https://buildit.aosc.io/api/pipeline/new_pr";

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    let env_log = EnvFilter::try_from_default_env();

    if let Ok(filter) = env_log {
        tracing_subscriber::registry()
            .with(
                fmt::layer()
                    .event_format(
                        tracing_subscriber::fmt::format()
                            .with_file(true)
                            .with_line_number(true),
                    )
                    .with_filter(filter),
            )
            .init();
    } else {
        tracing_subscriber::registry()
            .with(
                fmt::layer()
                    .event_format(
                        tracing_subscriber::fmt::format()
                            .with_file(true)
                            .with_line_number(true),
                    )
                    .with_filter(LevelFilter::INFO),
            )
            .init();
    }

    let webhook_uri = std::env::var("autopr_webhook")?;
    // let secret = std::env::var("autopr_secret")?;
    let github_token = std::env::var("github_token")?;
    let update_list = fs::File::open(current_dir()?.join("update_list")).await?;
    let bot_name = std::env::var("bot_name")?;
    let mut f_lines = BufReader::new(update_list).lines();
    let mut lines = vec![];

    while let Some(x) = f_lines.next_line().await? {
        let line = x.trim().to_string();
        lines.push(line);
    }

    let client = ClientBuilder::new().user_agent("autopr").build()?;
    let github_client = octocrab::Octocrab::builder()
        .user_access_token(github_token)
        .build()?;

    let app = Router::new()
        .route("/", post(handler))
        .with_state(AppState {
            lines,
            client,
            github_client,
            bot_name,
            repo_url: std::env::var("repo_url")?,
        });

    let listener = tokio::net::TcpListener::bind(webhook_uri).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

struct EyreError {
    err: eyre::Error,
}

impl IntoResponse for EyreError {
    fn into_response(self) -> axum::response::Response {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Something went wrong: {}", self.err),
        )
            .into_response()
    }
}

impl<E> From<E> for EyreError
where
    E: Into<eyre::Error>,
{
    fn from(err: E) -> Self {
        EyreError { err: err.into() }
    }
}

#[derive(Deserialize, Debug)]
struct Webhook {
    pusher: Option<Pusher>,
    repository: Option<WebhookRepo>,
}

#[derive(Deserialize, Debug)]
struct Pusher {
    name: Option<String>,
    _email: Option<String>,
}

#[derive(Deserialize, Debug)]
struct WebhookRepo {
    clone_url: Option<String>,
}

async fn handler(State(state): State<AppState>, Json(json): Json<Value>) -> Result<(), EyreError> {
    info!("Github webhook got message: {json:#?}");

    let json: Webhook = serde_json::from_value(json)?;

    let pusher_name = json.pusher.and_then(|x| x.name);
    let clone_url = json.repository.and_then(|x| x.clone_url);

    if pusher_name == Some(state.bot_name) {
        info!("Ignoring webhook from self");
        return Ok(());
    }

    if clone_url != Some(state.repo_url) {
        info!("Ignoring webhook from wrong repo");
        return Ok(());
    }

    tokio::spawn(async move {
        let res = fetch_pkgs_updates(&state.client, state.lines, &state.github_client).await;
        info!("{res:?}");
    });

    Ok(())
}

#[derive(Deserialize)]
struct PkgUpdate {
    name: String,
    after: String,
}

async fn fetch_pkgs_updates(
    client: &Client,
    lines: Vec<String>,
    octoctab: &Octocrab,
) -> Result<()> {
    let lock = ABBS_REPO_LOCK.lock().await;

    let json = client
        .get("https://raw.githubusercontent.com/AOSC-Dev/anicca/main/pkgsupdate.json")
        .send()
        .await?
        .json::<Vec<PkgUpdate>>()
        .await?;

    for i in lines {
        let entry = json.iter().find(|x| x.name == i);
        match entry {
            None => {
                info!("Package has no update: {}", i);
                continue;
            }
            Some(x) => {
                info!("Creating Pull Request: {}", x.name);
                let pr = create_pr(octoctab, x.name.clone(), x.after.clone()).await;

                match pr {
                    Ok((num, url)) => {
                        info!("Pull Request created: {}: {}", num, url);
                        match build_pr(client, num).await {
                            Ok(()) => {
                                info!("PR pipeline is created.");
                            }
                            Err(e) => warn!("Failed to create pr pipeline: {e}"),
                        }
                    }
                    Err(e) => warn!("Failed to create pr: {e}"),
                }
            }
        }
    }

    drop(lock);

    Ok(())
}

async fn create_pr(client: &Octocrab, pkg: String, after: String) -> Result<(u64, String)> {
    let path = Path::new("./aosc-os-abbs").to_path_buf();
    if !path.is_dir() {
        Command::new("git")
            .arg("clone")
            .arg("git@github.com:aosc-dev/aosc-os-abbs")
            .output()
            .await?;

        Command::new("git")
            .arg("config")
            .arg("user.email")
            .arg("maintainers@aosc.io")
            .current_dir(&path)
            .output()
            .await?;

        Command::new("git")
            .arg("config")
            .arg("user.name")
            .arg("AOSC Maintainers")
            .current_dir(&path)
            .output()
            .await?;
    }

    let page = client
        .pulls("AOSC-Dev", "aosc-os-abbs")
        .list()
        // Optional Parameters
        .state(params::State::Open)
        .base("stable")
        // Send the request
        .send()
        .await?;

    for old_pr in page.items {
        if old_pr.title == format!("{}: update to {}", pkg, after).into() {
            bail!("PR exists");
        }
    }

    let find_update = find_update_and_update_checksum(pkg, path.clone()).await?;
    let pr = open_pr(
        OpenPRRequest {
            git_ref: find_update.branch,
            abbs_path: path,
            packages: find_update.package,
            title: find_update.title,
            tags: None,
            archs: None,
        },
        client,
    )
    .await?;

    Ok(pr)
}

#[derive(Serialize)]
struct PrRequest {
    id: u64,
}

async fn build_pr(client: &Client, num: u64) -> Result<()> {
    info!("Creating Pull Request pipeline: {}", num);

    client
        .post(NEW_PR_URL)
        .json(&PrRequest { id: num })
        .send()
        .await?
        .error_for_status()?;

    Ok(())
}
