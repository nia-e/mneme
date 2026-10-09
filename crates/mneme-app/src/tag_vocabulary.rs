//! Bounded observed vocabulary over semantic tag indexes, not an inferred taxonomy.
use crate::list::{DEFAULT_LIST_LIMIT, ListError, MAX_LIST_PAGE_BYTES};
use mneme_core::ports::{
    MAX_TAG_VOCABULARY_EXAMPLES, MAX_TAG_VOCABULARY_ITEMS, MAX_TAG_VOCABULARY_MEMBERSHIPS,
    MAX_TAG_VOCABULARY_SEEKS, TagVocabularyCount, TagVocabularyRequest, TagVocabularyStatus,
};
use mneme_engine::Memory;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use ulid::Ulid;

// A cursor contains two tag-sized strings; JSON escaping needs at most six
// bytes per UTF-8 byte. Reserve the remainder for version, owner and selection.
const MAX_CURSOR_BYTES: usize = 4096;

#[derive(Serialize)]
pub struct PreparedTagList {
    kind: String,
    prefix: String,
    status: TagVocabularyStatus,
    limit: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    after: Option<String>,
}

fn default_limit() -> usize {
    DEFAULT_LIST_LIMIT
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    db_id: Ulid,
    prefix: String,
    status: TagVocabularyStatus,
    after: String,
}

impl Cursor {
    fn parse(encoded: &str) -> Result<Self, ListError> {
        if encoded.len() > MAX_CURSOR_BYTES || encoded.chars().any(char::is_control) {
            return Err("tags list cursor exceeds its bounds".into());
        }
        let raw = encoded
            .strip_prefix("tags-v1:")
            .ok_or("invalid tags list cursor version")?;
        let cursor: Self = serde_json::from_str(raw)?;
        TagVocabularyRequest {
            prefix: cursor.prefix.clone(),
            status: cursor.status,
            after: Some(cursor.after.clone()),
            limit: 1,
        }
        .validate()?;
        Ok(cursor)
    }

    fn encode(&self) -> Result<String, ListError> {
        let encoded = format!("tags-v1:{}", serde_json::to_string(self)?);
        if encoded.len() > MAX_CURSOR_BYTES {
            return Err("tags list continuation exceeds its byte allowance".into());
        }
        Ok(encoded)
    }
}

impl PreparedTagList {
    pub(crate) fn parse(raw: &Value) -> Result<Self, ListError> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Input {
            kind: String,
            #[serde(default)]
            prefix: String,
            #[serde(default)]
            status: TagVocabularyStatus,
            #[serde(default = "default_limit")]
            limit: usize,
            after: Option<String>,
        }
        for field in ["kind", "prefix", "status", "limit", "after"] {
            if raw.get(field).is_some_and(Value::is_null) {
                return Err(format!("tags list {field} must not be null").into());
            }
        }
        let raw_input: Input = serde_json::from_value(raw.clone())?;
        let input = Self {
            kind: raw_input.kind,
            prefix: raw_input.prefix,
            status: raw_input.status,
            limit: raw_input.limit,
            after: raw_input.after,
        };
        if input.kind != "tags" {
            return Err("tags list kind must be tags".into());
        }
        input.request(None).validate()?;
        if let Some(encoded) = &input.after {
            let cursor = Cursor::parse(encoded)?;
            if cursor.prefix != input.prefix || cursor.status != input.status {
                return Err("tags list cursor selection mismatch; keep prefix/status unchanged or start a new list".into());
            }
        }
        Ok(input)
    }

    fn request(&self, after: Option<String>) -> TagVocabularyRequest {
        TagVocabularyRequest {
            prefix: self.prefix.clone(),
            status: self.status,
            after,
            limit: self.limit,
        }
    }

    pub(crate) fn into_json(self) -> Value {
        serde_json::to_value(self).expect("tag list contains serializable values")
    }

    pub(crate) async fn run(self, memory: &Memory, db_id: Ulid) -> Result<Value, ListError> {
        let after = if let Some(encoded) = &self.after {
            let cursor = Cursor::parse(encoded)?;
            if cursor.db_id != db_id {
                return Err("tags list cursor database identity mismatch; start a new list in this database".into());
            }
            Some(cursor.after)
        } else {
            None
        };
        let request = self.request(after);
        let page = memory.tag_vocabulary_page(&request).await?;
        if page.items.len() > self.limit
            || page.work.name_seeks > MAX_TAG_VOCABULARY_SEEKS
            || page.work.membership_rows > MAX_TAG_VOCABULARY_MEMBERSHIPS
        {
            return Err("tag vocabulary backend exceeded its requested work allowance".into());
        }
        let mut previous = request.after.as_deref();
        for item in &page.items {
            mneme_core::validate_tag(&item.name)?;
            if !item.name.starts_with(&self.prefix)
                || previous.is_some_and(|last| item.name.as_str() <= last)
                || item.examples.len() > MAX_TAG_VOCABULARY_EXAMPLES
            {
                return Err(
                    "tag vocabulary backend returned an invalid or out-of-order item".into(),
                );
            }
            previous = Some(&item.name);
        }
        let has_more = page.next.is_some();
        let counts_complete = page
            .items
            .iter()
            .all(|item| matches!(item.count, TagVocabularyCount::Exact { .. }));
        let next_cursor = page
            .next
            .map(|after| {
                if request.after.as_ref().is_some_and(|last| &after <= last)
                    || previous.is_some_and(|last| after.as_str() < last)
                    || !after.starts_with(&self.prefix)
                {
                    return Err(
                        "tag vocabulary backend returned a non-progressing continuation".into(),
                    );
                }
                mneme_core::validate_tag(&after)?;
                Cursor {
                    db_id,
                    prefix: self.prefix.clone(),
                    status: self.status,
                    after,
                }
                .encode()
            })
            .transpose()?;
        let result = json!({"kind":"tags","db_id":db_id.to_string(),"items":page.items,
            "next_cursor":next_cursor,"has_more":has_more,"partial":has_more || !counts_complete,
            "coverage":{"semantic_only":true,"snapshot":false,"order":"tag_ascending",
                "prefix":self.prefix,"status":self.status,
                "counts_complete":counts_complete,
                "name_seeks":page.work.name_seeks,"membership_rows":page.work.membership_rows,
                "stopped":page.work.stopped,"max_name_seeks":MAX_TAG_VOCABULARY_SEEKS,
                "max_membership_rows":MAX_TAG_VOCABULARY_MEMBERSHIPS,
                "max_examples_per_tag":MAX_TAG_VOCABULARY_EXAMPLES}});
        if serde_json::to_vec(&result)?.len() > MAX_LIST_PAGE_BYTES {
            return Err("tag vocabulary response exceeds its page byte allowance".into());
        }
        Ok(result)
    }
}

pub(crate) fn input_schema() -> Value {
    json!({"type":"object","additionalProperties":false,"required":["kind"],
    "description":"Observed semantic-only tag vocabulary, in exact ascending tag order. No inferred taxonomy, episode tags, bodies, whole-graph scan or inference. Prefix is exact and case-sensitive, with no normalization. Counts are bounded exact/lower_bound/unavailable observations; examples are bounded node identities, not ranked representatives. Cursors bind database identity and prefix/status, not a snapshot. Follow next_cursor until null, including empty filtered pages; concurrent changes can be missed and require a fresh pass.",
    "properties":{
        "kind":{"type":"string","const":"tags"},
        "prefix":{"type":"string","maxLength":mneme_core::MAX_TAG_BYTES,"default":""},
        "status":{"type":"string","enum":["active","archived","all"],"default":"all"},
        "after":{"type":"string","minLength":1,"maxLength":MAX_CURSOR_BYTES},
        "limit":{"type":"integer","minimum":1,"maximum":MAX_TAG_VOCABULARY_ITEMS,"default":DEFAULT_LIST_LIMIT}
    }})
}
