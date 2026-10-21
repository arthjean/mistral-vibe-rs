use std::path::Path;

use super::{canonicalize, resolve_lenient, simplified, strip_verbatim_prefix};

#[test]
fn the_reference_form_drops_every_verbatim_prefix() {
    assert_eq!(strip_verbatim_prefix(r"\\?\C:\work\proj"), r"C:\work\proj");
    assert_eq!(
        strip_verbatim_prefix(r"\\?\UNC\server\share\repo"),
        r"\\server\share\repo"
    );
    assert_eq!(strip_verbatim_prefix("/work/proj"), "/work/proj");
}

#[test]
fn a_canonical_path_carries_no_verbatim_prefix() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let canonical = canonicalize(directory.path()).expect("the directory resolves");
    let text = canonical.to_string_lossy();
    assert!(!text.starts_with(r"\\?\"), "{text}");
    assert!(canonical.is_absolute(), "{text}");
    assert!(canonical.is_dir(), "{text}");
}

#[test]
fn a_missing_tail_resolves_lexically_under_its_existing_prefix() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let root = canonicalize(directory.path()).expect("the directory resolves");
    std::fs::create_dir(root.join("kept")).expect("directory");
    assert_eq!(
        resolve_lenient(&root.join("missing/../kept/./new.txt")),
        root.join("kept/new.txt")
    );
    assert_eq!(resolve_lenient(&root.join("kept/..")), root);
}

#[cfg(windows)]
#[test]
fn a_path_a_plain_spelling_cannot_name_keeps_its_prefix() {
    // A UNC share and a reserved device name mean something else without the
    // prefix, so both stay verbatim and stay usable.
    for kept in [r"\\?\UNC\server\share\file.txt", r"\\?\C:\work\con.txt"] {
        assert_eq!(simplified(Path::new(kept)), Path::new(kept));
    }
    assert_eq!(
        simplified(Path::new(r"\\?\C:\work\proj")),
        Path::new(r"C:\work\proj")
    );

    let directory = tempfile::tempdir().expect("temporary directory");
    let verbatim = std::fs::canonicalize(directory.path()).expect("the directory resolves");
    let reserved = verbatim.join("con.txt");
    std::fs::write(&reserved, "device name").expect("a verbatim path writes a reserved name");
    assert_eq!(simplified(&reserved), reserved.as_path());
    assert_eq!(
        std::fs::read_to_string(simplified(&reserved)).expect("and reads it back"),
        "device name"
    );
}

#[cfg(windows)]
#[test]
fn a_non_unicode_path_keeps_its_prefix() {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use std::path::PathBuf;

    // An unpaired surrogate: valid on NTFS, not representable as UTF-8.
    let mut wide = r"\\?\C:\work\".encode_utf16().collect::<Vec<_>>();
    wide.push(0xD800);
    let path = PathBuf::from(OsString::from_wide(&wide));
    assert_eq!(simplified(&path), path.as_path());
}

#[cfg(not(windows))]
#[test]
fn simplification_is_the_identity_off_windows() {
    let path = Path::new(r"\\?\C:\work");
    assert_eq!(simplified(path), path);
}
