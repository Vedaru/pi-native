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
    /// Append items to the array at `path` (chord's `a`).
    Append { path: Path, items: Vec<Value> },
    /// Remove `remove` items from the end of the array at `path` (chord's `t`).
    Truncate { path: Path, remove: usize },
    /// Replace `delete_count` items at `start` with `items` in the array at `path`.
    Splice {
        path: Path,
        start: usize,
        delete_count: usize,
        items: Vec<Value>,
    },
    /// Reorder the array at `path`: `new[i] = old[permutation[i]]`.
    Move { path: Path, permutation: Vec<usize> },
}

/// Compute operations that transform `before` into `after`.
///
/// Objects are diffed key by key; arrays and scalars are replaced wholesale.
/// Returns an empty batch when the revisions are equal.
pub fn diff(before: &Value, after: &Value) -> Vec<Op> {
    let mut ops = Vec::new();
    diff_into(before, after, &Vec::new(), &mut ops);
    ops
}

fn diff_into(before: &Value, after: &Value, path: &Path, ops: &mut Vec<Op>) {
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
                    Some(before_value) => diff_into(before_value, after_value, &key_path, ops),
                    None => ops.push(Op::Set {
                        path: key_path,
                        value: after_value.clone(),
                    }),
                }
            }
        }
        (Value::Array(before_items), Value::Array(after_items)) => {
            // Incremental array edits when the change is an append, truncation,
            // or reorder; otherwise replace the array wholesale.
            if after_items.len() >= before_items.len()
                && after_items[..before_items.len()] == before_items[..]
            {
                ops.push(Op::Append {
                    path: path.clone(),
                    items: after_items[before_items.len()..].to_vec(),
                });
            } else if before_items.len() > after_items.len()
                && before_items[..after_items.len()] == after_items[..]
            {
                ops.push(Op::Truncate {
                    path: path.clone(),
                    remove: before_items.len() - after_items.len(),
                });
            } else if let Some(permutation) = permutation(before_items, after_items) {
                ops.push(Op::Move {
                    path: path.clone(),
                    permutation,
                });
            } else {
                ops.push(Op::Set {
                    path: path.clone(),
                    value: after.clone(),
                });
            }
        }
        _ => ops.push(Op::Set {
            path: path.clone(),
            value: after.clone(),
        }),
    }
}

/// A permutation `new[i] = old[permutation[i]]`, or `None` when the arrays are
/// not a reordering of each other.
fn permutation(before: &[Value], after: &[Value]) -> Option<Vec<usize>> {
    if before.len() != after.len() {
        return None;
    }
    let mut used = vec![false; before.len()];
    let mut permutation = Vec::with_capacity(after.len());
    for value in after {
        let index = before
            .iter()
            .enumerate()
            .position(|(index, candidate)| !used[index] && candidate == value)?;
        used[index] = true;
        permutation.push(index);
    }
    Some(permutation)
}

/// Apply an operation batch to a revision, returning the new revision.
pub fn apply(base: &Value, ops: &[Op]) -> Value {
    let mut root = base.clone();
    for op in ops {
        match op {
            Op::Set { path, value } => set_at(&mut root, path, value.clone()),
            Op::Delete { path } => delete_at(&mut root, path),
            Op::Append { path, items } => {
                if let Some(array) = array_at_mut(&mut root, path) {
                    array.extend(items.iter().cloned());
                }
            }
            Op::Truncate { path, remove } => {
                if let Some(array) = array_at_mut(&mut root, path) {
                    let keep = array.len().saturating_sub(*remove);
                    array.truncate(keep);
                }
            }
            Op::Splice {
                path,
                start,
                delete_count,
                items,
            } => {
                if let Some(array) = array_at_mut(&mut root, path) {
                    let start = (*start).min(array.len());
                    let end = (start + delete_count).min(array.len());
                    array.splice(start..end, items.clone());
                }
            }
            Op::Move { path, permutation } => {
                if let Some(array) = array_at_mut(&mut root, path) {
                    let old = array.clone();
                    let reordered: Vec<Value> = permutation
                        .iter()
                        .filter_map(|&index| old.get(index).cloned())
                        .collect();
                    if reordered.len() == old.len() {
                        *array = reordered;
                    }
                }
            }
        }
    }
    root
}

fn array_at_mut<'a>(root: &'a mut Value, path: &[String]) -> Option<&'a mut Vec<Value>> {
    let mut current = root;
    for segment in path {
        current = current.as_object_mut()?.get_mut(segment)?;
    }
    current.as_array_mut()
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
