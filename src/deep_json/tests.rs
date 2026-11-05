use super::*;
use crate::test_support::small_stack;

fn payload(depth: usize) -> String {
    format!(
        "{}null{}",
        r#"{"layer":["#.repeat(depth),
        "]}".repeat(depth)
    )
}

#[test]
fn malformed_deep_values_return_errors() {
    small_stack(|| {
        let deep = payload(5_000);
        for invalid in [
            format!(r#"{{"payload":{deep},"invalid":!}}"#),
            format!("[{deep}, !]"),
        ] {
            let text = format!("{}{invalid}{}", r#"{"child":"#.repeat(400), "}".repeat(400));
            assert!(from_str(&text).is_err());
            assert!(from_reader(text.as_bytes()).is_err());
        }
    });
}

#[test]
fn duplicate_keys_dispose_of_deep_replaced_values() {
    small_stack(|| {
        let text = format!(r#"{{"duplicate":{},"duplicate":7}}"#, payload(10_000));
        for value in [
            from_str(&text).unwrap(),
            from_reader(text.as_bytes()).unwrap(),
        ] {
            assert_eq!(value["duplicate"], 7);
            assert_eq!(value.as_object().unwrap().len(), 1);
        }
    });
}

#[test]
fn parser_keeps_json_validation_and_value_semantics() {
    for text in [
        r#"{"z":null,"a":[true,false,-1,18446744073709551615,1.25,1e40,"\uD834\uDD1E"],"a":"last"}"#,
        "[null,true,false,0,-0.0,1.5,\"escaped\\ntext\"]",
    ] {
        let expected: Value = serde_json::from_str(text).unwrap();
        assert_eq!(*from_str(text).unwrap(), expected);
        assert_eq!(*from_reader(text.as_bytes()).unwrap(), expected);
    }
    for text in ["[1,]", "{\"a\":}", "1 2", "\"\\uD800\"", "1e400"] {
        assert!(from_str(text).is_err(), "{text}");
        assert!(from_reader(text.as_bytes()).is_err(), "{text}");
    }
    small_stack(|| {
        let trailing = format!("{} trailing", payload(10_000));
        assert!(from_str(&trailing).is_err());
        assert!(from_reader(trailing.as_bytes()).is_err());
    });
}
