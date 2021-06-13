use crate::{
    database::Database,
    error::{mdbx_result, Error, Result},
    flags::EnvironmentFlags,
    transaction::{RO, RW},
    Mode, Transaction, TransactionKind,
};
use byteorder::{ByteOrder, NativeEndian};
use libc::c_uint;
use mem::size_of;
#[cfg(windows)]
use std::ffi::OsStr;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
use std::{
    ffi::CString,
    fmt,
    fmt::Debug,
    marker::PhantomData,
    mem,
    ops::{Bound, RangeBounds},
    path::Path,
    ptr, result,
    sync::mpsc::{sync_channel, SyncSender},
    thread::sleep,
    time::Duration,
};

#[cfg(windows)]
/// Adding a 'missing' trait from windows OsStrExt
trait OsStrExtLmdb {
    fn as_bytes(&self) -> &[u8];
}
#[cfg(windows)]
impl OsStrExtLmdb for OsStr {
    fn as_bytes(&self) -> &[u8] {
        &self.to_str().unwrap().as_bytes()
    }
}

mod private {
    use super::*;

    pub trait Sealed {}

    impl<'env> Sealed for NoWriteMap {}
    impl<'env> Sealed for WriteMap {}
}

pub trait EnvironmentKind: private::Sealed + Debug + 'static {
    const EXTRA_FLAGS: ffi::MDBX_env_flags_t;
}

#[derive(Debug)]
pub struct NoWriteMap;
#[derive(Debug)]
pub struct WriteMap;

impl EnvironmentKind for NoWriteMap {
    const EXTRA_FLAGS: ffi::MDBX_env_flags_t = ffi::MDBX_ENV_DEFAULTS;
}
impl EnvironmentKind for WriteMap {
    const EXTRA_FLAGS: ffi::MDBX_env_flags_t = ffi::MDBX_WRITEMAP;
}

pub type Environment = GenericEnvironment<NoWriteMap>;

#[derive(Copy, Clone, Debug)]
pub(crate) struct TxnPtr(pub *mut ffi::MDBX_txn);
unsafe impl Send for TxnPtr {}
unsafe impl Sync for TxnPtr {}

#[derive(Copy, Clone, Debug)]
pub(crate) struct EnvPtr(pub *mut ffi::MDBX_env);
unsafe impl Send for EnvPtr {}
unsafe impl Sync for EnvPtr {}

pub(crate) enum TxnManagerMessage {
    Begin {
        parent: TxnPtr,
        flags: ffi::MDBX_txn_flags_t,
        sender: SyncSender<Result<TxnPtr>>,
    },
    Abort {
        tx: TxnPtr,
        sender: SyncSender<Result<bool>>,
    },
    Commit {
        tx: TxnPtr,
        sender: SyncSender<Result<bool>>,
    },
}

/// An environment supports multiple databases, all residing in the same shared-memory map.
pub struct GenericEnvironment<E>
where
    E: EnvironmentKind,
{
    env: *mut ffi::MDBX_env,
    pub(crate) txn_manager: Option<SyncSender<TxnManagerMessage>>,
    _marker: PhantomData<E>,
}

impl<E> GenericEnvironment<E>
where
    E: EnvironmentKind,
{
    /// Creates a new builder for specifying options for opening an MDBX environment.
    #[allow(clippy::new_ret_no_self)]
    pub fn new() -> EnvironmentBuilder<E> {
        EnvironmentBuilder {
            flags: EnvironmentFlags::default(),
            max_readers: None,
            max_dbs: None,
            geometry: None,
            _marker: PhantomData,
        }
    }

    /// Returns a raw pointer to the underlying MDBX environment.
    ///
    /// The caller **must** ensure that the pointer is not dereferenced after the lifetime of the
    /// environment.
    pub fn env(&self) -> *mut ffi::MDBX_env {
        self.env
    }

    /// Create a read-only transaction for use with the environment.
    pub fn begin_ro_txn(&self) -> Result<Transaction<'_, RO, E>> {
        Transaction::new(self)
    }

    /// Create a read-write transaction for use with the environment. This method will block while
    /// there are any other read-write transactions open on the environment.
    pub fn begin_rw_txn(&self) -> Result<Transaction<'_, RW, E>> {
        let sender = self.txn_manager.as_ref().ok_or(Error::Access)?;
        let txn = loop {
            let (tx, rx) = sync_channel(0);
            sender
                .send(TxnManagerMessage::Begin {
                    parent: TxnPtr(ptr::null_mut()),
                    flags: RW::OPEN_FLAGS,
                    sender: tx,
                })
                .unwrap();
            let res = rx.recv().unwrap();
            if let Err(Error::Busy) = &res {
                sleep(Duration::from_millis(250));
                continue;
            }

            break res;
        }?;
        Ok(Transaction::new_from_ptr(self, txn.0))
    }

    /// Flush the environment data buffers to disk.
    pub fn sync(&self, force: bool) -> Result<bool> {
        mdbx_result(unsafe { ffi::mdbx_env_sync_ex(self.env(), force, false) })
    }

    /// Retrieves statistics about this environment.
    pub fn stat(&self) -> Result<Stat> {
        unsafe {
            let mut stat = Stat::new();
            mdbx_result(ffi::mdbx_env_stat_ex(
                self.env(),
                ptr::null(),
                stat.mdb_stat(),
                size_of::<Stat>(),
            ))?;
            Ok(stat)
        }
    }

    /// Retrieves info about this environment.
    pub fn info(&self) -> Result<Info> {
        unsafe {
            let mut info = Info(mem::zeroed());
            mdbx_result(ffi::mdbx_env_info_ex(
                self.env(),
                ptr::null(),
                &mut info.0,
                size_of::<Info>(),
            ))?;
            Ok(info)
        }
    }

    /// Retrieves the total number of pages on the freelist.
    ///
    /// Along with [GenericEnvironment::info()], this can be used to calculate the exact number
    /// of used pages as well as free pages in this environment.
    ///
    /// ```
    /// # use mdbx::Environment;
    /// let dir = tempfile::tempdir().unwrap();
    /// let env = Environment::new().open(dir.path()).unwrap();
    /// let info = env.info().unwrap();
    /// let stat = env.stat().unwrap();
    /// let freelist = env.freelist().unwrap();
    /// let last_pgno = info.last_pgno() + 1; // pgno is 0 based.
    /// let total_pgs = info.map_size() / stat.page_size() as usize;
    /// let pgs_in_use = last_pgno - freelist;
    /// let pgs_free = total_pgs - pgs_in_use;
    /// ```
    ///
    /// Note:
    ///
    /// * LMDB stores all the freelists in the designated database 0 in each environment,
    ///   and the freelist count is stored at the beginning of the value as `libc::size_t`
    ///   in the native byte order.
    ///
    /// * It will create a read transaction to traverse the freelist database.
    pub fn freelist(&self) -> Result<usize> {
        let mut freelist: usize = 0;
        let txn = self.begin_ro_txn()?;
        let db = Database::freelist_db();
        let mut cursor = txn.cursor(&db)?;

        for result in cursor.iter() {
            let (_key, value) = result?;
            if value.len() < mem::size_of::<usize>() {
                return Err(Error::Corrupted);
            }

            let s = &value[..mem::size_of::<usize>()];
            if cfg!(target_pointer_width = "64") {
                freelist += NativeEndian::read_u64(s) as usize;
            } else {
                freelist += NativeEndian::read_u32(s) as usize;
            }
        }

        Ok(freelist)
    }
}

/// Environment statistics.
///
/// Contains information about the size and layout of an MDBX environment or database.
#[repr(transparent)]
pub struct Stat(ffi::MDBX_stat);

impl Stat {
    /// Create a new Stat with zero'd inner struct `ffi::MDB_stat`.
    pub(crate) fn new() -> Stat {
        unsafe { Stat(mem::zeroed()) }
    }

    /// Returns a mut pointer to `ffi::MDB_stat`.
    pub(crate) fn mdb_stat(&mut self) -> *mut ffi::MDBX_stat {
        &mut self.0
    }
}

impl Stat {
    /// Size of a database page. This is the same for all databases in the environment.
    #[inline]
    pub fn page_size(&self) -> u32 {
        self.0.ms_psize
    }

    /// Depth (height) of the B-tree.
    #[inline]
    pub fn depth(&self) -> u32 {
        self.0.ms_depth
    }

    /// Number of internal (non-leaf) pages.
    #[inline]
    pub fn branch_pages(&self) -> usize {
        self.0.ms_branch_pages as usize
    }

    /// Number of leaf pages.
    #[inline]
    pub fn leaf_pages(&self) -> usize {
        self.0.ms_leaf_pages as usize
    }

    /// Number of overflow pages.
    #[inline]
    pub fn overflow_pages(&self) -> usize {
        self.0.ms_overflow_pages as usize
    }

    /// Number of data items.
    #[inline]
    pub fn entries(&self) -> usize {
        self.0.ms_entries as usize
    }
}

#[repr(transparent)]
pub struct GeometryInfo(ffi::MDBX_envinfo__bindgen_ty_1);

impl GeometryInfo {
    pub fn min(&self) -> u64 {
        self.0.lower
    }
}

/// Environment information.
///
/// Contains environment information about the map size, readers, last txn id etc.
#[repr(transparent)]
pub struct Info(ffi::MDBX_envinfo);

impl Info {
    pub fn geometry(&self) -> GeometryInfo {
        GeometryInfo(self.0.mi_geo)
    }

    /// Size of memory map.
    #[inline]
    pub fn map_size(&self) -> usize {
        self.0.mi_mapsize as usize
    }

    /// Last used page number
    #[inline]
    pub fn last_pgno(&self) -> usize {
        self.0.mi_last_pgno as usize
    }

    /// Last transaction ID
    #[inline]
    pub fn last_txnid(&self) -> usize {
        self.0.mi_recent_txnid as usize
    }

    /// Max reader slots in the environment
    #[inline]
    pub fn max_readers(&self) -> usize {
        self.0.mi_maxreaders as usize
    }

    /// Max reader slots used in the environment
    #[inline]
    pub fn num_readers(&self) -> usize {
        self.0.mi_numreaders as usize
    }
}

unsafe impl<E> Send for GenericEnvironment<E> where E: EnvironmentKind {}
unsafe impl<E> Sync for GenericEnvironment<E> where E: EnvironmentKind {}

impl<E> fmt::Debug for GenericEnvironment<E>
where
    E: EnvironmentKind,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> result::Result<(), fmt::Error> {
        f.debug_struct("Environment").finish()
    }
}

impl<E> Drop for GenericEnvironment<E>
where
    E: EnvironmentKind,
{
    fn drop(&mut self) {
        unsafe {
            ffi::mdbx_env_close_ex(self.env, false);
        }
    }
}

///////////////////////////////////////////////////////////////////////////////////////////////////
//// Environment Builder
///////////////////////////////////////////////////////////////////////////////////////////////////

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PageSize {
    MinimalAcceptable,
    Set(usize),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Geometry<R> {
    pub size: Option<R>,
    pub growth_step: Option<isize>,
    pub shrink_threshold: Option<isize>,
    pub page_size: Option<PageSize>,
}

impl<R> Default for Geometry<R> {
    fn default() -> Self {
        Self {
            size: None,
            growth_step: None,
            shrink_threshold: None,
            page_size: None,
        }
    }
}

/// Options for opening or creating an environment.
#[derive(Debug, Clone)]
pub struct EnvironmentBuilder<E>
where
    E: EnvironmentKind,
{
    flags: EnvironmentFlags,
    max_readers: Option<c_uint>,
    max_dbs: Option<usize>,
    geometry: Option<Geometry<(Option<usize>, Option<usize>)>>,
    _marker: PhantomData<E>,
}

impl<E> EnvironmentBuilder<E>
where
    E: EnvironmentKind,
{
    /// Open an environment.
    ///
    /// On UNIX, the database files will be opened with 644 permissions.
    ///
    /// The path may not contain the null character, Windows UNC (Uniform Naming Convention)
    /// paths are not supported either.
    pub fn open(&self, path: &Path) -> Result<GenericEnvironment<E>> {
        let mut env = self.open_with_permissions(path, 0o644)?;

        if let Mode::ReadWrite { .. } = self.flags.mode {
            let (tx, rx) = std::sync::mpsc::sync_channel(0);
            let e = EnvPtr(env.env);
            std::thread::spawn(move || loop {
                match rx.recv() {
                    Ok(msg) => match msg {
                        TxnManagerMessage::Begin {
                            parent,
                            flags,
                            sender,
                        } => {
                            let mut txn: *mut ffi::MDBX_txn = ptr::null_mut();
                            sender
                                .send(
                                    mdbx_result(unsafe {
                                        ffi::mdbx_txn_begin_ex(
                                            e.0,
                                            parent.0,
                                            flags,
                                            &mut txn,
                                            ptr::null_mut(),
                                        )
                                    })
                                    .map(|_| TxnPtr(txn)),
                                )
                                .unwrap()
                        }
                        TxnManagerMessage::Abort { tx, sender } => {
                            sender
                                .send(mdbx_result(unsafe { ffi::mdbx_txn_abort(tx.0) }))
                                .unwrap();
                        }
                        TxnManagerMessage::Commit { tx, sender } => {
                            sender
                                .send(mdbx_result(unsafe {
                                    ffi::mdbx_txn_commit_ex(tx.0, ptr::null_mut())
                                }))
                                .unwrap();
                        }
                    },
                    Err(_) => return,
                }
            });

            env.txn_manager = Some(tx);
        }

        Ok(env)
    }

    /// Open an environment with the provided UNIX permissions.
    ///
    /// On Windows, the permissions will be ignored.
    ///
    /// The path may not contain the null character, Windows UNC (Uniform Naming Convention)
    /// paths are not supported either.
    pub fn open_with_permissions(
        &self,
        path: &Path,
        mode: ffi::mdbx_mode_t,
    ) -> Result<GenericEnvironment<E>> {
        let mut env: *mut ffi::MDBX_env = ptr::null_mut();
        unsafe {
            mdbx_result(ffi::mdbx_env_create(&mut env))?;
            if let Err(e) = (|| {
                if let Some(geometry) = &self.geometry {
                    let mut min_size = -1;
                    let mut max_size = -1;

                    if let Some(size) = geometry.size {
                        if let Some(size) = size.0 {
                            min_size = size as isize;
                        }

                        if let Some(size) = size.1 {
                            max_size = size as isize;
                        }
                    }

                    mdbx_result(ffi::mdbx_env_set_geometry(
                        env,
                        min_size,
                        -1,
                        max_size,
                        geometry.growth_step.unwrap_or(-1),
                        geometry.shrink_threshold.unwrap_or(-1),
                        match geometry.page_size {
                            None => -1,
                            Some(PageSize::MinimalAcceptable) => 0,
                            Some(PageSize::Set(size)) => size as isize,
                        },
                    ))?;
                }
                if let Some(max_dbs) = self.max_dbs {
                    mdbx_result(ffi::mdbx_env_set_option(
                        env,
                        ffi::MDBX_opt_max_db,
                        max_dbs as u64,
                    ))?;
                }
                let path = match CString::new(path.as_os_str().as_bytes()) {
                    Ok(path) => path,
                    Err(..) => return Err(crate::Error::Invalid),
                };
                mdbx_result(ffi::mdbx_env_open(
                    env,
                    path.as_ptr(),
                    self.flags.make_flags() | E::EXTRA_FLAGS,
                    mode,
                ))?;

                Ok(())
            })() {
                ffi::mdbx_env_close_ex(env, false);

                return Err(e);
            }
        }
        Ok(GenericEnvironment {
            env,
            txn_manager: None,
            _marker: PhantomData,
        })
    }

    /// Sets the provided options in the environment.
    pub fn set_flags(&mut self, flags: EnvironmentFlags) -> &mut Self {
        self.flags = flags;
        self
    }

    /// Sets the maximum number of threads or reader slots for the environment.
    ///
    /// This defines the number of slots in the lock table that is used to track readers in the
    /// the environment. The default is 126. Starting a read-only transaction normally ties a lock
    /// table slot to the [Transaction] object until it or the [GenericEnvironment] object is destroyed.
    pub fn set_max_readers(&mut self, max_readers: c_uint) -> &mut Self {
        self.max_readers = Some(max_readers);
        self
    }

    /// Sets the maximum number of named databases for the environment.
    ///
    /// This function is only needed if multiple databases will be used in the
    /// environment. Simpler applications that use the environment as a single
    /// unnamed database can ignore this option.
    ///
    /// Currently a moderate number of slots are cheap but a huge number gets
    /// expensive: 7-120 words per transaction, and every [Transaction::open_db()]
    /// does a linear search of the opened slots.
    pub fn set_max_dbs(&mut self, max_dbs: usize) -> &mut Self {
        self.max_dbs = Some(max_dbs);
        self
    }

    /// Set all size-related parameters of environment, including page size and the min/max size of the memory map.
    pub fn set_geometry<R: RangeBounds<usize>>(&mut self, geometry: Geometry<R>) -> &mut Self {
        let convert_bound = |bound: Bound<&usize>| match bound {
            Bound::Included(v) | Bound::Excluded(v) => Some(*v),
            _ => None,
        };
        self.geometry = Some(Geometry {
            size: geometry.size.map(|range| {
                (
                    convert_bound(range.start_bound()),
                    convert_bound(range.end_bound()),
                )
            }),
            growth_step: geometry.growth_step,
            shrink_threshold: geometry.shrink_threshold,
            page_size: geometry.page_size,
        });
        self
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::flags::*;
    use byteorder::{ByteOrder, LittleEndian};
    use tempfile::tempdir;

    #[test]
    fn test_open() {
        let dir = tempdir().unwrap();

        // opening non-existent env with read-only should fail
        assert!(Environment::new()
            .set_flags(Mode::ReadOnly.into())
            .open(dir.path())
            .is_err());

        // opening non-existent env should succeed
        assert!(Environment::new().open(dir.path()).is_ok());

        // opening env with read-only should succeed
        assert!(Environment::new()
            .set_flags(Mode::ReadOnly.into())
            .open(dir.path())
            .is_ok());
    }

    #[test]
    fn test_begin_txn() {
        let dir = tempdir().unwrap();

        {
            // writable environment
            let env = Environment::new().open(dir.path()).unwrap();

            assert!(env.begin_rw_txn().is_ok());
            assert!(env.begin_ro_txn().is_ok());
        }

        {
            // read-only environment
            let env = Environment::new()
                .set_flags(Mode::ReadOnly.into())
                .open(dir.path())
                .unwrap();

            assert!(env.begin_rw_txn().is_err());
            assert!(env.begin_ro_txn().is_ok());
        }
    }

    #[test]
    fn test_open_db() {
        let dir = tempdir().unwrap();
        let env = Environment::new().set_max_dbs(1).open(dir.path()).unwrap();

        let txn = env.begin_ro_txn().unwrap();
        assert!(txn.open_db(None).is_ok());
        assert!(txn.open_db(Some("testdb")).is_err());
    }

    #[test]
    fn test_create_db() {
        let dir = tempdir().unwrap();
        let env = Environment::new().set_max_dbs(11).open(dir.path()).unwrap();

        let txn = env.begin_rw_txn().unwrap();
        assert!(txn.open_db(Some("testdb")).is_err());
        assert!(txn
            .create_db(Some("testdb"), DatabaseFlags::empty())
            .is_ok());
        assert!(txn.open_db(Some("testdb")).is_ok())
    }

    #[test]
    fn test_close_database() {
        let dir = tempdir().unwrap();
        let env = Environment::new().set_max_dbs(10).open(dir.path()).unwrap();

        let txn = env.begin_rw_txn().unwrap();
        txn.create_db(Some("db"), DatabaseFlags::empty()).unwrap();
        txn.open_db(Some("db")).unwrap();
    }

    #[test]
    fn test_sync() {
        let dir = tempdir().unwrap();
        {
            let env = Environment::new().open(dir.path()).unwrap();
            env.sync(true).unwrap();
        }
        {
            let env = Environment::new()
                .set_flags(Mode::ReadOnly.into())
                .open(dir.path())
                .unwrap();
            env.sync(true).unwrap_err();
        }
    }

    #[test]
    fn test_stat() {
        let dir = tempdir().unwrap();
        let env = Environment::new().open(dir.path()).unwrap();

        // Stats should be empty initially.
        let stat = env.stat().unwrap();
        assert_eq!(stat.page_size(), 4096);
        assert_eq!(stat.depth(), 0);
        assert_eq!(stat.branch_pages(), 0);
        assert_eq!(stat.leaf_pages(), 0);
        assert_eq!(stat.overflow_pages(), 0);
        assert_eq!(stat.entries(), 0);

        // Write a few small values.
        for i in 0..64 {
            let mut value = [0u8; 8];
            LittleEndian::write_u64(&mut value, i);
            let tx = env.begin_rw_txn().expect("begin_rw_txn");
            tx.put(
                &tx.open_db(None).unwrap(),
                &value,
                &value,
                WriteFlags::default(),
            )
            .expect("tx.put");
            tx.commit().expect("tx.commit");
        }

        // Stats should now reflect inserted values.
        let stat = env.stat().unwrap();
        assert_eq!(stat.page_size(), 4096);
        assert_eq!(stat.depth(), 1);
        assert_eq!(stat.branch_pages(), 0);
        assert_eq!(stat.leaf_pages(), 1);
        assert_eq!(stat.overflow_pages(), 0);
        assert_eq!(stat.entries(), 64);
    }

    #[test]
    fn test_info() {
        let map_size = 1024 * 1024;
        let dir = tempdir().unwrap();
        let env = Environment::new()
            .set_geometry(Geometry {
                size: Some(map_size..),
                ..Default::default()
            })
            .open(dir.path())
            .unwrap();

        let info = env.info().unwrap();
        assert_eq!(info.geometry().min(), map_size as u64);
        // assert_eq!(info.last_pgno(), 1);
        // assert_eq!(info.last_txnid(), 0);
        assert_eq!(info.num_readers(), 0);
    }

    #[test]
    fn test_freelist() {
        let dir = tempdir().unwrap();
        let env = Environment::new().open(dir.path()).unwrap();

        let mut freelist = env.freelist().unwrap();
        assert_eq!(freelist, 0);

        // Write a few small values.
        for i in 0..64 {
            let mut value = [0u8; 8];
            LittleEndian::write_u64(&mut value, i);
            let tx = env.begin_rw_txn().expect("begin_rw_txn");
            tx.put(
                &tx.open_db(None).unwrap(),
                &value,
                &value,
                WriteFlags::default(),
            )
            .expect("tx.put");
            tx.commit().expect("tx.commit");
        }
        let tx = env.begin_rw_txn().expect("begin_rw_txn");
        tx.clear_db(&tx.open_db(None).unwrap()).expect("clear");
        tx.commit().expect("tx.commit");

        // Freelist should not be empty after clear_db.
        freelist = env.freelist().unwrap();
        assert!(freelist > 0);
    }
}
