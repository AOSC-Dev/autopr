mod worker;

use std::{
    env::current_dir,
    path::{Path, PathBuf},
    sync::Arc,
};

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
use worker::{find_old_pr, find_update_and_update_checksum, old_prs_100, open_pr, OpenPRRequest};

#[derive(Clone)]
struct AppState {
    update_list: PathBuf,
    client: Client,
    github_client: Arc<Octocrab>,
    bot_name: Arc<String>,
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
    let update_list = current_dir()?.join("update_list");
    let bot_name = Arc::new(std::env::var("bot_name")?);

    let client = ClientBuilder::new().user_agent("autopr").build()?;
    let github_client = Arc::new(
        Octocrab::builder()
            .user_access_token(github_token)
            .build()?,
    );

    let app = Router::new()
        .route("/", post(handler))
        .with_state(AppState {
            update_list,
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
    info!("Github webhook got message.");

    let json: Webhook = serde_json::from_value(json)?;

    let pusher_name = json.pusher.and_then(|x| x.name);
    let clone_url = json.repository.and_then(|x| x.clone_url);

    if pusher_name.as_ref() == Some(&state.bot_name) {
        info!("Ignoring webhook from self");
        return Ok(());
    }

    if clone_url != Some(state.repo_url) {
        info!("Ignoring webhook from wrong repo");
        return Ok(());
    }

    let old_pr = old_prs_100(state.github_client.clone()).await?;

    if old_pr.items.len() == 100 {
        info!("Too manys pull request is open. avoid webhook request");
        return Ok(());
    }

    tokio::spawn(async move {
        let res = fetch_pkgs_updates(
            &state.client,
            state.update_list,
            state.github_client,
            state.bot_name,
        )
        .await;
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
    update_list: PathBuf,
    octoctab: Arc<Octocrab>,
    bot_name: Arc<String>,
) -> Result<()> {
    let lock = ABBS_REPO_LOCK.lock().await;

    let json = client
        .get("https://raw.githubusercontent.com/AOSC-Dev/anicca/main/pkgsupdate.json")
        .send()
        .await?
        .json::<Vec<PkgUpdate>>()
        .await?;

    let update_list = fs::File::open(update_list).await?;
    let mut f_lines = BufReader::new(update_list).lines();
    let mut update_list = vec![];

    while let Some(x) = f_lines.next_line().await? {
        let line = x.trim().to_string();
        update_list.push(line);
    }

    let abbs_path = Path::new("./aosc-os-abbs");

    for i in update_list {
        if i.starts_with("#") {
            continue;
        }

        let pkgs = if i.starts_with("groups/") {
            let mut pkgs = vec![];
            group_pkgs(&mut pkgs, &i, abbs_path).await?;
            pkgs
        } else {
            vec![i]
        };

        for i in pkgs {
            let entry = json.iter().find(|x| x.name == i);
            match entry {
                None => {
                    info!("Package has no update: {}", i);
                    continue;
                }
                Some(x) => {
                    info!("Creating Pull Request: {}", x.name);
                    let octocrab_shared = octoctab.clone();
                    let pr = create_pr(octoctab.clone(), x.name.clone(), x.after.clone(), abbs_path).await;

                    match pr {
                        Ok(Some((num, url))) => {
                            info!("Pull Request created: {}: {}", num, url);
                            match build_pr(client, num).await {
                                Ok(()) => {
                                    info!("PR pipeline is created.");
                                }
                                Err(e) => warn!("Failed to create pr pipeline: {e}"),
                            }
                        }
                        Ok(None) => {
                            warn!("Branch already exists.");
                        }
                        Err(e) => {
                            warn!("Failed to create pr: {e}");
                            let e = e.to_string();
                            if e != "PR exists" {
                                let bot_name = bot_name.clone();
                                tokio::spawn(async move {
                                    if let Err(e) = create_issue(
                                        octocrab_shared,
                                        &e,
                                        &format!(
                                            "autopr: failed to create pull request for package: {}",
                                            i
                                        ),
                                        bot_name,
                                    )
                                    .await
                                    {
                                        warn!("{e}");
                                    }
                                });
                            }
                        }
                    }
                }
            }
        }
    }

    drop(lock);

    Ok(())
}

async fn group_pkgs(pkgs: &mut Vec<String>, group: &str, abbs_path: &Path) -> Result<()> {
    let f = fs::File::open(abbs_path.join(group)).await?;

    let mut lines = BufReader::new(f).lines();

    while let Some(line) = lines.next_line().await? {
        if line.starts_with("groups/") {
            Box::pin(group_pkgs(pkgs, &line, abbs_path)).await?;
        } else {
            pkgs.push(line.trim().to_string());
        }
    }

    Ok(())
}

async fn create_pr(
    client: Arc<Octocrab>,
    pkg: String,
    after: String,
    abbs_path: &Path,
) -> Result<Option<(u64, String)>> {
    let branch = format!("{pkg}-{after}");
    find_old_pr(client.clone(), &branch).await?;

    let path = abbs_path.to_path_buf();
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

    let find_update = find_update_and_update_checksum(pkg, path.clone()).await?;

    if let Some(find_update) = find_update {
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

        Ok(Some(pr))
    } else {
        Ok(None)
    }
}

#[derive(Serialize)]
struct PrRequest {
    pr: u64,
}

async fn build_pr(client: &Client, num: u64) -> Result<()> {
    info!("Creating Pull Request pipeline: {}", num);

    client
        .post(NEW_PR_URL)
        .json(&PrRequest { pr: num })
        .send()
        .await?
        .error_for_status()?;

    Ok(())
}

async fn create_issue(
    octoctab: Arc<Octocrab>,
    body: &str,
    title: &str,
    bot_name: Arc<String>,
) -> Result<u64> {
    info!("Creating issue: {}", title);

    let page = octoctab
        .issues("AOSC-Dev", "aosc-os-abbs")
        .list()
        .per_page(100)
        .creator(&*bot_name)
        // Optional Parameters
        .state(params::State::Open)
        // Send the request
        .send()
        .await?;

    for old_issue in page.items {
        if old_issue.title == title {
            bail!("issue exists");
        }
    }

    let issue = octoctab
        .issues("AOSC-Dev", "aosc-os-abbs")
        .create(title)
        .body(body)
        .send()
        .await?;

    // Add autopr
    octoctab
        .issues("AOSC-Dev", "aosc-os-abbs")
        .add_labels(issue.number, &["autopr".to_string()])
        .await?;

    Ok(issue.number)
}
