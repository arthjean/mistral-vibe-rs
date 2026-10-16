use std::path::Path;

/// The server reads the vibe home and expands `~` through the resolver every
/// binary shares, so it cannot place state where `vibe` and `vibe-acp` do not.
#[test]
fn the_server_resolves_the_home_every_entry_point_resolves() {
    let working_directory = std::env::current_dir().expect("working directory");
    if let Ok(shared) = vibe_core::platform::vibe_home(&working_directory) {
        assert_eq!(super::vibe_home(), shared);
    }
    assert_eq!(
        super::expand_home(Path::new("~/x")),
        vibe_core::platform::expand_home(Path::new("~/x"))
    );
}
