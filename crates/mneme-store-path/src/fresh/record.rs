use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use super::{FreshFileIdentityV1, FreshStoreStageSpecV1, invalid_input};
use crate::{StoreLease, UnixIdentity};

const INTENT_DOMAIN: &[u8] = b"mneme.fresh-store-stage.intent.v1\0";
const PREPARED_DOMAIN: &[u8] = b"mneme.fresh-store-stage.prepared.v1\0";
const MAX_BOUND_TARGET_BYTES: usize = 4096;
pub(super) const MAX_RECORD_BYTES: usize = 16 * 1024;

pub(super) fn require_target_binding_len(target: &Path) -> io::Result<()> {
    if target.as_os_str().as_bytes().len() > MAX_BOUND_TARGET_BYTES {
        return Err(invalid_input(
            "fresh-store target path exceeds its fixed record bound",
        ));
    }
    Ok(())
}

pub(super) fn encode_intent(
    lease: &StoreLease,
    target: &Path,
    stage: UnixIdentity,
    spec: FreshStoreStageSpecV1,
) -> io::Result<Vec<u8>> {
    require_target_binding_len(target)?;
    let target = target.as_os_str().as_bytes();
    let mut bytes = Vec::with_capacity(INTENT_DOMAIN.len() + 192 + target.len());
    bytes.extend_from_slice(INTENT_DOMAIN);
    bytes.extend_from_slice(&spec.operation_id.to_bytes());
    bytes.extend_from_slice(&spec.policy_binding);
    encode_unix_identity(&mut bytes, lease.parent_identity);
    encode_unix_identity(&mut bytes, lease.lock_identity);
    encode_unix_identity(&mut bytes, stage);
    bytes.extend_from_slice(
        &u32::try_from(target.len())
            .map_err(|_| invalid_input("fresh-store target path is too long"))?
            .to_be_bytes(),
    );
    bytes.extend_from_slice(target);
    Ok(bytes)
}

pub(super) fn encode_prepared(intent: &[u8], main: FreshFileIdentityV1) -> io::Result<Vec<u8>> {
    if intent.len() > MAX_RECORD_BYTES {
        return Err(invalid_input("fresh-store intent record is too large"));
    }
    let mut bytes = Vec::with_capacity(PREPARED_DOMAIN.len() + 4 + intent.len() + 48);
    bytes.extend_from_slice(PREPARED_DOMAIN);
    bytes.extend_from_slice(
        &u32::try_from(intent.len())
            .map_err(|_| invalid_input("fresh-store intent record is too large"))?
            .to_be_bytes(),
    );
    bytes.extend_from_slice(intent);
    for value in [
        main.device,
        main.inode,
        u64::from(main.owner),
        u64::from(main.mode),
        main.links,
        main.len,
    ] {
        bytes.extend_from_slice(&value.to_be_bytes());
    }
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(invalid_input("fresh-store prepared record is too large"));
    }
    Ok(bytes)
}

fn encode_unix_identity(bytes: &mut Vec<u8>, identity: UnixIdentity) {
    for value in [
        identity.device,
        identity.inode,
        u64::from(identity.owner),
        u64::from(identity.mode),
        identity.links,
    ] {
        bytes.extend_from_slice(&value.to_be_bytes());
    }
}
