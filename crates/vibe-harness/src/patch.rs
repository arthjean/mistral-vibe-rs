//! The JSON patch a Unified Harness notification carries.
//!
//! Every `session/updated` and `history/entryUpdated` the reference's unified
//! adapter publishes is the difference between two states, computed by the
//! `jsonpatch` package's `make_patch` and then reshaped for the wire
//! (`vibe/app_server/_patch.py`): a `move` becomes a `remove` and an `add`,
//! and a `replace` of a streaming text path whose old value is a prefix of
//! the new one becomes an `append` of the suffix. This module computes the
//! same operations. Where the package walks an object's keys in the order of
//! a Python set, which changes from one process to the next, this walk sorts
//! them, so an object's operations come out in key order.

use std::collections::HashMap;

use serde_json::{Map, Value, json};

/// One operation of a wire patch.
#[derive(Debug, Clone, PartialEq)]
pub struct PatchOperation {
    /// `add`, `remove`, `replace` or `append`.
    pub op: &'static str,
    pub path: String,
    /// The value an `add` or a `replace` writes, the suffix an `append` adds,
    /// and `null` for a `remove`.
    pub value: Value,
}

impl PatchOperation {
    /// The operation as the wire model serializes it, `value` included even
    /// when it is `null`.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({"op": self.op, "path": self.path, "value": self.value})
    }
}

/// The operations that turn `source` into `target`.
///
/// A `replace` of one of `append_paths` whose old and new values are strings,
/// the new one extending the old, is published as an `append` of what was
/// added (reference `make_json_patch`).
#[must_use]
pub fn make_json_patch(
    source: &Value,
    target: &Value,
    append_paths: &[&str],
) -> Vec<PatchOperation> {
    let mut builder = DiffBuilder::new(target);
    builder.compare_values(&[], None, source, target);
    let mut operations = Vec::new();
    for raw in builder.execute() {
        let modeled = model(source, raw);
        let streaming = modeled.len() == 1
            && modeled[0].op == "replace"
            && append_paths.contains(&modeled[0].path.as_str());
        if !streaming {
            operations.extend(modeled);
            continue;
        }
        let previous = source.pointer(&modeled[0].path).and_then(Value::as_str);
        let current = modeled[0].value.as_str();
        match (previous, current) {
            (Some(previous), Some(current)) if current.starts_with(previous) => {
                operations.push(PatchOperation {
                    op: "append",
                    path: modeled[0].path.clone(),
                    value: Value::String(current[previous.len()..].to_owned()),
                });
            }
            _ => operations.extend(modeled),
        }
    }
    operations
}

/// The operations as a wire array.
#[must_use]
pub fn patch_json(operations: &[PatchOperation]) -> Value {
    Value::Array(operations.iter().map(PatchOperation::to_json).collect())
}

/// Reference `_model_operations`: the wire models no `move`, so one is
/// published as the `remove` of its source and the `add` of what was there.
fn model(source: &Value, raw: RawOperation) -> Vec<PatchOperation> {
    match raw.kind {
        Kind::Move => {
            let from = raw.from.as_deref().map(pointer).unwrap_or_default();
            let value = source.pointer(&from).cloned().unwrap_or(Value::Null);
            vec![
                PatchOperation {
                    op: "remove",
                    path: from,
                    value: Value::Null,
                },
                PatchOperation {
                    op: "add",
                    path: pointer(&raw.parts),
                    value,
                },
            ]
        }
        Kind::Add => vec![PatchOperation {
            op: "add",
            path: pointer(&raw.parts),
            value: raw.value.unwrap_or(Value::Null),
        }],
        Kind::Remove => vec![PatchOperation {
            op: "remove",
            path: pointer(&raw.parts),
            value: Value::Null,
        }],
        Kind::Replace => vec![PatchOperation {
            op: "replace",
            path: pointer(&raw.parts),
            value: raw.value.unwrap_or(Value::Null),
        }],
    }
}

fn escape(part: &str) -> String {
    part.replace('~', "~0").replace('/', "~1")
}

/// The JSON pointer naming `parts`.
fn pointer(parts: &[String]) -> String {
    parts
        .iter()
        .map(|part| format!("/{}", escape(part)))
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Add,
    Remove,
    Replace,
    Move,
}

#[derive(Debug, Clone)]
struct RawOperation {
    kind: Kind,
    parts: Vec<String>,
    from: Option<Vec<String>>,
    value: Option<Value>,
}

/// The last segment of a pointer, read as an array index when it is one.
fn index_of(parts: &[String]) -> Option<i64> {
    parts.last().and_then(|part| part.parse::<i64>().ok())
}

fn parent_of(parts: &[String]) -> &[String] {
    &parts[..parts.len().saturating_sub(1)]
}

fn set_index(parts: &mut [String], index: i64) {
    if let Some(last) = parts.last_mut() {
        *last = index.to_string();
    }
}

impl RawOperation {
    /// How this operation's own index shifts, and what it does to `index`,
    /// when a `remove` at `index` under `parent` is taken back.
    fn undo_remove(&mut self, parent: &[String], index: i64) -> i64 {
        let mut index = index;
        match self.kind {
            Kind::Replace => {}
            Kind::Remove => {
                if parent_of(&self.parts) == parent
                    && let Some(own) = index_of(&self.parts)
                {
                    if own >= index {
                        set_index(&mut self.parts, own + 1);
                    } else {
                        index -= 1;
                    }
                }
            }
            Kind::Add => {
                if parent_of(&self.parts) == parent
                    && let Some(own) = index_of(&self.parts)
                {
                    if own > index {
                        set_index(&mut self.parts, own + 1);
                    } else {
                        index += 1;
                    }
                }
            }
            Kind::Move => {
                if let Some(from) = self.from.as_mut()
                    && parent_of(from) == parent
                    && let Some(own) = index_of(from)
                {
                    if own >= index {
                        set_index(from, own + 1);
                    } else {
                        index -= 1;
                    }
                }
                if parent_of(&self.parts) == parent
                    && let Some(own) = index_of(&self.parts)
                {
                    if own > index {
                        set_index(&mut self.parts, own + 1);
                    } else {
                        index += 1;
                    }
                }
            }
        }
        index
    }

    /// The same for an `add` at `index` under `parent` taken back.
    fn undo_add(&mut self, parent: &[String], index: i64) -> i64 {
        let mut index = index;
        match self.kind {
            Kind::Replace => {}
            Kind::Remove => {
                if parent_of(&self.parts) == parent
                    && let Some(own) = index_of(&self.parts)
                {
                    if own > index {
                        set_index(&mut self.parts, own - 1);
                    } else {
                        index -= 1;
                    }
                }
            }
            Kind::Add => {
                if parent_of(&self.parts) == parent
                    && let Some(own) = index_of(&self.parts)
                {
                    if own > index {
                        set_index(&mut self.parts, own - 1);
                    } else {
                        index += 1;
                    }
                }
            }
            Kind::Move => {
                if let Some(from) = self.from.as_mut()
                    && parent_of(from) == parent
                    && let Some(own) = index_of(from)
                {
                    if own > index {
                        set_index(from, own - 1);
                    } else {
                        index -= 1;
                    }
                }
                if parent_of(&self.parts) == parent
                    && let Some(own) = index_of(&self.parts)
                {
                    if own > index {
                        set_index(&mut self.parts, own - 1);
                    } else {
                        index += 1;
                    }
                }
            }
        }
        index
    }
}

/// Which pending operations a value can still be paired with.
#[derive(Debug, Clone, Copy)]
enum Pending {
    Removed = 0,
    Added = 1,
}

/// A value as the package indexes it: scalars by value and type, containers
/// by equality.
fn typed_key(value: &Value) -> Option<String> {
    match value {
        Value::Null => Some("n".to_owned()),
        Value::Bool(flag) => Some(format!("b{flag}")),
        Value::Number(number) if number.is_f64() => Some(format!("f{number}")),
        Value::Number(number) => Some(format!("i{number}")),
        Value::String(text) => Some(format!("s{text}")),
        Value::Array(_) | Value::Object(_) => None,
    }
}

/// The package's `DiffBuilder`: operations kept in the order they were
/// recorded, a removal and an addition of equal values folded into a move.
struct DiffBuilder<'a> {
    target: &'a Value,
    operations: Vec<Option<RawOperation>>,
    scalars: [HashMap<String, Vec<usize>>; 2],
    containers: [Vec<(Value, usize)>; 2],
}

impl<'a> DiffBuilder<'a> {
    fn new(target: &'a Value) -> Self {
        Self {
            target,
            operations: Vec::new(),
            scalars: [HashMap::new(), HashMap::new()],
            containers: [Vec::new(), Vec::new()],
        }
    }

    fn insert(&mut self, operation: RawOperation) -> usize {
        self.operations.push(Some(operation));
        self.operations.len() - 1
    }

    fn store(&mut self, value: &Value, slot: usize, pending: Pending) {
        match typed_key(value) {
            Some(key) => self.scalars[pending as usize]
                .entry(key)
                .or_default()
                .push(slot),
            None => self.containers[pending as usize].push((value.clone(), slot)),
        }
    }

    fn take(&mut self, value: &Value, pending: Pending) -> Option<usize> {
        match typed_key(value) {
            Some(key) => self.scalars[pending as usize].get_mut(&key)?.pop(),
            None => {
                let storage = &mut self.containers[pending as usize];
                let position = storage.iter().rposition(|(stored, _)| stored == value)?;
                Some(storage.remove(position).1)
            }
        }
    }

    /// The live operations recorded after `slot`.
    fn later(&self, slot: usize) -> Vec<usize> {
        (slot + 1..self.operations.len())
            .filter(|index| self.operations[*index].is_some())
            .collect()
    }

    /// `index` says whether `key` is an array index, which is when an
    /// earlier removal paired with this addition has its own index shifted.
    fn item_added(&mut self, parent: &[String], key: &str, index: bool, item: &Value) {
        let mut parts = parent.to_vec();
        parts.push(key.to_owned());
        match self.take(item, Pending::Removed) {
            Some(slot) => {
                let Some(mut removed) = self.operations[slot].take() else {
                    return;
                };
                if let (Some(mut own), true) = (index_of(&removed.parts), index) {
                    let own_parent = parent_of(&removed.parts).to_vec();
                    for later in self.later(slot) {
                        if let Some(operation) = self.operations[later].as_mut() {
                            own = operation.undo_remove(&own_parent, own);
                        }
                    }
                    set_index(&mut removed.parts, own);
                }
                if removed.parts != parts {
                    self.insert(RawOperation {
                        kind: Kind::Move,
                        parts,
                        from: Some(removed.parts),
                        value: None,
                    });
                }
            }
            None => {
                let slot = self.insert(RawOperation {
                    kind: Kind::Add,
                    parts,
                    from: None,
                    value: Some(item.clone()),
                });
                self.store(item, slot, Pending::Added);
            }
        }
    }

    fn item_removed(&mut self, parent: &[String], key: &str, item: &Value) {
        let mut parts = parent.to_vec();
        parts.push(key.to_owned());
        let paired = self.take(item, Pending::Added);
        let slot = self.insert(RawOperation {
            kind: Kind::Remove,
            parts: parts.clone(),
            from: None,
            value: None,
        });
        let Some(added_slot) = paired else {
            self.store(item, slot, Pending::Removed);
            return;
        };
        let Some(mut added) = self.operations[added_slot].take() else {
            return;
        };
        let added_into_array = self
            .target
            .pointer(&pointer(parent_of(&added.parts)))
            .is_some_and(Value::is_array);
        if added_into_array && let Some(mut own) = index_of(&added.parts) {
            let own_parent = parent_of(&added.parts).to_vec();
            for later in self.later(added_slot) {
                if let Some(operation) = self.operations[later].as_mut() {
                    own = operation.undo_add(&own_parent, own);
                }
            }
            set_index(&mut added.parts, own);
        }
        // Taking the addition back may have shifted this removal's own index.
        let removed_at = self.operations[slot]
            .as_ref()
            .map_or(parts, |operation| operation.parts.clone());
        if removed_at == added.parts {
            self.operations[slot] = None;
        } else {
            self.operations[slot] = Some(RawOperation {
                kind: Kind::Move,
                parts: added.parts,
                from: Some(removed_at),
                value: None,
            });
        }
    }

    fn item_replaced(&mut self, parent: &[String], key: Option<&str>, item: &Value) {
        let mut parts = parent.to_vec();
        if let Some(key) = key {
            parts.push(key.to_owned());
        }
        self.insert(RawOperation {
            kind: Kind::Replace,
            parts,
            from: None,
            value: Some(item.clone()),
        });
    }

    fn compare_objects(
        &mut self,
        path: &[String],
        source: &Map<String, Value>,
        target: &Map<String, Value>,
    ) {
        let mut removed: Vec<&String> = source
            .keys()
            .filter(|key| !target.contains_key(*key))
            .collect();
        let mut added: Vec<&String> = target
            .keys()
            .filter(|key| !source.contains_key(*key))
            .collect();
        let mut common: Vec<&String> = source
            .keys()
            .filter(|key| target.contains_key(*key))
            .collect();
        removed.sort();
        added.sort();
        common.sort();
        for key in removed {
            self.item_removed(path, key, &source[key]);
        }
        for key in added {
            self.item_added(path, key, false, &target[key]);
        }
        for key in common {
            self.compare_values(path, Some(key), &source[key], &target[key]);
        }
    }

    fn compare_arrays(&mut self, path: &[String], source: &[Value], target: &[Value]) {
        let shortest = source.len().min(target.len());
        for index in 0..source.len().max(target.len()) {
            let key = index.to_string();
            if index < shortest {
                let (old, new) = (&source[index], &target[index]);
                if old == new {
                    continue;
                }
                match (old, new) {
                    (Value::Object(old), Value::Object(new)) => {
                        let mut child = path.to_vec();
                        child.push(key);
                        self.compare_objects(&child, old, new);
                    }
                    (Value::Array(old), Value::Array(new)) => {
                        let mut child = path.to_vec();
                        child.push(key);
                        self.compare_arrays(&child, old, new);
                    }
                    _ => {
                        self.item_removed(path, &key, old);
                        self.item_added(path, &key, true, new);
                    }
                }
            } else if source.len() > target.len() {
                self.item_removed(path, &target.len().to_string(), &source[index]);
            } else {
                self.item_added(path, &key, true, &target[index]);
            }
        }
    }

    fn compare_values(
        &mut self,
        path: &[String],
        key: Option<&str>,
        source: &Value,
        target: &Value,
    ) {
        let mut child = path.to_vec();
        if let Some(key) = key {
            child.push(key.to_owned());
        }
        match (source, target) {
            (Value::Object(source), Value::Object(target)) => {
                self.compare_objects(&child, source, target)
            }
            (Value::Array(source), Value::Array(target)) => {
                self.compare_arrays(&child, source, target)
            }
            // The package compares serialized forms, which tells `1` from
            // `1.0` and `true` from `1`.
            _ if serde_json::to_vec(source).ok() == serde_json::to_vec(target).ok() => {}
            _ => self.item_replaced(path, key, target),
        }
    }

    /// The recorded operations, a removal followed by an addition at the same
    /// place published as one replacement.
    fn execute(self) -> Vec<RawOperation> {
        let live: Vec<RawOperation> = self.operations.into_iter().flatten().collect();
        let mut result = Vec::with_capacity(live.len());
        let mut index = 0;
        while index < live.len() {
            if let Some(next) = live.get(index + 1)
                && live[index].kind == Kind::Remove
                && next.kind == Kind::Add
                && live[index].parts == next.parts
            {
                result.push(RawOperation {
                    kind: Kind::Replace,
                    parts: next.parts.clone(),
                    from: None,
                    value: next.value.clone(),
                });
                index += 2;
                continue;
            }
            result.push(live[index].clone());
            index += 1;
        }
        result
    }
}

/// Reference `apply_json_patch`: `operations`, as the wire carries them,
/// applied to `document` in order. An `append` adds its suffix to the string
/// it names.
///
/// # Errors
///
/// An operation is malformed or names a location the document lacks.
pub fn apply_json_patch(document: &mut Value, operations: &[Value]) -> Result<(), String> {
    for operation in operations {
        let op = operation["op"]
            .as_str()
            .ok_or("a patch operation names no op")?;
        let path = operation["path"]
            .as_str()
            .ok_or("a patch operation names no path")?;
        let value = operation.get("value").cloned().unwrap_or(Value::Null);
        apply_one(document, op, path, value)?;
    }
    Ok(())
}

fn unescape(part: &str) -> String {
    part.replace("~1", "/").replace("~0", "~")
}

fn apply_one(document: &mut Value, op: &str, path: &str, value: Value) -> Result<(), String> {
    if path.is_empty() {
        return match op {
            "add" | "replace" => {
                *document = value;
                Ok(())
            }
            _ => Err(format!("cannot {op} the whole document")),
        };
    }
    let (parent_path, last) = path
        .rsplit_once('/')
        .ok_or_else(|| format!("not a JSON pointer: {path}"))?;
    let last = unescape(last);
    let parent = document
        .pointer_mut(parent_path)
        .ok_or_else(|| format!("no location at {parent_path}"))?;
    match parent {
        Value::Object(map) => match op {
            "add" | "replace" => {
                map.insert(last, value);
                Ok(())
            }
            "remove" => map
                .remove(&last)
                .map(drop)
                .ok_or_else(|| format!("nothing to remove at {path}")),
            "append" => match (map.get_mut(&last), value) {
                (Some(Value::String(current)), Value::String(suffix)) => {
                    current.push_str(&suffix);
                    Ok(())
                }
                _ => Err(format!("an append needs two strings at {path}")),
            },
            other => Err(format!("unsupported patch op {other}")),
        },
        Value::Array(items) => {
            let index = if last == "-" {
                items.len()
            } else {
                last.parse::<usize>()
                    .map_err(|_| format!("not an array index: {path}"))?
            };
            match op {
                "add" if index <= items.len() => {
                    items.insert(index, value);
                    Ok(())
                }
                "replace" if index < items.len() => {
                    items[index] = value;
                    Ok(())
                }
                "remove" if index < items.len() => {
                    items.remove(index);
                    Ok(())
                }
                "append" => match (items.get_mut(index), value) {
                    (Some(Value::String(current)), Value::String(suffix)) => {
                        current.push_str(&suffix);
                        Ok(())
                    }
                    _ => Err(format!("an append needs two strings at {path}")),
                },
                _ => Err(format!("cannot {op} at {path}")),
            }
        }
        _ => Err(format!("no container at {parent_path}")),
    }
}

#[cfg(test)]
mod patch_tests;
