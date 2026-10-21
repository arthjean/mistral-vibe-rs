use super::yaml_scalar;

/// What the pinned reference's `yaml.safe_dump` writes for each string: plain
/// where PyYAML's resolver reads the plain form back as a string, quoted where
/// it would read an integer, a float, a timestamp or a merge or value key.
#[test]
fn a_string_is_quoted_exactly_where_the_reference_resolver_reads_another_type() {
    for (value, dumped) in [
        ("08", "08"),
        ("_1", "_1"),
        ("1.5e5", "1.5e5"),
        ("1:60", "1:60"),
        ("._5", "._5"),
        ("0x_", "'0x_'"),
        ("1:30", "'1:30'"),
        ("2024-01-01", "'2024-01-01'"),
        ("2024-1-2 3:04:05", "'2024-1-2 3:04:05'"),
        ("<<", "'<<'"),
        ("=", "'='"),
    ] {
        assert_eq!(yaml_scalar(value), dumped, "{value:?}");
    }
}
