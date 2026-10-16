use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use super::{HomeNotFound, expand_home_with, user_home_in, vibe_home_in};

fn environment(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
    let values = pairs
        .iter()
        .map(|(name, value)| ((*name).to_owned(), OsString::from(value)))
        .collect::<BTreeMap<_, _>>();
    move |name| values.get(name).cloned()
}

fn windows_home(pairs: &[(&str, &str)]) -> Result<PathBuf, HomeNotFound> {
    vibe_home_in(&environment(pairs), true, Path::new("project"))
}

fn posix_home(pairs: &[(&str, &str)]) -> Result<PathBuf, HomeNotFound> {
    vibe_home_in(&environment(pairs), false, Path::new("project"))
}

#[test]
fn windows_reads_the_profile_and_never_home() {
    let profile = PathBuf::from(r"C:\Users\A");
    assert_eq!(
        windows_home(&[("USERPROFILE", r"C:\Users\A")]),
        Ok(profile.join(".vibe"))
    );
    // Git Bash exports `HOME`; the reference ignores it on Windows.
    assert_eq!(
        windows_home(&[("USERPROFILE", r"C:\Users\A"), ("HOME", "/c/other")]),
        Ok(profile.join(".vibe"))
    );
    assert_eq!(
        user_home_in(
            &environment(&[("HOMEDRIVE", "D:"), ("HOMEPATH", r"\Users\B")]),
            true
        ),
        Some(PathBuf::from(r"D:\Users\B"))
    );
}

#[test]
fn windows_without_a_profile_refuses_rather_than_using_the_workspace() {
    assert_eq!(windows_home(&[]), Err(HomeNotFound));
    assert_eq!(windows_home(&[("HOME", "/c/Users/A")]), Err(HomeNotFound));
    assert_eq!(windows_home(&[("HOMEDRIVE", "C:")]), Err(HomeNotFound));
    assert_eq!(windows_home(&[("USERPROFILE", "")]), Err(HomeNotFound));
    assert_eq!(
        windows_home(&[("VIBE_HOME", r"~\state")]),
        Err(HomeNotFound),
        "a tilde with no home to expand it against is refused too"
    );
    assert!(HomeNotFound.to_string().contains("VIBE_HOME"));
}

#[test]
fn vibe_home_wins_on_every_platform_and_expands_a_tilde() {
    assert_eq!(
        windows_home(&[("VIBE_HOME", r"D:\state"), ("USERPROFILE", r"C:\Users\A")]),
        Ok(PathBuf::from(r"D:\state"))
    );
    assert_eq!(
        windows_home(&[("VIBE_HOME", r"D:\state")]),
        Ok(PathBuf::from(r"D:\state")),
        "an explicit home needs no profile"
    );
    assert_eq!(
        posix_home(&[("VIBE_HOME", "/srv/state"), ("HOME", "/home/a")]),
        Ok(PathBuf::from("/srv/state"))
    );
    assert_eq!(
        posix_home(&[("VIBE_HOME", "~/state"), ("HOME", "/home/a")]),
        Ok(Path::new("/home/a").join("state"))
    );
    assert_eq!(
        windows_home(&[("VIBE_HOME", r"~\state"), ("USERPROFILE", r"C:\Users\A")]),
        Ok(Path::new(r"C:\Users\A").join("state"))
    );
    assert_eq!(
        posix_home(&[("VIBE_HOME", ""), ("HOME", "/home/a")]),
        Ok(Path::new("/home/a").join(".vibe")),
        "an empty VIBE_HOME is unset, as the reference's truthiness test reads it"
    );
}

#[test]
fn posix_keeps_home_and_its_workspace_fallback() {
    assert_eq!(
        posix_home(&[("HOME", "/home/a"), ("USERPROFILE", "/ignored")]),
        Ok(Path::new("/home/a").join(".vibe"))
    );
    assert_eq!(posix_home(&[]), Ok(Path::new("project").join(".vibe")));
}

#[test]
fn a_tilde_expands_only_before_a_separator() {
    let home = Path::new("/home/a");
    assert_eq!(expand_home_with(Path::new("~"), Some(home), false), home);
    assert_eq!(
        expand_home_with(Path::new("~/x/y"), Some(home), false),
        home.join("x").join("y")
    );
    assert_eq!(
        expand_home_with(Path::new("~user/x"), Some(home), false),
        Path::new("~user/x")
    );
    assert_eq!(
        expand_home_with(Path::new(r"~\x"), Some(home), false),
        Path::new(r"~\x"),
        "a backslash separates only on Windows"
    );
    assert_eq!(
        expand_home_with(Path::new("~/x"), None, false),
        Path::new("~/x")
    );
}
