//! Headers for the connector tools a Unified session reaches through
//! `run_typescript`, keyed by their `connector_<name>.<function>` route.
//!
//! The harness reports them under that raw key; the adapter shows the verb
//! and noun a person reads instead (reference `_TOOL_LABELS` in
//! `vibe/app_server/_unified_tool_projection.py`). Each row is the live verb,
//! the noun and the settled verb.

/// The header of a connector tool this surface names, if any.
pub(crate) fn label(name: &str) -> Option<(&'static str, &'static str, &'static str)> {
    Some(match name {
        "connector_github_app.add_comment_to_pending_review" => {
            ("Adding", "a review comment", "Added")
        }
        "connector_github_app.add_issue_comment" => ("Commenting", "on an issue", "Commented"),
        "connector_github_app.add_reply_to_pull_request_comment" => {
            ("Replying", "to a pull request comment", "Replied")
        }
        "connector_github_app.create_branch" => ("Creating", "a branch", "Created"),
        "connector_github_app.create_or_update_file" => {
            ("Creating", "or updating a file", "Created")
        }
        "connector_github_app.create_pull_request" => ("Creating", "a pull request", "Created"),
        "connector_github_app.create_repository" => ("Creating", "a repository", "Created"),
        "connector_github_app.delete_file" => ("Deleting", "a file", "Deleted"),
        "connector_github_app.fork_repository" => ("Forking", "a repository", "Forked"),
        "connector_github_app.get_commit" => ("Reading", "a commit", "Read"),
        "connector_github_app.get_file_contents" => ("Reading", "file contents", "Read"),
        "connector_github_app.get_label" => ("Reading", "a label", "Read"),
        "connector_github_app.get_latest_release" => ("Reading", "the latest release", "Read"),
        "connector_github_app.get_me" => ("Reading", "GitHub profile", "Read"),
        "connector_github_app.get_release_by_tag" => ("Reading", "a release by tag", "Read"),
        "connector_github_app.get_tag" => ("Reading", "a tag", "Read"),
        "connector_github_app.get_team_members" => ("Reading", "team members", "Read"),
        "connector_github_app.get_teams" => ("Reading", "teams", "Read"),
        "connector_github_app.issue_read" => ("Reading", "an issue", "Read"),
        "connector_github_app.issue_write" => ("Writing", "an issue", "Wrote"),
        "connector_github_app.list_branches" => ("Listing", "branches", "Listed"),
        "connector_github_app.list_commits" => ("Listing", "commits", "Listed"),
        "connector_github_app.list_issue_fields" => ("Listing", "issue fields", "Listed"),
        "connector_github_app.list_issue_types" => ("Listing", "issue types", "Listed"),
        "connector_github_app.list_issues" => ("Listing", "issues", "Listed"),
        "connector_github_app.list_pull_requests" => ("Listing", "pull requests", "Listed"),
        "connector_github_app.list_releases" => ("Listing", "releases", "Listed"),
        "connector_github_app.list_repository_collaborators" => {
            ("Listing", "repository collaborators", "Listed")
        }
        "connector_github_app.list_tags" => ("Listing", "tags", "Listed"),
        "connector_github_app.merge_pull_request" => ("Merging", "a pull request", "Merged"),
        "connector_github_app.pull_request_read" => ("Reading", "a pull request", "Read"),
        "connector_github_app.pull_request_review_write" => {
            ("Writing", "a pull request review", "Wrote")
        }
        "connector_github_app.push_files" => ("Pushing", "files", "Pushed"),
        "connector_github_app.request_copilot_review" => {
            ("Requesting", "a Copilot review", "Requested")
        }
        "connector_github_app.run_secret_scanning" => ("Scanning", "for secrets", "Scanned"),
        "connector_github_app.search_code" => ("Searching", "code", "Searched"),
        "connector_github_app.search_commits" => ("Searching", "commits", "Searched"),
        "connector_github_app.search_issues" => ("Searching", "issues", "Searched"),
        "connector_github_app.search_pull_requests" => ("Searching", "pull requests", "Searched"),
        "connector_github_app.search_repositories" => ("Searching", "repositories", "Searched"),
        "connector_github_app.search_users" => ("Searching", "users", "Searched"),
        "connector_github_app.sub_issue_write" => ("Writing", "a sub-issue", "Wrote"),
        "connector_github_app.update_pull_request" => ("Updating", "a pull request", "Updated"),
        "connector_github_app.update_pull_request_branch" => {
            ("Updating", "a pull request branch", "Updated")
        }
        "connector_google_calendar.create_event" => ("Creating", "a calendar event", "Created"),
        "connector_google_calendar.delete_event" => ("Deleting", "a calendar event", "Deleted"),
        "connector_google_calendar.get_event" => ("Reading", "a calendar event", "Read"),
        "connector_google_calendar.list_calendars" => ("Listing", "calendars", "Listed"),
        "connector_google_calendar.list_events" => ("Listing", "calendar events", "Listed"),
        "connector_google_calendar.respond_to_event" => {
            ("Responding", "to a calendar event", "Responded")
        }
        "connector_google_calendar.update_event" => ("Updating", "a calendar event", "Updated"),
        "connector_google_drive_mcp.copy_file" => ("Copying", "a Drive file", "Copied"),
        "connector_google_drive_mcp.create_file" => ("Creating", "a Drive file", "Created"),
        "connector_google_drive_mcp.download_file_content" => {
            ("Downloading", "a Drive file", "Downloaded")
        }
        "connector_google_drive_mcp.get_file_metadata" => {
            ("Reading", "Drive file metadata", "Read")
        }
        "connector_google_drive_mcp.get_file_permissions" => {
            ("Reading", "Drive file permissions", "Read")
        }
        "connector_google_drive_mcp.list_recent_files" => {
            ("Listing", "recent Drive files", "Listed")
        }
        "connector_google_drive_mcp.read_file_content" => ("Reading", "a Drive file", "Read"),
        "connector_google_drive_mcp.search_files" => ("Searching", "Drive files", "Searched"),
        "connector_mistral_ai.delete_skill" => ("Deleting", "a skill", "Deleted"),
        "connector_mistral_ai.list_skills" => ("Listing", "skills", "Listed"),
        "connector_mistral_ai.read_skill" => ("Reading", "a skill", "Read"),
        "connector_mistral_ai.read_skill_asset" => ("Reading", "a skill asset", "Read"),
        "connector_mistral_ai.search_workspace_members" => {
            ("Searching", "workspace members", "Searched")
        }
        "connector_mistral_ai.set_skill_sharing" => ("Setting", "skill sharing", "Set"),
        "connector_mistral_ai.write_skill" => ("Writing", "a skill", "Wrote"),
        "connector_slack.slack_add_reaction" => ("Adding", "a Slack reaction", "Added"),
        "connector_slack.slack_create_canvas" => ("Creating", "a Slack canvas", "Created"),
        "connector_slack.slack_create_conversation" => {
            ("Creating", "a Slack conversation", "Created")
        }
        "connector_slack.slack_get_reactions" => ("Reading", "Slack reactions", "Read"),
        "connector_slack.slack_list_channel_members" => {
            ("Listing", "Slack channel members", "Listed")
        }
        "connector_slack.slack_read_canvas" => ("Reading", "a Slack canvas", "Read"),
        "connector_slack.slack_read_channel" => ("Reading", "a Slack channel", "Read"),
        "connector_slack.slack_read_file" => ("Reading", "a Slack file", "Read"),
        "connector_slack.slack_read_thread" => ("Reading", "a Slack thread", "Read"),
        "connector_slack.slack_read_user_profile" => ("Reading", "a Slack user profile", "Read"),
        "connector_slack.slack_schedule_message" => ("Scheduling", "a Slack message", "Scheduled"),
        "connector_slack.slack_search_channels" => ("Searching", "Slack channels", "Searched"),
        "connector_slack.slack_search_emojis" => ("Searching", "Slack emojis", "Searched"),
        "connector_slack.slack_search_public" => ("Searching", "public Slack messages", "Searched"),
        "connector_slack.slack_search_public_and_private" => {
            ("Searching", "Slack messages", "Searched")
        }
        "connector_slack.slack_search_users" => ("Searching", "Slack users", "Searched"),
        "connector_slack.slack_send_message" => ("Sending", "a Slack message", "Sent"),
        "connector_slack.slack_send_message_draft" => ("Drafting", "a Slack message", "Drafted"),
        "connector_slack.slack_update_canvas" => ("Updating", "a Slack canvas", "Updated"),
        "connector_web_search.finance_search" => ("Searching", "financial data", "Searched"),
        "connector_web_search.news_search" => ("Searching", "news", "Searched"),
        "connector_web_search.open_url" => ("Opening", "a URL", "Opened"),
        "connector_web_search.weather_search" => ("Searching", "weather", "Searched"),
        "connector_web_search.web_search" => ("Searching", "the web", "Searched"),
        _ => return None,
    })
}
