use abbs_update_checksum_core::{get_new_spec, ParseErrors};
use eyre::{bail, eyre, Context, OptionExt, Result};
use fancy_regex::Regex;
use gix::{
    prelude::ObjectIdExt,
    sec::{self, trust::DefaultForLevel},
    Repository, ThreadSafeRepository,
};
use octocrab::{models::pulls::PullRequest, params, Octocrab, Page};
use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    io::{BufRead as StdBufRead, BufReader as StdBufReader},
    path::{Path, PathBuf},
    process::Output,
    sync::Arc,
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{self, Command},
    task::spawn_blocking,
};
use tracing::{debug, error, info, warn};
use walkdir::WalkDir;

use crate::UpdateEntry;

macro_rules! PR {
    () => {
        "Topic Description\n-----------------\n\n{}\n\nPackage(s) Affected\n-------------------\n\n{}\n\nSecurity Update?\n----------------\n\nNo\n\nBuild Order\n-----------\n\n```\n{}\n```\n\nTest Build(s) Done\n------------------\n\n{}"
    };
}

pub async fn find_update(
    entry: &UpdateEntry,
    abbs_path: PathBuf,
    head_count: &mut usize,
) -> Vec<String> {
    let mut v = vec![];
    for pkg in &entry.pkgs {
        let res = find_update_and_update_checksum(
            pkg.to_owned(),
            abbs_path.clone(),
            head_count,
            &entry.branch,
        )
        .await;
        match res {
            Ok(Some(s)) => v.push(s),
            Ok(None) => warn!("{} has no update", pkg),
            Err(e) => error!("{:?}", e),
        }
    }

    v
}

async fn find_update_and_update_checksum(
    pkg: String,
    abbs_path: PathBuf,
    head_count: &mut usize,
    branch: &str,
) -> Result<Option<String>> {
    info!("Running aosc-findupdate ...");

    let output = Command::new("aosc-findupdate")
        .arg("-c")
        .arg("-i")
        .arg(format!(".*/{pkg}$"))
        .current_dir(&abbs_path)
        .output()
        .await?;

    print_stdout_and_stderr(&output);

    let status = Command::new("git")
        .arg("status")
        .arg("--porcelain")
        .current_dir(&abbs_path)
        .output()
        .await?;

    let status = BufReader::new(&*status.stdout).lines().next_line().await;

    if let Ok(Some(status)) = status {
        let split_status = status.trim().split_once(' ');
        if let Some((status_porcelain, _)) = split_status {
            if status_porcelain != "M" {
                warn!("git status: {}", status_porcelain);
                warn!("BUG: git status error, so ignore this update");
                return Ok(None);
            }

            let absolute_abbs_path = std::fs::canonicalize(&abbs_path)?;

            let pkgc = pkg.clone();
            let pkgcc = pkg.clone();

            info!("Writting new checksum ...");
            let res = write_new_spec(absolute_abbs_path, pkgc).await;

            if let Err(e) = res {
                git_reset(&abbs_path, *head_count).await?;
                bail!("Failed to run acbs-build to update checksum: {}", e);
            }

            let abbs_path_c = abbs_path.clone();

            let ver = spawn_blocking(move || {
                find_version_by_packages(&[pkgcc], &abbs_path_c)
                    .into_iter()
                    .next()
            })
            .await?;

            let mut ver = ver
                .ok_or_eyre(format!("Failed to find pkg version: {}", pkg))?
                .1;

            // skip epoch
            if let Some((_prefix, suffix)) = ver.split_once(':') {
                ver = suffix.to_string();
            }

            let branches = Command::new("git").arg("branch").output().await?;

            let mut branches_stdout = BufReader::new(&*branches.stdout).lines();
            let mut branches_stderr = BufReader::new(&*branches.stderr).lines();

            while let Ok(Some(line)) = branches_stdout.next_line().await {
                debug!("Exist branch: {line}");
                if line.contains(branch) {
                    git_reset(&abbs_path, *head_count).await?;
                    return Ok(None);
                }
            }

            while let Ok(Some(line)) = branches_stderr.next_line().await {
                debug!("Exist branch: {line}");
                if line.contains(branch) {
                    git_reset(&abbs_path, *head_count).await?;
                    return Ok(None);
                }
            }

            let title = format!("{pkg}: update to {ver}");

            Command::new("git")
                .arg("branch")
                .arg("-f")
                .arg(branch)
                .arg("stable")
                .current_dir(&abbs_path)
                .output()
                .await?;

            Command::new("git")
                .arg("checkout")
                .arg(branch)
                .current_dir(&abbs_path)
                .output()
                .await?;

            Command::new("git")
                .arg("add")
                .arg(".")
                .current_dir(&abbs_path)
                .output()
                .await?;

            Command::new("git")
                .arg("commit")
                .arg("-m")
                .arg(&title)
                .current_dir(&abbs_path)
                .output()
                .await?;

            *head_count += 1;

            return Ok(Some(pkg.to_string()));
        }
    }

    warn!("{pkg} has no update");

    Ok(None)
}

pub async fn git_push(abbs_path: &Path, branch: &str) -> Result<()> {
    Command::new("git")
        .arg("push")
        .arg("--set-upstream")
        .arg("origin")
        .arg(branch)
        .current_dir(abbs_path)
        .output()
        .await?;

    Ok(())
}

async fn git_reset(abbs_path: &Path, head_count: usize) -> Result<()> {
    Command::new("git")
        .arg("reset")
        .arg(format!("HEAD{}", "^".repeat(head_count)))
        .arg("--hard")
        .current_dir(abbs_path)
        .output()
        .await?;

    Ok(())
}

async fn write_new_spec(abbs_path: PathBuf, pkg: String) -> Result<()> {
    let pkg_shared = pkg.clone();
    let abbs_path_shared = abbs_path.clone();
    let (mut spec, p) = spawn_blocking(move || get_spec(&abbs_path_shared, &pkg_shared)).await??;

    for i in 1..=5 {
        if i > 1 {
            info!("({i}/5) Retrying to get new spec...");
        }
        match get_new_spec(&mut spec).await {
            Ok(()) => {
                tokio::fs::write(p, spec).await?;
                return Ok(());
            }
            Err(e) => {
                if let Some(e) = e.downcast_ref::<ParseErrors>() {
                    warn!("{e}, try use acbs-build fallback to get new checksum ...");
                    acbs_build_gw(&pkg, &abbs_path).await?;
                } else {
                    error!("Failed to get new spec: {e}");
                    if i == 5 {
                        bail!("{e}");
                    }
                }
            }
        }
    }

    Ok(())
}

pub fn get_spec(path: &Path, pkgname: &str) -> Result<(String, PathBuf)> {
    let mut spec = None;
    for_each_abbs(path, |pkg, p| {
        if pkgname == pkg {
            let p = p.join("spec");
            spec = std::fs::read_to_string(&p).ok().map(|x| (x, p));
        }
    });

    spec.ok_or_eyre(format!("{pkgname} does not exist"))
}

async fn acbs_build_gw(pkg_shared: &str, abbs_path_shared: &Path) -> Result<()> {
    let output = Command::new("acbs-build")
        .arg("-gw")
        .arg(pkg_shared)
        .arg("--log-dir")
        .arg(&abbs_path_shared.join("acbs-log"))
        .arg("--cache-dir")
        .arg(&abbs_path_shared.join("acbs-cache"))
        .arg("--temp-dir")
        .arg(&abbs_path_shared.join("acbs-temp"))
        .arg("--tree-dir")
        .arg(abbs_path_shared)
        .current_dir(abbs_path_shared)
        .output()
        .await?;

    print_stdout_and_stderr(&output);

    if !output.status.success() {
        bail!(
            "Failed to run acbs-build to update checksum: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(())
}

pub async fn update_abbs<P: AsRef<Path>>(git_ref: &str, abbs_path: P) -> Result<()> {
    info!("Running git checkout -b stable ...");

    let abbs_path = abbs_path.as_ref();

    let output = process::Command::new("git")
        .arg("checkout")
        .arg("-b")
        .arg("stable")
        .current_dir(abbs_path)
        .output()
        .await?;

    print_stdout_and_stderr(&output);

    info!("Running git checkout stable ...");

    let output = process::Command::new("git")
        .arg("checkout")
        .arg("stable")
        .current_dir(abbs_path)
        .output()
        .await?;

    print_stdout_and_stderr(&output);

    info!("Running git pull ...");

    let output = process::Command::new("git")
        .arg("pull")
        .current_dir(abbs_path)
        .output()
        .await?;

    print_stdout_and_stderr(&output);

    info!("Running git fetch ...");

    let output = process::Command::new("git")
        .arg("fetch")
        .arg("origin")
        .arg("--prune")
        .arg("--tags")
        .arg("--force")
        .current_dir(abbs_path)
        .output()
        .await?;

    if !output.status.success() {
        bail!("Failed to fetch origin git-ref: {git_ref}");
    }

    let output = process::Command::new("git")
        .arg("fetch")
        .arg("origin")
        .arg(git_ref)
        .current_dir(abbs_path)
        .output()
        .await?;

    print_stdout_and_stderr(&output);

    if !output.status.success() {
        bail!("Failed to fetch origin git-ref: {git_ref}");
    }

    info!("Running git checkout -b {git_ref} ...");

    let output = process::Command::new("git")
        .arg("checkout")
        .arg("-b")
        .arg(git_ref)
        .current_dir(abbs_path)
        .output()
        .await?;

    print_stdout_and_stderr(&output);

    info!("Running git checkout {git_ref} ...");

    let output = process::Command::new("git")
        .arg("checkout")
        .arg(git_ref)
        .current_dir(abbs_path)
        .output()
        .await?;

    print_stdout_and_stderr(&output);

    if !output.status.success() {
        bail!("Failed to checkout {git_ref}");
    }

    info!("Running git reset FETCH_HEAD --hard ...");

    let output = process::Command::new("git")
        .args(["reset", "FETCH_HEAD", "--hard"])
        .current_dir(abbs_path)
        .output()
        .await?;

    print_stdout_and_stderr(&output);

    if !output.status.success() {
        bail!("Failed to checkout {git_ref}");
    }

    Ok(())
}

pub fn print_stdout_and_stderr(output: &Output) {
    info!("Output:");
    info!("  Stdout:");
    info!(" {}", String::from_utf8_lossy(&output.stdout));
    info!("  Stderr:");
    info!(" {}", String::from_utf8_lossy(&output.stderr));
}

pub fn find_version_by_packages(pkgs: &[String], p: &Path) -> Vec<(String, String)> {
    let mut res = vec![];

    for_each_abbs(p, |pkg, path| {
        if !pkgs.contains(&pkg.to_string()) {
            return;
        }

        let spec = path.join("spec");
        let spec = std::fs::read_to_string(spec);
        let defines_list = locate_defines(path);

        if let Ok(spec) = spec {
            let spec = read_ab_with_apml(&spec);
            let ver = spec.get("VER");
            let rel = spec.get("REL");
            if ver.is_none() {
                warn!("{pkg} has no VER variable");
                return;
            }

            for i in defines_list {
                if let Ok(defines) = std::fs::read_to_string(i) {
                    let defines = read_ab_with_apml(&defines);

                    if let Some(pkgname) = defines.get("PKGNAME") {
                        let epoch = defines.get("PKGEPOCH");

                        let mut final_version = String::new();
                        if let Some(epoch) = epoch {
                            final_version.push_str(&format!("{epoch}:"));
                        }

                        final_version.push_str(ver.unwrap());

                        if let Some(rel) = rel {
                            final_version.push_str(&format!("-{rel}"));
                        }

                        res.push((pkgname.clone(), final_version));
                    } else {
                        warn!("{pkg} has no PKGNAME variable");
                    }
                }
            }
        }
    });

    res.sort();

    res
}

pub fn for_each_abbs<F: FnMut(&str, &Path)>(path: &Path, mut f: F) {
    for i in WalkDir::new(path)
        .max_depth(2)
        .min_depth(2)
        .into_iter()
        .flatten()
    {
        if i.path().is_file() {
            continue;
        }

        let pkg = i.file_name().to_str();

        if pkg.is_none() {
            debug!("Failed to convert str: {}", i.path().display());
            continue;
        }

        let pkg = pkg.unwrap();

        f(pkg, i.path());
    }
}

pub fn locate_defines(path: &Path) -> Vec<PathBuf> {
    if path.join("autobuild").exists() {
        vec![path.join("autobuild").join("defines")]
    } else {
        // handle split packages

        let mut defines_list = vec![];
        for i in WalkDir::new(path)
            .max_depth(1)
            .min_depth(1)
            .into_iter()
            .flatten()
        {
            if !i.path().is_dir() {
                continue;
            }
            let defines_path = i.path().join("defines");
            if defines_path.exists() {
                defines_list.push(defines_path);
            }
        }

        defines_list
    }
}

pub fn read_ab_with_apml(file: &str) -> HashMap<String, String> {
    let mut context = HashMap::new();

    // Try to set some ab3 flags to reduce the chance of returning errors
    for i in ["ARCH", "PKGDIR", "SRCDIR"] {
        context.insert(i.to_string(), "".to_string());
    }

    match abbs_meta_apml::parse(file, &mut context) {
        Ok(()) => (),
        Err(e) => {
            error!("{e:?}, buildit will use fallback method to parse file");
            for line in file.split('\n') {
                let stmt = line.split_once('=');
                if let Some((name, value)) = stmt {
                    context.insert(name.to_string(), value.replace('\"', ""));
                }
            }
        }
    };

    context
}

struct OpenPR<'a> {
    title: &'a str,
    head: &'a str,
    packages: &'a str,
    desc: &'a str,
    pkg_affected: &'a [String],
    tags: Option<&'a [String]>,
    archs: &'a [&'a str],
}

#[derive(Debug)]
pub struct OpenPRRequest<'a> {
    pub git_ref: String,
    pub abbs_path: PathBuf,
    pub packages: String,
    pub title: String,
    pub tags: Option<Vec<String>>,
    /// If None, automatically deduced via `get_archs()`
    pub archs: Option<Vec<&'a str>>,
}

pub async fn open_pr(
    openpr_request: OpenPRRequest<'_>,
    client: Arc<Octocrab>,
) -> Result<(u64, String)> {
    let OpenPRRequest {
        git_ref,
        abbs_path,
        packages,
        title,
        tags,
        archs,
    } = openpr_request;

    update_abbs(&git_ref, &abbs_path).await?;

    let abbs_path_clone = abbs_path.clone();
    let commits = spawn_blocking(move || get_commits(&abbs_path_clone)).await??;
    let commits = spawn_blocking(move || handle_commits(&commits)).await??;
    let pkgs = packages
        .split(',')
        .map(|x| x.to_string())
        .collect::<Vec<_>>();

    // handle modifiers and groups
    let resolved_pkgs = resolve_packages(&pkgs, &abbs_path)?;

    // deduce archs if not specified
    let archs = match archs {
        Some(archs) => archs,
        None => {
            let resolved_pkgs_clone = resolved_pkgs.clone();
            let abbs_path_clone = abbs_path.clone();
            spawn_blocking(move || get_archs(&abbs_path_clone, &resolved_pkgs_clone)).await?
        }
    };

    let abbs_path_clone = abbs_path.clone();
    let pkg_affected =
        spawn_blocking(move || find_version_by_packages_list(&resolved_pkgs, &abbs_path_clone))
            .await?;

    let pr = open_pr_inner(
        OpenPR {
            title: &title,
            head: &git_ref,
            packages: &packages,
            desc: &commits,
            pkg_affected: &pkg_affected,
            tags: tags.as_deref(),
            archs: &archs,
        },
        client,
    )
    .await?;

    Ok((
        pr.number,
        pr.html_url.map(|x| x.to_string()).unwrap_or_else(|| pr.url),
    ))
}

fn find_version_by_packages_list(pkgs: &[String], p: &Path) -> Vec<String> {
    let mut res = vec![];

    for (name, version) in find_version_by_packages(pkgs, p) {
        res.push(format!("- {name}: {version}"));
    }

    res
}

/// Compute new commits on top of stable
fn get_commits(path: &Path) -> Result<Vec<Commit>> {
    let mut res = vec![];
    let repo = get_repo(path)?;
    let commits = repo
        .head()?
        .try_into_peeled_id()?
        .ok_or_eyre("Failed to get peeled id")?
        .ancestors()
        .all()?;

    let refrences = repo.references()?;
    let stable_branch = refrences
        .local_branches()?
        .filter_map(Result::ok)
        .find(|x| x.name().shorten() == "stable")
        .ok_or(eyre!("failed to get stable branch"))?;

    // Collect commits on stable branch
    let commits_on_stable = stable_branch
        .into_fully_peeled_id()?
        .object()?
        .into_commit()
        .ancestors()
        .all()?;

    let mut commits_on_stable_set = HashSet::new();
    for i in commits_on_stable {
        let id = i?.id;
        commits_on_stable_set.insert(id);
    }

    // Collect commits on new branch, but not on stable branch
    // Mimic git log stable..HEAD
    for i in commits {
        let id = i?.id;
        if commits_on_stable_set.contains(&id) {
            continue;
        }

        let o = id.attach(&repo).object()?;
        let commit = o.into_commit();
        let commit_str = commit.id.to_string();

        let msg = commit.message()?;

        res.push(Commit {
            _id: commit_str,
            msg: (msg.title.to_string(), msg.body.map(|x| x.to_string())),
        })
    }

    Ok(res)
}

pub const COMMITS_COUNT_LIMIT: usize = 10;

/// Describe new commits for pull request
fn handle_commits(commits: &[Commit]) -> Result<String> {
    let mut s = String::new();
    for (i, c) in commits.iter().enumerate() {
        if i == COMMITS_COUNT_LIMIT {
            let more = commits.len() - COMMITS_COUNT_LIMIT;
            if more > 0 {
                s.push_str(&format!("\n... and {more} more commits"));
            }
            break;
        }

        s.push_str(&format!("- {}\n", c.msg.0.trim()));
        if let Some(body) = &c.msg.1 {
            let body = body.split('\n');
            for line in body {
                let line = line.trim();
                if !line.is_empty() {
                    s.push_str(&format!("    {line}\n"));
                }
            }
        }
    }

    while s.ends_with('\n') {
        s.pop();
    }

    Ok(s)
}

struct Commit {
    _id: String,
    msg: (String, Option<String>),
}

// strip modifiers and expand groups
pub fn resolve_packages(pkgs: &[String], p: &Path) -> Result<Vec<String>> {
    let mut req_pkgs = vec![];
    for i in pkgs {
        // strip modifiers: e.g. llvm:+stage2 becomes llvm
        let i = strip_modifiers(i);
        if i.starts_with("groups/") {
            let f = std::fs::File::open(p.join(i))?;
            let lines = StdBufReader::new(f).lines();

            for i in lines {
                let i = i?;
                let pkg = i.split('/').next_back().unwrap_or(&i);
                req_pkgs.push(pkg.to_string());
            }
        } else {
            req_pkgs.push(i.to_string());
        }
    }
    Ok(req_pkgs)
}

pub fn strip_modifiers(pkg: &str) -> &str {
    match pkg.split_once(':') {
        Some((prefix, _suffix)) => prefix,
        None => pkg,
    }
}

pub(crate) const ALL_ARCH: &[&str] = &[
    "amd64",
    "arm64",
    "loongarch64",
    "loongson3",
    "mips64r6el",
    "ppc64el",
    "riscv64",
];

pub fn get_archs<'a>(p: &'a Path, packages: &'a [String]) -> Vec<&'static str> {
    let mut is_noarch = vec![];
    let mut fail_archs = vec![];

    for_each_abbs(p, |pkg, path| {
        if !packages.contains(&pkg.to_string()) {
            return;
        }

        let defines_list = locate_defines(path);

        for i in defines_list {
            let defines = std::fs::read_to_string(i);

            if let Ok(defines) = defines {
                let defines = read_ab_with_apml(&defines);

                is_noarch.push(
                    defines
                        .get("ABHOST")
                        .map(|x| x == "noarch")
                        .unwrap_or(false),
                );

                if let Some(fail_arch) = defines.get("FAIL_ARCH") {
                    fail_archs.push(fail_arch_regex(fail_arch).ok())
                } else {
                    fail_archs.push(None);
                };
            }
        }
    });

    if is_noarch.is_empty() || is_noarch.iter().any(|x| !x) {
        if fail_archs.is_empty() {
            return ALL_ARCH.iter().map(|x| x.to_owned()).collect();
        }

        if fail_archs.iter().any(|x| x.is_none()) {
            ALL_ARCH.iter().map(|x| x.to_owned()).collect()
        } else {
            let mut res = vec![];

            for i in fail_archs {
                let r = i.unwrap();
                for a in ALL_ARCH.iter().map(|x| x.to_owned()) {
                    if !r.is_match(a).unwrap_or(false) && !res.contains(&a) {
                        res.push(a);
                    }
                }
            }

            res
        }
    } else {
        vec!["noarch"]
    }
}

pub fn fail_arch_regex(expr: &str) -> Result<Regex> {
    let mut regex = String::from("^");
    let mut negated = false;
    let mut sup_bracket = false;

    if expr.len() < 3 {
        bail!("Pattern too short.");
    }

    let expr = expr.as_bytes();
    for (i, c) in expr.iter().enumerate() {
        if i == 0 && c == &b'!' {
            negated = true;
            if expr.get(1) != Some(&b'(') {
                regex += "(";
                sup_bracket = true;
            }
            continue;
        }
        if negated {
            if c == &b'(' {
                regex += "(?!";
                continue;
            } else if i == 1 && sup_bracket {
                regex += "?!";
            }
        }
        regex += std::str::from_utf8(&[*c])?;
    }

    if sup_bracket {
        regex += ")";
    }

    Ok(Regex::new(&regex)?)
}

pub fn get_repo(path: &Path) -> Result<Repository> {
    let mut git_open_opts_map = sec::trust::Mapping::<gix::open::Options>::default();

    let config = gix::open::permissions::Config {
        git_binary: false,
        system: false,
        git: false,
        user: false,
        env: true,
        includes: true,
    };

    git_open_opts_map.reduced = git_open_opts_map
        .reduced
        .permissions(gix::open::Permissions {
            config,
            ..gix::open::Permissions::default_for_level(sec::Trust::Reduced)
        });

    git_open_opts_map.full = git_open_opts_map.full.permissions(gix::open::Permissions {
        config,
        ..gix::open::Permissions::default_for_level(sec::Trust::Full)
    });

    let shared_repo = ThreadSafeRepository::discover_with_environment_overrides_opts(
        path,
        Default::default(),
        git_open_opts_map,
    )
    .context("Failed to find git repo")?;

    let repository = shared_repo.to_thread_local();

    Ok(repository)
}

async fn open_pr_inner(pr: OpenPR<'_>, crab: Arc<Octocrab>) -> Result<PullRequest> {
    let OpenPR {
        title,
        head,
        packages,
        desc,
        pkg_affected,
        tags,
        archs,
    } = pr;

    // pr body
    let body = format!(
        PR!(),
        desc,
        pkg_affected.join("\n"),
        format!("#buildit {}", packages.replace(',', " ")),
        format_archs(archs)
    );

    // pr tags
    let tags = if let Some(tags) = tags {
        Cow::Borrowed(tags)
    } else {
        Cow::Owned(auto_add_label(title))
    };

    // check if there are existing open pr
    find_old_pr(crab.clone(), head).await?;

    // create a new pr
    let pr = crab_pr(crab, title, head, body, tags).await.map_err(|e| {
        debug!("{e:?}");
        e
    })?;

    Ok(pr)
}

async fn crab_pr(
    crab: Arc<Octocrab>,
    title: &str,
    head: &str,
    body: String,
    tags: Cow<'_, [String]>,
) -> Result<PullRequest, octocrab::Error> {
    let pr = crab
        .pulls("AOSC-Dev", "aosc-os-abbs")
        .create(title, head, "stable")
        .draft(true)
        .maintainer_can_modify(true)
        .body(&body)
        .send()
        .await?;

    if !tags.is_empty() {
        crab.issues("AOSC-Dev", "aosc-os-abbs")
            .add_labels(pr.number, &tags)
            .await?;
    }

    Ok(pr)
}

pub async fn find_old_pr(crab: Arc<Octocrab>, head: &str) -> Result<()> {
    let page = pr_pages(crab, head).await.map_err(|e| {
        debug!("{e:?}");
        e
    })?;

    for old_pr in page.items {
        if old_pr.head.ref_field == head {
            bail!("PR exists");
        }
    }

    Ok(())
}

async fn pr_pages(crab: Arc<Octocrab>, head: &str) -> Result<Page<PullRequest>, octocrab::Error> {
    let page = crab
        .pulls("AOSC-Dev", "aosc-os-abbs")
        .list()
        // Optional Parameters
        .state(params::State::Open)
        .head(format!("AOSC-Dev:{}", head))
        .base("stable")
        .per_page(100)
        // Send the request
        .send()
        .await?;

    Ok(page)
}

pub async fn old_prs_100(crab: Arc<Octocrab>) -> Result<Page<PullRequest>> {
    let page = crab
        .pulls("AOSC-Dev", "aosc-os-abbs")
        .list()
        // Optional Parameters
        .state(params::State::Open)
        .base("stable")
        .per_page(100)
        // Send the request
        .send()
        .await?;

    Ok(page)
}

fn auto_add_label(title: &str) -> Vec<String> {
    let mut labels = vec![];
    let title = title
        .to_ascii_lowercase()
        .split_ascii_whitespace()
        .map(|x| {
            x.chars()
                .filter(|x| x.is_ascii_alphabetic() || x.is_ascii_alphanumeric())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join(" ");

    let v = vec![
        ("fix", vec![String::from("has-fix")]),
        ("update", vec![String::from("upgrade")]),
        ("upgrade", vec![String::from("upgrade")]),
        ("downgrade", vec![String::from("downgrade")]),
        ("survey", vec![String::from("survey")]),
        ("drop", vec![String::from("drop-package")]),
        ("security", vec![String::from("security")]),
        ("cve", vec![String::from("security")]),
        ("0day", vec![String::from("0day"), String::from("security")]),
        ("improve", vec![String::from("enhancement")]),
        ("enhance", vec![String::from("enhancement")]),
        ("dep", vec![String::from("dependencies")]),
        ("dependencies", vec![String::from("dependencies")]),
        ("dependency", vec![String::from("dependencies")]),
        ("pkgdep", vec![String::from("dependencies")]),
        ("builddep", vec![String::from("dependencies")]),
        ("depend", vec![String::from("dependencies")]),
        ("core", vec![String::from("core")]),
        ("mips64r6el", vec![String::from("cip-pilot")]),
        ("mipsisa64r6el", vec![String::from("cip-pilot")]),
        ("mipsr6", vec![String::from("cip-pilot")]),
        ("r6", vec![String::from("cip-pilot")]),
        ("linux-kernel", vec![String::from("kernel")]),
        ("new", vec![String::from("new-packages")]),
        ("preview", vec![String::from("preview")]),
        (
            "ftbfs",
            vec![String::from("has-fix"), String::from("ftbfs")],
        ),
        ("rework", vec![String::from("rework")]),
    ];

    for (k, v) in v {
        if title.contains(k) {
            labels.extend(v);
        }
    }

    // de-duplicate
    let mut res = vec![];
    for i in labels {
        if res.contains(&i) {
            continue;
        }

        res.push(i);
    }

    // Add autopr label
    res.push("autopr".to_string());

    res
}

pub const AMD64: &str = "AMD64 `amd64`";
pub const ARM64: &str = "AArch64 `arm64`";
pub const NOARCH: &str = "Architecture-independent `noarch`";
pub const LOONGARCH64: &str = "LoongArch 64-bit `loongarch64`";
pub const LOONGSON3: &str = "Loongson 3 `loongson3`";
pub const MIPS64R6EL: &str = "MIPS R6 64-bit (Little Endian) `mips64r6el`";
pub const PPC64EL: &str = "PowerPC 64-bit (Little Endian) `ppc64el`";
pub const RISCV64: &str = "RISC-V 64-bit `riscv64`";

fn format_archs(archs: &[&str]) -> String {
    let mut s = "".to_string();

    let mut map = HashMap::new();
    map.insert("amd64", AMD64);
    map.insert("arm64", ARM64);
    map.insert("noarch", NOARCH);
    map.insert("loongarch64", LOONGARCH64);
    map.insert("loongson3", LOONGSON3);
    map.insert("mips64r6el", MIPS64R6EL);
    map.insert("ppc64el", PPC64EL);
    map.insert("riscv64", RISCV64);

    let mut newline = false;

    // Primary Architectures
    if archs.contains(&"amd64")
        || archs.contains(&"arm64")
        || archs.contains(&"loongarch64")
        || archs.contains(&"noarch")
    {
        s.push_str("**Primary Architectures**\n\n");
        newline = true;
    }

    for i in ["amd64", "arm64", "loongarch64", "noarch"] {
        if archs.contains(&i) {
            s.push_str(&format!("- [ ] {}\n", map[i]));
        }
    }

    // Secondary Architectures
    if archs.contains(&"loongson3") || archs.contains(&"ppc64el") || archs.contains(&"riscv64") {
        if newline {
            s.push('\n');
        }
        s.push_str("**Secondary Architectures**\n\n");
        newline = true;
    }

    for i in ["loongson3", "ppc64el", "riscv64"] {
        if archs.contains(&i) {
            s.push_str(&format!("- [ ] {}\n", map[i]));
        }
    }

    // Experimental Architectures
    if archs.contains(&"mips64r6el") {
        if newline {
            s.push('\n');
        }
        s.push_str("**Experimental Architectures**\n\n");
    }

    for i in ["mips64r6el"] {
        if archs.contains(&i) {
            s.push_str(&format!("- [ ] {}\n", map[i]));
        }
    }

    s
}
