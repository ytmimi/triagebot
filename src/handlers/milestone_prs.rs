use crate::{
    github::{Event, GithubClient, IssuesAction, IssuesEvent, Label},
    handlers::Context,
};
use anyhow::Context as _;
use regex::Regex;
use reqwest::StatusCode;
use tracing as log;

pub(super) async fn handle(ctx: &Context, event: &Event) -> anyhow::Result<()> {
    let Event::Issue(e) = event else {
        return Ok(());
    };

    // Only trigger on closed issues
    if e.action != IssuesAction::Closed {
        return Ok(());
    }

    let repo = e.issue.repository();
    if !(repo.organization == "rust-lang" && repo.repository == "rust") {
        return Ok(());
    }

    if e.issue.merged_at.is_none() {
        log::trace!(
            "Ignoring closing of rust-lang/rust#{}: not merged",
            e.issue.number
        );
        return Ok(());
    }

    let Some(merge_sha) = &e.issue.merge_commit_sha else {
        log::error!(
            "rust-lang/rust#{}: no merge_commit_sha in event",
            e.issue.number
        );
        return Ok(());
    };

    // Fetch the version from the upstream repository.
    let Some(version) = get_version_standalone(&ctx.github, merge_sha).await? else {
        log::error!("could not find the version of {merge_sha:?}");
        return Ok(());
    };

    if !version.starts_with("1.") && version.len() < 8 {
        log::error!("Weird version {version:?} for {merge_sha:?}");
        return Ok(());
    }

    // Associate this merged PR with the version it merged into.
    //
    // Note that this should work for rollup-merged PRs too. It will *not*
    // auto-update when merging a beta-backport, for example, but that seems
    // fine; we can manually update without too much trouble in that case, and
    // eventually automate it separately.
    e.issue.set_milestone(&ctx.github, &version).await?;

    milestone_submodules(&ctx.github, e, &version).await?;
    milestone_rustfmt(&ctx.github, e, &version).await?;

    Ok(())
}

async fn get_version_standalone(
    gh: &GithubClient,
    merge_sha: &str,
) -> anyhow::Result<Option<String>> {
    let resp = gh
        .raw()
        .get(format!(
            "https://raw.githubusercontent.com/rust-lang/rust/{merge_sha}/src/version"
        ))
        .send()
        .await
        .with_context(|| format!("retrieving src/version for {merge_sha}"))?;

    match resp.status() {
        StatusCode::OK => {}
        // Don't treat a 404 as a failure, we'll try another way to retrieve the version.
        StatusCode::NOT_FOUND => return Ok(None),
        status => anyhow::bail!(
            "unexpected status code {status} while retrieving src/version for {merge_sha}"
        ),
    }

    Ok(Some(
        resp.text()
            .await
            .with_context(|| format!("deserializing src/version for {merge_sha}"))?
            .trim()
            .to_string(),
    ))
}

async fn milestone_submodules(
    gh: &GithubClient,
    event: &IssuesEvent,
    version: &str,
) -> anyhow::Result<()> {
    let Some(files) = event.issue.diff(gh).await? else {
        return Ok(());
    };
    for (repo, submodule) in [
        ("rust-lang/cargo", "src/tools/cargo"),
        ("rust-lang/reference", "src/doc/reference"),
    ] {
        if let Some(fd) = files.iter().find(|fd| fd.filename == submodule) {
            // The webhook timeout of 10 seconds can be too short, so process in
            // the background.
            let diff = fd.patch.clone();
            let ver = version.to_string();
            tokio::task::spawn(async move {
                let gh = GithubClient::new_from_env();
                if let Err(e) = milestone_submodule(&gh, repo, submodule, &ver, &diff).await {
                    log::error!("failed to milestone {submodule}: {e:?}");
                }
            });
        }
    }

    Ok(())
}

/// Milestones all PRs in the submodule when the submodule is synced in
/// rust-lang/rust.
async fn milestone_submodule(
    gh: &GithubClient,
    repo_name: &str,
    submodule: &str,
    release_version: &str,
    submodule_diff: &str,
) -> anyhow::Result<()> {
    // Determine the start/end range of commits in this submodule update by
    // looking at the diff content which indicates the old and new hash.
    let subproject_re = Regex::new("Subproject commit ([0-9a-f]+)").unwrap();
    let mut caps = subproject_re.captures_iter(submodule_diff);
    let submodule_start_hash = &caps.next().unwrap()[1];
    let submodule_end_hash = &caps.next().unwrap()[1];
    if let Some(next) = caps.next() {
        anyhow::bail!("unexpected submodule capture {}", &next[1]);
    }

    // Get all of the git commits in the submodule repo.
    let submodule_repo = gh.repository(repo_name).await?;
    log::info!(
        "loading submodule {repo_name} changes {submodule_start_hash}...{submodule_end_hash}"
    );
    let commits = submodule_repo
        .github_commits_in_range(gh, submodule_start_hash, submodule_end_hash)
        .await?;

    // For each commit, look for a message that indicates which PR was merged.
    //
    // GitHub has a specific API for this at
    // /repos/{owner}/{repo}/commits/{commit_sha}/pulls
    // <https://docs.github.com/en/rest/commits/commits?apiVersion=2022-11-28#list-pull-requests-associated-with-a-commit>,
    // but it is a little awkward to use, only works on the default branch,
    // and this is a bit simpler/faster. However, it is sensitive to the
    // specific messages generated by bors or GitHub merge queue, and won't
    // catch things merged beyond them.
    let merge_re =
        Regex::new(r"(?:Auto merge of|Merge pull request) #([0-9]+)|\(#([0-9]+)\)$").unwrap();

    let pr_nums = commits
        .iter()
        .filter(|commit|
            // Assumptions:
            // * A merge commit always has two parent commits.
            // * Submodule PRs never got merged as fast-forward / rebase / squash merge.
            commit.parents.len() == 2)
        .filter_map(|commit| {
            let first = commit.commit.message.lines().next().unwrap_or_default();
            merge_re.captures(first).map(|cap| {
                cap.get(1)
                    .or_else(|| cap.get(2))
                    .unwrap()
                    .as_str()
                    .parse::<u64>()
                    .expect("digits only")
            })
        });
    let milestone = submodule_repo
        .get_or_create_milestone(gh, release_version, "closed")
        .await?;
    for pr_num in pr_nums {
        log::info!("setting submodule {submodule} milestone {milestone:?} for {pr_num}");
        submodule_repo.set_milestone(gh, &milestone, pr_num).await?;
    }

    Ok(())
}

/// This approach is currently unique to rustfmt.
///
/// Trying to figure out exactly which PRs within rust-lang/rustfmt are getting synced
/// with rust-lang/rust is difficult, so we're relying on GitHub labels and timestamps,
/// which should be good enough for rustfmt right now.
///
/// Triagebot is configured to label all PRs that get merged into the rust-lang/rustfmt
/// `main` branch with a `release-notes` tag. Milestone assignment will leverage this
/// label to add a milestone to all PRs that currently don't have a Milestone assigned to them.
async fn milestone_rustfmt(
    gh: &GithubClient,
    event: &IssuesEvent,
    milestone_version: &str,
) -> anyhow::Result<()> {
    // TODO(ytmimi):
    // These are just for subtree syncs. Might need to come up with a different approach for
    // beta backports, but for now I think it's fine if those milestones are added by hand
    let subtree_sync_labels = ["T-rustfmt", "subtree-sync"];

    if !subtree_sync_labels
        .iter()
        .all(|l| event.issue.contains_label(&Label::from(*l)))
    {
        // Not a rustfmt subtree-sync because it doesn't have the right rust-lang/rust labels.
        // We should make sure that both labels are getting automatically assigned for syncs.
        return Ok(());
    }

    let gh = gh.clone();
    let mv = milestone_version.to_string();
    // cutoff time is an hour before the subtree-sync PR was created.
    // The time check isn't super precise, but it should be good enough to prevent
    // rustbot from adding a milestone to a PRs that didn't make it into the sync.
    let cutoff_time = event.issue.created_at.clone() - chrono::Duration::hours(1);

    tokio::task::spawn(async move {
        let Ok(subtree) = gh.repository("rust-lang/rustfmt").await else {
            log::error!("failed to fetch repository details for rust-lang/rustfmt");
            return;
        };

        let cutoff_time_str = cutoff_time.format("<%Y-%m-%dT%H:%M:%S%:z").to_string();

        // Search for closed PRs with the `release-notes` label that don't already have a milestone,
        // which were merged at least an hour before the subtree-sync PR was created.
        let query = crate::github::issue_query::Query {
            filters: vec![
                ("is", "pull-request"),
                ("is", "closed"),
                ("no", "milestone"),
                ("merged", &cutoff_time_str),
            ],
            include_labels: vec!["release-notes"],
            exclude_labels: vec![],
        };

        let Ok(pull_requests) = subtree.get_issues(&gh, &query).await else {
            log::error!("failed to fetch PRs for rust-lang/rustfmt");
            return;
        };

        if pull_requests.is_empty() {
            log::error!("the pull request query for rust-lang/rustfmt didn't return any PRs");
            return;
        }

        let Ok(milestone) = subtree.get_or_create_milestone(&gh, &mv, "open").await else {
            log::error!("failed to get or create milestone {mv} for rust-lang/rustfmt");
            return;
        };

        for pr in pull_requests {
            let pr_number = pr.number;
            log::info!("setting rust-lang/rustfmt subtree milestone {milestone:?} for {pr_number}");
            if let Err(e) = subtree.set_milestone(&gh, &milestone, pr_number).await {
                log::error!(
                    "failed to set rust-lang/rustfmt subtree milestone {:?} for {}: {e:?}",
                    milestone,
                    pr_number
                );
            };
        }
    });

    Ok(())
}
