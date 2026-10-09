//! Indexed vocabulary reads and guarded semantic tag edits in the reference owner.
use super::*;
use mneme_core::ports::*;
use std::ops::Bound::{Excluded, Included, Unbounded};

impl MemStore {
    pub(super) fn replace_tags_checked(
        &self,
        id: NodeId,
        expected: &mneme_core::BoundedTagSet,
        tags: &mneme_core::BoundedTagSet,
        guards: Option<&RetagContentGuards>,
    ) -> Result<Node> {
        let mut g = self.lock();
        g.require_semantic_ids(&[id])?;
        if let Some(guards) = guards {
            guards.validate()?;
            for guard in &guards.guard_nodes {
                guard.check(g.nodes.get(&guard.id))?;
            }
        }
        let current = g.nodes.get(&id).ok_or(Error::NotFound)?;
        if let Some(guards) = guards {
            guards.check_target(current)?;
        }
        current
            .validate()
            .map_err(|error| Error::InvalidInput(error.to_string()))?;
        if current.tag_set() != expected {
            return Err(Error::Conflict(
                "node tags changed; inspect current tags before retrying".into(),
            ));
        }
        let mut replacement = current.clone();
        replacement.replace_tags(tags.clone());
        if g.touchstones.contains_key(&id) {
            validate_touchstone_owner_replacement(current, &replacement)?;
        }
        let sample = (stable_tag_sample_hash(id), id);
        let projection_needs_repair = !g.tag_projection.indexes_node_exactly(current, sample);
        let changed = current.tag_set() != tags || projection_needs_repair;
        let next_epoch = g.preflight_epoch_advance(changed)?;
        if changed {
            if projection_needs_repair {
                g.replace_node_repairing_projection(replacement.clone());
            } else {
                g.replace_node(replacement.clone());
            }
            g.finish_epoch_advance(next_epoch);
        }
        Ok(replacement)
    }

    pub(super) fn read_tag_vocabulary(
        &self,
        request: &TagVocabularyRequest,
    ) -> Result<TagVocabularyPage> {
        request.validate()?;
        let inner = self.lock();
        let mut page = TagVocabularyPage::default();
        let mut after = request.after.clone();
        loop {
            // Both lifecycle maps need one indexed next-name seek; no member walk.
            if page.work.name_seeks + 2 > MAX_TAG_VOCABULARY_SEEKS {
                page.work.stopped = TagVocabularyStop::SeekBudget;
                page.next = after;
                break;
            }
            let lower = after
                .as_deref()
                .map_or(Included(request.prefix.as_str()), Excluded);
            let active = inner
                .tag_projection
                .active
                .range::<str, _>((lower.clone(), Unbounded))
                .next();
            let archived = inner
                .tag_projection
                .archived
                .range::<str, _>((lower, Unbounded))
                .next();
            page.work.name_seeks += 2;
            let name = match (active, archived) {
                (None, None) => break,
                (Some((a, _)), Some((b, _))) => a.min(b),
                (Some((a, _)), None) | (None, Some((a, _))) => a,
            };
            if !name.starts_with(&request.prefix) {
                break;
            }
            let buckets = [
                inner
                    .tag_projection
                    .active
                    .get(name)
                    .filter(|_| request.status.allows(NodeStatus::Active)),
                inner
                    .tag_projection
                    .archived
                    .get(name)
                    .filter(|_| request.status.allows(NodeStatus::Archived)),
            ];
            let count: usize = buckets.iter().flatten().map(|b| b.len()).sum();
            if count > 0 {
                // Lookahead proves whether an item-limit page needs a continuation.
                if page.items.len() == request.limit {
                    page.work.stopped = TagVocabularyStop::ItemLimit;
                    page.next = after;
                    break;
                }
                let mut examples = Vec::new();
                for bucket in buckets.into_iter().flatten() {
                    for (_, id) in bucket
                        .iter()
                        .take(MAX_TAG_VOCABULARY_EXAMPLES - examples.len())
                    {
                        examples.push(*id);
                        page.work.membership_rows += 1;
                    }
                }
                page.items.push(TagVocabularyItem {
                    name: name.clone(),
                    count: TagVocabularyCount::Exact { value: count },
                    examples,
                });
            }
            after = Some(name.clone());
        }
        Ok(page)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn tagged(id: NodeId, tags: &[&str], status: NodeStatus) -> Node {
        let mut node = crate::touchstone_tests::target(id, "vocabulary fixture");
        node.replace_tags(mneme_core::BoundedTagSet::try_from_iter(tags.iter().copied()).unwrap());
        node.set_status(status);
        node
    }
    async fn vocabulary_contract(store: &dyn GraphStore) {
        for (tags, status) in [
            (vec!["People", "People/ExactHandle"], NodeStatus::Active),
            (vec!["People", "People/archive"], NodeStatus::Archived),
            (vec!["people"], NodeStatus::Active),
        ] {
            store
                .put_node(&tagged(NodeId(Ulid::new()), &tags, status))
                .await
                .unwrap();
        }
        let mut request = TagVocabularyRequest {
            prefix: "People".into(),
            status: TagVocabularyStatus::All,
            after: None,
            limit: 1,
        };
        let first = store.tag_vocabulary_page(&request).await.unwrap();
        assert_eq!(first.items[0].name, "People");
        assert_eq!(first.items[0].count, TagVocabularyCount::Exact { value: 2 });
        assert_eq!(first.items[0].examples.len(), 2);
        assert_eq!(first.next.as_deref(), Some("People"));
        request.after = first.next;
        let second = store.tag_vocabulary_page(&request).await.unwrap();
        assert_eq!(second.items[0].name, "People/ExactHandle");
        request.after = second.next;
        let third = store.tag_vocabulary_page(&request).await.unwrap();
        assert_eq!(third.items[0].name, "People/archive");
        assert!(third.next.is_none());
        request.after = None;
        request.status = TagVocabularyStatus::Archived;
        request.limit = 64;
        let archived = store.tag_vocabulary_page(&request).await.unwrap();
        assert_eq!(
            archived
                .items
                .iter()
                .map(|i| i.name.as_str())
                .collect::<Vec<_>>(),
            ["People", "People/archive"]
        );
        assert!(
            archived
                .items
                .iter()
                .all(|i| i.count == TagVocabularyCount::Exact { value: 1 })
        );
        assert!(archived.work.name_seeks <= MAX_TAG_VOCABULARY_SEEKS);

        let source = mneme_core::CaptureSource::new_with_codec(
            "tag-stewardship-test",
            "episode",
            "test://episode",
            None::<&str>,
            None::<&str>,
            [4; 32],
            mneme_core::CaptureRequestCodec::EpisodeV1,
        )
        .unwrap();
        let edition = Node::try_new(
            source.node_id(),
            "historical event, not semantic vocabulary",
            mneme_core::BodyRef::new("inline://episode").unwrap(),
            ["episode-only"],
            Provenance::External { source },
            0.5,
            0.5,
            NodeStatus::Active,
            1,
        )
        .unwrap();
        let facet = mneme_core::EpisodeFacet::initial(
            edition.id(),
            mneme_core::OccurrenceSpan::Unknown,
            None,
            mneme_core::EpisodeTime::new(1).unwrap(),
        )
        .unwrap();
        let edition = edition.with_episode(facet).unwrap();
        store
            .episodes()
            .unwrap()
            .commit_episode(mneme_core::EpisodeCommit {
                node: &edition,
                embedding: &[1., 0., 0., 0.],
                links: &[],
                expectation: mneme_core::EpisodeWriteExpectation::NewRoot,
            })
            .await
            .unwrap();
        let episodes = store
            .tag_vocabulary_page(&TagVocabularyRequest {
                prefix: "episode-only".into(),
                status: TagVocabularyStatus::All,
                after: None,
                limit: 64,
            })
            .await
            .unwrap();
        assert!(episodes.items.is_empty());
        let guards = RetagContentGuards {
            target_fingerprint: routing_content_fingerprint(&edition),
            guard_nodes: Vec::new(),
        };
        assert!(
            store
                .compare_replace_node_tags_guarded(
                    edition.id(),
                    edition.tag_set(),
                    edition.tag_set(),
                    &guards
                )
                .await
                .is_err()
        );
        let semantic = store
            .get_node(first.items[0].examples[0])
            .await
            .unwrap()
            .unwrap();
        let guards = RetagContentGuards {
            target_fingerprint: routing_content_fingerprint(&semantic),
            guard_nodes: vec![NodeContentGuard {
                id: edition.id(),
                content_fingerprint: routing_content_fingerprint(&edition),
            }],
        };
        assert!(matches!(
            store
                .compare_replace_node_tags_guarded(
                    semantic.id(),
                    semantic.tag_set(),
                    semantic.tag_set(),
                    &guards
                )
                .await,
            Err(Error::Conflict(_))
        )); // An episode cannot become the policy guide.
    }
    async fn guarded_contract(store: &dyn GraphStore) {
        let mut target = tagged(NodeId(Ulid::new()), &["keep", "old"], NodeStatus::Active);
        let mut guide = tagged(NodeId(Ulid::new()), &["guide"], NodeStatus::Archived);
        store.put_node(&target).await.unwrap();
        store.put_node(&guide).await.unwrap();
        let original = target.tag_set().clone();
        let replacement = mneme_core::BoundedTagSet::try_from_iter(["keep", "new"]).unwrap();
        let guards = RetagContentGuards {
            target_fingerprint: routing_content_fingerprint(&target),
            guard_nodes: vec![NodeContentGuard {
                id: guide.id(),
                content_fingerprint: routing_content_fingerprint(&guide),
            }],
        };
        // A changed summary with unchanged tags must invalidate classifier work.
        target.set_summary(mneme_core::NodeSummary::new("changed target meaning").unwrap());
        store.put_node(&target).await.unwrap();
        assert!(matches!(
            store
                .compare_replace_node_tags_guarded(target.id(), &original, &replacement, &guards)
                .await,
            Err(Error::Conflict(_))
        ));
        assert_eq!(
            store
                .get_node(target.id())
                .await
                .unwrap()
                .unwrap()
                .tag_set(),
            &original
        );
        let mut fresh = guards.clone();
        fresh.target_fingerprint = routing_content_fingerprint(&target);
        guide.set_summary(mneme_core::NodeSummary::new("changed guide meaning").unwrap());
        store.put_node(&guide).await.unwrap();
        assert!(matches!(
            store
                .compare_replace_node_tags_guarded(target.id(), &original, &replacement, &fresh)
                .await,
            Err(Error::Conflict(_))
        ));
        fresh.guard_nodes[0].content_fingerprint = routing_content_fingerprint(&guide);
        // The guide is canonical summary text, not body bytes. Ownership changes
        // are irrelevant and do not invalidate its observed policy meaning.
        guide.set_body_ownership(mneme_core::BodyOwnership::Managed);
        store.put_node(&guide).await.unwrap();
        store
            .compare_replace_node_tags_guarded(target.id(), &original, &original, &fresh)
            .await
            .unwrap();
        let result = store
            .compare_replace_node_tags_guarded(target.id(), &original, &replacement, &fresh)
            .await
            .unwrap();
        assert_eq!(result.tag_set(), &replacement);
        assert_eq!(result.summary(), target.summary());
        assert!(matches!(
            store
                .compare_replace_node_tags_guarded(target.id(), &replacement, &replacement, &fresh)
                .await,
            Err(Error::Conflict(_))
        ));
        fresh.target_fingerprint = routing_content_fingerprint(&result);
        store.delete_node(guide.id()).await.unwrap();
        assert!(matches!(
            store
                .compare_replace_node_tags_guarded(target.id(), &replacement, &replacement, &fresh)
                .await,
            Err(Error::Conflict(_))
        ));
    }
    #[tokio::test]
    async fn reference_tag_stewardship_contract() {
        let store = MemStore::new(4);
        vocabulary_contract(&store).await;
        guarded_contract(&store).await;
    }
    #[cfg(feature = "cozo")]
    #[tokio::test]
    async fn cozo_tag_stewardship_contract() {
        let store = CozoStore::new(4).unwrap();
        vocabulary_contract(&store).await;
        guarded_contract(&store).await;
    }
    #[tokio::test]
    async fn reference_popular_tag_seeks_do_not_walk_memberships() {
        let store = MemStore::new(4);
        for _ in 0..MAX_TAG_VOCABULARY_MEMBERSHIPS + 1 {
            store
                .put_node(&tagged(
                    NodeId(Ulid::new()),
                    &["a-popular"],
                    NodeStatus::Active,
                ))
                .await
                .unwrap();
        }
        store
            .put_node(&tagged(
                NodeId(Ulid::new()),
                &["b-rare"],
                NodeStatus::Active,
            ))
            .await
            .unwrap();
        let page = store
            .tag_vocabulary_page(&TagVocabularyRequest {
                prefix: String::new(),
                status: TagVocabularyStatus::All,
                after: None,
                limit: 64,
            })
            .await
            .unwrap();
        assert_eq!(page.items.len(), 2);
        assert_eq!(
            page.items[0].count,
            TagVocabularyCount::Exact {
                value: MAX_TAG_VOCABULARY_MEMBERSHIPS + 1
            }
        );
        assert_eq!(page.work.membership_rows, 4);
        assert_eq!(page.work.name_seeks, 6);
    }

    #[cfg(feature = "cozo")]
    #[tokio::test]
    async fn cozo_popular_tag_bounds_counts_but_not_name_discovery() {
        let store = CozoStore::new(4).unwrap();
        for _ in 0..MAX_TAG_VOCABULARY_MEMBERSHIPS + 1 {
            store
                .put_node(&tagged(
                    NodeId(Ulid::new()),
                    &["a-popular"],
                    NodeStatus::Active,
                ))
                .await
                .unwrap();
        }
        store
            .put_node(&tagged(
                NodeId(Ulid::new()),
                &["b-rare"],
                NodeStatus::Active,
            ))
            .await
            .unwrap();
        let mut request = TagVocabularyRequest {
            prefix: String::new(),
            status: TagVocabularyStatus::All,
            after: None,
            limit: 64,
        };
        let page = store.tag_vocabulary_page(&request).await.unwrap();
        assert_eq!(page.items.len(), 2);
        assert_eq!(page.items[0].name, "a-popular");
        assert_eq!(
            page.items[0].count,
            TagVocabularyCount::LowerBound {
                value: MAX_TAG_VOCABULARY_MEMBERSHIPS
            }
        );
        assert_eq!(page.items[1].name, "b-rare");
        assert_eq!(page.items[1].count, TagVocabularyCount::Unavailable);
        assert_eq!(page.work.membership_rows, MAX_TAG_VOCABULARY_MEMBERSHIPS);
        assert_eq!(page.work.name_seeks, 3);
        assert!(page.next.is_none()); // Membership exhaustion did not hide any names.
        request.after = Some("a-popular".into());
        let rare = store.tag_vocabulary_page(&request).await.unwrap();
        assert_eq!(rare.items[0].count, TagVocabularyCount::Exact { value: 1 });
        assert_eq!(rare.work.membership_rows, 1);
    }

    async fn empty_filtered_page_contract(store: &dyn GraphStore) {
        for i in 0..140 {
            store
                .put_node(&tagged(
                    NodeId(Ulid::new()),
                    &[&format!("scan/{i:03}")],
                    NodeStatus::Active,
                ))
                .await
                .unwrap();
        }
        let mut request = TagVocabularyRequest {
            prefix: "scan/".into(),
            status: TagVocabularyStatus::Archived,
            after: None,
            limit: 64,
        };
        let page = store.tag_vocabulary_page(&request).await.unwrap();
        assert!(page.items.is_empty());
        assert_eq!(page.work.name_seeks, MAX_TAG_VOCABULARY_SEEKS);
        assert_eq!(page.work.stopped, TagVocabularyStop::SeekBudget);
        assert!(page.next.is_some());
        request.after = page.next;
        let final_page = store.tag_vocabulary_page(&request).await.unwrap();
        assert!(final_page.items.is_empty());
        assert!(final_page.next.is_none());
        assert_eq!(final_page.work.stopped, TagVocabularyStop::Exhausted);
    }
    #[tokio::test]
    async fn reference_empty_filtered_vocabulary_progresses() {
        empty_filtered_page_contract(&MemStore::new(4)).await;
    }
    #[cfg(feature = "cozo")]
    #[tokio::test]
    async fn cozo_empty_filtered_vocabulary_progresses() {
        empty_filtered_page_contract(&CozoStore::new(4).unwrap()).await;
    }
}
