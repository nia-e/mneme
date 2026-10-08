//! Host-generation activation for receipt-feedback retry proofs.
//!
//! Existing current stores open read-only with respect to their semantic rows.
//! Before an admitted host publishes such a handle, it activates exactly one
//! volatile receipt epoch here. Activation is the explicit mutating boundary:
//! it atomically removes every bounded row that predates the new live receipt
//! capability. Caller-chosen epoch text must never resurrect replay authority
//! from a prior process.

use mneme_core::MAX_FEEDBACK_EPOCH_BYTES;

use super::*;

const LEDGER_CENSUS_LIMIT: usize = MAX_FEEDBACK_RETRY_RECORDS + 1;
const PROOF_DELETE_CHUNK: usize = 128;
const ORDER_DELETE_CHUNK: usize = 64;

#[derive(Clone)]
struct ProofRow {
    key: String,
}

#[derive(Clone)]
struct OrderRow {
    epoch: String,
    sequence: i64,
    key: String,
}

impl CozoStore {
    /// Bind an admitted persistent handle to one volatile feedback epoch.
    ///
    /// The first successful call atomically purges the bounded retry ledger and
    /// returns the number of logical keys removed. Repeating the same epoch is
    /// a zero-work no-op. A live handle can never rotate to another epoch;
    /// destroy it and admit a new leased handle.
    pub fn activate_feedback_epoch(&self, epoch: &str) -> Result<usize> {
        validate_feedback_epoch(epoch)?;
        let authority = self.persistent_authority.as_ref().ok_or_else(|| {
            Error::Conflict(
                "feedback epoch activation requires an admitted persistent store handle".into(),
            )
        })?;
        let mut active = authority.lock_feedback_epoch()?;
        match active.as_deref() {
            Some(current) if current == epoch => return Ok(0),
            Some(_) => {
                return Err(Error::Conflict(
                    "feedback epoch is already activated for this live store handle".into(),
                ));
            }
            None => {}
        }

        authority.require_live_guards("before feedback epoch activation")?;
        let tx = self.db.multi_transaction(true);
        let staged = stage_feedback_epoch_activation(&tx, epoch);
        let purged = match staged {
            Ok(purged) => {
                if let Err(error) = authority
                    .require_live_guards("immediately before feedback epoch activation commit")
                {
                    let _ = tx.abort();
                    return Err(error);
                }
                tx.commit().map_err(backend)?;
                purged
            }
            Err(error) => {
                let _ = tx.abort();
                return Err(error);
            }
        };
        *active = Some(epoch.to_owned());
        Ok(purged)
    }
}

fn validate_feedback_epoch(epoch: &str) -> Result<()> {
    if epoch.is_empty() || epoch.len() > MAX_FEEDBACK_EPOCH_BYTES {
        return Err(Error::InvalidInput(format!(
            "feedback epoch must contain 1..={MAX_FEEDBACK_EPOCH_BYTES} bytes"
        )));
    }
    Ok(())
}

/// Stage one bounded activation cleanup. Keeping this separate makes it
/// possible to prove that dropping the outer transaction publishes no prefix.
pub(super) fn stage_feedback_epoch_activation(tx: &MultiTransaction, epoch: &str) -> Result<usize> {
    validate_feedback_epoch(epoch)?;
    let proofs = read_proofs(tx)?;
    let orders = read_orders(tx)?;

    let mut keys = BTreeSet::new();
    keys.extend(proofs.iter().map(|proof| proof.key.clone()));
    keys.extend(orders.iter().map(|order| order.key.clone()));
    if keys.len() > MAX_FEEDBACK_RETRY_RECORDS {
        return Err(activation_capacity_error("feedback retry ledger keys"));
    }

    remove_order_rows(tx, &orders)?;
    remove_proof_rows(tx, &proofs)?;
    Ok(keys.len())
}

fn read_proofs(tx: &MultiTransaction) -> Result<Vec<ProofRow>> {
    let rows = tx_run(
        tx,
        &format!("?[key] := *feedback_retry{{key}} :limit {LEDGER_CENSUS_LIMIT}"),
        BTreeMap::new(),
    )?;
    if rows.rows.len() > MAX_FEEDBACK_RETRY_RECORDS {
        return Err(activation_capacity_error("feedback retry proof records"));
    }
    rows.rows
        .iter()
        .map(|row| {
            Ok(ProofRow {
                key: want_str(&row[0])?.to_owned(),
            })
        })
        .collect()
}

fn read_orders(tx: &MultiTransaction) -> Result<Vec<OrderRow>> {
    let rows = tx_run(
        tx,
        &format!(
            "?[epoch, sequence, key] := \
               *feedback_retry_order{{epoch, sequence, key}} \
             :limit {LEDGER_CENSUS_LIMIT}"
        ),
        BTreeMap::new(),
    )?;
    if rows.rows.len() > MAX_FEEDBACK_RETRY_RECORDS {
        return Err(activation_capacity_error("feedback retry order records"));
    }
    rows.rows
        .iter()
        .map(|row| {
            Ok(OrderRow {
                epoch: want_str(&row[0])?.to_owned(),
                sequence: want_i64(&row[1])?,
                key: want_str(&row[2])?.to_owned(),
            })
        })
        .collect()
}

fn remove_proof_rows(tx: &MultiTransaction, rows: &[ProofRow]) -> Result<()> {
    for chunk in rows.chunks(PROOF_DELETE_CHUNK) {
        let mut params = BTreeMap::new();
        let input = chunk
            .iter()
            .enumerate()
            .map(|(index, row)| {
                let name = format!("activation_proof_key_{index}");
                params.insert(name.clone(), dv_str(&row.key));
                format!("[${name}]")
            })
            .collect::<Vec<_>>()
            .join(", ");
        tx_run(
            tx,
            &format!(
                "dead[key] <- [{input}]\n\
                 ?[key] := dead[key] :rm feedback_retry {{key}}"
            ),
            params,
        )?;
    }
    Ok(())
}

fn remove_order_rows(tx: &MultiTransaction, rows: &[OrderRow]) -> Result<()> {
    for chunk in rows.chunks(ORDER_DELETE_CHUNK) {
        let mut params = BTreeMap::new();
        let input = chunk
            .iter()
            .enumerate()
            .map(|(index, row)| {
                let epoch = format!("activation_order_epoch_{index}");
                let sequence = format!("activation_order_sequence_{index}");
                let key = format!("activation_order_key_{index}");
                params.insert(epoch.clone(), dv_str(&row.epoch));
                params.insert(sequence.clone(), dv_int(row.sequence));
                params.insert(key.clone(), dv_str(&row.key));
                format!("[${epoch}, ${sequence}, ${key}]")
            })
            .collect::<Vec<_>>()
            .join(", ");
        tx_run(
            tx,
            &format!(
                "dead[epoch, sequence, key] <- [{input}]\n\
                 ?[epoch, sequence, key] := dead[epoch, sequence, key] \
                   :rm feedback_retry_order {{epoch, sequence, key}}"
            ),
            params,
        )?;
    }
    Ok(())
}

fn activation_capacity_error(resource: &'static str) -> Error {
    Error::CapacityExceeded {
        resource,
        limit: MAX_FEEDBACK_RETRY_RECORDS,
    }
}
