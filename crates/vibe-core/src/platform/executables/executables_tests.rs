use super::*;

#[test]
fn a_name_with_an_extension_is_tried_as_written() {
    assert_eq!(executable_names("npx.cmd"), ["npx.cmd"]);
    assert_eq!(executable_names("server.py"), ["server.py"]);
}

/// Windows tries every `PATHEXT` extension in order, so `npx` reaches the
/// `npx.cmd` shim; elsewhere the bare name is the only candidate.
#[test]
fn a_bare_name_is_tried_under_the_host_extensions() {
    let names = executable_names("npx");
    if cfg!(windows) {
        assert!(names.len() > 1, "{names:?}");
        assert!(names.contains(&"npx.cmd".to_owned()), "{names:?}");
        assert!(
            names.iter().all(|name| name.starts_with("npx.")),
            "{names:?}"
        );
    } else {
        assert_eq!(names, ["npx"]);
    }
}

#[test]
fn a_quoted_path_entry_is_read_without_its_quotes() {
    assert_eq!(
        strip_quotes(PathBuf::from("\"C:\\Program Files\\nodejs\"")),
        PathBuf::from("C:\\Program Files\\nodejs")
    );
    assert_eq!(
        strip_quotes(PathBuf::from("/usr/bin")),
        PathBuf::from("/usr/bin")
    );
}
