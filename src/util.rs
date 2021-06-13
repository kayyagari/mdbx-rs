use crate::{error::mdbx_result, transaction::TransactionKind, Error};
use lifetimed_bytes::Bytes;
use std::slice;

pub unsafe fn freeze_bytes<'b, K: TransactionKind>(
    txn: *const ffi::MDBX_txn,
    data_val: &ffi::MDBX_val,
) -> Result<Bytes<'b>, Error> {
    let is_dirty = (!K::ONLY_CLEAN) && mdbx_result(ffi::mdbx_is_dirty(txn, data_val.iov_base))?;

    let s = slice::from_raw_parts(data_val.iov_base as *const u8, data_val.iov_len);

    Ok(if is_dirty {
        Bytes::from(s.to_vec())
    } else {
        Bytes::from(s)
    })
}
