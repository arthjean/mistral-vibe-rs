use super::*;

fn spelled(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[test]
fn a_missing_dropped_path_stays_as_typed() {
    let workspace = tempfile::tempdir().expect("workspace");
    let missing = spelled(&workspace.path().join("absent.png"));
    assert_eq!(normalize_pasted_text(&missing), missing);
    let typed = format!("look at {missing}");
    assert_eq!(normalize_typed_text(&typed), typed);
}

#[test]
fn a_unc_share_is_quoted_like_every_windows_root() {
    let quoted = quote_path_if_needed(r"\\server\share\a.png");
    if cfg!(windows) {
        assert_eq!(quoted, r"'\\server\share\a.png'");
    } else {
        assert_eq!(quoted, r"\\server\share\a.png");
    }
}

#[cfg(windows)]
mod windows {
    use std::path::PathBuf;

    use super::*;
    use crate::path_resources::{PathResourceKind, build_path_prompt_payload};

    /// A PNG fixture under `directory`, named as the operator would see it.
    fn image(directory: &Path, name: &str) -> PathBuf {
        let path = directory.join(name);
        std::fs::write(&path, b"image").expect("image fixture");
        path
    }

    #[test]
    fn a_dropped_path_with_a_space_becomes_a_quoted_mention() {
        let workspace = tempfile::tempdir().expect("workspace");
        let path = spelled(&image(workspace.path(), "image one.png"));
        let expected = format!("@'{path}'");
        // Windows Terminal and conhost quote a dropped path that holds a space,
        // and a paste may carry it bare.
        for dropped in [format!("\"{path}\""), path.clone()] {
            let mention = normalize_pasted_text(&dropped);
            assert_eq!(mention, expected, "pasted {dropped:?}");
            assert!(!mention.contains("\\ "), "escaped space in {mention:?}");
        }
        assert_eq!(normalize_typed_text(&format!("\"{path}\"")), expected);
    }

    #[test]
    fn an_unquoted_drive_path_typed_in_a_prompt_is_an_image_mention() {
        let workspace = tempfile::tempdir().expect("workspace");
        let path = image(workspace.path(), "x.png");
        let typed = format!("describe {} please", spelled(&path));
        let rewritten = normalize_typed_text(&typed);
        assert_eq!(rewritten, format!("describe @'{}' please", spelled(&path)));

        let payload = build_path_prompt_payload(workspace.path(), &rewritten);
        let images = payload
            .resources
            .iter()
            .filter(|resource| resource.kind == PathResourceKind::Image)
            .collect::<Vec<_>>();
        assert_eq!(images.len(), 1, "no image mention in {rewritten:?}");
        assert_eq!(
            crate::platform::canonicalize(&images[0].path).expect("mentioned image"),
            crate::platform::canonicalize(&path).expect("image fixture"),
        );
    }

    #[test]
    fn a_forward_slash_drive_path_is_a_root_too() {
        let workspace = tempfile::tempdir().expect("workspace");
        let path = spelled(&image(workspace.path(), "y.png")).replace('\\', "/");
        assert_eq!(normalize_typed_text(&path), format!("@'{path}'"));
    }

    #[test]
    fn a_path_quoted_for_powershell_reads_a_doubled_apostrophe_as_one() {
        let workspace = tempfile::tempdir().expect("workspace");
        let path = spelled(&image(workspace.path(), "Capture d'écran.png"));
        let dropped = format!("'{}'", path.replace('\'', "''"));
        assert_eq!(normalize_pasted_text(&dropped), format!("@\"{path}\""));
    }

    #[test]
    fn a_dropped_unc_path_becomes_a_quoted_mention() {
        let workspace = tempfile::tempdir().expect("workspace");
        let local = image(workspace.path(), "a.png");
        let local = crate::platform::canonicalize(&local).expect("image fixture");
        let local = spelled(&local);
        let Some((drive, rest)) = local.split_once(":\\") else {
            panic!("the fixture is not on a drive: {local}");
        };
        // The administrative share of the fixture's own drive is the one UNC
        // path a test can reach without provisioning a share.
        let unc = format!(r"\\localhost\{drive}$\{rest}");
        if !Path::new(&unc).is_file() {
            eprintln!("skipping the live UNC probe: {unc} is not reachable from this token");
            return;
        }
        assert_eq!(normalize_pasted_text(&unc), format!("@'{unc}'"));
        assert_eq!(normalize_typed_text(&unc), format!("@'{unc}'"));
    }
}
