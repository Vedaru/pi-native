//! JSON revision diffing and operation batches.
//!
//! Ports the core of `@earendil-works/chord/delta`: produce an operation batch
//! that, applied to an earlier revision, yields a later one. This first slice
//! covers the `s` (set) and `d` (delete) operations over object keys; arrays and
//! scalars are set wholesale. The `a`/`t`/`p`/`m` array operations from chord
//! can be added without changing this model.
//!
//! Ops are plain data so a batch can cross a wire or be persisted.

use serde_json::{Map, Value};

/// A field name path from the revision root.
pub type Path = Vec<String>;

/// One operation in a batch.
#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    /// Set the value at `path` (an empty path replaces the root).
    Set { path: Path, value: Value },
    /// Remove the key at `path`.
    Delete { path: Path },
}

/// Compute operations that transform `before` into `after`.
///
/// Objects are diffed key by key; arrays and scalars are replaced wholesale.
/// Returns an empty batch when the revisions are equal.
pub fn diff(before: &Value, after: &Value) -> Vec<Op> {
    let mut ops = Vec::new();
    diff_into(before, after, &mut Vec::new(), &mut ops);
    ops
}

fn diff_into(before: &Value, after: &Value, path: &mut Path, ops: &mut Vec<Op>) {
    if before == after {
        return;
    }
    match (before, after) {
        (Value::Object(before_map), Value::Object(after_map)) => {
            for key in before_map.keys() {
                if !after_map.contains_key(key) {
                    let mut key_path = path.clone();
                    key_path.push(key.clone());
                    ops.push(Op::Delete { path: key_path });
                }
            }
            for (key, after_value) in after_map {
                let mut key_path = path.clone();
                key_path.push(key.clone());
                match before_map.get(key) {
                    Some(before_value) => diff_into(before_value, after_value, &mut key_path, ops),
                    None => ops.push(Op::Set {
                        path: key_path,
                        value: after_value.clone(),
                    }),
                }
            }
        }
        _ => ops.push(Op::Set {
            path: path.clone(),
            value: after.clone(),
        }),
    }
}

/// Apply an operation batch to a revision, returning the new revision.
pub fn apply(base: &Value, ops: &[Op]) -> Value {
    let mut root = base.clone();
    for op in ops {
        match op {
            Op::Set { path, value } => set_at(&mut root, path, value.clone()),
            Op::Delete { path } => delete_at(&mut root, path),
        }
    }
    root
}

fn set_at(root: &mut Value, path: &[String], value: Value) {
    let Some((last, parents)) = path.split_last() else {
        *root = value;
        return;
    };
    let mut current = root;
    for segment in parents {
        if !current.is_object() {
            *current = Value::Object(Map::new());
        }
        current = current
            .as_object_mut()
            .expect("object")
            .entry(segment.clone())
            .or_insert(Value::Null);
    }
    if !current.is_object() {
        *current = Value::Object(Map::new());
    }
    current
        .as_object_mut()
        .expect("object")
        .insert(last.clone(), value);
}

fn delete_at(root: &mut Value, path: &[String]) {
    let Some((last, parents)) = path.split_last() else {
        return;
    };
    let mut current = root;
    for segment in parents {
        match current
            .as_object_mut()
            .and_then(|object| object.get_mut(segment))
        {
            Some(next) => current = next,
            None => return,
        }
    }
    if let Some(object) = current.as_object_mut() {
        object.remove(last);
    }
}

#[cfg(test)]
#[path = "../tests/unit/lib.rs"]
mod tests;
