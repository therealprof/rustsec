//! An efficient way to check whether a given package has been yanked
use std::collections::HashMap;

use crate::{
    error::{Error, ErrorKind},
    package::{self, Package},
};

pub use tame_index::external::reqwest::blocking::ClientBuilder;

enum Index {
    Git(tame_index::index::RemoteGitIndex),
    SparseCached(tame_index::index::SparseIndex),
    SparseRemote(tame_index::index::RemoteSparseIndex),
}

impl Index {
    #[inline]
    fn krate(&self, name: &package::Name) -> Result<Option<tame_index::IndexKrate>, Error> {
        let name = name.as_str().try_into()?;
        let res = match self {
            Self::Git(gi) => gi.krate(name, true),
            Self::SparseCached(si) => si.cached_krate(name),
            Self::SparseRemote(rsi) => rsi.krate(name, true),
        };

        Ok(res?)
    }
}

/// Provides an efficient way to check if the given package has been yanked.
///
/// Operations on crates.io index are rather slow.
/// Instead of peforming an index lookup for every version of every crate,
/// this implementation looks up each crate only once and caches the result.
/// This usually doesn't result in any dramatic performance wins
/// when auditing a single `Cargo.lock` file because the same crate rarely appears multiple times,
/// but makes a huge difference when auditing many `Cargo.lock`s or many binaries.
pub struct CachedIndex {
    index: Index,
    /// The inner hash map is logically HashMap<Version, IsYanked>
    /// but we don't parse semver because crates.io registry contains invalid semver:
    /// <https://github.com/rustsec/rustsec/issues/759>
    // The outer map can later be changed to DashMap or some such for thread safety.
    cache: HashMap<package::Name, HashMap<String, bool>>,
}

impl CachedIndex {
    /// Open the local crates.io index
    ///
    /// If this opens a git index, it will perform a fetch to get the latest index
    /// information.
    ///
    /// If this is a sparse index, it will allow [`Self::populate_cache`] to
    /// fetch the latest information from the remote HTTP index
    pub fn fetch(client: Option<ClientBuilder>) -> Result<Self, Error> {
        let index = tame_index::index::ComboIndexCache::new(tame_index::IndexLocation::new(
            tame_index::IndexUrl::crates_io(None, None, None)?,
        ))?;

        let index = match index {
            tame_index::index::ComboIndexCache::Git(gi) => {
                let mut rgi = tame_index::index::RemoteGitIndex::new(gi)?;
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

                Index::SparseRemote(tame_index::index::RemoteSparseIndex::new(si, client))
            }
        };

        Ok(CachedIndex {
            index,
            cache: Default::default(),
        })
    }

    /// Open the local crates.io index
    ///
    /// If this opens a git index, it allows reading of index entries from the
    /// repository
    ///
    /// If this is a sparse index, it only allows reading of index entries that
    /// are already cached locally
    pub fn open() -> Result<Self, Error> {
        let index = tame_index::index::ComboIndexCache::new(tame_index::IndexLocation::new(
            tame_index::IndexUrl::crates_io(None, None, None)?,
        ))?;

        let index = match index {
            tame_index::index::ComboIndexCache::Git(gi) => {
                let rgi = tame_index::index::RemoteGitIndex::new(gi)?;
                Index::Git(rgi)
            }
            tame_index::index::ComboIndexCache::Sparse(si) => Index::SparseCached(si),
        };

        Ok(CachedIndex {
            index,
            cache: Default::default(),
        })
    }

    /// Load all version of the given crate from the crates.io index and put them into the cache
    pub fn populate_cache(&mut self, package: &package::Name) -> Result<(), Error> {
        let crate_releases = self.index.krate(package)?.ok_or_else(|| {
            format_err!(
                ErrorKind::NotFound,
                "no such crate in the crates.io index: {package}"
            )
        })?;

        // We already loaded the full crate information, so populate all the versions in the cache
        let versions: HashMap<String, bool> = crate_releases
            .versions
            .iter()
            .map(|v| (v.version.to_string(), v.is_yanked()))
            .collect();
        self.cache.insert(package.to_owned(), versions);
        Ok(())
    }

    /// Is the given package yanked?
    pub fn is_yanked(&mut self, package: &Package) -> Result<bool, Error> {
        let crate_is_cached = { self.cache.contains_key(&package.name) };
        if !crate_is_cached {
            self.populate_cache(&package.name)?
        };
        match &self.cache[&package.name].get(&package.version.to_string()) {
            Some(is_yanked) => Ok(**is_yanked),
            None => Err(format_err!(
                ErrorKind::NotFound,
                "No such version in crates.io index: {} {}",
                &package.name,
                &package.version
            )),
        }
    }

    /// Iterate over the provided packages, returning a vector of the
    /// packages which have been yanked.
    pub fn find_yanked<'a, I>(&mut self, packages: I) -> Result<Vec<&'a Package>, Error>
    where
        I: IntoIterator<Item = &'a Package>,
    {
        let mut yanked = Vec::new();

        for package in packages {
            if self.is_yanked(package)? {
                yanked.push(package);
            }
        }

        Ok(yanked)
    }
}
