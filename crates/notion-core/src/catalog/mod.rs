//! Provides types for working with Notion's local _catalog_, the local repository
//! of available tool versions.

use std::collections::{BTreeSet, HashSet};
use std::fs::{remove_dir_all, File};
use std::io::{self, Write};
use std::marker::PhantomData;
use std::str::FromStr;
use std::string::ToString;
use std::time::{Duration, SystemTime};

use lazycell::LazyCell;
use readext::ReadExt;
use reqwest;
use reqwest::header::{CacheControl, CacheDirective, Expires, HttpDate};
use serde_json;
use tempfile::NamedTempFile;
use toml;

use config::{Config, ToolConfig};
use distro::node::NodeDistro;
use distro::yarn::YarnDistro;
use distro::{Distro, Fetched};
use fs::{ensure_containing_dir_exists, read_file_opt, touch};
use notion_fail::{ExitCode, Fallible, NotionError, NotionFail, ResultExt};
use path::{self, user_catalog_file};
use semver::{Version, VersionReq};
use style::progress_spinner;
use version::VersionSpec;

pub(crate) mod serial;

#[cfg(feature = "mock-network")]
use mockito;

// ISSUE (#86): Move public repository URLs to config file
cfg_if! {
    if #[cfg(feature = "mock-network")] {
        fn public_node_version_index() -> String {
            format!("{}/node-dist/index.json", mockito::SERVER_URL)
        }
        fn public_yarn_version_index() -> String {
            format!("{}/yarn-releases/index.json", mockito::SERVER_URL)
        }
        fn public_yarn_latest_version() -> String {
            format!("{}/yarn-latest", mockito::SERVER_URL)
        }
    } else {
        /// Returns the URL of the index of available Node versions on the public Node server.
        fn public_node_version_index() -> String {
            "https://nodejs.org/dist/index.json".to_string()
        }
        /// Return the URL of the index of available Yarn versions on the public git repository.
        fn public_yarn_version_index() -> String {
            "https://github.com/notion-cli/yarn-releases/raw/master/index.json".to_string()
        }
        /// URL of the latest Yarn version on the public yarnpkg.com
        fn public_yarn_latest_version() -> String {
            "https://yarnpkg.com/latest-version".to_string()
        }
    }
}

/// Lazily loaded tool catalog.
pub struct LazyCatalog {
    catalog: LazyCell<Catalog>,
}

impl LazyCatalog {
    /// Constructs a new `LazyCatalog`.
    pub fn new() -> LazyCatalog {
        LazyCatalog {
            catalog: LazyCell::new(),
        }
    }

    /// Forces the loading of the catalog and returns an immutable reference to it.
    pub fn get(&self) -> Fallible<&Catalog> {
        self.catalog.try_borrow_with(|| Catalog::current())
    }

    /// Forces the loading of the catalog and returns a mutable reference to it.
    pub fn get_mut(&mut self) -> Fallible<&mut Catalog> {
        self.catalog.try_borrow_mut_with(|| Catalog::current())
    }
}

pub struct Collection<D: Distro> {
    /// The currently activated Node version, if any.
    pub default: Option<Version>,

    // A sorted collection of the available versions in the catalog.
    pub versions: BTreeSet<Version>,

    pub phantom: PhantomData<D>,
}

pub type NodeCollection = Collection<NodeDistro>;
pub type YarnCollection = Collection<YarnDistro>;

/// The catalog of tool versions available locally.
pub struct Catalog {
    pub node: NodeCollection,
    pub yarn: YarnCollection,
}

impl Catalog {
    /// Returns the current tool catalog.
    fn current() -> Fallible<Catalog> {
        let path = user_catalog_file()?;
        let src = touch(&path)?.read_into_string().unknown()?;
        src.parse()
    }

    /// Returns a pretty-printed TOML representation of the contents of the catalog.
    pub fn to_string(&self) -> String {
        toml::to_string_pretty(&self.to_serial()).unwrap()
    }

    /// Saves the contents of the catalog to the user's catalog file.
    pub fn save(&self) -> Fallible<()> {
        let path = user_catalog_file()?;
        let mut file = File::create(&path).unknown()?;
        file.write_all(self.to_string().as_bytes()).unknown()?;
        Ok(())
    }

    /// Sets the Node version in the user toolchain to one matching the specified semantic versioning requirements.
    pub fn set_user_node(&mut self, matching: &VersionSpec, config: &Config) -> Fallible<()> {
        let fetched = self.fetch_node(matching, config)?;
        let version = Some(fetched.into_version());

        if self.node.default != version {
            self.node.default = version;
            self.save()?;
        }

        Ok(())
    }

    /// Fetches a Node version matching the specified semantic versioning requirements.
    pub fn fetch_node(&mut self, matching: &VersionSpec, config: &Config) -> Fallible<Fetched> {
        let distro = self.node.resolve_remote(matching, config.node.as_ref())?;
        let fetched = distro.fetch(&self.node).unknown()?;

        if let &Fetched::Now(ref version) = &fetched {
            self.node.versions.insert(version.clone());
            self.save()?;
        }

        Ok(fetched)
    }

    /// Resolves a Node version matching the specified semantic versioning requirements.
    pub fn resolve_node(&self, matching: &VersionSpec, config: &Config) -> Fallible<Version> {
        let distro = self.node.resolve_remote(&matching, config.node.as_ref())?;
        Ok(distro.version().clone())
    }

    /// Uninstalls a specific Node version from the local catalog.
    pub fn uninstall_node(&mut self, version: &Version) -> Fallible<()> {
        if self.node.contains(version) {
            let home = path::node_version_dir(&version.to_string())?;

            if !home.is_dir() {
                Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("{} is not a directory", home.to_string_lossy()),
                )).unknown()?;
            }

            remove_dir_all(home).unknown()?;

            self.node.versions.remove(version);

            self.save()?;
        }

        Ok(())
    }

    // ISSUE (#87) Abstract Catalog's activate, install and uninstall methods
    // And potentially share code between node and yarn
    /// Sets the Yarn version in the user toolchain to one matching the specified semantic versioning requirements.
    pub fn set_user_yarn(&mut self, matching: &VersionSpec, config: &Config) -> Fallible<()> {
        let fetched = self.fetch_yarn(matching, config)?;
        let version = Some(fetched.into_version());

        if self.yarn.default != version {
            self.yarn.default = version;
            self.save()?;
        }

        Ok(())
    }

    /// Fetches a Yarn version matching the specified semantic versioning requirements.
    pub fn fetch_yarn(&mut self, matching: &VersionSpec, config: &Config) -> Fallible<Fetched> {
        let distro = self.yarn.resolve_remote(&matching, config.yarn.as_ref())?;
        let fetched = distro.fetch(&self.yarn).unknown()?;

        if let &Fetched::Now(ref version) = &fetched {
            self.yarn.versions.insert(version.clone());
            self.save()?;
        }

        Ok(fetched)
    }

    /// Resolves a Yarn version matching the specified semantic versioning requirements.
    pub fn resolve_yarn(&self, matching: &VersionSpec, config: &Config) -> Fallible<Version> {
        let distro = self.yarn.resolve_remote(&matching, config.yarn.as_ref())?;
        Ok(distro.version().clone())
    }

    /// Uninstalls a specific Yarn version from the local catalog.
    pub fn uninstall_yarn(&mut self, version: &Version) -> Fallible<()> {
        if self.yarn.contains(version) {
            let home = path::yarn_version_dir(&version.to_string())?;

            if !home.is_dir() {
                Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("{} is not a directory", home.to_string_lossy()),
                )).unknown()?;
            }

            remove_dir_all(home).unknown()?;

            self.yarn.versions.remove(version);

            self.save()?;
        }

        Ok(())
    }
}

/// Thrown when there is no Node version matching a requested semver specifier.
#[derive(Debug, Fail, NotionFail)]
#[fail(display = "No Node version found for {}", matching)]
#[notion_fail(code = "NoVersionMatch")]
struct NoNodeVersionFoundError {
    matching: VersionSpec,
}

/// Thrown when there is no Yarn version matching a requested semver specifier.
#[derive(Debug, Fail, NotionFail)]
#[fail(display = "No Yarn version found for {}", matching)]
#[notion_fail(code = "NoVersionMatch")]
struct NoYarnVersionFoundError {
    matching: VersionReq,
}

impl<D: Distro> Collection<D> {
    /// Tests whether this Collection contains the specified Tool version.
    pub fn contains(&self, version: &Version) -> bool {
        self.versions.contains(version)
    }
}

pub trait Resolve<D: Distro> {
    /// Resolves the specified semantic versioning requirements from a remote distributor.
    fn resolve_remote(
        &self,
        matching: &VersionSpec,
        config: Option<&ToolConfig<D>>,
    ) -> Fallible<D> {
        match config {
            Some(ToolConfig {
                resolve: Some(ref plugin),
                ..
            }) => plugin.resolve(matching),
            _ => self.resolve_public(matching),
        }
    }

    /// Resolves the specified semantic versioning requirements from the public distributor (e.g. `https://nodejs.org`).
    fn resolve_public(&self, matching: &VersionSpec) -> Fallible<D>;
}

/// Thrown when the public registry for Node or Yarn could not be downloaded.
#[derive(Debug, Fail, NotionFail)]
#[fail(display = "Could not fetch public registry\n{}", error)]
#[notion_fail(code = "NetworkError")]
pub(crate) struct RegistryFetchError {
    error: String,
}

impl RegistryFetchError {
    pub(crate) fn from_error(error: &reqwest::Error) -> RegistryFetchError {
        RegistryFetchError {
            error: error.to_string(),
        }
    }
}

impl Resolve<NodeDistro> for NodeCollection {
    fn resolve_public(&self, matching: &VersionSpec) -> Fallible<NodeDistro> {
        let version_opt = {
            let index: Index = resolve_node_versions()?.into_index()?;
            let mut entries = index.entries.into_iter();
            let entry = match *matching {
                VersionSpec::Latest => {
                    // NOTE: This assumes the registry always produces a list in sorted order
                    //       from newest to oldest. This should be specified as a requirement
                    //       when we document the plugin API.
                    entries.next()
                }
                VersionSpec::Semver(ref matching) => {
                    // ISSUE #34: also make sure this OS is available for this version
                    entries.find(|&(ref k, _)| matching.matches(k))
                }
            };
            entry.map(|(k, _)| k)
        };

        if let Some(version) = version_opt {
            NodeDistro::public(version)
        } else {
            throw!(NoNodeVersionFoundError {
                matching: matching.clone()
            })
        }
    }
}

impl Resolve<YarnDistro> for YarnCollection {
    /// Resolves the specified semantic versioning requirements from the public distributor.
    fn resolve_public(&self, matching: &VersionSpec) -> Fallible<YarnDistro> {
        let version = match *matching {
            VersionSpec::Latest => {
                let mut response: reqwest::Response =
                    reqwest::get(public_yarn_latest_version().as_str())
                        .with_context(RegistryFetchError::from_error)?;
                response.text().unknown()?
            }
            VersionSpec::Semver(ref matching) => {
                let spinner = progress_spinner(&format!(
                    "Fetching public registry: {}",
                    public_yarn_version_index()
                ));
                let releases: Vec<String> = reqwest::get(public_yarn_version_index().as_str())
                    .with_context(RegistryFetchError::from_error)?
                    .json()
                    .unknown()?;
                spinner.finish_and_clear();
                let version = releases.into_iter().find(|v| {
                    let v = Version::parse(v).unwrap();
                    matching.matches(&v)
                });

                if let Some(version) = version {
                    version
                } else {
                    throw!(NoYarnVersionFoundError {
                        matching: matching.clone(),
                    });
                }
            }
        };
        YarnDistro::public(Version::parse(&version).unknown()?)
    }
}

/// The index of the public Node server.
pub struct Index {
    entries: Vec<(Version, VersionData)>,
}

/// The set of available files on the public Node server for a given Node version.
pub struct VersionData {
    pub files: HashSet<String>,
}

impl FromStr for Catalog {
    type Err = NotionError;

    fn from_str(src: &str) -> Result<Self, Self::Err> {
        let serial: serial::Catalog = toml::from_str(src).unknown()?;
        Ok(serial.into_catalog()?)
    }
}

/// Reads a public index from the Node cache, if it exists and hasn't expired.
fn read_cached_opt() -> Fallible<Option<serial::Index>> {
    let expiry: Option<String> = read_file_opt(&path::node_index_expiry_file()?).unknown()?;

    if let Some(string) = expiry {
        let expiry_date: HttpDate = HttpDate::from_str(&string).unknown()?;
        let current_date: HttpDate = HttpDate::from(SystemTime::now());

        if current_date < expiry_date {
            let cached: Option<String> = read_file_opt(&path::node_index_file()?).unknown()?;

            if let Some(string) = cached {
                return Ok(serde_json::de::from_str(&string).unknown()?);
            }
        }
    }

    Ok(None)
}

/// Get the cache max-age of an HTTP reponse.
fn max_age(response: &reqwest::Response) -> u32 {
    if let Some(cache_control_header) = response.headers().get::<CacheControl>() {
        for cache_directive in cache_control_header.iter() {
            if let CacheDirective::MaxAge(max_age) = cache_directive {
                return *max_age;
            }
        }
    }

    // Default to four hours.
    4 * 60 * 60
}

fn resolve_node_versions() -> Result<serial::Index, NotionError> {
    match read_cached_opt().unknown()? {
        Some(serial) => Ok(serial),
        None => {
            let spinner = progress_spinner(&format!(
                "Fetching public registry: {}",
                public_node_version_index()
            ));
            let mut response: reqwest::Response = reqwest::get(
                public_node_version_index().as_str(),
            ).with_context(RegistryFetchError::from_error)?;
            let response_text: String = response.text().unknown()?;
            let cached: NamedTempFile = NamedTempFile::new().unknown()?;

            // Block to borrow cached for cached_file.
            {
                let mut cached_file: &File = cached.as_file();
                cached_file.write(response_text.as_bytes()).unknown()?;
            }

            let index_cache_file = path::node_index_file()?;
            ensure_containing_dir_exists(&index_cache_file)?;
            cached.persist(index_cache_file).unknown()?;

            let expiry: NamedTempFile = NamedTempFile::new().unknown()?;

            // Block to borrow expiry for expiry_file.
            {
                let mut expiry_file: &File = expiry.as_file();

                if let Some(expires_header) = response.headers().get::<Expires>() {
                    write!(expiry_file, "{}", expires_header).unknown()?;
                } else {
                    let expiry_date =
                        SystemTime::now() + Duration::from_secs(max_age(&response).into());

                    write!(expiry_file, "{}", HttpDate::from(expiry_date)).unknown()?;
                }
            }

            let index_expiry_file = path::node_index_expiry_file()?;
            ensure_containing_dir_exists(&index_expiry_file)?;
            expiry.persist(index_expiry_file).unknown()?;

            let serial: serial::Index = serde_json::de::from_str(&response_text).unknown()?;

            spinner.finish_and_clear();
            Ok(serial)
        }
    }
}
