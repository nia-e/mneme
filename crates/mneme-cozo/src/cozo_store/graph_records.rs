use super::*;

pub(super) fn encode_storage_timestamp(value: Timestamp, field: &str) -> Result<i64> {
    i64::try_from(value)
        .map_err(|_| Error::InvalidInput(format!("{field} exceeds the storage integer range")))
}

fn decode_storage_i64(value: &DataValue, field: &str) -> Result<i64> {
    match value {
        DataValue::Num(Num::Int(value)) => Ok(*value),
        _ => Err(backend_str(format!("stored {field} is not an integer"))),
    }
}

pub(super) fn decode_storage_u32(value: &DataValue, field: &str) -> Result<u32> {
    u32::try_from(decode_storage_i64(value, field)?)
        .map_err(|_| backend_str(format!("stored {field} is outside the u32 range")))
}

pub(super) fn decode_storage_timestamp(value: &DataValue, field: &str) -> Result<Timestamp> {
    Timestamp::try_from(decode_storage_i64(value, field)?)
        .map_err(|_| backend_str(format!("stored {field} is negative")))
}

fn decode_canonical_weight(value: &DataValue, field: &str) -> Result<f32> {
    let stored = want_f64(value)?;
    let decoded = stored as f32;
    if !stored.is_finite()
        || !decoded.is_finite()
        || f64::from(decoded) != stored
        || !(0.0..=1.0).contains(&decoded)
        || decoded == 0.0 && decoded.is_sign_negative()
    {
        return Err(backend_str(format!(
            "stored {field} is not a canonical exact f32 weight in [0, 1]"
        )));
    }
    Ok(decoded)
}

pub(super) fn decode_remote_edge_weight(value: &DataValue) -> Result<f32> {
    decode_canonical_weight(value, "remote edge weight")
}

pub(super) fn decode_canonical_ulid(value: &DataValue, field: &str) -> Result<Ulid> {
    let raw = want_str(value)?;
    let parsed = Ulid::from_string(raw)
        .map_err(|error| backend_str(format!("stored {field} is not a ULID: {error}")))?;
    if parsed.to_string() != raw {
        return Err(backend_str(format!(
            "stored {field} is not canonical uppercase ULID text"
        )));
    }
    Ok(parsed)
}

pub(super) fn decode_canonical_node_id(value: &DataValue, field: &str) -> Result<NodeId> {
    decode_canonical_ulid(value, field).map(NodeId)
}

pub(super) fn decode_remote_edge(
    source_db: Ulid,
    from: NodeId,
    target_db: &DataValue,
    target: &DataValue,
    weight: &DataValue,
) -> Result<RemoteEdge> {
    let edge = RemoteEdge::new(
        from,
        decode_canonical_ulid(target_db, "remote edge target database")?,
        decode_canonical_node_id(target, "remote edge target node")?,
        decode_remote_edge_weight(weight)?,
    );
    edge.validate_for_source_database(source_db)
        .map_err(|error| backend_str(format!("invalid stored remote edge: {error}")))?;
    Ok(edge)
}

pub(super) fn decode_body_span(start: &DataValue, end: &DataValue) -> Result<BodySpan> {
    let span = BodySpan::new(
        decode_storage_u32(start, "edge anchor start")?,
        decode_storage_u32(end, "edge anchor end")?,
    );
    if span.start > span.end {
        return Err(backend_str("stored edge anchor start exceeds end".into()));
    }
    Ok(span)
}

/// Build an [`Edge`] from a row of `[weight, kind, last_reinforced, trials,
/// interference]` (identity comes from the `from`/`to` key).
pub(super) fn row_to_edge(from: NodeId, to: NodeId, row: &[DataValue]) -> Result<Edge> {
    if row.len() != 5 {
        return Err(backend_str(format!(
            "stored edge row has {} values, expected 5",
            row.len()
        )));
    }
    let edge = Edge::from_stored(
        from,
        to,
        edge_kind_from(want_str(&row[1])?)?,
        None,
        decode_canonical_weight(&row[0], "edge weight")?,
        decode_storage_timestamp(&row[2], "edge last_reinforced")?,
        decode_storage_u32(&row[3], "edge trials")?,
        decode_storage_u32(&row[4], "edge interference")?,
    );
    edge.validate()
        .map_err(|error| backend_str(format!("invalid stored edge: {error}")))?;
    Ok(edge)
}
