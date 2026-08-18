use serde_json::Value;
use url::Url;

use super::super::interaction::{
    Overlay, OverlayAction, OverlayItem, OverlayKind, RemoteProjectAction, RemoteProjectDraft,
    RemoteProjectField, TeleportPushAction,
};

#[must_use]
pub fn teleport_push_overlay(event: &Value) -> Option<Overlay> {
    let operation_id = event.get("operationId").and_then(Value::as_str)?;
    let count = event
        .get("unpushedCount")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let branch = event
        .get("branchNotPushed")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let detail = if branch {
        "Publish the current branch before starting Teleport".to_owned()
    } else {
        format!(
            "Push {count} unpushed commit{} before starting Teleport",
            if count == 1 { "" } else { "s" }
        )
    };
    let action = |approved, id: &str, label: &str, description: &str| {
        OverlayItem::new(id, label, description, false).with_action(OverlayAction::TeleportPush(
            TeleportPushAction {
                operation_id: operation_id.to_owned(),
                approved,
            },
        ))
    };
    let mut overlay = Overlay::new(
        OverlayKind::TeleportApproval,
        "Teleport needs to push changes",
        vec![
            action(true, "teleport:push:approve", "Push and continue", &detail),
            action(
                false,
                "teleport:push:cancel",
                "Cancel",
                "Cancel Teleport without pushing",
            ),
        ],
    );
    overlay.notice = Some("Choose whether Teleport may push this repository.".to_owned());
    Some(overlay)
}

#[must_use]
pub fn remote_project_create_overlay(draft: &RemoteProjectDraft) -> Overlay {
    let mut overlay = Overlay::new(
        OverlayKind::RemoteProjectCreate,
        "Create Vibe Code Web project",
        vec![
            OverlayItem::new(RemoteProjectField::Name.id(), "Name", &draft.name, false),
            OverlayItem::new(
                RemoteProjectField::DefaultBranch.id(),
                "Default branch",
                &draft.default_branch,
                false,
            ),
            OverlayItem::new(
                RemoteProjectField::Submit.id(),
                "Create project",
                "Validate these values before creating the remote project",
                draft.name.trim().is_empty() || draft.default_branch.trim().is_empty(),
            )
            .with_action(OverlayAction::RemoteProject(RemoteProjectAction::Create {
                name: draft.name.clone(),
                default_branch: draft.default_branch.clone(),
            })),
        ],
    );
    overlay.notice = Some("Edit each field, then choose Create project. Esc cancels.".to_owned());
    overlay
}

#[must_use]
pub fn remote_projects_overlay(view: &Value) -> Overlay {
    let repo_url = view
        .pointer("/context/repoUrl")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let saved_project_id = view
        .pointer("/context/savedLink/projectId")
        .and_then(Value::as_str);
    let mut projects = view
        .pointer("/state/projects")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|project| {
            !project
                .get("isReadOnly")
                .and_then(Value::as_bool)
                .unwrap_or(false)
                && (repo_url.is_empty()
                    || project
                        .get("repositories")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(|repository| repository.get("repoUrl").and_then(Value::as_str))
                        .any(|candidate| same_repository(candidate, repo_url)))
        })
        .collect::<Vec<_>>();
    projects.sort_by_key(|project| {
        let id = project
            .get("projectId")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let repositories = project
            .get("repositories")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        (
            if saved_project_id == Some(id) {
                0
            } else if repositories == 1 {
                1
            } else {
                2
            },
            project
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_ascii_lowercase(),
        )
    });
    let has_projects = !projects.is_empty();
    let mut items = projects
        .into_iter()
        .filter_map(|project| {
            let id = project.get("projectId").and_then(Value::as_str)?;
            let name = project.get("name").and_then(Value::as_str).unwrap_or(id);
            let repositories = project
                .get("repositories")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            let status = if saved_project_id == Some(id) {
                "Currently linked"
            } else if repositories == 1 {
                "Exact match found"
            } else {
                "Working repository found"
            };
            Some(
                OverlayItem::new(
                    format!("remote-project:select:{id}"),
                    name,
                    format!(
                        "{repositories} repo{} · {status}",
                        if repositories == 1 { "" } else { "s" }
                    ),
                    false,
                )
                .with_action(OverlayAction::RemoteProject(
                    RemoteProjectAction::Select {
                        project_id: id.to_owned(),
                    },
                )),
            )
        })
        .collect::<Vec<_>>();
    if view
        .pointer("/state/nextCursor")
        .is_some_and(|cursor| !cursor.is_null())
    {
        items.push(
            OverlayItem::new("remote-project:more", "Load more projects...", "", false)
                .with_action(OverlayAction::RemoteProject(RemoteProjectAction::More)),
        );
    }
    if !items.is_empty() {
        items.insert(
            0,
            OverlayItem::new("heading:projects", "Projects", "", true),
        );
    }
    let suggested_name = view
        .pointer("/context/repoName")
        .and_then(Value::as_str)
        .filter(|name| !name.trim().is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| suggested_project_name(repo_url));
    let default_branch = view
        .pointer("/git/defaultBranch")
        .and_then(Value::as_str)
        .filter(|branch| !branch.trim().is_empty())
        .or_else(|| {
            view.pointer("/git/branch")
                .and_then(Value::as_str)
                .filter(|branch| !branch.trim().is_empty())
        })
        .unwrap_or("main")
        .to_owned();
    // The reference separates the project list from the actions with two blank
    // rows and an `Actions` heading (`picker.py:_option_list_items`).
    items.push(OverlayItem::new("gap:actions:1", "", "", true));
    items.push(OverlayItem::new("gap:actions:2", "", "", true));
    items.push(OverlayItem::new("heading:actions", "Actions", "", true));
    items.push(
        OverlayItem::new(
            "remote-project:create",
            "Create new project",
            if has_projects {
                "repo-linked project"
            } else {
                "recommended"
            },
            false,
        )
        .with_action(OverlayAction::RemoteProject(RemoteProjectAction::Create {
            name: suggested_name,
            default_branch,
        })),
    );
    if view
        .pointer("/context/savedLink")
        .is_some_and(Value::is_object)
    {
        items.push(
            OverlayItem::new("remote-project:unlink", "Unlink project", "", false)
                .with_action(OverlayAction::RemoteProject(RemoteProjectAction::Unlink)),
        );
    }
    let mut overlay = Overlay::new(OverlayKind::RemoteProjects, PICKER_TITLE, items);
    overlay.notice = if view
        .get("savedProjectLinkCleared")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        Some(
            "The saved project link no longer matches this repository. Select a project."
                .to_owned(),
        )
    } else {
        Some(format!("Repository: {}", repository_label(repo_url)))
    };
    overlay
}

/// `app.py:2992` mounts the reference picker under this title, whether or not
/// the selection feeds Teleport.
const PICKER_TITLE: &str = "Vibe Code project";

/// Mirrors `vibe/utils/repository.repo_url_label`: an SSH remote collapses to
/// `host/path`, a URL keeps its authority and path, and `.git` is dropped. Case
/// is preserved, unlike `normalize_repo_url`.
fn repository_label(repo_url: &str) -> String {
    let value = repo_url.trim().trim_end_matches('/');
    let label = value
        .strip_prefix("ssh://")
        .unwrap_or(value)
        .strip_prefix("git@")
        .map(|rest| rest.replacen(':', "/", 1))
        .unwrap_or_else(|| {
            Url::parse(value).ok().map_or_else(
                || value.to_owned(),
                |url| match (url.host_str(), url.path().trim_start_matches('/')) {
                    (Some(host), path) if !path.is_empty() => format!("{host}/{path}"),
                    _ => value.to_owned(),
                },
            )
        });
    let label = label.trim_end_matches('/');
    label.strip_suffix(".git").unwrap_or(label).to_owned()
}

fn suggested_project_name(repo_url: &str) -> String {
    normalized_repository(repo_url)
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or("vibe-project")
        .to_owned()
}

fn same_repository(left: &str, right: &str) -> bool {
    normalized_repository(left) == normalized_repository(right)
}

fn normalized_repository(value: &str) -> String {
    let value = value.trim();
    let normalized = if let Some((authority, path)) = value.split_once(':')
        && !value.contains("://")
        && !authority.contains('/')
    {
        let host = authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host);
        format!("{host}/{}", path.trim_start_matches('/'))
    } else if let Ok(url) = Url::parse(value) {
        match (url.host_str(), url.path().trim_matches('/')) {
            (Some(host), path) if !path.is_empty() => format!("{host}/{path}"),
            _ => value.to_owned(),
        }
    } else {
        value.to_owned()
    };
    let normalized = normalized.trim().trim_end_matches('/').to_ascii_lowercase();
    normalized
        .strip_suffix(".git")
        .unwrap_or(&normalized)
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creation_is_a_separate_editable_confirmation_form() {
        let draft = RemoteProjectDraft {
            name: "parity".to_owned(),
            default_branch: "main".to_owned(),
        };
        let overlay = remote_project_create_overlay(&draft);
        assert_eq!(overlay.kind, OverlayKind::RemoteProjectCreate);
        assert_eq!(overlay.items.len(), 3);
        assert!(matches!(
            &overlay.items[2].action,
            OverlayAction::RemoteProject(RemoteProjectAction::Create {
                name,
                default_branch,
            }) if name == "parity" && default_branch == "main"
        ));

        let invalid = remote_project_create_overlay(&RemoteProjectDraft {
            name: " ".to_owned(),
            default_branch: "main".to_owned(),
        });
        assert!(invalid.items[2].disabled);
    }

    #[test]
    fn repository_identity_matches_https_and_scp_syntax() {
        assert!(same_repository(
            "https://github.com/acme/repo.git",
            "git@github.com:acme/repo"
        ));
    }
}
