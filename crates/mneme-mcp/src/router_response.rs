//! Model-visible routing context without rewriting native domain/provenance data.
use crate::host::AnyErr;
use serde_json::{Value, json};

pub(super) fn envelope(db: &str, snapshot: Option<&Value>, result: Value) -> Value {
    let mut value = json!({"db":db,"result":result});
    if let Some(snapshot) = snapshot {
        value["snapshot"] = snapshot.clone();
    }
    value
}

pub(super) fn refusal(db: &str, snapshot: Option<&Value>, message: &str) -> Value {
    let mut result = crate::response::tool_success(&envelope(db, snapshot, json!(message)));
    result["isError"] = json!(true);
    result
}

pub(super) fn wrap(mut native: Value, db: &str, snapshot: Option<&Value>) -> Result<Value, AnyErr> {
    let object = native.as_object().ok_or("invalid upstream tool result")?;
    if object.keys().any(|key| {
        !matches!(
            key.as_str(),
            "content" | "isError" | "_meta" | "structuredContent"
        )
    }) {
        return Err("unsupported upstream result channel".into());
    }
    if native
        .get("isError")
        .is_some_and(|value| !value.is_boolean())
    {
        return Err("invalid upstream isError flag".into());
    }
    let blocks = native["content"]
        .as_array()
        .ok_or("invalid upstream content")?;
    if blocks.len() != 1
        || blocks[0]["type"] != "text"
        || blocks[0].as_object().is_none_or(|block| {
            block
                .keys()
                .any(|key| !matches!(key.as_str(), "type" | "text" | "annotations"))
        })
    {
        return Err("upstream result must contain one native JSON text block".into());
    }
    let text = blocks[0]["text"].as_str().ok_or("invalid upstream text")?;
    if text.len() > crate::response::MAX_TOOL_TEXT_BYTES {
        return Err("upstream text exceeds router response envelope".into());
    }
    let domain = match serde_json::from_str(text) {
        Ok(domain) => domain,
        Err(_) if native["isError"] == true => json!(text),
        Err(_) => return Err("upstream success text is not native domain JSON".into()),
    };
    let wrapped = envelope(db, snapshot, domain);
    let text = serde_json::to_string(&wrapped)?;
    if text.len() > crate::response::MAX_TOOL_TEXT_BYTES {
        return Err("wrapped upstream result exceeds router response envelope".into());
    }
    native["content"][0]["text"] = json!(text);
    if let Some(structured) = native.get_mut("structuredContent") {
        *structured = envelope(db, snapshot, structured.clone());
    }
    Ok(native)
}
