//! The foreign adapters' shared helpers. What the adapters resolve is
//! measured against the reference in `plugins_parity_tests`.

use std::path::Path;

#[test]
fn a_command_source_name_drops_only_the_last_suffix() {
    let base = Path::new("/p/commands");
    assert_eq!(
        super::command_source_name(Path::new("/p/commands/a/b.md"), base),
        "a/b"
    );
    assert_eq!(
        super::command_source_name(Path::new("/p/commands/x.y.md"), base),
        "x.y"
    );
    assert_eq!(
        super::command_source_name(Path::new("/p/commands/.md"), base),
        ".md"
    );
}
