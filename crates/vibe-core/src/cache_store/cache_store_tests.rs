use std::fs;

use super::CacheStore;

#[test]
fn a_write_updates_its_section_where_the_document_holds_it() {
    let home = tempfile::tempdir().expect("vibe home");
    let store = CacheStore::new(home.path());
    fs::write(
        store.path(),
        "plain = 1\n\n[first]\nkept = \"yes\"\nvalue = 1\n\n[second]\nvalue = 2\n",
    )
    .expect("cache fixture");

    store.write_section(
        "first",
        toml::Table::from_iter([("value".to_owned(), 3.into())]),
    );
    store.write_section(
        "plain",
        toml::Table::from_iter([("value".to_owned(), 4.into())]),
    );

    let document: toml::Table =
        toml::from_str(&fs::read_to_string(store.path()).expect("cache file")).expect("cache toml");
    assert_eq!(
        document.keys().collect::<Vec<_>>(),
        ["plain", "first", "second"]
    );
    assert_eq!(
        document["first"]
            .as_table()
            .map(|table| table.keys().map(String::as_str).collect::<Vec<_>>()),
        Some(vec!["kept", "value"])
    );
    assert_eq!(document["first"]["value"].as_integer(), Some(3));
    assert_eq!(store.read_section("plain")["value"].as_integer(), Some(4));
}
