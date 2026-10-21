use std::collections::BTreeMap;

use super::merge_headers;

/// A header named again in another case replaces the first spelling's value
/// rather than travelling beside it, which is where this port departs from
/// the reference's dictionary merge (`docs/parity.md`, accepted divergences).
#[test]
fn a_header_named_in_another_case_is_sent_once() {
    let mut headers = vec![
        ("User-Agent".to_owned(), "vibe".to_owned()),
        ("Content-Type".to_owned(), "application/json".to_owned()),
    ];
    merge_headers(&mut headers, [("user-agent", "custom")]);
    merge_headers(
        &mut headers,
        &BTreeMap::from([("x-extra".to_owned(), "1".to_owned())]),
    );
    merge_headers(&mut headers, &[("X-EXTRA".to_owned(), "2".to_owned())]);

    assert_eq!(
        headers,
        [
            ("User-Agent".to_owned(), "custom".to_owned()),
            ("Content-Type".to_owned(), "application/json".to_owned()),
            ("x-extra".to_owned(), "2".to_owned()),
        ]
    );
}
