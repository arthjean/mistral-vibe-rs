use super::*;

#[test]
fn github_remotes_parse_in_every_spelling_the_parser_accepts() {
    let parsed = |url: &str| parse_github_url(url);
    let expected = Some(("Owner".to_owned(), "Repo".to_owned()));
    for url in [
        "https://github.com/Owner/Repo.git",
        "https://github.com/Owner/Repo",
        "https://user:token@github.com/Owner/Repo.git",
        "git+https://github.com/Owner/Repo/",
        "git@github.com:Owner/Repo.git",
        "ssh://git@github.com/Owner/Repo.git",
        "git://github.com/Owner/Repo.git",
        "https://github.com/Owner/Repo/tree/main/src",
    ] {
        assert_eq!(parsed(url), expected, "{url}");
    }
    assert_eq!(
        parsed("https://gist.github.com/Owner/Repo"),
        Some(("Owner".to_owned(), "Repo".to_owned()))
    );
}

#[test]
fn a_remote_on_another_host_is_not_github() {
    for url in [
        "https://gitlab.com/owner/repo.git",
        "https://www.github.com/owner/repo.git",
        "git@github.example.com:owner/repo.git",
        "/srv/git/repo.git",
        "",
    ] {
        assert_eq!(parse_github_url(url), None, "{url}");
    }
}

#[test]
fn repository_urls_normalize_to_one_comparable_spelling() {
    assert_eq!(
        normalize_repo_url("git@github.com:Owner/Repo.git"),
        "github.com/owner/repo"
    );
    assert_eq!(
        normalize_repo_url(" https://github.com/Owner/Repo.git/ "),
        "github.com/owner/repo"
    );
    assert_eq!(
        normalize_repo_url("https://github.com/owner/repo?tab=readme"),
        "github.com/owner/repo"
    );
    assert_eq!(
        normalize_repo_url("ssh://git@github.com/owner/repo"),
        "git@github.com/owner/repo"
    );
    assert_eq!(normalize_repo_url("Not A URL/"), "not a url");
}
