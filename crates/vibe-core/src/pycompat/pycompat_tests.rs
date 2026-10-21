//! Every expectation below was printed by the pinned checkout's interpreter.

use serde_json::json;

use super::pydantic::{self, LineError};
use super::{
    Dumps, dumps, isprintable, isspace, os_error, parse_float, parse_int, repr_float, repr_json,
    repr_str, splitlines, str_json, strip, thousands, title, truthy,
};

#[test]
fn whitespace_counts_the_four_information_separators() {
    assert!(isspace('\u{1c}') && isspace('\u{1f}') && isspace('\u{85}') && isspace('\u{3000}'));
    assert!(!isspace('\u{200b}'));
    assert_eq!(strip("\u{1c} text \u{1f}\n"), "text");
}

#[test]
fn splitlines_breaks_on_python_line_boundaries() {
    let text = "a\r\nb\rc\n\nd\u{b}e\u{1c}f\u{2028}g";
    assert_eq!(
        splitlines(text, false),
        ["a", "b", "c", "", "d", "e", "f", "g"]
    );
    assert_eq!(
        splitlines(text, true),
        [
            "a\r\n",
            "b\r",
            "c\n",
            "\n",
            "d\u{b}",
            "e\u{1c}",
            "f\u{2028}",
            "g"
        ]
    );
    assert!(splitlines("", true).is_empty());
    assert_eq!(splitlines("x\n", false), ["x"]);
}

#[test]
fn printable_refuses_controls_formats_separators_and_unassigned() {
    let answers = [
        ' ', '~', '\u{7f}', '\u{a0}', '\u{ad}', 'é', '\u{378}', '\u{2028}', '\u{3000}', '😀',
        '\u{e000}', '\u{200b}',
    ]
    .map(isprintable);
    assert_eq!(
        answers,
        [
            true, true, false, false, false, true, false, false, false, true, false, false
        ]
    );
}

#[test]
fn titles_restart_after_every_uncased_character() {
    assert_eq!(
        title("hello wORLD's x1y code_review v2x"),
        "Hello World'S X1Y Code_Review V2X"
    );
    assert_eq!(title("code reviewer"), "Code Reviewer");
    assert_eq!(title("my AGENT"), "My Agent");
}

#[test]
fn string_reprs_pick_their_quote_and_escape_what_is_not_printable() {
    assert_eq!(repr_str(""), "''");
    assert_eq!(repr_str("it's"), "\"it's\"");
    assert_eq!(repr_str("\"it's\""), "'\"it\\'s\"'");
    assert_eq!(repr_str("a\u{0}b\u{a0}c\u{2028}"), "'a\\x00b\\xa0c\\u2028'");
    assert_eq!(repr_str("é"), "'é'");
    assert_eq!(repr_str("tab\there"), "'tab\\there'");
    assert_eq!(
        repr_str("\u{7f}\u{9f}\u{ad}\u{378}\u{e0001}😀"),
        "'\\x7f\\x9f\\xad\\u0378\\U000e0001😀'"
    );
    assert_eq!(repr_str("back\\slash"), "'back\\\\slash'");
}

#[test]
fn float_reprs_switch_to_scientific_outside_the_positional_range() {
    for (value, expected) in [
        (2.5, "2.5"),
        (1.0, "1.0"),
        (100.0, "100.0"),
        (0.0001, "0.0001"),
        (0.00001, "1e-05"),
        (1.5e-7, "1.5e-07"),
        (1e16, "1e+16"),
        (1.234_567_890_123_456_8e17, "1.2345678901234568e+17"),
        (9_999_999_999_999_998.0, "9999999999999998.0"),
        (-0.5, "-0.5"),
        (0.1, "0.1"),
        (123.456, "123.456"),
        (-0.0, "-0.0"),
        (5e-324, "5e-324"),
        (1e22, "1e+22"),
        (123_456_789_012_345.6, "123456789012345.6"),
        (3.712_556_001_119_083, "3.712556001119083"),
        (123_456.5, "123456.5"),
        (-2.5e-7, "-2.5e-07"),
        (1.5e15, "1500000000000000.0"),
        (f64::NAN, "nan"),
        (f64::NEG_INFINITY, "-inf"),
    ] {
        assert_eq!(repr_float(value), expected, "{value}");
    }
}

#[test]
fn json_reprs_keep_key_order_and_python_literals() {
    let value: serde_json::Value =
        serde_json::from_str(r#"{"z": 1, "a": [true, null, "x'y", 2.5, 1e16], "n": {"k": -3}}"#)
            .expect("fixture");
    assert_eq!(
        repr_json(&value),
        "{'z': 1, 'a': [True, None, \"x'y\", 2.5, 1e+16], 'n': {'k': -3}}"
    );
    assert_eq!(str_json(&json!("x'y")), "x'y");
    assert_eq!(str_json(&json!([1.0, false])), "[1.0, False]");
}

#[test]
fn dumps_follows_every_keyword_argument() {
    let value = json!({"b": 10.0, "a": ["é", null, true, 0.5], "c": "q\"\\\n😀\u{7f}\u{1f}"});
    assert_eq!(
        dumps(&value, Dumps::DEFAULT),
        r#"{"b": 10.0, "a": ["\u00e9", null, true, 0.5], "c": "q\"\\\n\ud83d\ude00\u007f\u001f"}"#
    );
    assert_eq!(
        dumps(
            &value,
            Dumps {
                ensure_ascii: false,
                ..Dumps::DEFAULT
            }
        ),
        "{\"b\": 10.0, \"a\": [\"é\", null, true, 0.5], \"c\": \"q\\\"\\\\\\n😀\u{7f}\\u001f\"}"
    );
    assert_eq!(
        dumps(
            &value,
            Dumps {
                sort_keys: true,
                separators: Some((",", ":")),
                ..Dumps::DEFAULT
            }
        ),
        r#"{"a":["\u00e9",null,true,0.5],"b":10.0,"c":"q\"\\\n\ud83d\ude00\u007f\u001f"}"#
    );
    assert_eq!(
        dumps(
            &json!({"e": [], "o": {}, "n": [[]], "a": ["é", null]}),
            Dumps {
                ensure_ascii: false,
                indent: Some(2),
                ..Dumps::DEFAULT
            }
        ),
        "{\n  \"e\": [],\n  \"o\": {},\n  \"n\": [\n    []\n  ],\n  \"a\": [\n    \"é\",\n    null\n  ]\n}"
    );
}

#[test]
fn a_parsed_repeated_key_keeps_its_first_position_and_last_value() {
    let value: serde_json::Value =
        serde_json::from_str(r#"{"a": 1, "b": 2, "a": 3}"#).expect("fixture");
    assert_eq!(dumps(&value, Dumps::DEFAULT), r#"{"a": 3, "b": 2}"#);
}

#[test]
fn truthiness_is_false_only_for_null_false_zero_and_empty() {
    for falsy in [
        json!(null),
        json!(false),
        json!(0),
        json!(0.0),
        json!(-0.0),
        json!(""),
        json!([]),
        json!({}),
    ] {
        assert!(!truthy(&falsy), "{falsy}");
    }
    for truthy_value in [
        json!(true),
        json!(1),
        json!(-2.5),
        json!(" "),
        json!([0]),
        json!({"a": null}),
    ] {
        assert!(truthy(&truthy_value), "{truthy_value}");
    }
}

#[test]
fn thousands_group_by_three() {
    assert_eq!(thousands(1_234_567_u64), "1,234,567");
    assert_eq!(thousands(-1_234_567_i64), "-1,234,567");
    assert_eq!(thousands(0_u8), "0");
    assert_eq!(thousands(999_u16), "999");
    assert_eq!(thousands(1_000_u32), "1,000");
}

#[test]
fn os_errors_name_the_path_as_its_repr() {
    let error = std::io::Error::from_raw_os_error(2);
    let strerror = error
        .to_string()
        .strip_suffix(" (os error 2)")
        .map(str::to_owned)
        .expect("an operating system error names its code");
    assert_eq!(
        os_error(&error, Some(std::path::Path::new("it's"))),
        format!("[Errno 2] {strerror}: \"it's\"")
    );
    assert_eq!(os_error(&error, None), format!("[Errno 2] {strerror}"));
    let synthetic = std::io::Error::other("no number");
    assert_eq!(os_error(&synthetic, None), "no number");
}

#[test]
fn int_and_float_read_what_python_reads() {
    for (raw, expected) in [
        ("3", 3),
        (" +3\u{3000}", 3),
        ("-5", -5),
        ("1_000", 1000),
        ("007", 7),
    ] {
        assert_eq!(parse_int(raw), Some(expected), "{raw:?}");
    }
    for raw in [
        "", "1e3", "1__0", "_1", "1_", "+_1", "1.0", "+", "- 1", "\u{1c}1",
    ] {
        assert_eq!(parse_int(raw), None, "{raw:?}");
    }
    for (raw, expected) in [
        ("1.5", 1.5),
        (" 1e2 ", 100.0),
        ("1_000.5", 1000.5),
        (".5_5", 0.55),
        ("5.", 5.0),
        ("-iNfInItY", f64::NEG_INFINITY),
    ] {
        assert_eq!(parse_float(raw), Some(expected), "{raw:?}");
    }
    assert!(parse_float("+nan").is_some_and(f64::is_nan));
    for raw in [
        "", "1__0", "1_.5", "1._5", "_1", "1_e5", "in_f", "1e", "0x10", "\u{1c}1",
    ] {
        assert_eq!(parse_float(raw), None, "{raw:?}");
    }
}

#[test]
fn lax_mode_reads_strings_the_way_pydantic_does() {
    assert_eq!(pydantic::lax_bool("Yes"), Some(true));
    assert_eq!(pydantic::lax_bool("OFF"), Some(false));
    for raw in [" true", "true ", "2", ""] {
        assert_eq!(pydantic::lax_bool(raw), None, "{raw:?}");
    }
    for (raw, expected) in [
        (" 1 ", 1),
        ("1.00", 1),
        ("1_000.0", 1000),
        ("-1.000", -1),
        ("\u{a0}1", 1),
        ("9999999999999999999999", 9_999_999_999_999_999_999_999),
    ] {
        assert_eq!(pydantic::lax_int(raw), Some(expected), "{raw:?}");
    }
    for raw in [
        "1.", "1.5", "1e3", "_1", "1__0", "1_", "+_1", ".0", "1.0_0", "0x10", "",
    ] {
        assert_eq!(pydantic::lax_int(raw), None, "{raw:?}");
    }
    for (raw, expected) in [
        (" 1.5 ", 1.5),
        ("1_.5", 1.5),
        ("1._5", 1.5),
        ("1e_5", 100_000.0),
        ("-_1", -1.0),
        ("in_f", f64::INFINITY),
    ] {
        assert_eq!(pydantic::lax_float(raw), Some(expected), "{raw:?}");
    }
    for raw in ["_inf", "1_", "1__0", "infinity_", "\u{1c}1", "", "1e"] {
        assert_eq!(pydantic::lax_float(raw), None, "{raw:?}");
    }
}

#[test]
fn literal_errors_list_every_choice_as_its_repr() {
    assert_eq!(pydantic::expected_literals(&[]), "");
    assert_eq!(pydantic::expected_literals(&[json!("read")]), "'read'");
    assert_eq!(
        pydantic::expected_literals(&[json!("read"), json!(1), json!(null)]),
        "'read', 1 or None"
    );
}

#[test]
fn validation_errors_render_as_pydantic_prints_them() {
    let long = json!(["x".repeat(30), "é".repeat(30), 1.5, 1e16, null, true]);
    let rendered = pydantic::validation_error(
        "M",
        &[
            LineError {
                location: "a",
                kind: "string_type",
                message: "Input should be a valid string",
                input: &long,
            },
            LineError {
                location: "",
                kind: "value_error",
                message: "Value error, bad",
                input: &json!("zz"),
            },
        ],
    );
    assert_eq!(
        rendered,
        "2 validation errors for M\na\n  Input should be a valid string [type=string_type, \
         input_value=['xxxxxxxxxxxxxxxxxxxxxxx... 1.5, 1e+16, None, True], input_type=list]\n  \
         Value error, bad [type=value_error, input_value='zz', input_type=str]"
    );
}
