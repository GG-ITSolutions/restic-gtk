use std::collections::BTreeMap;

use url::{Host, Url};

use super::RepoConfig;

/// Location of S3 repositories; the details are in [`RepoConfig::options`].
pub const LOCATION: &str = "opendal:s3";

/// Option that carries the secret key. Only set for [`Session::open`](super::Session::open),
/// never stored.
pub(crate) const SECRET_OPTION: &str = "secret_access_key";

/// Region given to OpenDAL when none is set; S3-compatible services ignore it.
const AUTO_REGION: &str = "auto";

/// An S3 or S3-compatible repository, as edited in the repository dialog.
///
/// Stored as [`LOCATION`] plus OpenDAL's S3 options. The secret key is not part of it,
/// it is asked for when unlocking.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct S3Location {
    /// `http(s)://host[:port]`, without bucket.
    pub endpoint: String,
    pub bucket: String,
    /// Folder of the repository inside the bucket, empty for its root.
    pub root: String,
    /// Empty for “auto”, which every service besides AWS accepts.
    pub region: String,
    pub access_key_id: String,
    /// Address the bucket as `bucket.host` instead of `host/bucket`.
    pub virtual_host_style: bool,
}

impl S3Location {
    /// The S3 settings of `config`, `None` for other repositories.
    pub fn from_config(config: &RepoConfig) -> Option<Self> {
        if config.location != LOCATION {
            return None;
        }
        let option = |key: &str| config.options.get(key).cloned().unwrap_or_default();
        let region = option("region");
        Some(Self {
            endpoint: option("endpoint"),
            bucket: option("bucket"),
            root: option("root").trim_matches('/').to_owned(),
            region: if region == AUTO_REGION {
                String::new()
            } else {
                region
            },
            access_key_id: option("access_key_id"),
            virtual_host_style: option("enable_virtual_host_style") == "true",
        })
    }

    /// The options to store in [`RepoConfig::options`], with [`LOCATION`] as location.
    ///
    /// Credentials and region come only from here, never from `AWS_*` variables or
    /// `~/.aws`: otherwise the same entry would behave differently per user and shell.
    pub fn to_options(&self) -> BTreeMap<String, String> {
        let mut options = BTreeMap::from(
            [
                ("endpoint", self.endpoint_url()),
                ("bucket", self.bucket.trim().to_owned()),
                ("root", format!("/{}", self.root.trim().trim_matches('/'))),
                ("region", self.effective_region()),
                ("access_key_id", self.access_key_id.trim().to_owned()),
                ("disable_config_load", "true".to_owned()),
                ("disable_ec2_metadata", "true".to_owned()),
            ]
            .map(|(key, value)| (key.to_owned(), value)),
        );
        if self.virtual_host_style {
            options.insert("enable_virtual_host_style".into(), "true".into());
        }
        options
    }

    /// Checks the settings before they are stored. Returns a short, user-facing reason otherwise.
    pub fn validate(&self) -> Result<(), String> {
        let url =
            Url::parse(&self.endpoint_url()).map_err(|_| "Not a valid endpoint".to_string())?;
        if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
            return Err("Use an endpoint like https://host".into());
        }
        if url.path() != "/" || url.query().is_some() {
            return Err("Enter the endpoint without bucket or path".into());
        }
        let bucket = self.bucket.trim();
        if bucket.is_empty() {
            return Err("Enter a bucket".into());
        }
        if bucket.contains('/') {
            return Err("Enter the folder inside the bucket as path".into());
        }
        if self.access_key_id.trim().is_empty() {
            return Err("Enter an access key ID".into());
        }
        // OpenDAL would sign for, and connect to, s3.auto.amazonaws.com.
        if url.host_str() == Some("s3.amazonaws.com") && self.region.trim().is_empty() {
            return Err("Enter the region of the bucket".into());
        }
        if self.virtual_host_style {
            if matches!(url.host(), Some(Host::Ipv4(_) | Host::Ipv6(_)))
                || url.host_str() == Some("localhost")
            {
                return Err("Virtual-host style needs a host name, not an address".into());
            }
            if bucket.contains('.') {
                return Err("Virtual-host style doesn't work with “.” in the bucket".into());
            }
        }
        Ok(())
    }

    /// `s3:https://host/bucket/path`, the form restic uses.
    pub fn display(&self) -> String {
        let root = self.root.trim().trim_matches('/');
        let mut display = format!("s3:{}/{}", self.endpoint_url(), self.bucket.trim());
        if !root.is_empty() {
            display.push('/');
            display.push_str(root);
        }
        display
    }

    /// Reads a restic repository URL: `s3:https://host/bucket/path`, `s3://host/bucket/path`
    /// or `s3:host/bucket/path`; the latter two use https.
    pub fn parse_restic(url: &str) -> Option<Self> {
        let url = url.trim().strip_prefix("s3:")?;
        let (scheme, rest) = if let Some(rest) = url.strip_prefix("https://") {
            ("https", rest)
        } else if let Some(rest) = url.strip_prefix("http://") {
            ("http", rest)
        } else {
            ("https", url.strip_prefix("//").unwrap_or(url))
        };
        let (host, path) = rest.split_once('/')?;
        let (bucket, root) = path.split_once('/').unwrap_or((path, ""));
        if host.is_empty() || bucket.is_empty() {
            return None;
        }
        Some(Self {
            endpoint: format!("{scheme}://{host}"),
            bucket: bucket.to_owned(),
            root: root.trim_matches('/').to_owned(),
            region: aws_region(host).unwrap_or_default().to_owned(),
            ..Self::default()
        })
    }

    /// The endpoint with scheme and without trailing `/`; OpenDAL also defaults to https.
    fn endpoint_url(&self) -> String {
        let endpoint = self.endpoint.trim().trim_end_matches('/');
        if endpoint.contains("://") {
            endpoint.to_owned()
        } else {
            format!("https://{endpoint}")
        }
    }

    fn effective_region(&self) -> String {
        let region = self.region.trim();
        if !region.is_empty() {
            return region.to_owned();
        }
        Url::parse(&self.endpoint_url())
            .ok()
            .and_then(|url| url.host_str().and_then(aws_region).map(str::to_owned))
            .unwrap_or_else(|| AUTO_REGION.to_owned())
    }
}

/// The region in an AWS endpoint like `s3.eu-central-1.amazonaws.com`.
fn aws_region(host: &str) -> Option<&str> {
    let host = host.split(':').next()?;
    host.strip_prefix("s3.")?
        .strip_suffix(".amazonaws.com")
        .filter(|region| !region.is_empty() && !region.contains('.'))
}
