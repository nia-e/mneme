//! Authored-reference discovery and compact projection from frozen anchors.
//! This owner keeps read admission, record/health caches and its scheduler
//! together, independently of learned association edges and semantic ranking.

#[cfg(test)]
use std::time::Duration;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    future::Future,
    num::NonZeroU16,
    time::Instant,
};

use mneme_core::ports::Error;
use mneme_core::{
    NodeId, SummarySnapshot, TouchstoneCursor, TouchstonePage, TouchstonePageRequest,
    TouchstoneRecord, TouchstoneReferrersRequest,
};
use mneme_engine::Memory;
use mneme_present::{
    PrimaryInputCard, TouchstoneOrigin, TouchstoneReferenceView, TouchstoneResolution,
    TouchstoneRetrieval, TouchstoneStopReason, TouchstoneView,
};

use super::discovery::{MAX_LINKED_CAPACITY, remaining};
use crate::ContextError;

// Touchstone reference metadata is independent of learned association edges.
// Only the frozen original anchors are queried; an incoming owner is hydrated
// for its view, but never added to the discovery queue.
pub(super) trait TouchstoneReader: Sync {
    fn catalog(&self) -> impl Future<Output = Result<TouchstonePage, Error>> + Send;
    fn referrers(
        &self,
        id: NodeId,
        after: Option<TouchstoneCursor>,
    ) -> impl Future<Output = Result<TouchstonePage, Error>> + Send;
    fn record(
        &self,
        id: NodeId,
    ) -> impl Future<Output = Result<Option<TouchstoneRecord>, Error>> + Send;
    fn snapshot(
        &self,
        id: NodeId,
    ) -> impl Future<Output = Result<Option<SummarySnapshot>, Error>> + Send;
}
impl TouchstoneReader for Memory {
    async fn catalog(&self) -> Result<TouchstonePage, Error> {
        self.touchstones_page(&TouchstonePageRequest::new(None, None, 1)?)
            .await
    }
    async fn referrers(
        &self,
        id: NodeId,
        after: Option<TouchstoneCursor>,
    ) -> Result<TouchstonePage, Error> {
        self.touchstone_referrers_page(&TouchstoneReferrersRequest::new(id, after, 1)?)
            .await
    }
    async fn record(&self, id: NodeId) -> Result<Option<TouchstoneRecord>, Error> {
        self.get_touchstone(id).await
    }
    async fn snapshot(&self, id: NodeId) -> Result<Option<SummarySnapshot>, Error> {
        self.summary_snapshot(id).await
    }
}

pub(super) struct TouchstoneWindow {
    pub(super) views: BTreeMap<NodeId, TouchstoneView>,
    pub(super) incoming: Vec<PrimaryInputCard>,
    pub(super) coverage: TouchstoneRetrieval,
}

fn touchstone_error(error: &Error) -> TouchstoneStopReason {
    match error {
        Error::InvalidInput(message)
            if message == "native touchstone operations are unsupported by this store" =>
        {
            TouchstoneStopReason::Unsupported
        }
        _ => TouchstoneStopReason::ReadError,
    }
}

async fn touchstone_read<T>(
    deadline: Instant,
    future: impl Future<Output = Result<T, Error>>,
) -> Result<T, TouchstoneStopReason> {
    let duration = remaining(deadline).ok_or(TouchstoneStopReason::Deadline)?;
    match tokio::time::timeout(duration, future).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(touchstone_error(&error)),
        Err(_) => Err(TouchstoneStopReason::Deadline),
    }
}

fn admit_touchstone_read(coverage: &mut TouchstoneRetrieval, deadline: Instant) -> bool {
    let reason = if remaining(deadline).is_none() {
        Some(TouchstoneStopReason::Deadline)
    } else if coverage.reads() >= coverage.read_limit {
        Some(TouchstoneStopReason::Budget)
    } else {
        None
    };
    if let Some(reason) = reason {
        coverage.stop_reason.get_or_insert(reason);
        coverage.further_tail_unknown = true;
        false
    } else {
        true
    }
}

async fn project_touchstone(
    reader: &impl TouchstoneReader,
    record: &TouchstoneRecord,
    original: &BTreeSet<NodeId>,
    direct: bool,
    origin: TouchstoneOrigin,
    coverage: &mut TouchstoneRetrieval,
    health: &mut BTreeMap<NodeId, Option<SummarySnapshot>>,
    deadline: Instant,
) -> Result<TouchstoneView, ContextError> {
    // Leave room for the author's own note, rather than reserving the full
    // immutable record. Large subjects/origin unions still pack atomically.
    const COMPACT_REFERENCE_BYTES: usize = 1024;
    let total = u32::try_from(record.references().len())
        .map_err(|_| Error::Backend("touchstone reference collection exceeds u32".into()))?;
    let mut view = TouchstoneView::new(
        record.subject().as_str().to_owned(),
        Vec::new(),
        total,
        vec![origin],
    )?;
    for snapshot in record.references() {
        if !direct && !original.contains(&snapshot.id()) {
            continue;
        }
        let mut row = TouchstoneReferenceView::new(
            NodeId(snapshot.db_id()),
            snapshot.id(),
            snapshot.digest().to_hex(),
            snapshot.summary().as_str(),
            128,
            TouchstoneResolution::Unavailable,
        )?;
        // Check whole-row feasibility before spending optional health work.
        let mut trial = view.clone();
        trial.references.push(row.clone());
        trial.references_omitted -= 1;
        if serde_json::to_vec(&trial)
            .expect("typed touchstone view encodes")
            .len()
            > COMPACT_REFERENCE_BYTES
        {
            break;
        }
        if let Some(current) = health.get(&snapshot.id()) {
            row.resolution = match current {
                Some(current) if current.digest() == snapshot.digest() => {
                    TouchstoneResolution::MatchesSnapshot
                }
                Some(_) => TouchstoneResolution::ChangedSnapshot,
                None => TouchstoneResolution::Missing,
            };
        } else if admit_touchstone_read(coverage, deadline) {
            coverage.target_reads += 1;
            match touchstone_read(deadline, reader.snapshot(snapshot.id())).await {
                Ok(current) => {
                    row.resolution = match &current {
                        Some(current) if current.digest() == snapshot.digest() => {
                            TouchstoneResolution::MatchesSnapshot
                        }
                        Some(_) => TouchstoneResolution::ChangedSnapshot,
                        None => TouchstoneResolution::Missing,
                    };
                    health.insert(snapshot.id(), current);
                }
                Err(reason) => {
                    coverage.stop_reason.get_or_insert(reason);
                    coverage.further_tail_unknown = true;
                }
            }
        }
        // Resolution spellings have different sizes: final exact row, not the
        // provisional unavailable spelling, decides whole-row admission.
        trial.references.pop();
        trial.references.push(row);
        if serde_json::to_vec(&trial)
            .expect("typed touchstone view encodes")
            .len()
            > COMPACT_REFERENCE_BYTES
        {
            break;
        }
        view = trial;
    }
    Ok(view)
}

pub(super) async fn compose_touchstones(
    reader: &impl TouchstoneReader,
    frozen: &[NodeId],
    semantic: &[NodeId],
    read_limit: usize,
    deadline: Instant,
) -> Result<TouchstoneWindow, ContextError> {
    let mut output = TouchstoneWindow {
        views: BTreeMap::new(),
        incoming: Vec::new(),
        coverage: TouchstoneRetrieval::default(),
    };
    if frozen.is_empty() || read_limit == 0 {
        return Ok(output);
    }
    let original = frozen.iter().copied().collect::<BTreeSet<_>>();
    let direct = semantic.iter().copied().collect::<BTreeSet<_>>();
    output.coverage.searched = true;
    output.coverage.read_limit = read_limit.min(2 * MAX_LINKED_CAPACITY) as u32;
    output.coverage.anchors_total = original.len() as u32;
    if !admit_touchstone_read(&mut output.coverage, deadline) {
        return Ok(output);
    }
    output.coverage.catalog_reads += 1;
    let catalog = match touchstone_read(deadline, reader.catalog()).await {
        Ok(page) => page,
        Err(reason) => {
            output.coverage.stop_reason = Some(reason);
            output.coverage.further_tail_unknown = true;
            return Ok(output);
        }
    };
    if catalog.items.len() > 1 {
        return Err(Error::Backend("touchstone catalog exceeded one-row request".into()).into());
    }
    // One indexed empty catalog observation avoids N empty owner probes on
    // ordinary graphs. It is not a corpus snapshot lasting beyond this read.
    if catalog.items.is_empty() && catalog.next.is_none() {
        return Ok(output);
    }
    enum Work {
        Owner(NodeId),
        Referrers(NodeId, Option<TouchstoneCursor>),
    }
    let mut work = VecDeque::new();
    let mut queued = BTreeSet::new();
    for &id in frozen {
        if !queued.insert(id) {
            continue;
        }
        if direct.contains(&id) {
            work.push_back(Work::Owner(id));
        }
        work.push_back(Work::Referrers(id, None));
    }
    let mut records = BTreeMap::<NodeId, Option<TouchstoneRecord>>::new();
    let mut health = BTreeMap::new();
    let mut examined = BTreeSet::new();
    let mut incoming_indices = BTreeMap::<NodeId, usize>::new();
    while let Some(job) = work.pop_front() {
        if !admit_touchstone_read(&mut output.coverage, deadline) {
            break;
        }
        match job {
            Work::Owner(id) => {
                examined.insert(id);
                output.coverage.record_reads += 1;
                match touchstone_read(deadline, reader.record(id)).await {
                    Ok(Some(record)) => {
                        let view = project_touchstone(
                            reader,
                            &record,
                            &original,
                            true,
                            TouchstoneOrigin::Direct,
                            &mut output.coverage,
                            &mut health,
                            deadline,
                        )
                        .await?;
                        output.views.insert(id, view);
                        records.insert(id, Some(record));
                    }
                    Ok(None) => {
                        records.insert(id, None);
                    }
                    Err(reason) => {
                        output.coverage.stop_reason.get_or_insert(reason);
                        output.coverage.further_tail_unknown = true;
                        break;
                    }
                }
            }
            Work::Referrers(id, after) => {
                examined.insert(id);
                output.coverage.referrer_page_reads += 1;
                let page =
                    match touchstone_read(deadline, reader.referrers(id, after.clone())).await {
                        Ok(page) => page,
                        Err(reason) => {
                            output.coverage.stop_reason.get_or_insert(reason);
                            output.coverage.further_tail_unknown = true;
                            break;
                        }
                    };
                if page.items.len() > 1
                    || page
                        .next
                        .as_ref()
                        .is_some_and(|next| after.as_ref() == Some(next))
                {
                    return Err(Error::Backend(
                        "touchstone referrer page exceeded bounds or did not advance".into(),
                    )
                    .into());
                }
                for header in page.items {
                    // Archive is not broken reference health. Ordinary semantic
                    // recall still selects active owners; browse retains others.
                    if header.status != mneme_core::NodeStatus::Active {
                        continue;
                    }
                    let owner = header.id;
                    let origin = TouchstoneOrigin::Referrer { anchor_id: id };
                    if let Some(view) = output.views.get_mut(&owner) {
                        view.origins.push(origin);
                        *view = TouchstoneView::new(
                            view.subject.clone(),
                            view.references.clone(),
                            view.references_omitted,
                            view.origins.clone(),
                        )?;
                    } else {
                        let record = if let Some(record) = records.get(&owner) {
                            record.clone()
                        } else {
                            if !admit_touchstone_read(&mut output.coverage, deadline) {
                                break;
                            }
                            output.coverage.record_reads += 1;
                            let record = match touchstone_read(deadline, reader.record(owner)).await
                            {
                                Ok(record) => record,
                                Err(reason) => {
                                    output.coverage.stop_reason.get_or_insert(reason);
                                    output.coverage.further_tail_unknown = true;
                                    break;
                                }
                            };
                            records.insert(owner, record.clone());
                            record
                        };
                        let Some(record) = record else {
                            output.coverage.further_tail_unknown = true;
                            continue;
                        };
                        if record.owner() != owner || record.subject() != &header.subject {
                            return Err(Error::Backend(
                                "touchstone referrer record identity mismatch".into(),
                            )
                            .into());
                        }
                        let mut view = project_touchstone(
                            reader,
                            &record,
                            &original,
                            direct.contains(&owner),
                            origin,
                            &mut output.coverage,
                            &mut health,
                            deadline,
                        )
                        .await?;
                        if direct.contains(&owner) {
                            view.origins.push(TouchstoneOrigin::Direct);
                            view = TouchstoneView::new(
                                view.subject.clone(),
                                view.references.clone(),
                                view.references_omitted,
                                view.origins,
                            )?;
                        }
                        output.views.insert(owner, view);
                    }
                    if !direct.contains(&owner) {
                        if let Some(&index) = incoming_indices.get(&owner) {
                            output.incoming[index] = PrimaryInputCard::new(
                                owner,
                                header.created,
                                NonZeroU16::new(1).unwrap(),
                                header.summary.as_str(),
                            )?
                            .with_touchstone(output.views[&owner].clone());
                        } else {
                            incoming_indices.insert(owner, output.incoming.len());
                            output.incoming.push(
                                PrimaryInputCard::new(
                                    owner,
                                    header.created,
                                    NonZeroU16::new(1).unwrap(),
                                    header.summary.as_str(),
                                )?
                                .with_touchstone(output.views[&owner].clone()),
                            );
                            output.coverage.owners_discovered += 1;
                        }
                    }
                }
                if let Some(next) = page.next {
                    work.push_back(Work::Referrers(id, Some(next)));
                }
            }
        }
        if output.coverage.stop_reason.is_some() {
            break;
        }
    }
    output.coverage.anchors_examined = examined.len() as u32;
    output.coverage.further_tail_unknown |= !work.is_empty();
    output.coverage.validate()?;
    Ok(output)
}

#[cfg(test)]
#[path = "../context_touchstone_tests.rs"]
mod tests;
