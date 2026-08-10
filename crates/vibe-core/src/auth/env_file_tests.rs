//! The dotenv editing primitives: replacement in place, survival of foreign
//! lines, the owner-only creation mode, and the no-op paths.

use std::fs;

use super::env_file::{remove_env_file_key, write_env_file_key};
use crate::config::DotenvValues;

#[test]
fn a_write_creates_parents_and_an_owner_only_file() {
    let temporary = tempfile::tempdir().expect("temporary root");
    let path = temporary.path().join("nested").join(".env");
    write_env_file_key(&path, "MISTRAL_API_KEY", "fresh-value").expect("the write succeeds");
    let contents = fs::read_to_string(&path).expect("the file is readable");
    assert_eq!(contents, "MISTRAL_API_KEY='fresh-value'\n");
    assert_eq!(
        DotenvValues::load(&path).file_variable("MISTRAL_API_KEY"),
        Some("fresh-value")
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&path).expect("metadata").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "the fallback file holds a credential");
    }
}

#[test]
fn a_rewrite_replaces_the_declaring_line_and_keeps_the_rest() {
    let temporary = tempfile::tempdir().expect("temporary root");
    let path = temporary.path().join(".env");
    fs::write(
        &path,
        "# comment\nOTHER=kept\nexport MISTRAL_API_KEY=old\nTRAILING=kept\n",
    )
    .expect("fixture");
    write_env_file_key(&path, "MISTRAL_API_KEY", "new-value").expect("the rewrite succeeds");
    let contents = fs::read_to_string(&path).expect("readable");
    assert_eq!(
        contents,
        "# comment\nOTHER=kept\nMISTRAL_API_KEY='new-value'\nTRAILING=kept\n"
    );
}

#[test]
fn a_removal_drops_only_the_declaring_line() {
    let temporary = tempfile::tempdir().expect("temporary root");
    let path = temporary.path().join(".env");
    fs::write(&path, "OTHER=kept\nMISTRAL_API_KEY='secret'\n").expect("fixture");
    remove_env_file_key(&path, "MISTRAL_API_KEY").expect("the removal succeeds");
    assert_eq!(fs::read_to_string(&path).expect("readable"), "OTHER=kept\n");
}

#[test]
fn removing_from_a_missing_file_or_a_missing_key_is_a_no_op() {
    let temporary = tempfile::tempdir().expect("temporary root");
    let path = temporary.path().join(".env");
    remove_env_file_key(&path, "MISTRAL_API_KEY").expect("a missing file is fine");
    assert!(!path.exists());
    fs::write(&path, "OTHER=kept\n").expect("fixture");
    remove_env_file_key(&path, "MISTRAL_API_KEY").expect("a missing key is fine");
    assert_eq!(fs::read_to_string(&path).expect("readable"), "OTHER=kept\n");
}

#[test]
fn a_rewrite_keeps_the_mode_the_operator_gave_an_existing_file() {
    let temporary = tempfile::tempdir().expect("temporary root");
    let path = temporary.path().join(".env");
    fs::write(&path, "OTHER=kept\n").expect("fixture");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).expect("fixture chmod");
    }
    write_env_file_key(&path, "MISTRAL_API_KEY", "fresh-value").expect("the rewrite succeeds");
    assert_eq!(
        fs::read_to_string(&path).expect("readable"),
        "OTHER=kept\nMISTRAL_API_KEY='fresh-value'\n"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Reference `rewrite` restores the mode it read through `lstat`.
        let mode = fs::metadata(&path).expect("metadata").permissions().mode();
        assert_eq!(mode & 0o777, 0o640);
    }
}

/// The reference declines to follow symlinks so a credential never lands at a
/// path the operator did not name. A rewrite through the link would write the
/// key into the target.
#[cfg(unix)]
#[test]
fn a_write_replaces_a_symlinked_dotenv_instead_of_writing_through_it() {
    use std::os::unix::fs::PermissionsExt;

    let temporary = tempfile::tempdir().expect("temporary root");
    let target = temporary.path().join("elsewhere.env");
    let path = temporary.path().join(".env");
    fs::write(&target, "OTHER=kept\n").expect("fixture");
    std::os::unix::fs::symlink(&target, &path).expect("the symlink fixture");
    write_env_file_key(&path, "MISTRAL_API_KEY", "fresh-value").expect("the write succeeds");
    assert_eq!(
        fs::read_to_string(&target).expect("readable"),
        "OTHER=kept\n",
        "the symlink target never receives the credential"
    );
    assert!(
        fs::symlink_metadata(&path)
            .expect("metadata")
            .file_type()
            .is_file(),
        "the link itself is replaced by a regular file"
    );
    assert_eq!(
        DotenvValues::load(&path).file_variable("MISTRAL_API_KEY"),
        Some("fresh-value")
    );
    let mode = fs::metadata(&path).expect("metadata").permissions().mode();
    assert_eq!(mode & 0o777, 0o600, "a path that was not a regular file");
}

#[test]
fn a_value_carrying_a_quote_survives_the_round_trip() {
    let temporary = tempfile::tempdir().expect("temporary root");
    let path = temporary.path().join(".env");
    write_env_file_key(&path, "MISTRAL_API_KEY", "it's-quoted").expect("the write succeeds");
    assert_eq!(
        DotenvValues::load(&path).file_variable("MISTRAL_API_KEY"),
        Some("it's-quoted")
    );
}
