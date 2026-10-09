//! 规范化视图与内容哈希（03 §5.4）。
//!
//! > 哈希在**规范化视图**（递归键排序）上计算——既不依赖第三方库的默认键序，
//! > 也**与线上序列化格式无关**，因此更换格式不改变 `event_id`，
//! > 去重与替换语义不漂移。
//!
//! 实现要点：
//!
//! 1. 输入先经 `serde` 转成 [`serde_json::Value`] **数据树**（不是线上字节），
//!    因此 JSON / MessagePack / Protobuf 任何格式算出的都是同一棵树；
//! 2. [`canonical_view`] 对**每一层**的 object 键做字节序排序（递归键排序），
//!    并输出一种与任何线上格式都不同的**规范文本**；
//! 3. [`hash_text`] 在该规范文本上取 SHA-256。
//!
//! 排序发生在数据树层而非文本层，所以即便启用 `serde_json/preserve_order`
//! （此时 `Map` 是 `IndexMap`，插入序），结果也不变。

use crate::body::Body;
use crate::error::ProtocolError;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// 递归键排序的规范文本。
///
/// 这**不是** JSON：它是平台内部用于哈希的唯一规范表示，字节序稳定，
/// 与 `Serializer` 选择的线上格式解耦。
pub fn canonical_view(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_unstable(); // 递归键排序：本层字节序
            let mut out = String::from("{");
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&quote(k));
                out.push('=');
                out.push_str(&canonical_view(&map[*k]));
            }
            out.push('}');
            out
        }
        Value::Array(items) => {
            let mut out = String::from("[");
            for (i, v) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&canonical_view(v));
            }
            out.push(']');
            out
        }
        Value::String(s) => quote(s),
        // serde_json::Number 的 Display 走最短往返表示，稳定且可复现。
        other => other.to_string(),
    }
}

/// 把家族体转成规范化视图。
pub fn canonical_view_of_body(body: &Body) -> Result<String, ProtocolError> {
    let tree = serde_json::to_value(body)
        .map_err(|e| ProtocolError::Canonical(format!("body → Value: {e}")))?;
    Ok(canonical_view(&tree))
}

/// 家族体的内容哈希（SHA-256 十六进制，64 字符）。
///
/// **只覆盖内容（家族体），不覆盖信封头**：`received_ts` 每次推送都变，
/// 若纳入哈希则幂等键永不可能重复，去重语义失效。
pub fn content_hash_of(body: &Body) -> Result<String, ProtocolError> {
    Ok(hash_text(&canonical_view_of_body(body)?))
}

/// SHA-256 十六进制。
pub fn hash_text(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    let out = hasher.finalize();
    let mut s = String::with_capacity(out.len() * 2);
    for b in out {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn quote(s: &str) -> String {
    // 复用 serde_json 的字符串转义，保证唯一且确定。
    serde_json::Value::String(s.to_string()).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_key_order_is_irrelevant() {
        let a: Value = serde_json::from_str(r#"{"b":1,"a":{"z":true,"y":[2,1]}}"#).unwrap();
        let b: Value = serde_json::from_str(r#"{"a":{"y":[2,1],"z":true},"b":1}"#).unwrap();
        assert_eq!(canonical_view(&a), canonical_view(&b));
        assert_eq!(hash_text(&canonical_view(&a)), hash_text(&canonical_view(&b)));
    }

    #[test]
    fn array_order_is_significant() {
        let a: Value = serde_json::from_str("[1,2]").unwrap();
        let b: Value = serde_json::from_str("[2,1]").unwrap();
        assert_ne!(canonical_view(&a), canonical_view(&b));
    }

    #[test]
    fn strings_are_not_confused_with_keys() {
        let a: Value = serde_json::from_str(r#"{"a=b":1}"#).unwrap();
        let b: Value = serde_json::from_str(r#"{"a":{"b":1}}"#).unwrap();
        assert_ne!(canonical_view(&a), canonical_view(&b));
    }
}
