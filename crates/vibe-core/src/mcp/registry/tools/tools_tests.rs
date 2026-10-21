use super::*;

#[test]
fn persisted_disable_entries_resolve_onto_the_published_name() {
    let registered = vec!["docs_search".to_owned(), "docs_read".to_owned()];
    let configured = BTreeSet::from([
        // The published name, written by a current build.
        "docs_search".to_owned(),
        // The bare remote name, which the operator may type.
        "read".to_owned(),
        // The `mcp_`-prefixed name this port published before it adopted
        // the reference naming rule.
        "mcp_docs_read".to_owned(),
        // An entry naming no tool of this server.
        "absent".to_owned(),
    ]);

    assert_eq!(
        normalize_disabled_tools("docs", &registered, &configured),
        BTreeSet::from(["docs_search".to_owned(), "docs_read".to_owned()]),
        "a preference written before the rename must still disable its tool"
    );
}

/// An entry that is both a remote name and another tool's published name
/// disables the remote tool it names, as the reference reads it.
#[test]
fn a_remote_name_outranks_the_published_name_it_collides_with() {
    let registered = vec!["docs_search".to_owned(), "docs_docs_search".to_owned()];
    let configured = BTreeSet::from(["docs_search".to_owned()]);

    assert_eq!(
        normalize_disabled_tools("docs", &registered, &configured),
        BTreeSet::from(["docs_docs_search".to_owned()])
    );
}
