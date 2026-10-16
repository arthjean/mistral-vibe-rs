use std::path::Path;

use super::file_uri;

#[cfg(not(windows))]
#[test]
fn a_posix_path_takes_the_bare_file_scheme_and_encodes_reserved_bytes() {
    assert_eq!(
        file_uri(Path::new("/work/my notes.txt")),
        "file:///work/my%20notes.txt"
    );
}

#[cfg(windows)]
#[test]
fn a_drive_path_publishes_the_drive_and_forward_slashes() {
    assert_eq!(
        file_uri(Path::new(r"C:\Users\me\my notes.txt")),
        "file:///C:/Users/me/my%20notes.txt"
    );
}

#[cfg(windows)]
#[test]
fn a_unc_path_keeps_its_host_after_the_scheme() {
    assert_eq!(
        file_uri(Path::new(r"\\server\share\notes.txt")),
        "file://server/share/notes.txt"
    );
}
