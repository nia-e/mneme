//! Bounded physical tag-name seeks. Never count a group with an unbounded aggregate.
use super::*;
use mneme_core::ports::*;

fn scan(
    tx: &mut BoundedReadTransaction,
    prefix: Vec<DataValue>,
    lower: PrimaryKeyScanBound,
    limit: usize,
) -> Result<NamedRows> {
    let rows = tx
        .scan_relation_by_primary_key(
            "node_tag_v2",
            PrimaryKeyScan {
                prefix,
                lower,
                upper: PrimaryKeyScanBound::Unbounded,
                direction: PrimaryKeyScanDirection::Ascending,
                limit,
            },
        )
        .map_err(backend)?;
    if rows.rows.len() > limit {
        return Err(backend_str(
            "tag vocabulary backend exceeded row allowance".into(),
        ));
    }
    Ok(rows)
}

fn member(row: &[DataValue], name: &str, status: Option<&str>) -> Result<NodeId> {
    if row.len() != 4 {
        return Err(backend_str("malformed tag vocabulary membership".into()));
    }
    let actual_name = want_str(&row[0])?;
    mneme_core::validate_tag(actual_name).map_err(|e| backend_str(e.to_string()))?;
    let actual_status = want_str(&row[1])?;
    let id = graph_records::decode_canonical_node_id(&row[3], "tag vocabulary member")?;
    if actual_name != name
        || !matches!(actual_status, "active" | "archived")
        || status.is_some_and(|s| s != actual_status)
        || want_i64(&row[2])? != stable_tag_sample_hash(id)
    {
        return Err(backend_str(
            "tag vocabulary membership escaped prefix or has invalid sample hash".into(),
        ));
    }
    Ok(id)
}

pub(super) fn read(db: &DbInstance, request: &TagVocabularyRequest) -> Result<TagVocabularyPage> {
    request.validate()?;
    let mut tx = db
        .read_multi_transaction_with_timeout(TAGGED_READ_TIMEOUT)
        .map_err(backend)?;
    let result = (|| {
        let marker = tx
            .run_script(
                "?[v] := *meta{k:$key,v}",
                BTreeMap::from([("key".into(), dv_str(crate::tag_projection::META_KEY))]),
            )
            .map_err(backend)?;
        if marker.rows.len() != 1
            || want_str(&marker.rows[0][0])? != crate::tag_projection::META_VALUE
        {
            return Err(backend_str(
                "tag vocabulary requires the verified semantic tag projection".into(),
            ));
        }
        let mut page = TagVocabularyPage::default();
        let mut after = request.after.clone();
        loop {
            // Reserve a name seek and, if needed, one requested-status probe.
            let seeks = if request.status == TagVocabularyStatus::All {
                1
            } else {
                2
            };
            if page.work.name_seeks + seeks > MAX_TAG_VOCABULARY_SEEKS {
                page.work.stopped = TagVocabularyStop::SeekBudget;
                page.next = after;
                break;
            }
            let lower = match &after {
                // Tags exclude controls. name + NUL lies strictly after this whole
                // exact-name group and before every valid extension of name.
                // Public primary-key scans require complete suffix-key bounds.
                Some(name) => PrimaryKeyScanBound::Included(vec![
                    dv_str(&format!("{name}\0")),
                    dv_str(""),
                    dv_int(i64::MIN),
                    dv_str(""),
                ]),
                None => PrimaryKeyScanBound::Included(vec![
                    dv_str(&request.prefix),
                    dv_str(""),
                    dv_int(i64::MIN),
                    dv_str(""),
                ]),
            };
            let rows = scan(&mut tx, Vec::new(), lower, 1)?;
            page.work.name_seeks += 1;
            let Some(row) = rows.rows.first() else {
                break;
            };
            let name = want_str(&row[0])?.to_owned();
            member(row, &name, None)?;
            if !name.starts_with(&request.prefix) {
                break;
            }
            if after.as_ref().is_some_and(|last| name <= *last) {
                return Err(backend_str(
                    "tag vocabulary returned nonprogressing names".into(),
                ));
            }
            let statuses: &[&str] = match request.status {
                TagVocabularyStatus::All => &["active", "archived"],
                TagVocabularyStatus::Active => &["active"],
                TagVocabularyStatus::Archived => &["archived"],
            };
            let admitted = if request.status == TagVocabularyStatus::All {
                true
            } else {
                let rows = scan(
                    &mut tx,
                    vec![dv_str(&name), dv_str(statuses[0])],
                    PrimaryKeyScanBound::Unbounded,
                    1,
                )?;
                page.work.name_seeks += 1;
                if let Some(row) = rows.rows.first() {
                    member(row, &name, Some(statuses[0]))?;
                    true
                } else {
                    false
                }
            };
            if admitted {
                if page.items.len() == request.limit {
                    page.work.stopped = TagVocabularyStop::ItemLimit;
                    page.next = after;
                    break;
                }
                let mut value = 0;
                let mut exact = true;
                let mut examples = Vec::new();
                let initial_work = page.work.membership_rows;
                for status in statuses {
                    let remaining = MAX_TAG_VOCABULARY_MEMBERSHIPS - page.work.membership_rows;
                    if remaining == 0 {
                        exact = false;
                        break;
                    }
                    let rows = scan(
                        &mut tx,
                        vec![dv_str(&name), dv_str(status)],
                        PrimaryKeyScanBound::Unbounded,
                        remaining,
                    )?;
                    page.work.membership_rows += rows.rows.len();
                    if rows.rows.len() == remaining {
                        exact = false;
                    }
                    for row in &rows.rows {
                        let id = member(row, &name, Some(status))?;
                        value += 1;
                        if examples.len() < MAX_TAG_VOCABULARY_EXAMPLES {
                            examples.push(id);
                        }
                    }
                }
                let count = if exact {
                    TagVocabularyCount::Exact { value }
                } else if page.work.membership_rows == initial_work {
                    TagVocabularyCount::Unavailable
                } else {
                    TagVocabularyCount::LowerBound { value }
                };
                page.items.push(TagVocabularyItem {
                    name: name.clone(),
                    count,
                    examples,
                });
            }
            after = Some(name);
        }
        Ok(page)
    })();
    let close = tx.close_and_join().map_err(backend);
    match (result, close) {
        (Ok(page), Ok(())) => Ok(page),
        (Err(error), _) | (_, Err(error)) => Err(error),
    }
}
