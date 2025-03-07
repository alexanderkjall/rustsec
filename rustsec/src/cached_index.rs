//! An efficient way to check whether a given package has been yanked
use std::{
    collections::{BTreeSet, HashMap},
    time::Duration,
};

use crate::{
    error::{Error, ErrorKind},
    package::{self, Package},
};

use tame_index::external::gix;
pub use tame_index::external::reqwest::ClientBuilder;

enum Index {
    Git(tame_index::index::RemoteGitIndex),
    SparseCached(tame_index::index::SparseIndex),
    SparseRemote(tame_index::index::AsyncRemoteSparseIndex),
}

impl Index {
    #[inline]
    fn krate(&self, name: &package::Name) -> Result<Option<tame_index::IndexKrate>, Error> {
        let name = name.as_str().try_into().map_err(Error::from_tame)?;
        let res = match self {
            Self::Git(gi) => gi.krate(name, true),
            Self::SparseCached(si) => si.cached_krate(name),
            Self::SparseRemote(rsi) => rsi.cached_krate(name),
        }
        .map_err(Error::from_tame)?;

        Ok(res)
    }
}

/// Provides an efficient way to check if the given package has been yanked.
///
/// Operations on crates.io index are rather slow.
/// Instead of peforming an index lookup for every version of every crate,
/// this implementation looks up each crate only once and caches the result in memory.
pub struct CachedIndex {
    index: Index,
    /// The inner hash map is logically HashMap<Version, IsYanked>
    /// but we don't parse semver because crates.io registry contains invalid semver:
    /// <https://github.com/rustsec/rustsec/issues/759>
    // The outer map can later be changed to DashMap or some such for thread safety.
    cache: HashMap<package::Name, Result<Option<HashMap<String, bool>>, Error>>,
}

impl CachedIndex {
    /// Open the local crates.io index
    ///
    /// If this opens a git index, it will perform a fetch to get the latest index
    /// information.
    ///
    /// If this is a sparse index, it will be downloaded later on demand.
    ///
    /// ## Locking
    ///
    /// This function will wait for up to `lock_timeout` for the filesystem lock on the repository.
    /// It will fail with [`rustsec::Error::LockTimeout`](Error) if the lock is still held
    /// after that time.
    ///
    /// If `lock_timeout` is set to `std::time::Duration::from_secs(0)`, it will not wait at all,
    /// and instead return an error immediately if it fails to aquire the lock.
    ///
    /// Regardless of the timeout, this function relies on `panic = unwind` to avoid leaving stale locks
    /// if the process is interrupted with Ctrl+C. To support `panic = abort` you also need to register
    /// the `gix` signal handler to clean up the locks, see [`gix::interrupt::init_handler`].
    pub fn fetch(client: Option<ClientBuilder>, lock_timeout: Duration) -> Result<Self, Error> {
        Self::fetch_inner(client, lock_timeout).map_err(Error::from_tame)
    }

    fn fetch_inner(
        client: Option<ClientBuilder>,
        lock_timeout: Duration,
    ) -> Result<Self, tame_index::Error> {
        let index = tame_index::index::ComboIndexCache::new(tame_index::IndexLocation::new(
            tame_index::IndexUrl::crates_io(None, None, None)?,
        ))?;

        let index = match index {
            tame_index::index::ComboIndexCache::Git(gi) => {
                let mut rgi = new_remote_git_index(gi, lock_timeout)?;
                rgi.fetch()?;
                Index::Git(rgi)
            }
            tame_index::index::ComboIndexCache::Sparse(si) => {
                let client_builder = client.unwrap_or_default();
                // note: this would need to change if rustsec ever adds the capability
                // to query other indices that _might_ not support HTTP/2, but
                // hopefully that would never need to happen
                let client = client_builder
                    .http2_prior_knowledge()
                    .build()
                    .map_err(tame_index::Error::from)?;

                Index::SparseRemote(tame_index::index::AsyncRemoteSparseIndex::new(si, client))
            }
            _ => panic!("Unsupported crates.io index type"),
        };

        Ok(CachedIndex {
            index,
            cache: Default::default(),
        })
    }

    /// Open the local crates.io index
    ///
    /// If this opens a git index, it allows reading of index entries from the repository.
    ///
    /// If this is a sparse index, it only allows reading of index entries that are already cached locally.
    ///
    /// ## Locking
    ///
    /// This function will wait for up to `lock_timeout` for the filesystem lock on the repository.
    /// It will fail with [`rustsec::Error::LockTimeout`](Error) if the lock is still held
    /// after that time.
    ///
    /// If `lock_timeout` is set to `std::time::Duration::from_secs(0)`, it will not wait at all,
    /// and instead return an error immediately if it fails to aquire the lock.
    ///
    /// Regardless of the timeout, this function relies on `panic = unwind` to avoid leaving stale locks
    /// if the process is interrupted with Ctrl+C. To support `panic = abort` you also need to register
    /// the `gix` signal handler to clean up the locks, see [`gix::interrupt::init_handler`].
    pub fn open(lock_timeout: Duration) -> Result<Self, Error> {
        Self::open_inner(lock_timeout).map_err(Error::from_tame)
    }

    fn open_inner(lock_timeout: Duration) -> Result<Self, tame_index::Error> {
        let index = tame_index::index::ComboIndexCache::new(tame_index::IndexLocation::new(
            tame_index::IndexUrl::crates_io(None, None, None)?,
        ))?;

        let index = match index {
            tame_index::index::ComboIndexCache::Git(gi) => {
                let rgi = new_remote_git_index(gi, lock_timeout)?;
                Index::Git(rgi)
            }
            tame_index::index::ComboIndexCache::Sparse(si) => Index::SparseCached(si),
            _ => panic!("Unsupported crates.io index type"),
        };

        Ok(CachedIndex {
            index,
            cache: Default::default(),
        })
    }

    /// Populates the cache entries for all of the specified crates.
    fn populate_cache(&mut self, packages: BTreeSet<&package::Name>) -> Result<(), Error> {
        match &self.index {
            Index::Git(_) | Index::SparseCached(_) => {
                for pkg in packages {
                    self.insert(pkg.to_owned(), self.index.krate(pkg));
                }
            }
            Index::SparseRemote(rsi) => {
                // Ensure we have a runtime
                let rt = tame_index::external::tokio::runtime::Runtime::new().map_err(|err| {
                    format_err!(
                        ErrorKind::Registry,
                        "unable to start a tokio runtime: {}",
                        err
                    )
                })?;
                let _rt = rt.enter();

                /// This is the timeout per individual crate. If a crate fails to be
                /// requested for a retriable reason then it will be retried until
                /// this time limit is reached
                const REQUEST_TIMEOUT: Option<Duration> = Some(Duration::from_secs(10));

                let results = rsi
                    .krates_blocking(
                        packages
                            .into_iter()
                            .map(|p| p.as_str().to_owned())
                            .collect(),
                        true,
                        REQUEST_TIMEOUT,
                    )
                    .map_err(|err| {
                        format_err!(
                            ErrorKind::Registry,
                            "unable to acquire tokio runtime: {}",
                            err
                        )
                    })?;

                for (name, res) in results {
                    self.insert(
                        name.parse().expect("this was a package name before"),
                        res.map_err(Error::from_tame),
                    );
                }
            }
        }

        Ok(())
    }

    #[inline]
    fn insert(
        &mut self,
        package: package::Name,
        krate_res: Result<Option<tame_index::IndexKrate>, Error>,
    ) {
        let krate_res = krate_res.map(|ik| {
            ik.map(|ik| {
                ik.versions
                    .into_iter()
                    .map(|v| (v.version.to_string(), v.is_yanked()))
                    .collect()
            })
        });

        self.cache.insert(package, krate_res);
    }

    /// Is the given package yanked?
    fn is_yanked(&mut self, package: &Package) -> Result<bool, Error> {
        if !self.cache.contains_key(&package.name) {
            self.insert(package.name.to_owned(), self.index.krate(&package.name));
        }

        match &self.cache[&package.name] {
            Ok(Some(ik)) => match ik.get(&package.version.to_string()) {
                Some(is_yanked) => Ok(*is_yanked),
                None => Err(format_err!(
                    ErrorKind::NotFound,
                    "No such version in crates.io index: {} {}",
                    &package.name,
                    &package.version
                )),
            },
            Ok(None) => Err(format_err!(
                ErrorKind::NotFound,
                "No such crate in crates.io index: {}",
                &package.name,
            )),
            Err(err) => Err(format_err!(
                ErrorKind::Registry,
                "Failed to retrieve {} from crates.io index: {}",
                &package.name,
                err,
            )),
        }
    }

    /// Iterate over the provided packages, returning a vector of the
    /// packages which have been yanked.
    ///
    /// This function should be called with many packages at once rather than one by one;
    /// that way it can download the status of a large number of packages at once from the sparse index
    /// very quickly, orders of magnitude faster than requesting packages one by one.
    pub fn find_yanked<'a, I>(&mut self, packages: I) -> Vec<Result<&'a Package, Error>>
    where
        I: IntoIterator<Item = &'a Package>,
    {
        let mut yanked = Vec::new();

        let dedup_packages: BTreeSet<&Package> = packages.into_iter().collect();
        let package_names: BTreeSet<&package::Name> =
            dedup_packages.iter().map(|p| &p.name).collect();
        if let Err(e) = self.populate_cache(package_names) {
            yanked.push(Err(Error::new(ErrorKind::Registry,
                &format!("Failed to download crates.io index: {}\nData may be missing or stale when checking for yanked packages.", e)
            )));
        }

        for package in dedup_packages {
            match self.is_yanked(package) {
                Ok(false) => {} // not yanked, nothing to report
                Ok(true) => yanked.push(Ok(package)),
                Err(error) => yanked.push(Err(error)),
            }
        }

        yanked
    }
}

/// Replacement to [tame_index::index::RemoteGitIndex::new] that also supports passing the lock timeout
fn new_remote_git_index(
    index: tame_index::index::git::GitIndex,
    lock_timeout: Duration,
) -> Result<tame_index::index::RemoteGitIndex, tame_index::Error> {
    let lock_policy = if lock_timeout == Duration::from_secs(0) {
        gix::lock::acquire::Fail::Immediately
    } else {
        gix::lock::acquire::Fail::AfterDurationWithBackoff(lock_timeout)
    };
    tame_index::index::RemoteGitIndex::with_options(
        index,
        gix::progress::Discard,
        &gix::interrupt::IS_INTERRUPTED,
        lock_policy,
    )
}
