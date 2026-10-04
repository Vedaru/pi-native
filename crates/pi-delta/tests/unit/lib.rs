use super::*;
use serde_json::json;

#[test]
fn equal_revisions_produce_no_ops() {
    let value = json!({"a": 1, "b": [1, 2]});
    assert!(diff(&value, &value).is_empty());
}

#[test]
fn round_trips_nested_changes() {
    let before = json!({"a": 1, "nested": {"x": 1, "y": 2}, "keep": true});
    let after = json!({"a": 2, "nested": {"x": 1, "z": 3}, "keep": true});
    let ops = diff(&before, &after);
    assert_eq!(apply(&before, &ops), after);
    // Deletes the removed key and sets the added/changed ones.
    assert!(ops.iter().any(|op| matches!(op, Op::Delete { path } if path == &vec!["nested".to_string(), "y".to_string()])));
    assert!(ops
        .iter()
        .any(|op| matches!(op, Op::Set { path, .. } if path == &vec!["a".to_string()])));
}

#[test]
fn replaces_arrays_wholesale() {
    let before = json!({"list": [1, 2, 3]});
    let after = json!({"list": [1, 2, 4]});
    let ops = diff(&before, &after);
    assert_eq!(ops.len(), 1);
    assert_eq!(apply(&before, &ops), after);
}

#[test]
fn handles_root_replacement() {
    let before = json!([1, 2, 3]);
    let after = json!({"a": 1});
    let ops = diff(&before, &after);
    assert_eq!(
        ops,
        vec![Op::Set {
            path: vec![],
            value: after.clone()
        }]
    );
    assert_eq!(apply(&before, &ops), after);
}

#[test]
fn apply_creates_missing_intermediate_objects() {
    let base = json!({});
    let ops = vec![Op::Set {
        path: vec!["a".to_string(), "b".to_string()],
        value: json!(1),
    }];
    assert_eq!(apply(&base, &ops), json!({"a": {"b": 1}}));
}

#[test]
fn delete_missing_path_is_a_no_op() {
    let base = json!({"a": 1});
    let ops = vec![Op::Delete {
        path: vec!["missing".to_string()],
    }];
    assert_eq!(apply(&base, &ops), base);
}
