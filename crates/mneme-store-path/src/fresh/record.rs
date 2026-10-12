use std::io;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use super::{FreshFileIdentityV1, FreshStoreStageSpecV1, StageIdentity, invalid_input};
use crate::StoreLease;
#[cfg(unix)]
use crate::UnixIdentity;

#[cfg(unix)]
const INTENT_DOMAIN: &[u8] = b"mneme.fresh-store-stage.intent.v1\0";
#[cfg(unix)]
const PREPARED_DOMAIN: &[u8] = b"mneme.fresh-store-stage.prepared.v1\0";
#[cfg(windows)]
const INTENT_DOMAIN: &[u8] = b"mneme.fresh-store-stage.intent.windows.v1\0";
#[cfg(windows)]
const PREPARED_DOMAIN: &[u8] = b"mneme.fresh-store-stage.prepared.windows.v1\0";
const MAX_BOUND_TARGET_BYTES: usize = 4096;
pub(super) const MAX_RECORD_BYTES: usize = 16 * 1024;

pub(super) fn require_target_binding_len(target: &Path) -> io::Result<()> {
    if target_binding_len(target) > MAX_BOUND_TARGET_BYTES {
        return Err(invalid_input(
            "fresh-store target path exceeds its fixed record bound",
        ));
    }
    Ok(())
}

pub(super) fn encode_intent(
    lease: &StoreLease,
    target: &Path,
    stage: StageIdentity,
    spec: FreshStoreStageSpecV1,
) -> io::Result<Vec<u8>> {
    require_target_binding_len(target)?;
    let target = target_binding_bytes(target);
    let mut bytes = Vec::with_capacity(INTENT_DOMAIN.len() + 192 + target.len());
    bytes.extend_from_slice(INTENT_DOMAIN);
    bytes.extend_from_slice(&spec.operation_id.to_bytes());
    bytes.extend_from_slice(&spec.policy_binding);
    encode_stage_identity(&mut bytes, lease.parent_identity);
    encode_lease_identity(&mut bytes, lease.lock_identity);
    encode_stage_identity(&mut bytes, stage);
    bytes.extend_from_slice(
        &u32::try_from(target.len())
            .map_err(|_| invalid_input("fresh-store target path is too long"))?
            .to_be_bytes(),
    );
    bytes.extend_from_slice(&target);
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
    encode_main_identity(&mut bytes, main);
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(invalid_input("fresh-store prepared record is too large"));
    }
    Ok(bytes)
}

#[cfg(unix)]
fn encode_stage_identity(bytes: &mut Vec<u8>, identity: UnixIdentity) {
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

#[cfg(unix)]
fn target_binding_len(target: &Path) -> usize {
    target.as_os_str().as_bytes().len()
}

#[cfg(windows)]
fn target_binding_len(target: &Path) -> usize {
    // Bound before allocation; encode_wide preserves even unpaired UTF-16.
    target
        .as_os_str()
        .encode_wide()
        .take(MAX_BOUND_TARGET_BYTES / 2 + 1)
        .count()
        * 2
}

#[cfg(unix)]
fn target_binding_bytes(target: &Path) -> Vec<u8> {
    target.as_os_str().as_bytes().to_vec()
}

#[cfg(windows)]
fn target_binding_bytes(target: &Path) -> Vec<u8> {
    target
        .as_os_str()
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect()
}

#[cfg(unix)]
fn encode_main_identity(bytes: &mut Vec<u8>, main: FreshFileIdentityV1) {
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
}

#[cfg(windows)]
fn encode_stage_identity(bytes: &mut Vec<u8>, identity: StageIdentity) {
    // Directory link counts and lengths vary as children are created/removed.
    // Recovery binds the volume plus full 128-bit object ID, not Unix-shaped
    // uid/mode/inode stand-ins or transient directory sizes.
    bytes.extend_from_slice(&identity.volume.to_be_bytes());
    bytes.extend_from_slice(&identity.id);
}

#[cfg(windows)]
fn encode_main_identity(bytes: &mut Vec<u8>, main: FreshFileIdentityV1) {
    encode_stage_identity(bytes, main);
    bytes.extend_from_slice(&main.links.to_be_bytes());
    bytes.extend_from_slice(&main.len.to_be_bytes());
}

#[cfg(unix)]
fn encode_lease_identity(bytes: &mut Vec<u8>, identity: StageIdentity) {
    encode_stage_identity(bytes, identity);
}

#[cfg(windows)]
fn encode_lease_identity(bytes: &mut Vec<u8>, identity: StageIdentity) {
    encode_main_identity(bytes, identity);
}

#[cfg(all(test, windows))]
#[test]
fn windows_target_binding_preserves_utf16_including_unpaired_surrogates() {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    let path = OsString::from_wide(&[0x0043, 0x003a, 0x005c, 0x00e9, 0xd834, 0xdd1e, 0xd800]);
    assert_eq!(
        target_binding_bytes(Path::new(&path)),
        [
            0x43, 0, 0x3a, 0, 0x5c, 0, 0xe9, 0, 0x34, 0xd8, 0x1e, 0xdd, 0, 0xd8
        ]
    );
}

#[cfg(all(test, unix))]
#[test]
fn unix_v1_identity_wire_encoding_remains_unchanged() {
    let identity = FreshFileIdentityV1 {
        device: 0x0102_0304_0506_0708,
        inode: 0x1112_1314_1516_1718,
        owner: 0x2122_2324,
        mode: 0x3132_3334,
        links: 0x4142_4344_4546_4748,
        len: 0x5152_5354_5556_5758,
    };
    let mut bytes = Vec::new();
    encode_main_identity(&mut bytes, identity);
    assert_eq!(
        bytes,
        [
            1, 2, 3, 4, 5, 6, 7, 8, 17, 18, 19, 20, 21, 22, 23, 24, 0, 0, 0, 0, 33, 34, 35, 36, 0,
            0, 0, 0, 49, 50, 51, 52, 65, 66, 67, 68, 69, 70, 71, 72, 81, 82, 83, 84, 85, 86, 87,
            88,
        ]
    );
    bytes.clear();
    encode_lease_identity(
        &mut bytes,
        UnixIdentity {
            device: identity.device,
            inode: identity.inode,
            owner: identity.owner,
            mode: identity.mode,
            links: identity.links,
        },
    );
    assert_eq!(bytes.len(), 40);
    assert_eq!(&bytes[32..], &[65, 66, 67, 68, 69, 70, 71, 72]);
    assert_eq!(INTENT_DOMAIN, b"mneme.fresh-store-stage.intent.v1\0");
    assert_eq!(PREPARED_DOMAIN, b"mneme.fresh-store-stage.prepared.v1\0");
}
