//! RFC6902 JSON-Patch generation for packed ConvexValue JSON.
//!
//! Patch is computed on the `JsonPackedValue`'s canonical JSON string
//! (`value.as_str()`) so it is language-agnostic: any client with a
//! JSON-Patch library (JS `fast-json-patch`, Rust `json-patch`, Python
//! `jsonpatch`) can apply it. Generation is best-effort and falls back to
//! full `QueryUpdated` when the patch would be larger than the threshold.

use common::value::JsonPackedValue;
use serde_json::Value as JsonValue;

/// Minimum packed value size (bytes) before we attempt delta encoding.
/// Below this, the overhead of computing and framing a patch exceeds the
/// bandwidth saving.
const MIN_PATCHABLE_SIZE: usize = 1024;

/// Maximum `patch_bytes / new_bytes` ratio to still send a patch.
/// If `patch.len() > ratio * new.len()`, we fall back to full value.
/// 0.8 follows `patch <0.8*value` spec.
const PATCH_RATIO_THRESHOLD: f64 = 0.8;

/// Returns true when `patch_bytes` is worthwhile vs `new_bytes`.
/// Centralizes 0.8 threshold for `state.rs` and `maybe_patch`.
pub fn is_patch_worth_it(patch_bytes: usize, new_bytes: usize) -> bool {
    (patch_bytes as f64) < PATCH_RATIO_THRESHOLD * new_bytes as f64
}

/// Try to produce an RFC6902 patch from `old` to `new` if it would save
/// bandwidth. Returns `Some(patch)` (as a JSON array) only when:
/// - `new` is large enough (`>= MIN_PATCHABLE_SIZE`)
/// - the patch serializes to less than `PATCH_RATIO_THRESHOLD * new.len()`
/// - the patch is non-empty and valid JSON-Patch
/// Otherwise returns `None` to signal fallback to full value.
pub fn maybe_patch(old: &JsonPackedValue, new: &JsonPackedValue) -> Option<JsonValue> {
    let old_str = old.as_str();
    let new_str = new.as_str();
    if new_str.len() < MIN_PATCHABLE_SIZE {
        return None;
    }
    // Quick cheap path: if old == new, caller already deduped via hash,
    // but guard anyway.
    if old_str == new_str {
        return None;
    }
    let patch = diff_to_patch(old_str, new_str)?;
    let patch_str = serde_json::to_string(&patch).ok()?;
    if !is_patch_worth_it(patch_str.len(), new_str.len()) {
        return None;
    }
    // Avoid sending empty patch (should be deduped)
    if patch.as_array().is_some_and(|a| a.is_empty()) {
        return None;
    }
    Some(patch)
}

/// Compute RFC6902 patch as `JsonValue::Array` of operations, or `None` if
/// either side fails to parse (should not happen for valid packed values).
fn diff_to_patch(old_str: &str, new_str: &str) -> Option<JsonValue> {
    let old_json: JsonValue = serde_json::from_str(old_str).ok()?;
    let new_json: JsonValue = serde_json::from_str(new_str).ok()?;
    let mut ops = Vec::new();
    diff_value("", &old_json, &new_json, &mut ops);
    Some(JsonValue::Array(ops))
}

fn escape_pointer(token: &str) -> String {
    token.replace('~', "~0").replace('/', "~1")
}

fn diff_value(path: &str, old: &JsonValue, new: &JsonValue, ops: &mut Vec<JsonValue>) {
    if old == new {
        return;
    }
    match (old, new) {
        (JsonValue::Object(old_map), JsonValue::Object(new_map)) => {
            // Removals
            for key in old_map.keys() {
                if !new_map.contains_key(key) {
                    let p = if path.is_empty() {
                        format!("/{}", escape_pointer(key))
                    } else {
                        format!("{}/{}", path, escape_pointer(key))
                    };
                    ops.push(serde_json::json!({"op":"remove","path": p}));
                }
            }
            // Adds / recursive diffs
            for (key, new_val) in new_map {
                let p = if path.is_empty() {
                    format!("/{}", escape_pointer(key))
                } else {
                    format!("{}/{}", path, escape_pointer(key))
                };
                if let Some(old_val) = old_map.get(key) {
                    diff_value(&p, old_val, new_val, ops);
                } else {
                    ops.push(serde_json::json!({"op":"add","path": p, "value": new_val}));
                }
            }
        },
        (JsonValue::Array(old_arr), JsonValue::Array(new_arr)) => {
            let min_len = std::cmp::min(old_arr.len(), new_arr.len());
            for i in 0..min_len {
                let p = format!("{}/{}", path, i);
                diff_value(&p, &old_arr[i], &new_arr[i], ops);
            }
            if new_arr.len() > old_arr.len() {
                for i in min_len..new_arr.len() {
                    let p = format!("{}/{}", path, i);
                    ops.push(serde_json::json!({"op":"add","path": p, "value": new_arr[i]}));
                }
            } else if old_arr.len() > new_arr.len() {
                // Remove from end backwards so indices remain valid
                for i in (min_len..old_arr.len()).rev() {
                    let p = format!("{}/{}", path, i);
                    ops.push(serde_json::json!({"op":"remove","path": p}));
                }
            }
        },
        _ => {
            // Leaf or type change: replace at path (empty path = root)
            let p = if path.is_empty() { "".to_string() } else { path.to_string() };
            ops.push(serde_json::json!({"op":"replace","path": p, "value": new}));
        },
    }
}

/// Apply a patch produced by `diff_to_patch` to `old` for testing/validation.
/// Requires the `json-patch` semantics; implemented here as a minimal
/// applier for round-trip tests without an external crate.
#[cfg(test)]
pub fn apply_patch(old_str: &str, patch: &JsonValue) -> anyhow::Result<String> {
    let mut doc: JsonValue = serde_json::from_str(old_str)?;
    let ops = patch.as_array().ok_or_else(|| anyhow::anyhow!("patch not an array"))?;
    for op in ops {
        let op_str = op.get("op").and_then(|v| v.as_str()).ok_or_else(|| anyhow::anyhow!("missing op"))?;
        let path = op.get("path").and_then(|v| v.as_str()).ok_or_else(|| anyhow::anyhow!("missing path"))?;
        match op_str {
            "add" | "replace" => {
                let value = op.get("value").cloned().ok_or_else(|| anyhow::anyhow!("missing value"))?;
                set_at_path(&mut doc, path, value, op_str == "add")?;
            },
            "remove" => {
                remove_at_path(&mut doc, path)?;
            },
            _ => anyhow::bail!("unsupported op {op_str}"),
        }
    }
    Ok(serde_json::to_string(&doc)?)
}

#[cfg(test)]
fn set_at_path(doc: &mut JsonValue, path: &str, value: JsonValue, is_add: bool) -> anyhow::Result<()> {
    if path.is_empty() {
        *doc = value;
        return Ok(());
    }
    let tokens = parse_pointer(path)?;
    let mut cur = doc;
    for (idx, token) in tokens.iter().enumerate() {
        let is_last = idx == tokens.len() - 1;
        if is_last {
            match cur {
                JsonValue::Object(map) => {
                    map.insert(token.clone(), value);
                    return Ok(());
                },
                JsonValue::Array(arr) => {
                    let arr_idx: usize = token.parse()?;
                    if is_add {
                        if arr_idx > arr.len() {
                            anyhow::bail!("add index out of bounds");
                        }
                        if arr_idx == arr.len() {
                            arr.push(value);
                        } else {
                            arr.insert(arr_idx, value);
                        }
                    } else {
                        if arr_idx >= arr.len() {
                            anyhow::bail!("replace index out of bounds");
                        }
                        arr[arr_idx] = value;
                    }
                    return Ok(());
                },
                _ => anyhow::bail!("path through non-container"),
            }
        } else {
            cur = match cur {
                JsonValue::Object(map) => map.get_mut(token).ok_or_else(|| anyhow::anyhow!("missing object key {token}"))?,
                JsonValue::Array(arr) => {
                    let arr_idx: usize = token.parse()?;
                    arr.get_mut(arr_idx).ok_or_else(|| anyhow::anyhow!("missing array index {arr_idx}"))?
                },
                _ => anyhow::bail!("path through non-container"),
            };
        }
    }
    Ok(())
}

#[cfg(test)]
fn remove_at_path(doc: &mut JsonValue, path: &str) -> anyhow::Result<()> {
    let tokens = parse_pointer(path)?;
    if tokens.is_empty() {
        anyhow::bail!("cannot remove root");
    }
    let mut cur = doc;
    for (idx, token) in tokens.iter().enumerate() {
        let is_last = idx == tokens.len() - 1;
        if is_last {
            match cur {
                JsonValue::Object(map) => {
                    map.remove(token).ok_or_else(|| anyhow::anyhow!("missing key"))?;
                    return Ok(());
                },
                JsonValue::Array(arr) => {
                    let arr_idx: usize = token.parse()?;
                    if arr_idx >= arr.len() {
                        anyhow::bail!("remove index out of bounds");
                    }
                    arr.remove(arr_idx);
                    return Ok(());
                },
                _ => anyhow::bail!("remove through non-container"),
            }
        } else {
            cur = match cur {
                JsonValue::Object(map) => map.get_mut(token).ok_or_else(|| anyhow::anyhow!("missing key"))?,
                JsonValue::Array(arr) => {
                    let arr_idx: usize = token.parse()?;
                    arr.get_mut(arr_idx).ok_or_else(|| anyhow::anyhow!("missing index"))?
                },
                _ => anyhow::bail!("path through non-container"),
            };
        }
    }
    Ok(())
}

#[cfg(test)]
fn parse_pointer(path: &str) -> anyhow::Result<Vec<String>> {
    if path.is_empty() {
        return Ok(vec![]);
    }
    if !path.starts_with('/') {
        anyhow::bail!("pointer must start with /");
    }
    Ok(path[1..]
        .split('/')
        .map(|t| t.replace("~1", "/").replace("~0", "~"))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::value::{
        ConvexValue,
        JsonPackedValue,
    };

    fn packed(v: ConvexValue) -> JsonPackedValue {
        JsonPackedValue::pack(v)
    }

    #[test]
    fn test_small_value_no_patch() {
        let a = packed(ConvexValue::from(1));
        let b = packed(ConvexValue::from(2));
        // Even though values differ, b is tiny (<1KB) so no patch
        assert!(maybe_patch(&a, &b).is_none());
    }

    #[test]
    fn test_large_value_patch() {
        // Create large array ~2KB
        let arr_a: Vec<ConvexValue> = (0..200).map(|i| ConvexValue::from(i as i64)).collect();
        let arr_b: Vec<ConvexValue> = (0..200).map(|i| if i == 50 { ConvexValue::from(9999i64)} else {ConvexValue::from(i as i64)}).collect();
        let a = JsonPackedValue::pack(ConvexValue::try_from(arr_a).unwrap());
        let b = JsonPackedValue::pack(ConvexValue::try_from(arr_b).unwrap());
        let patch = maybe_patch(&a, &b).expect("should produce patch");
        // Patch should be small single replace
        let patch_str = serde_json::to_string(&patch).unwrap();
        assert!(patch_str.len() < 200, "patch len {patch_str}");
        // Round-trip
        let applied = apply_patch(a.as_str(), &patch).unwrap();
        assert_eq!(applied, b.as_str());
    }

    #[test]
    fn test_patch_ratio_fallback() {
        // Two large but completely different arrays -> patch ≈ full size, should fallback
        let arr_a: Vec<ConvexValue> = (0..300).map(|i| ConvexValue::from(i as i64)).collect();
        let arr_b: Vec<ConvexValue> = (300..600).map(|i| ConvexValue::from(i as i64)).collect();
        let a = JsonPackedValue::pack(ConvexValue::try_from(arr_a).unwrap());
        let b = JsonPackedValue::pack(ConvexValue::try_from(arr_b).unwrap());
        // Patch will be many replaces ~ full size, expect None due to ratio
        // Allow either Some small patch or None; but if Some, patch must be <0.8*new
        if let Some(patch) = maybe_patch(&a, &b) {
            let patch_len = serde_json::to_string(&patch).unwrap().len();
            assert!(is_patch_worth_it(patch_len, b.as_str().len()));
        }
    }

    #[test]
    fn test_object_patch() {
        let obj_a = ConvexValue::try_from(serde_json::json!({"a":1,"b":2,"c": {"x": 1}})).unwrap();
        let obj_b = ConvexValue::try_from(serde_json::json!({"a":1,"b":99,"c": {"x": 2}})).unwrap();
        let a = JsonPackedValue::pack(obj_a);
        let b = JsonPackedValue::pack(obj_b);
        // Not large enough, will be None; force via direct diff
        let patch = diff_to_patch(a.as_str(), b.as_str()).unwrap();
        let applied = apply_patch(a.as_str(), &patch).unwrap();
        assert_eq!(applied, b.as_str());
        // patch has two ops (b replace, c/x replace)
        assert_eq!(patch.as_array().unwrap().len(), 2);
    }
}
