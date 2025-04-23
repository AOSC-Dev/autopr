mod worker;

use std::{
    env::current_dir,
    net::SocketAddr,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use axum::{
    Json, Router,
    extract::{State, connect_info},
    response::IntoResponse,
    routing::post,
    serve::IncomingStream,
};
use chrono::Local;
use eyre::{Result, bail};
use rand::rng;

use crate::worker::update_abbs;
use octocrab::{Octocrab, params};
use once_cell::sync::Lazy;
use rand::prelude::SliceRandom;
use reqwest::{Client, ClientBuilder, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{
    fs,
    io::{AsyncBufReadExt, BufReader},
    net::{TcpListener, UnixListener, unix::UCred},
    process::Command,
};
use tracing::{info, level_filters::LevelFilter, warn};
use tracing_subscriber::{EnvFilter, Layer, fmt, layer::SubscriberExt, util::SubscriberInitExt};
use worker::{OpenPRRequest, find_old_pr, find_update, git_push, old_prs_100, open_pr};

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

impl connect_info::Connected<IncomingStream<'_, UnixListener>> for RemoteAddr {
    fn connect_info(stream: IncomingStream<'_, UnixListener>) -> Self {
        let peer_addr = stream.io().peer_addr().unwrap();
        let peer_cred = stream.io().peer_cred().unwrap();

        RemoteAddr::Uds(UdsSocketAddr {
            peer_addr: Arc::new(peer_addr),
            peer_cred,
        })
    }
}

impl connect_info::Connected<IncomingStream<'_, TcpListener>> for RemoteAddr {
    fn connect_info(stream: IncomingStream<'_, TcpListener>) -> Self {
        let peer_addr = stream.io().peer_addr().unwrap();
        RemoteAddr::Inet(peer_addr)
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

    if let Some(path) = webhook_uri.strip_prefix(UNIX_SOCKET_PREFIX) {
        let path = Path::new(path);
        info!("Listening on unix socket {}", path.display());
        // remove old unix socket to avoid "Already already in use" error
        if path.exists() {
            std::fs::remove_file(path)?;
        }

        let listener = tokio::net::UnixListener::bind(path)?;

        // chmod 777
        let mut perms = std::fs::metadata(path)?.permissions();
        perms.set_mode(0o777);
        std::fs::set_permissions(path, perms)?;

        // https://github.com/tokio-rs/axum/blob/main/examples/unix-domain-socket/src/main.rs
        let make_service = app.into_make_service_with_connect_info::<RemoteAddr>();
        axum::serve(listener, make_service).await.unwrap();
    } else {
        info!("autopr is listening on: {}", &webhook_uri);
        let listener = tokio::net::TcpListener::bind(webhook_uri).await?;
        axum::serve(listener, app).await?;
    }

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
    let exist_pr = Arc::new(AtomicUsize::new(old_pr.items.len()));

    tokio::spawn(async move {
        let res = fetch_pkgs_updates(
            &state.client,
            state.update_list,
            state.github_client,
            state.bot_name,
            Path::new("./aosc-os-abbs"),
            exist_pr.clone(),
        )
        .await;
        info!("{res:?}");
    });

    Ok(())
}

#[derive(Deserialize, Clone)]
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
    path: &Path,
    exist_pr: Arc<AtomicUsize>,
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

    random_update_list(&mut update_list);

    let success_open_pr_count = exist_pr;

    let mut avoid_unnecessary_update = false;

    for i in update_list {
        if i.starts_with('#') {
            continue;
        }

        if success_open_pr_count.load(Ordering::SeqCst) >= 50 {
            info!("Too manys pull request is open. avoid unnecessary find update request");
            avoid_unnecessary_update = true;
        }

        if avoid_unnecessary_update && !i.starts_with("!") {
            continue;
        }

        let i = i.strip_prefix("!").unwrap_or(&i).to_string();
        let entry = get_update_branch_by_entry(&json, &i, path).await;

        if let Some(entry) = entry {
            info!("Creating Pull Request: {}", entry.title);
            let octocrab_shared = octoctab.clone();
            let pr = create_pr(octoctab.clone(), entry).await;

            handle_pr(
                pr,
                client,
                bot_name.clone(),
                octocrab_shared,
                i,
                success_open_pr_count.clone(),
            )
            .await;
        }
    }

    drop(lock);

    Ok(())
}

fn random_update_list(list: &mut [String]) {
    let mut rng = rng();
    list.shuffle(&mut rng);
}

pub struct UpdateEntry {
    pub pkgs: Vec<String>,
    pub branch: String,
    title: String,
}

async fn get_update_branch_by_entry(
    json: &[PkgUpdate],
    entry: &str,
    path: &Path,
) -> Option<UpdateEntry> {
    if let Some(i) = entry.strip_prefix("groups/") {
        let mut list = vec![];
        let _ = group_pkgs(&path.join(entry), &mut list, path).await;

        let list = list
            .iter()
            .filter_map(|x| x.split('/').next_back())
            .filter(|x| json.iter().any(|y| y.name == *x && !contains_downgrade(y)))
            .map(|x| x.to_string())
            .collect::<Vec<_>>();

        if list.is_empty() {
            return None;
        }

        let date = Local::now().format("%Y%m%d");

        Some(UpdateEntry {
            pkgs: list,
            branch: format!("{}-survey-{}", i, date),
            title: format!("{} Survey {}", i, date),
        })
    } else {
        let pkg = json.iter().find(|x| x.name == entry)?;

        if contains_downgrade(pkg) {
            return None;
        }

        let version = &pkg.after;
        let branch = format!("{}-{}", entry, version);
        let title = format!("{}: update to {}", entry, version);

        Some(UpdateEntry {
            pkgs: vec![entry.to_string()],
            branch,
            title,
        })
    }
}

fn contains_downgrade(update_entry: &PkgUpdate) -> bool {
    update_entry
        .warnings
        .iter()
        .any(|x| x.starts_with("Possible downgrade"))
}

async fn handle_pr(
    pr: Result<Option<(u64, String)>>,
    client: &Client,
    bot_name: Arc<String>,
    octocrab_shared: Arc<Octocrab>,
    name: String,
    pr_len: Arc<AtomicUsize>,
) {
    match pr {
        Ok(Some((num, url))) => {
            info!("Pull Request created: {}: {}", num, url);
            pr_len.fetch_add(1, Ordering::SeqCst);
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

async fn create_pr(client: Arc<Octocrab>, entry: UpdateEntry) -> Result<Option<(u64, String)>> {
    find_old_pr(client.clone(), &entry.branch).await?;

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

    let mut head_count = 0usize;
    update_abbs("stable", &path).await?;
    let find_update = find_update(&entry, path.clone(), &mut head_count).await?;

    if find_update.is_empty() {
        return Ok(None);
    }

    git_push(&path, &entry.branch).await?;
    let pr = open_pr(
        OpenPRRequest {
            git_ref: entry.branch,
            abbs_path: path,
            packages: find_update.join(","),
            title: entry.title,
            tags: None,
            archs: None,
        },
        client,
    )
    .await?;

    Ok(Some(pr))
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
