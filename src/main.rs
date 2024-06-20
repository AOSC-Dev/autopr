mod worker;

use std::{
    convert::Infallible,
    env::current_dir,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
};

use axum::{
    extract::{connect_info, State},
    response::IntoResponse,
    routing::post,
    serve::IncomingStream,
    Json, Router,
};
use chrono::Local;
use eyre::{bail, Result};

use hyper::{body::Incoming, Request};
use hyper_util::{
    rt::{TokioExecutor, TokioIo},
    server,
};
use octocrab::{params, Octocrab};
use once_cell::sync::Lazy;
use reqwest::{Client, ClientBuilder, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{
    fs,
    io::{AsyncBufReadExt, BufReader},
    net::{unix::UCred, UnixStream},
    process::Command,
};
use tower::Service;
use tracing::{error, info, level_filters::LevelFilter, warn};
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer};
use worker::{
    find_old_pr, find_update_and_update_checksum, git_push, group_find_update, old_prs_100,
    open_pr, OpenPRRequest,
};
use crate::worker::update_abbs;

#[derive(Clone)]
struct AppState {
    update_list: PathBuf,
    client: Client,
    github_client: Arc<Octocrab>,
    bot_name: Arc<String>,
    repo_url: String,
}

// https://github.com/tokio-rs/axum/blob/main/examples/unix-domain-socket/src/main.rs
#[derive(Clone, Debug)]
pub enum RemoteAddr {
    Uds(UdsSocketAddr),
    Inet(SocketAddr),
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct UdsSocketAddr {
    peer_addr: Arc<tokio::net::unix::SocketAddr>,
    peer_cred: UCred,
}

impl connect_info::Connected<&UnixStream> for RemoteAddr {
    fn connect_info(target: &UnixStream) -> Self {
        let peer_addr = target.peer_addr().unwrap();
        let peer_cred = target.peer_cred().unwrap();

        Self::Uds(UdsSocketAddr {
            peer_addr: Arc::new(peer_addr),
            peer_cred,
        })
    }
}

impl<'a> connect_info::Connected<IncomingStream<'a>> for RemoteAddr {
    fn connect_info(target: IncomingStream) -> Self {
        Self::Inet(target.remote_addr())
    }
}

pub static ABBS_REPO_LOCK: Lazy<tokio::sync::Mutex<()>> = Lazy::new(|| tokio::sync::Mutex::new(()));
const NEW_PR_URL: &str = "https://buildit.aosc.io/api/pipeline/new_pr";
const UNIX_SOCKET_PREFIX: &str = "unix:";

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

    if let Some(socket) = webhook_uri.strip_prefix(UNIX_SOCKET_PREFIX) {
        info!("autopr is listening on: {}", &webhook_uri);
        let uds = tokio::net::UnixListener::bind(socket)?;
        let mut make_service = app.into_make_service_with_connect_info::<RemoteAddr>();

        // See https://github.com/tokio-rs/axum/blob/main/examples/serve-with-hyper/src/main.rs for
        // more details about this setup
        let task = tokio::spawn(async move {
            loop {
                let (socket, _remote_addr) = uds.accept().await.unwrap();

                let tower_service = unwrap_infallible(make_service.call(&socket).await);

                let socket = TokioIo::new(socket);

                let hyper_service =
                    hyper::service::service_fn(move |request: Request<Incoming>| {
                        tower_service.clone().call(request)
                    });

                if let Err(err) = server::conn::auto::Builder::new(TokioExecutor::new())
                    .serve_connection_with_upgrades(socket, hyper_service)
                    .await
                {
                    error!("failed to serve connection: {err:#}");
                }
            }
        });

        task.await?;
    } else {
        info!("autopr is listening on: {}", &webhook_uri);
        let listener = tokio::net::TcpListener::bind(webhook_uri).await?;
        axum::serve(listener, app).await?;
    }

    Ok(())
}

fn unwrap_infallible<T>(result: Result<T, Infallible>) -> T {
    match result {
        Ok(value) => value,
        Err(err) => match err {},
    }
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
    warnings: Vec<String>,
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

    for i in update_list {
        if i.starts_with('#') {
            continue;
        }

        if i.starts_with("groups/") {
            let pr = create_pr(octoctab.clone(), i.clone(), None).await;
            let octocrab_shared = octoctab.clone();
            handle_pr(pr, client, bot_name.clone(), octocrab_shared, i).await;
        } else {
            let entry = json.iter().find(|x| x.name == i);
            match entry {
                None => {
                    info!("Package has no update: {}", i);
                    continue;
                }
                Some(x) => {
                    for j in &x.warnings {
                        if j.starts_with("Possible downgrade") {
                            warn!("Possible downgrade: {}, so autopr will ignore it.", i);
                            continue;
                        }
                    }

                    info!("Creating Pull Request: {}", x.name);
                    let octocrab_shared = octoctab.clone();
                    let pr =
                        create_pr(octoctab.clone(), x.name.clone(), Some(x.after.clone())).await;

                    handle_pr(pr, client, bot_name.clone(), octocrab_shared, i.clone()).await;
                }
            }
        }
    }

    drop(lock);

    Ok(())
}

async fn handle_pr(
    pr: Result<Option<(u64, String)>>,
    client: &Client,
    bot_name: Arc<String>,
    octocrab_shared: Arc<Octocrab>,
    name: String,
) {
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
                            name
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

async fn create_pr(
    client: Arc<Octocrab>,
    pkg: String,
    after: Option<String>,
) -> Result<Option<(u64, String)>> {
    let mut is_groups = false;
    let branch = if let Some(v) = pkg.strip_prefix("groups/") {
        is_groups = true;
        format!("{v}-survey-{}", Local::now().format("%Y%m%d"))
    } else {
        format!("{pkg}-{}", after.unwrap())
    };

    find_old_pr(client.clone(), &branch).await?;

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

    let mut head_index = 0usize;
    let find_update = if is_groups {
        let mut list = vec![];
        group_pkgs(&path.join(&pkg), &mut list, &path).await?;
        update_abbs("stable", &path).await?;
        group_find_update(list, path.clone(), &mut head_index, &branch).await
    } else {
        update_abbs("stable", &path).await?;
        vec![find_update_and_update_checksum(pkg, path.clone(), &mut head_index, &branch).await?]
    };

    let find_update = find_update.iter().flatten().collect::<Vec<_>>();

    let mut branch_res = None;
    let mut pkgs = vec![];

    let title = if !is_groups {
        find_update.get(0).map(|x| x.title.clone())
    } else {
        Some(branch.replace("-", " "))
    };

    for i in find_update {
        branch_res = Some(i.branch.clone());
        pkgs.push(i.package.clone());
    }

    if pkgs.is_empty() {
        Ok(None)
    } else {
        let branch = branch_res.unwrap();
        git_push(&path, &branch).await?;
        let pr = open_pr(
            OpenPRRequest {
                git_ref: branch,
                abbs_path: path,
                packages: pkgs.join(","),
                title: title.unwrap(),
                tags: None,
                archs: None,
            },
            client,
        )
        .await?;

        Ok(Some(pr))
    }
}

async fn group_pkgs(p: &Path, list: &mut Vec<String>, abbs_path: &Path) -> Result<()> {
    let s = tokio::fs::read_to_string(p).await?;
    let lines = s.lines();

    for i in lines {
        let line = i.trim().to_string();
        if line.starts_with('#') {
            continue;
        }

        if !i.starts_with("groups/") {
            list.push(line);
        } else {
            Box::pin(group_pkgs(&abbs_path.join(i), list, abbs_path)).await?;
        }
    }

    Ok(())
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
