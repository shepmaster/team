mod api;

use self::api::{GitHub, TeamPrivacy, TeamRole};
use crate::{github::api::RepoPermission, TeamApi};
use anyhow::Error;
use log::{debug, info};
use rust_team_data::v1::Bot;
use std::collections::{HashMap, HashSet};

static DEFAULT_DESCRIPTION: &str = "Managed by the rust-lang/team repository.";
static DEFAULT_PRIVACY: TeamPrivacy = TeamPrivacy::Closed;

pub(crate) struct SyncGitHub {
    github: GitHub,
    teams: Vec<rust_team_data::v1::Team>,
    repos: Vec<rust_team_data::v1::Repo>,
    usernames_cache: HashMap<usize, String>,
    org_owners: HashMap<String, HashSet<usize>>,
}

impl SyncGitHub {
    pub(crate) fn new(token: String, team_api: &TeamApi, dry_run: bool) -> Result<Self, Error> {
        let github = GitHub::new(token, dry_run);
        let teams = team_api.get_teams()?;
        let repos = team_api.get_repos()?;

        debug!("caching mapping between user ids and usernames");
        let users = teams
            .iter()
            .filter_map(|t| t.github.as_ref().map(|gh| &gh.teams))
            .flatten()
            .flat_map(|team| &team.members)
            .copied()
            .collect::<HashSet<_>>();
        let usernames_cache = github.usernames(&users.into_iter().collect::<Vec<_>>())?;

        debug!("caching organization owners");
        let orgs = teams
            .iter()
            .filter_map(|t| t.github.as_ref())
            .flat_map(|gh| &gh.teams)
            .map(|gh_team| &gh_team.org)
            .collect::<HashSet<_>>();
        let mut org_owners = HashMap::new();
        for org in &orgs {
            org_owners.insert((*org).to_string(), github.org_owners(org)?);
        }

        Ok(SyncGitHub {
            github,
            teams,
            repos,
            usernames_cache,
            org_owners,
        })
    }

    pub(crate) fn diff_all(&self) -> anyhow::Result<Diff> {
        let team_diffs = self.diff_teams()?;
        let repo_diffs = self.diff_repos()?;

        Ok(Diff {
            team_diffs,
            repo_diffs,
        })
    }

    fn diff_teams(&self) -> anyhow::Result<Vec<TeamDiff>> {
        let mut diffs = Vec::new();
        let mut unseen_github_teams = HashMap::new();
        for team in &self.teams {
            if let Some(gh) = &team.github {
                for github_team in &gh.teams {
                    // Get existing teams we haven't seen yet
                    let unseen_github_teams = match unseen_github_teams.get_mut(&github_team.org) {
                        Some(ts) => ts,
                        None => {
                            let ts = self.github.org_teams(&github_team.org)?;
                            unseen_github_teams
                                .entry(github_team.org.clone())
                                .or_insert(ts)
                        }
                    };
                    // Remove the current team from the collection of unseen GitHub teams
                    unseen_github_teams.remove(&github_team.name);

                    diffs.push(self.diff_team(github_team)?);
                }
            }
        }

        let delete_diffs = unseen_github_teams
            .into_iter()
            .filter(|(org, _)| org == "rust-lang") // Only delete unmanaged teams in `rust-lang` for now
            .flat_map(|(org, remaining_github_teams)| {
                remaining_github_teams
                    .into_iter()
                    .map(move |t| (org.clone(), t))
            })
            // Don't delete the special bot teams
            .filter(|(_, remaining_github_team)| {
                !BOTS_TEAMS.contains(&remaining_github_team.as_str())
            })
            .map(|(org, remaining_github_team)| {
                TeamDiff::Delete(DeleteTeamDiff {
                    org,
                    name: remaining_github_team,
                })
            });

        diffs.extend(delete_diffs);

        Ok(diffs)
    }

    fn diff_team(&self, github_team: &rust_team_data::v1::GitHubTeam) -> anyhow::Result<TeamDiff> {
        // Ensure the team exists and is consistent
        let team = match self.github.team(&github_team.org, &github_team.name)? {
            Some(team) => team,
            None => {
                let members = github_team
                    .members
                    .iter()
                    .map(|member| {
                        let expected_role = self.expected_role(&github_team.org, *member);
                        (self.usernames_cache[member].clone(), expected_role)
                    })
                    .collect();
                return Ok(TeamDiff::Create(CreateTeamDiff {
                    org: github_team.org.clone(),
                    name: github_team.name.clone(),
                    description: DEFAULT_DESCRIPTION.to_owned(),
                    privacy: DEFAULT_PRIVACY,
                    members,
                }));
            }
        };
        let mut name_diff = None;
        if team.name != github_team.name {
            name_diff = Some(github_team.name.clone())
        }
        let mut description_diff = None;
        if team.description != DEFAULT_DESCRIPTION {
            description_diff = Some((team.description.clone(), DEFAULT_DESCRIPTION.to_owned()));
        }
        let mut privacy_diff = None;
        if team.privacy != DEFAULT_PRIVACY {
            privacy_diff = Some((team.privacy, DEFAULT_PRIVACY))
        }

        let mut member_diffs = Vec::new();

        let mut current_members = self.github.team_memberships(&team)?;

        // Ensure all expected members are in the team
        for member in &github_team.members {
            let expected_role = self.expected_role(&github_team.org, *member);
            let username = &self.usernames_cache[member];
            if let Some(member) = current_members.remove(member) {
                if member.role != expected_role {
                    member_diffs.push((
                        username.clone(),
                        MemberDiff::ChangeRole((member.role, expected_role)),
                    ));
                } else {
                    member_diffs.push((username.clone(), MemberDiff::Noop));
                }
            } else {
                member_diffs.push((username.clone(), MemberDiff::Create(expected_role)));
            }
        }

        // The previous cycle removed expected members from current_members, so it only contains
        // members to delete now.
        for member in current_members.values() {
            member_diffs.push((member.username.clone(), MemberDiff::Delete));
        }

        Ok(TeamDiff::Edit(EditTeamDiff {
            org: github_team.org.clone(),
            name: team.name,
            name_diff,
            description_diff,
            privacy_diff,
            member_diffs,
        }))
    }

    fn diff_repos(&self) -> Result<Vec<RepoDiff>, Error> {
        let mut diffs = Vec::new();
        for repo in &self.repos {
            diffs.push(self.diff_repo(repo)?);
        }
        Ok(diffs)
    }

    fn diff_repo(&self, expected_repo: &rust_team_data::v1::Repo) -> Result<RepoDiff, Error> {
        let actual_repo = match self.github.repo(&expected_repo.org, &expected_repo.name)? {
            Some(r) => r,
            None => {
                let mut permissions = Vec::new();
                for expected_team in &expected_repo.teams {
                    permissions.push((
                        RepoCollaborator::Team(expected_team.name.clone()),
                        convert_permission(&expected_team.permission),
                    ));
                }

                for bot in &expected_repo.bots {
                    permissions.push((
                        RepoCollaborator::User(bot_name(bot).to_owned()),
                        RepoPermission::Write,
                    ));
                }

                for member in &expected_repo.members {
                    permissions.push((
                        RepoCollaborator::User(member.name.clone()),
                        convert_permission(&member.permission),
                    ));
                }

                let mut branch_protections = Vec::new();
                for branch in &expected_repo.branches {
                    branch_protections.push((
                        branch.name.clone(),
                        branch_protection(expected_repo, branch),
                    ));
                }

                return Ok(RepoDiff::Create(CreateRepoDiff {
                    org: expected_repo.org.clone(),
                    name: expected_repo.name.clone(),
                    description: expected_repo.description.clone(),
                    permissions,
                    branch_protections,
                }));
            }
        };

        let permission_diffs = self.diff_permissions(expected_repo)?;
        let branch_protection_diffs = self.diff_branch_protections(&actual_repo, expected_repo)?;
        let name_diff =
            (actual_repo.name != expected_repo.name).then(|| expected_repo.name.clone());
        let description_diff =
            (actual_repo.description.as_ref() != Some(&expected_repo.description)).then(|| {
                (
                    actual_repo.description.clone(),
                    expected_repo.description.clone(),
                )
            });
        Ok(RepoDiff::Update(UpdateRepoDiff {
            org: expected_repo.org.clone(),
            name: actual_repo.name,
            name_diff,
            description_diff,
            permission_diffs,
            branch_protection_diffs,
        }))
    }

    fn diff_permissions(
        &self,
        expected_repo: &rust_team_data::v1::Repo,
    ) -> Result<Vec<RepoPermissionAssignmentDiff>, Error> {
        let mut actual_teams: HashMap<_, _> = self
            .github
            .repo_teams(&expected_repo.org, &expected_repo.name)?
            .into_iter()
            .map(|t| (t.name.clone(), t))
            .collect();
        let mut actual_collaborators: HashMap<_, _> = self
            .github
            .repo_collaborators(&expected_repo.org, &expected_repo.name)?
            .into_iter()
            .map(|u| (u.name.clone(), u))
            .collect();

        let mut permissions = Vec::new();
        // Sync team and bot permissions
        for expected_team in &expected_repo.teams {
            let permission = convert_permission(&expected_team.permission);
            let removed = actual_teams.remove(&expected_team.name);
            let collaborator = RepoCollaborator::Team(expected_team.name.clone());
            let diff = match removed {
                Some(t) if t.permission != permission => RepoPermissionAssignmentDiff {
                    collaborator,
                    diff: RepoPermissionDiff::Update(t.permission, permission),
                },
                Some(_) => continue,
                None => RepoPermissionAssignmentDiff {
                    collaborator,
                    diff: RepoPermissionDiff::Create(permission),
                },
            };
            permissions.push(diff);
        }

        let bots = expected_repo.bots.iter().map(|b| {
            let bot_name = bot_name(b);
            actual_teams.remove(bot_name);
            (bot_name, RepoPermission::Write)
        });
        let members = expected_repo
            .members
            .iter()
            .map(|m| (m.name.as_str(), convert_permission(&m.permission)));

        for (name, permission) in bots.chain(members) {
            let removed = actual_collaborators.remove(name);
            let collaborator = RepoCollaborator::User(name.to_owned());
            let diff = match removed {
                Some(t) if t.permission != permission => RepoPermissionAssignmentDiff {
                    collaborator,
                    diff: RepoPermissionDiff::Update(t.permission, permission),
                },
                Some(_) => continue,
                None => RepoPermissionAssignmentDiff {
                    collaborator,
                    diff: RepoPermissionDiff::Create(permission),
                },
            };
            permissions.push(diff);
        }

        // `actual_teams` now contains the teams that were not expected
        // but are still on GitHub. We now remove them.
        for (team, t) in actual_teams {
            permissions.push(RepoPermissionAssignmentDiff {
                collaborator: RepoCollaborator::Team(team),
                diff: RepoPermissionDiff::Delete(t.permission),
            });
        }

        // `actual_collaborators` now contains the collaborators that were not expected
        // but are still on GitHub. We now remove them.
        for (collaborator, u) in actual_collaborators {
            permissions.push(RepoPermissionAssignmentDiff {
                collaborator: RepoCollaborator::User(collaborator),
                diff: RepoPermissionDiff::Delete(u.permission),
            });
        }

        Ok(permissions)
    }

    fn diff_branch_protections(
        &self,
        actual_repo: &api::Repo,
        expected_repo: &rust_team_data::v1::Repo,
    ) -> Result<Vec<BranchProtectionDiff>, Error> {
        let mut branch_protection_diffs = Vec::new();
        let mut actual_protected_branches = self.github.protected_branches(actual_repo)?;
        let mut main_branch_commit = None;

        for branch in &expected_repo.branches {
            actual_protected_branches.remove(&branch.name);
            let expected_branch_protection = branch_protection(expected_repo, branch);
            let actual_branch =
                self.github
                    .branch(&expected_repo.org, &expected_repo.name, &branch.name)?;
            let operation = if actual_branch.is_none() {
                let main_branch_commit = match main_branch_commit.as_ref() {
                    Some(s) => s,
                    None => {
                        let head = self
                            .github
                            .branch(
                                &actual_repo.org,
                                &actual_repo.name,
                                &actual_repo.default_branch,
                            )?
                            .unwrap();

                        // cache the main branch head so we only need to get it once
                        main_branch_commit.get_or_insert(head)
                    }
                };
                BranchProtectionDiffOperation::CreateWithBranch(
                    expected_branch_protection,
                    main_branch_commit.clone(),
                )
            } else {
                let actual_branch_protection =
                    self.github.branch_protection(actual_repo, &branch.name)?;
                match actual_branch_protection {
                    Some(a) if a != expected_branch_protection => {
                        BranchProtectionDiffOperation::Update(a, expected_branch_protection)
                    }
                    None => BranchProtectionDiffOperation::Create(expected_branch_protection),
                    // The branch protection doesn't need to change
                    Some(_) => continue,
                }
            };
            branch_protection_diffs.push(BranchProtectionDiff {
                name: branch.name.clone(),
                operation,
            })
        }

        // `actual_branch_protections` now contains the branch protections that were not expected
        // but are still on GitHub. We want to delete them.
        branch_protection_diffs.extend(actual_protected_branches.into_iter().map(|name| {
            BranchProtectionDiff {
                name,
                operation: BranchProtectionDiffOperation::Delete,
            }
        }));

        Ok(branch_protection_diffs)
    }

    pub(crate) fn synchronize_all(&self) -> Result<(), Error> {
        for repo in &self.repos {
            self.synchronize_repo(repo)?;
        }

        Ok(())
    }

    fn synchronize_repo(&self, expected_repo: &rust_team_data::v1::Repo) -> Result<(), Error> {
        debug!(
            "synchronizing repo {}/{}",
            expected_repo.org, expected_repo.name
        );

        // Ensure the repo exists or create it.
        let (actual_repo, just_created) =
            match self.github.repo(&expected_repo.org, &expected_repo.name)? {
                Some(r) => {
                    debug!("repo already exists...");
                    (r, false)
                }
                None => {
                    let repo = self.github.create_repo(
                        &expected_repo.org,
                        &expected_repo.name,
                        &expected_repo.description,
                    )?;
                    (repo, true)
                }
            };

        // Ensure the repo is consistent between its expected state and current state
        if !just_created {
            if actual_repo.description.as_ref() != Some(&expected_repo.description) {
                self.github.edit_repo(
                    &actual_repo.org,
                    &actual_repo.name,
                    &expected_repo.description,
                )?;
            } else {
                debug!("repo is in synced state");
            }
        }

        let mut actual_teams: HashMap<_, _> = self
            .github
            .repo_teams(&expected_repo.org, &expected_repo.name)?
            .into_iter()
            .map(|t| (t.name.clone(), t))
            .collect();
        let mut actual_collaborators: HashMap<_, _> = self
            .github
            .repo_collaborators(&expected_repo.org, &expected_repo.name)?
            .into_iter()
            .map(|u| (u.name.clone(), u))
            .collect();

        // Sync team and bot permissions
        for expected_team in &expected_repo.teams {
            let permission = convert_permission(&expected_team.permission);
            actual_teams.remove(&expected_team.name);
            self.github.update_team_repo_permissions(
                &expected_repo.org,
                &expected_repo.name,
                &expected_team.name,
                &permission,
            )?;
        }

        for bot in &expected_repo.bots {
            let bot_name = bot_name(bot);
            actual_teams.remove(bot_name);
            actual_collaborators.remove(bot_name);
            self.github.update_user_repo_permissions(
                &expected_repo.org,
                &expected_repo.name,
                bot_name,
                &RepoPermission::Write,
            )?;
        }

        for member in &expected_repo.members {
            actual_collaborators.remove(&member.name);
            self.github.update_user_repo_permissions(
                &expected_repo.org,
                &expected_repo.name,
                &member.name,
                &convert_permission(&member.permission),
            )?;
        }

        // `actual_teams` now contains the teams that were not expected
        // but are still on GitHub. We now remove them.
        for team in actual_teams.keys() {
            self.github
                .remove_team_from_repo(&expected_repo.org, &expected_repo.name, team)?;
        }
        // `actual_collaborators` now contains the collaborators that were not expected
        // but are still on GitHub. We now remove them.
        for collaborator in actual_collaborators.keys() {
            self.github.remove_collaborator_from_repo(
                &expected_repo.org,
                &expected_repo.name,
                collaborator,
            )?;
        }

        let mut main_branch_commit = None;
        let mut actual_branch_protections = self.github.protected_branches(&actual_repo)?;

        for branch in &expected_repo.branches {
            actual_branch_protections.remove(&branch.name);

            // if the branch does not already exist, create it
            if self
                .github
                .branch(&actual_repo.org, &actual_repo.name, &branch.name)?
                .is_none()
            {
                // First, we need the sha of the head of the main branch
                let main_branch_commit = match main_branch_commit.as_ref() {
                    Some(s) => s,
                    None => {
                        let head = self.github.branch(
                            &actual_repo.org,
                            &actual_repo.name,
                            &actual_repo.default_branch,
                        )?;
                        // cache the main branch head so we only need to get it once
                        main_branch_commit.get_or_insert(head)
                    }
                };

                // If there is a main branch commit, create the new branch
                if let Some(main_branch_commit) = main_branch_commit {
                    self.github.create_branch(
                        &actual_repo.org,
                        &actual_repo.name,
                        &branch.name,
                        main_branch_commit,
                    )?;
                }
            }

            // Update the protection of the branch
            let protection_result = self.github.update_branch_protection(
                &actual_repo.org,
                &actual_repo.name,
                &branch.name,
                &branch_protection(expected_repo, branch),
            )?;
            if !protection_result {
                debug!(
                    "Did not update branch protection for \
                    '{}' on '{}/{}' as the branch does not exist.",
                    branch.name, actual_repo.org, actual_repo.name
                );
            }
        }

        // `actual_branch_protections` now contains the branch protections that were not expected
        // but are still on GitHub. We now remove them.
        for branch_protection in actual_branch_protections {
            debug!(
                "Deleting branch protection for '{}' on '{}/{}' as \
                the protection is not in the team repo",
                branch_protection, actual_repo.org, actual_repo.name
            );
            self.github.delete_branch_protection(
                &actual_repo.org,
                &actual_repo.name,
                &branch_protection,
            )?;
        }
        Ok(())
    }

    fn expected_role(&self, org: &str, user: usize) -> TeamRole {
        if let Some(true) = self
            .org_owners
            .get(org)
            .map(|owners| owners.contains(&user))
        {
            TeamRole::Maintainer
        } else {
            TeamRole::Member
        }
    }
}

fn bot_name(bot: &Bot) -> &str {
    match bot {
        Bot::Bors => "bors",
        Bot::Highfive => "rust-highfive",
        Bot::RustTimer => "rust-timer",
        Bot::Rustbot => "rustbot",
    }
}

fn convert_permission(p: &rust_team_data::v1::RepoPermission) -> RepoPermission {
    use rust_team_data::v1;
    match *p {
        v1::RepoPermission::Write => RepoPermission::Write,
        v1::RepoPermission::Admin => RepoPermission::Admin,
        v1::RepoPermission::Maintain => RepoPermission::Maintain,
        v1::RepoPermission::Triage => RepoPermission::Triage,
    }
}

fn branch_protection(
    expected_repo: &rust_team_data::v1::Repo,
    branch: &rust_team_data::v1::Branch,
) -> api::BranchProtection {
    let required_approving_review_count = if expected_repo.bots.contains(&Bot::Bors) {
        0
    } else {
        1
    };
    let allowed_users = expected_repo
        .bots
        .contains(&Bot::Bors)
        .then(|| {
            vec![api::branch_protection::UserRestriction::Name(
                "bors".to_owned(),
            )]
        })
        .unwrap_or_default();
    api::BranchProtection {
        required_status_checks: api::branch_protection::RequiredStatusChecks {
            strict: false,
            checks: branch
                .ci_checks
                .clone()
                .into_iter()
                .map(|c| api::branch_protection::Check { context: c })
                .collect(),
        },
        enforce_admins: api::branch_protection::EnforceAdmins::Bool(true),
        required_pull_request_reviews: api::branch_protection::PullRequestReviews {
            dismissal_restrictions: HashMap::new(),
            dismiss_stale_reviews: branch.dismiss_stale_review,
            required_approving_review_count,
        },
        restrictions: api::branch_protection::Restrictions {
            users: allowed_users,
            teams: Vec::new(),
        },
    }
}

/// The special bot teams
const BOTS_TEAMS: &[&str] = &["bors", "highfive", "rfcbot", "bots"];

/// A diff between the team repo and the state on GitHub
pub(crate) struct Diff {
    team_diffs: Vec<TeamDiff>,
    repo_diffs: Vec<RepoDiff>,
}

impl Diff {
    /// Apply the diff to GitHub
    pub(crate) fn apply(self, sync: &SyncGitHub) -> anyhow::Result<()> {
        for team_diff in self.team_diffs {
            team_diff.apply(sync)?;
        }
        for repo_diff in self.repo_diffs {
            repo_diff.apply(sync)?;
        }

        Ok(())
    }

    /// Print out the diff to the logs
    pub(crate) fn log(&self) {
        if !self.team_diffs.is_empty() {
            info!("💻 Team Diffs:");
        }
        for team_diff in &self.team_diffs {
            team_diff.log();
        }
        if !self.repo_diffs.is_empty() {
            info!("💻 Repo Diffs:");
        }
        for repo_diff in &self.repo_diffs {
            repo_diff.log();
        }
    }
}

enum RepoDiff {
    Create(CreateRepoDiff),
    Update(UpdateRepoDiff),
}

impl RepoDiff {
    pub(crate) fn log(&self) {
        match self {
            Self::Create(c) => c.log(),
            Self::Update(u) => u.log(),
        }
    }

    fn apply(&self, sync: &SyncGitHub) -> anyhow::Result<()> {
        match self {
            RepoDiff::Create(c) => c.apply(sync),
            RepoDiff::Update(u) => u.apply(sync),
        }
    }
}

struct CreateRepoDiff {
    org: String,
    name: String,
    description: String,
    permissions: Vec<(RepoCollaborator, RepoPermission)>,
    branch_protections: Vec<(String, api::BranchProtection)>,
}

impl CreateRepoDiff {
    fn log(&self) {
        info!("➕ Creating repo:");
        info!("  Org: {}", self.org);
        info!("  Name: {}", self.name);
        info!("  Description: {}", self.description);
        info!("  Permissions:");
        for (collaborator, permission) in &self.permissions {
            RepoPermissionAssignmentDiff {
                collaborator: collaborator.clone(),
                diff: RepoPermissionDiff::Create(*permission),
            }
            .log()
        }
        info!("  Branch Protections:");
        for (branch_name, branch_protection) in &self.branch_protections {
            log_branch_protection(branch_name, branch_protection)
        }
    }

    fn apply(&self, sync: &SyncGitHub) -> anyhow::Result<()> {
        let repo = sync
            .github
            .create_repo(&self.org, &self.name, &self.description)?;
        for (collaborator, permission) in &self.permissions {
            RepoPermissionAssignmentDiff {
                collaborator: collaborator.clone(),
                diff: RepoPermissionDiff::Create(*permission),
            }
            .apply(sync, &self.org, &self.name)?;
        }

        let main_branch_commit = sync
            .github
            .branch(&self.org, &self.name, &repo.default_branch)?
            .unwrap();
        for (branch, protection) in &self.branch_protections {
            BranchProtectionDiff {
                name: branch.clone(),
                operation: BranchProtectionDiffOperation::CreateWithBranch(
                    protection.clone(),
                    main_branch_commit.clone(),
                ),
            }
            .apply(sync, &self.org, &self.name)?;
        }
        Ok(())
    }
}

struct UpdateRepoDiff {
    org: String,
    name: String,
    name_diff: Option<String>,
    description_diff: Option<(Option<String>, String)>,
    permission_diffs: Vec<RepoPermissionAssignmentDiff>,
    branch_protection_diffs: Vec<BranchProtectionDiff>,
}

impl UpdateRepoDiff {
    fn log(&self) {
        if self.noop() {
            return;
        }
        info!("📝 Editing repo '{}/{}':", self.org, self.name);
        if let Some(n) = &self.name_diff {
            info!("  Changing name to: {}", n);
        }
        if let Some((old, new)) = &self.description_diff {
            if let Some(old) = old {
                info!("  New description: '{}' => '{}'", old, new);
            } else {
                info!("  Set description: '{}'", new);
            }
        }
        if !self.permission_diffs.is_empty() {
            info!("  Permission Changes:");
        }
        for permission_diff in &self.permission_diffs {
            permission_diff.log();
        }
        for branch_protection_diff in &self.branch_protection_diffs {
            branch_protection_diff.log()
        }
    }

    pub(crate) fn noop(&self) -> bool {
        self.name_diff.is_none()
            && self.description_diff.is_none()
            && self.permission_diffs.is_empty()
            && self.branch_protection_diffs.is_empty()
    }

    fn apply(&self, sync: &SyncGitHub) -> anyhow::Result<()> {
        // TODO: name diff?
        if let Some((_, description)) = &self.description_diff {
            sync.github.edit_repo(&self.org, &self.name, description)?;
        }
        for permission in &self.permission_diffs {
            permission.apply(sync, &self.org, &self.name)?;
        }
        for branch_protection in &self.branch_protection_diffs {
            branch_protection.apply(sync, &self.org, &self.name)?;
        }
        Ok(())
    }
}

struct RepoPermissionAssignmentDiff {
    collaborator: RepoCollaborator,
    diff: RepoPermissionDiff,
}

impl RepoPermissionAssignmentDiff {
    pub(crate) fn log(&self) {
        let name = match &self.collaborator {
            RepoCollaborator::Team(name) => format!("team '{name}'"),
            RepoCollaborator::User(name) => format!("user '{name}'"),
        };
        match &self.diff {
            RepoPermissionDiff::Create(p) => {
                info!("Giving {name} {p} permission");
            }
            RepoPermissionDiff::Update(old, new) => {
                info!("Changing {name}'s permission from {old} to {new}");
            }
            RepoPermissionDiff::Delete(p) => {
                info!("Removing {name}'s {p} permission ")
            }
        }
    }

    fn apply(&self, sync: &SyncGitHub, org: &str, repo_name: &str) -> anyhow::Result<()> {
        match &self.diff {
            RepoPermissionDiff::Create(p) | RepoPermissionDiff::Update(_, p) => {
                match &self.collaborator {
                    RepoCollaborator::Team(team_name) => sync
                        .github
                        .update_team_repo_permissions(org, repo_name, team_name, p)?,
                    RepoCollaborator::User(user_name) => sync
                        .github
                        .update_user_repo_permissions(org, repo_name, user_name, p)?,
                }
            }
            RepoPermissionDiff::Delete(_) => match &self.collaborator {
                RepoCollaborator::Team(team_name) => sync
                    .github
                    .remove_team_from_repo(org, repo_name, team_name)?,
                RepoCollaborator::User(user_name) => sync
                    .github
                    .remove_collaborator_from_repo(org, repo_name, user_name)?,
            },
        }
        Ok(())
    }
}

enum RepoPermissionDiff {
    Create(RepoPermission),
    Update(RepoPermission, RepoPermission),
    Delete(RepoPermission),
}

#[derive(Clone)]
enum RepoCollaborator {
    Team(String),
    User(String),
}

struct BranchProtectionDiff {
    name: String,
    operation: BranchProtectionDiffOperation,
}

impl BranchProtectionDiff {
    pub(crate) fn log(&self) {
        match &self.operation {
            BranchProtectionDiffOperation::CreateWithBranch(bp, _) => {
                info!("      Creating branch {}", self.name);
                log_branch_protection(&self.name, bp);
            }
            BranchProtectionDiffOperation::Create(bp) => log_branch_protection(&self.name, bp),
            BranchProtectionDiffOperation::Update(old, new) => {
                macro_rules! log {
                    ($str:literal, $old:expr, $new:expr) => {
                        if $old != $new {
                            info!($str, $old, $new);
                        }
                    };
                }
                info!("      {} protection:", self.name);
                log!(
                    "        Dismiss Stale Reviews: {} => {}",
                    old.required_pull_request_reviews.dismiss_stale_reviews,
                    new.required_pull_request_reviews.dismiss_stale_reviews
                );
                log!(
                    "        Required Approving Review Count: {} => {}",
                    old.required_pull_request_reviews
                        .required_approving_review_count,
                    new.required_pull_request_reviews
                        .required_approving_review_count
                );
                log!(
                    "        Checks: {:?} => {:?}",
                    old.required_status_checks.checks,
                    new.required_status_checks.checks
                );
                log!(
                    "        User Overrides: {:?} => {:?}",
                    old.restrictions.users,
                    new.restrictions.users
                );
                log!(
                    "        Team Overrides: {:?} => {:?}",
                    old.restrictions.teams,
                    new.restrictions.teams
                );
            }
            BranchProtectionDiffOperation::Delete => {
                info!("Deleting branch protection for branch '{}'", self.name)
            }
        }
    }

    fn apply(&self, sync: &SyncGitHub, org: &str, repo_name: &str) -> anyhow::Result<()> {
        if let BranchProtectionDiffOperation::CreateWithBranch(_, main_branch_commit) =
            &self.operation
        {
            sync.github
                .create_branch(org, repo_name, &self.name, main_branch_commit)?;
        }
        match &self.operation {
            BranchProtectionDiffOperation::CreateWithBranch(bp, _)
            | BranchProtectionDiffOperation::Create(bp)
            | BranchProtectionDiffOperation::Update(_, bp) => {
                // Update the protection of the branch
                let protection_result = sync
                    .github
                    .update_branch_protection(org, repo_name, &self.name, bp)?;
                if !protection_result {
                    debug!(
                        "Did not update branch protection for \
                    '{}' on '{}/{}' as the branch does not exist.",
                        self.name, org, repo_name
                    );
                }
            }
            BranchProtectionDiffOperation::Delete => {
                debug!(
                    "Deleting branch protection for '{}' on '{}/{}' as \
                the protection is not in the team repo",
                    self.name, org, repo_name
                );
                sync.github
                    .delete_branch_protection(org, repo_name, &self.name)?;
            }
        }

        Ok(())
    }
}

fn log_branch_protection(branch_name: &str, branch_protection: &api::BranchProtection) {
    info!("     {}", branch_name);
    info!(
        "         Dismiss Stale Reviews: {}",
        branch_protection
            .required_pull_request_reviews
            .dismiss_stale_reviews
    );
    info!(
        "         Required Approving Review Count: {}",
        branch_protection
            .required_pull_request_reviews
            .required_approving_review_count
    );
    info!(
        "         Checks: {:?}",
        branch_protection.required_status_checks.checks
    );
    info!(
        "         User Overrides: {:?}",
        branch_protection.restrictions.users
    );
    info!(
        "         Team Overrides: {:?}",
        branch_protection.restrictions.teams
    );
}

enum BranchProtectionDiffOperation {
    CreateWithBranch(
        api::BranchProtection,
        String, /* main branch HEAD commit */
    ),
    Create(api::BranchProtection),
    Update(api::BranchProtection, api::BranchProtection),
    Delete,
}

enum TeamDiff {
    Create(CreateTeamDiff),
    Edit(EditTeamDiff),
    Delete(DeleteTeamDiff),
}

impl TeamDiff {
    fn apply(self, sync: &SyncGitHub) -> anyhow::Result<()> {
        match self {
            TeamDiff::Create(c) => c.apply(sync)?,
            TeamDiff::Edit(e) => e.apply(sync)?,
            TeamDiff::Delete(d) => d.apply(sync)?,
        }

        Ok(())
    }

    fn log(&self) {
        match self {
            TeamDiff::Create(c) => c.log(),
            TeamDiff::Edit(e) => e.log(),
            TeamDiff::Delete(d) => d.log(),
        }
    }
}

struct CreateTeamDiff {
    org: String,
    name: String,
    description: String,
    privacy: TeamPrivacy,
    members: Vec<(String, TeamRole)>,
}

impl CreateTeamDiff {
    fn apply(self, sync: &SyncGitHub) -> anyhow::Result<()> {
        sync.github
            .create_team(&self.org, &self.name, &self.description, self.privacy)?;
        for (member_name, role) in self.members {
            MemberDiff::Create(role).apply(&self.org, &self.name, &member_name, sync)?;
        }

        Ok(())
    }

    fn log(&self) {
        info!("➕ Creating team:");
        info!("  Org: {}", self.org);
        info!("  Name: {}", self.name);
        info!("  Description: {}", self.description);
        info!(
            "  Privacy: {}",
            match self.privacy {
                TeamPrivacy::Secret => "secret",
                TeamPrivacy::Closed => "closed",
            }
        );
        info!("  Members:");
        for (name, role) in &self.members {
            info!("    {}: {}", name, role);
        }
    }
}

struct EditTeamDiff {
    org: String,
    name: String,
    name_diff: Option<String>,
    description_diff: Option<(String, String)>,
    privacy_diff: Option<(TeamPrivacy, TeamPrivacy)>,
    member_diffs: Vec<(String, MemberDiff)>,
}

impl EditTeamDiff {
    fn apply(self, sync: &SyncGitHub) -> anyhow::Result<()> {
        sync.github.edit_team(
            &self.org,
            &self.name,
            self.name_diff.as_deref(),
            self.description_diff.as_ref().map(|(_, d)| d.as_str()),
            self.privacy_diff.map(|(_, p)| p),
        )?;

        for (member_name, member_diff) in self.member_diffs {
            member_diff.apply(&self.org, &self.name, &member_name, sync)?;
        }

        Ok(())
    }

    fn log(&self) {
        if self.noop() {
            return;
        }
        info!("📝 Editing team '{}':", self.name);
        if let Some(n) = &self.name_diff {
            info!("  New name: {}", n);
        }
        if let Some((old, new)) = &self.description_diff {
            info!("  New description: '{}' => '{}'", old, new);
        }
        if let Some((old, new)) = &self.privacy_diff {
            let display = |privacy: &TeamPrivacy| match privacy {
                TeamPrivacy::Secret => "secret",
                TeamPrivacy::Closed => "closed",
            };
            info!("  New privacy: '{}' => '{}'", display(old), display(new));
        }
        for (member, diff) in &self.member_diffs {
            match diff {
                MemberDiff::Create(r) => info!("  Adding member '{member}' with {r} role"),
                MemberDiff::ChangeRole((o, n)) => {
                    info!("  Changing '{member}' role from {o} to {n}")
                }
                MemberDiff::Delete => info!("  Deleting member '{member}'"),
                MemberDiff::Noop => debug!("  Member '{member}' stays the same"),
            }
        }
    }

    fn noop(&self) -> bool {
        self.name_diff.is_none()
            && self.description_diff.is_none()
            && self.privacy_diff.is_none()
            && self.member_diffs.iter().all(|(_, d)| d.is_noop())
    }
}

enum MemberDiff {
    Create(TeamRole),
    ChangeRole((TeamRole, TeamRole)),
    Delete,
    Noop,
}

impl MemberDiff {
    fn apply(self, org: &str, team: &str, member: &str, sync: &SyncGitHub) -> anyhow::Result<()> {
        match self {
            MemberDiff::Create(role) | MemberDiff::ChangeRole((_, role)) => {
                sync.github.set_team_membership(org, team, member, role)?;
            }
            MemberDiff::Delete => sync.github.remove_team_membership(org, team, member)?,
            MemberDiff::Noop => {}
        }

        Ok(())
    }

    fn is_noop(&self) -> bool {
        matches!(self, Self::Noop)
    }
}

struct DeleteTeamDiff {
    org: String,
    name: String,
}

impl DeleteTeamDiff {
    fn apply(self, sync: &SyncGitHub) -> anyhow::Result<()> {
        sync.github.delete_team(&self.org, &self.name)?;
        Ok(())
    }

    fn log(&self) {
        info!("❌ Deleting team:");
        info!("  Org: {}", self.org);
        info!("  Name: {}", self.name);
    }
}
