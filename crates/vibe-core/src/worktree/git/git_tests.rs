use std::path::Path;

use super::is_untrusted_project_executable;

/// A binary inside the checkout stays untrusted however its path is spelled:
/// `platform::canonicalize` keeps the verbatim prefix below a reserved
/// component, so the candidate and the project can come back in different
/// forms.
#[cfg(windows)]
#[test]
fn a_project_binary_spelled_verbatim_stays_untrusted() {
    let project = Path::new(r"C:\work\proj");
    for candidate in [
        r"\\?\C:\work\proj\con\bin\git.exe",
        r"C:\work\proj\bin\git.exe",
    ] {
        assert!(
            is_untrusted_project_executable(Path::new(candidate), project),
            "{candidate}"
        );
    }
    assert!(is_untrusted_project_executable(
        Path::new(r"C:\work\proj\bin\git.exe"),
        Path::new(r"\\?\C:\work\proj"),
    ));
    assert!(!is_untrusted_project_executable(
        Path::new(r"\\?\C:\Program Files\Git\cmd\git.exe"),
        project,
    ));
}

#[cfg(not(windows))]
#[test]
fn a_project_binary_is_untrusted_and_an_outside_one_is_not() {
    let project = Path::new("/work/proj");
    assert!(is_untrusted_project_executable(
        Path::new("/work/proj/bin/git"),
        project
    ));
    assert!(!is_untrusted_project_executable(
        Path::new("/usr/bin/git"),
        project
    ));
}
