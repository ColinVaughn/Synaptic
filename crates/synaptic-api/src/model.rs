use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A package ecosystem supported by dependency inventory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Ecosystem {
    /// A package type not natively modeled by Synaptic. The original purl is
    /// retained on the dependency so this fallback never discards identity.
    Generic,
    Npm,
    Pypi,
    Cargo,
    Go,
    Maven,
    Nuget,
    Composer,
    Gem,
    Swift,
    Pub,
    Hex,
    Luarocks,
    Julia,
    Zig,
    Conan,
    Vcpkg,
    Cocoapods,
    Powershell,
    Fpm,
    Codeql,
    Salesforce,
    Com,
}

impl Ecosystem {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Generic => "generic",
            Self::Npm => "npm",
            Self::Pypi => "pypi",
            Self::Cargo => "cargo",
            Self::Go => "go",
            Self::Maven => "maven",
            Self::Nuget => "nuget",
            Self::Composer => "composer",
            Self::Gem => "gem",
            Self::Swift => "swift",
            Self::Pub => "pub",
            Self::Hex => "hex",
            Self::Luarocks => "luarocks",
            Self::Julia => "julia",
            Self::Zig => "zig",
            Self::Conan => "conan",
            Self::Vcpkg => "vcpkg",
            Self::Cocoapods => "cocoapods",
            Self::Powershell => "powershell",
            Self::Fpm => "fpm",
            Self::Codeql => "codeql",
            Self::Salesforce => "salesforce",
            Self::Com => "com",
        }
    }
}

impl fmt::Display for Ecosystem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Ecosystem {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "generic" => Ok(Self::Generic),
            "npm" => Ok(Self::Npm),
            "pypi" | "python" => Ok(Self::Pypi),
            "cargo" | "crates.io" => Ok(Self::Cargo),
            "go" | "gomod" => Ok(Self::Go),
            "maven" => Ok(Self::Maven),
            "nuget" => Ok(Self::Nuget),
            "composer" | "packagist" => Ok(Self::Composer),
            "gem" | "rubygems" | "bundler" => Ok(Self::Gem),
            "swift" | "swiftpm" => Ok(Self::Swift),
            "pub" | "pubdev" => Ok(Self::Pub),
            "hex" => Ok(Self::Hex),
            "luarocks" | "rock" => Ok(Self::Luarocks),
            "julia" | "juliapkg" => Ok(Self::Julia),
            "zig" => Ok(Self::Zig),
            "conan" => Ok(Self::Conan),
            "vcpkg" => Ok(Self::Vcpkg),
            "cocoapods" | "pod" => Ok(Self::Cocoapods),
            "powershell" | "psgallery" => Ok(Self::Powershell),
            "fpm" => Ok(Self::Fpm),
            "codeql" => Ok(Self::Codeql),
            "salesforce" | "apex" => Ok(Self::Salesforce),
            "com" => Ok(Self::Com),
            other => Err(format!("unsupported package ecosystem {other:?}")),
        }
    }
}

/// An ecosystem-qualified package identity (`npm:stripe`, `pypi:stripe`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PackageCoordinate {
    pub ecosystem: Ecosystem,
    pub name: String,
}

impl PackageCoordinate {
    pub fn new(ecosystem: Ecosystem, name: impl Into<String>) -> Self {
        let name = normalize_package_name(ecosystem, &name.into());
        Self { ecosystem, name }
    }
}

impl fmt::Display for PackageCoordinate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.ecosystem, self.name)
    }
}

impl FromStr for PackageCoordinate {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (ecosystem, name) = value.trim().split_once(':').ok_or_else(|| {
            format!("package coordinate must be <ecosystem>:<name>, got {value:?}")
        })?;
        let ecosystem = ecosystem.parse()?;
        if name.trim().is_empty() {
            return Err("package coordinate name cannot be empty".into());
        }
        Ok(Self::new(ecosystem, name))
    }
}

impl Serialize for PackageCoordinate {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for PackageCoordinate {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        raw.parse().map_err(serde::de::Error::custom)
    }
}

fn normalize_package_name(ecosystem: Ecosystem, name: &str) -> String {
    let trimmed = name.trim();
    match ecosystem {
        // PyPI treats runs of '-', '_', and '.' as equivalent.
        Ecosystem::Pypi => {
            let mut out = String::new();
            let mut separator = false;
            for ch in trimmed.chars().flat_map(char::to_lowercase) {
                if matches!(ch, '-' | '_' | '.') {
                    if !separator {
                        out.push('-');
                        separator = true;
                    }
                } else {
                    out.push(ch);
                    separator = false;
                }
            }
            out
        }
        Ecosystem::Generic
        | Ecosystem::Npm
        | Ecosystem::Cargo
        | Ecosystem::Nuget
        | Ecosystem::Composer
        | Ecosystem::Gem
        | Ecosystem::Swift
        | Ecosystem::Pub
        | Ecosystem::Hex
        | Ecosystem::Luarocks
        | Ecosystem::Julia
        | Ecosystem::Zig
        | Ecosystem::Conan
        | Ecosystem::Vcpkg
        | Ecosystem::Cocoapods
        | Ecosystem::Powershell
        | Ecosystem::Fpm
        | Ecosystem::Codeql
        | Ecosystem::Salesforce
        | Ecosystem::Com => trimmed.to_ascii_lowercase(),
        Ecosystem::Go | Ecosystem::Maven => trimmed.to_string(),
    }
}

/// Why a dependency is present in its declaring manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DependencyScope {
    Runtime,
    Development,
    Optional,
}

/// One dependency observed in a repository manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dependency {
    pub package: PackageCoordinate,
    pub source_file: String,
    pub scope: DependencyScope,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub declared_requirement: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub resolved_version: Option<String>,
    /// Canonical Package URL when the declaring source provides one.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub purl: Option<String>,
}

impl Dependency {
    pub fn new(
        package: PackageCoordinate,
        source_file: impl Into<String>,
        scope: DependencyScope,
    ) -> Self {
        Self {
            package,
            source_file: source_file.into().replace('\\', "/"),
            scope,
            declared_requirement: None,
            resolved_version: None,
            purl: None,
        }
    }
}

/// A parsed [Package URL](https://github.com/package-url/purl-spec) identity.
///
/// Unknown package types are deliberately accepted and map to the generic
/// ecosystem. This makes the inventory extensible without weakening identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageUrl {
    canonical: String,
    pub package_type: String,
    pub namespace: Option<String>,
    pub name: String,
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub qualifiers: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subpath: Option<String>,
}

impl PackageUrl {
    pub fn parse(value: &str) -> Result<Self, String> {
        let value = value.trim();
        let body = value
            .strip_prefix("pkg:")
            .ok_or_else(|| format!("package URL must start with pkg:, got {value:?}"))?;
        let (before_fragment, subpath) = body
            .split_once('#')
            .map_or((body, None), |(head, tail)| (head, Some(tail)));
        let (path_and_version, raw_qualifiers) = before_fragment
            .split_once('?')
            .map_or((before_fragment, None), |(head, tail)| (head, Some(tail)));
        let (package_type, raw_path) = path_and_version
            .split_once('/')
            .ok_or_else(|| "package URL must contain a type and name".to_string())?;
        let package_type = package_type.trim().to_ascii_lowercase();
        if package_type.is_empty() || !package_type.chars().all(is_purl_type_char) {
            return Err(format!("invalid package URL type {package_type:?}"));
        }
        let (raw_package_path, raw_version) = match raw_path.rsplit_once('@') {
            Some((path, version)) => {
                if version.is_empty() {
                    return Err("package URL version cannot be empty".into());
                }
                (path, Some(version))
            }
            None => (raw_path, None),
        };
        let raw_segments = raw_package_path
            .split('/')
            .filter(|segment| !segment.is_empty())
            .collect::<Vec<_>>();
        if raw_segments.is_empty() {
            return Err("package URL name cannot be empty".into());
        }
        let decoded = raw_segments
            .iter()
            .map(|segment| percent_decode(segment))
            .collect::<Result<Vec<_>, _>>()?;
        let name = decoded.last().cloned().unwrap_or_default();
        if name.is_empty() || matches!(name.as_str(), "." | "..") {
            return Err("package URL name cannot be empty or relative".into());
        }
        let namespace = (decoded.len() > 1).then(|| decoded[..decoded.len() - 1].join("/"));
        let version = raw_version.map(percent_decode).transpose()?;

        let mut qualifiers = BTreeMap::new();
        if let Some(raw) = raw_qualifiers {
            if raw.is_empty() {
                return Err("package URL qualifiers cannot be empty".into());
            }
            for pair in raw.split('&') {
                let (key, value) = pair
                    .split_once('=')
                    .ok_or_else(|| format!("invalid package URL qualifier {pair:?}"))?;
                let key = percent_decode(key)?.to_ascii_lowercase();
                let value = percent_decode(value)?;
                if key.is_empty() || value.is_empty() || qualifiers.insert(key, value).is_some() {
                    return Err(format!(
                        "invalid or duplicate package URL qualifier {pair:?}"
                    ));
                }
            }
        }
        let subpath = subpath.map(percent_decode).transpose()?;
        if subpath.as_deref().is_some_and(|path| {
            path.split('/')
                .any(|segment| matches!(segment, "" | "." | ".."))
        }) {
            return Err("package URL subpath contains an invalid segment".into());
        }

        Ok(Self {
            canonical: value.to_string(),
            package_type,
            namespace,
            name,
            version,
            qualifiers,
            subpath,
        })
    }

    pub fn to_coordinate(&self) -> PackageCoordinate {
        let ecosystem = match self.package_type.as_str() {
            "npm" => Ecosystem::Npm,
            "pypi" => Ecosystem::Pypi,
            "cargo" => Ecosystem::Cargo,
            "golang" => Ecosystem::Go,
            "maven" => Ecosystem::Maven,
            "nuget" => Ecosystem::Nuget,
            "composer" => Ecosystem::Composer,
            "gem" => Ecosystem::Gem,
            "swift" => Ecosystem::Swift,
            "pub" => Ecosystem::Pub,
            "hex" => Ecosystem::Hex,
            "luarocks" => Ecosystem::Luarocks,
            "julia" => Ecosystem::Julia,
            "conan" => Ecosystem::Conan,
            "vcpkg" => Ecosystem::Vcpkg,
            "cocoapods" => Ecosystem::Cocoapods,
            _ => Ecosystem::Generic,
        };
        let namespace = self.namespace.as_deref();
        let name = match ecosystem {
            Ecosystem::Npm if namespace.is_some() => {
                format!("{}/{}", namespace.unwrap_or_default(), self.name)
            }
            Ecosystem::Maven if namespace.is_some() => format!(
                "{}:{}",
                namespace.unwrap_or_default().replace('/', "."),
                self.name
            ),
            Ecosystem::Generic => match namespace {
                Some(namespace) => format!("{}/{namespace}/{}", self.package_type, self.name),
                None => format!("{}/{}", self.package_type, self.name),
            },
            _ => match namespace {
                Some(namespace) => format!("{namespace}/{}", self.name),
                None => self.name.clone(),
            },
        };
        PackageCoordinate::new(ecosystem, name)
    }
}

impl fmt::Display for PackageUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.canonical)
    }
}

impl FromStr for PackageUrl {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

fn is_purl_type_char(ch: char) -> bool {
    ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '.' | '+' | '-')
}

fn percent_decode(value: &str) -> Result<String, String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return Err(format!("truncated percent escape in {value:?}"));
            }
            let high = hex_value(bytes[index + 1])
                .ok_or_else(|| format!("invalid percent escape in {value:?}"))?;
            let low = hex_value(bytes[index + 2])
                .ok_or_else(|| format!("invalid percent escape in {value:?}"))?;
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).map_err(|_| format!("package URL contains invalid UTF-8: {value:?}"))
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Stable identity for an operation in a vendor API contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiOperationAnchor {
    pub id: String,
    pub vendor: String,
    pub protocol: String,
    pub method: String,
    pub canonical_path: String,
}

impl ApiOperationAnchor {
    pub fn new(vendor: &str, protocol: &str, method: &str, path: &str) -> Self {
        let vendor = vendor.trim().to_ascii_lowercase();
        let protocol = protocol.trim().to_ascii_lowercase();
        let method = method.trim().to_ascii_uppercase();
        let canonical_path = canonical_path(path);
        let identity = format!("{vendor}\0{protocol}\0{method}\0{canonical_path}");
        let digest = blake3::hash(identity.as_bytes()).to_hex();
        let readable = canonical_path
            .trim_matches('/')
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() {
                    ch.to_ascii_lowercase()
                } else {
                    '_'
                }
            })
            .collect::<String>();
        let readable = readable.trim_matches('_');
        let readable = if readable.is_empty() {
            "root"
        } else {
            readable
        };
        let id = format!(
            "api_operation:{}:{}:{}:{}:{}",
            sanitize_component(&vendor),
            sanitize_component(&protocol),
            method.to_ascii_lowercase(),
            readable.chars().take(48).collect::<String>(),
            &digest[..16]
        );
        Self {
            id,
            vendor,
            protocol,
            method,
            canonical_path,
        }
    }
}

fn canonical_path(path: &str) -> String {
    let path = path.trim().split(['?', '#']).next().unwrap_or("");
    let mut out = String::with_capacity(path.len() + 1);
    if !path.starts_with('/') {
        out.push('/');
    }
    let mut prior_slash = false;
    for ch in path.chars() {
        if ch == '/' {
            if !prior_slash {
                out.push('/');
            }
            prior_slash = true;
        } else {
            out.push(ch.to_ascii_lowercase());
            prior_slash = false;
        }
    }
    while out.len() > 1 && out.ends_with('/') {
        out.pop();
    }
    if out.is_empty() { "/".into() } else { out }
}

fn sanitize_component(value: &str) -> String {
    let out = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>();
    out.trim_matches('_').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pypi_names_follow_pep_503_normalization() {
        assert_eq!(
            PackageCoordinate::new(Ecosystem::Pypi, "My_Package.Name").name,
            "my-package-name"
        );
    }

    #[test]
    fn package_coordinate_round_trips_as_a_config_string() {
        let coordinate: PackageCoordinate = "npm:@acme/payments".parse().unwrap();
        let encoded = toml::to_string(&serde_json::json!({ "package": coordinate })).unwrap();
        assert!(encoded.contains("npm:@acme/payments"));
    }

    #[test]
    fn every_applicable_language_ecosystem_round_trips() {
        for ecosystem in [
            "composer",
            "gem",
            "swift",
            "pub",
            "hex",
            "luarocks",
            "julia",
            "zig",
            "conan",
            "vcpkg",
            "cocoapods",
            "powershell",
            "fpm",
            "codeql",
            "salesforce",
            "com",
        ] {
            let coordinate: PackageCoordinate = format!("{ecosystem}:Vendor.SDK")
                .parse()
                .unwrap_or_else(|error| panic!("{ecosystem} must parse: {error}"));
            assert_eq!(coordinate.ecosystem.as_str(), ecosystem);
            assert_eq!(coordinate.to_string(), format!("{ecosystem}:vendor.sdk"));
        }
    }
}
