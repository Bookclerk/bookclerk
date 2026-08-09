//! Daemon HTTP listen address list (`daemon.listen`).

use std::fmt;
use std::net::SocketAddr;
use std::str::FromStr;

use serde::de::{self, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// One or more bind addresses for the HTTP control plane.
///
/// TOML accepts either a string or an array; we always serialize as an array.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListenAddrs(Vec<String>);

impl Default for ListenAddrs {
    fn default() -> Self {
        Self(vec!["127.0.0.1:8787".into(), "[::1]:8787".into()])
    }
}

impl ListenAddrs {
    #[must_use]
    pub fn new(addrs: Vec<String>) -> Self {
        let cleaned: Vec<String> = addrs
            .into_iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if cleaned.is_empty() {
            Self::default()
        } else {
            Self(cleaned)
        }
    }

    /// Parse a single address or a comma-separated list
    /// (`127.0.0.1:8787,[::1]:8787`).
    pub fn parse_list(raw: &str) -> Result<Self, String> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err("daemon.listen must not be empty".into());
        }
        let parts: Vec<String> = if trimmed.contains(',') {
            trimmed
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToOwned::to_owned)
                .collect()
        } else {
            vec![trimmed.to_string()]
        };
        let addrs = Self::new(parts);
        addrs.validate()?;
        Ok(addrs)
    }

    #[must_use]
    pub fn as_slice(&self) -> &[String] {
        &self.0
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Comma-joined form for settings API / overrides.
    #[must_use]
    pub fn join_comma(&self) -> String {
        self.0.join(",")
    }

    /// Parse each entry as a [`SocketAddr`].
    pub fn socket_addrs(&self) -> Result<Vec<SocketAddr>, String> {
        self.0
            .iter()
            .map(|s| {
                s.parse::<SocketAddr>()
                    .map_err(|err| format!("invalid listen address '{s}': {err}"))
            })
            .collect()
    }

    /// Validate every entry parses as a socket address.
    pub fn validate(&self) -> Result<(), String> {
        if self.0.is_empty() {
            return Err("daemon.listen must not be empty".into());
        }
        self.socket_addrs().map(|_| ())
    }

    /// Port used for tray `http://localhost:<port>` (first entry's port).
    #[must_use]
    pub fn ui_port(&self) -> u16 {
        self.socket_addrs()
            .ok()
            .and_then(|addrs| addrs.first().map(|a| a.port()))
            .unwrap_or(8787)
    }

    /// Whether any configured bind is loopback.
    #[must_use]
    pub fn has_loopback(&self) -> bool {
        self.socket_addrs()
            .map(|addrs| addrs.iter().any(|a| a.ip().is_loopback()))
            .unwrap_or(false)
    }

    /// Prefer `http://localhost:<port>` when any loopback is configured; else
    /// the first concrete bind (bracket IPv6 as needed).
    #[must_use]
    pub fn tray_base_url(&self) -> String {
        let port = self.ui_port();
        if self.has_loopback() {
            return format!("http://localhost:{port}");
        }
        match self.socket_addrs().ok().and_then(|a| a.into_iter().next()) {
            Some(addr) => format!("http://{addr}"),
            None => format!("http://localhost:{port}"),
        }
    }
}

impl fmt::Display for ListenAddrs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.join_comma())
    }
}

impl FromStr for ListenAddrs {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse_list(s)
    }
}

impl From<&str> for ListenAddrs {
    fn from(value: &str) -> Self {
        Self::parse_list(value).unwrap_or_default()
    }
}

impl Serialize for ListenAddrs {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ListenAddrs {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ListenVisitor;

        impl<'de> Visitor<'de> for ListenVisitor {
            type Value = ListenAddrs;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a socket address string or an array of them")
            }

            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                ListenAddrs::parse_list(v).map_err(E::custom)
            }

            fn visit_string<E>(self, v: String) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                self.visit_str(&v)
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut out = Vec::new();
                while let Some(s) = seq.next_element::<String>()? {
                    let t = s.trim();
                    if !t.is_empty() {
                        out.push(t.to_string());
                    }
                }
                let addrs = ListenAddrs::new(out);
                addrs.validate().map_err(de::Error::custom)?;
                Ok(addrs)
            }
        }

        deserializer.deserialize_any(ListenVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_dual_loopback() {
        let d = ListenAddrs::default();
        assert_eq!(d.as_slice(), ["127.0.0.1:8787", "[::1]:8787"]);
        assert_eq!(d.ui_port(), 8787);
        assert!(d.has_loopback());
        assert_eq!(d.tray_base_url(), "http://localhost:8787");
    }

    #[test]
    fn parse_comma_separated() {
        let a = ListenAddrs::parse_list("0.0.0.0:9000,[::]:9000").unwrap();
        assert_eq!(a.as_slice(), ["0.0.0.0:9000", "[::]:9000"]);
        assert!(!a.has_loopback());
        assert_eq!(a.tray_base_url(), "http://0.0.0.0:9000");
    }

    #[test]
    fn toml_string_and_array() {
        #[derive(Deserialize)]
        struct Wrap {
            listen: ListenAddrs,
        }
        let s: Wrap = toml::from_str(r#"listen = "127.0.0.1:1""#).unwrap();
        assert_eq!(s.listen.as_slice(), ["127.0.0.1:1"]);
        let a: Wrap = toml::from_str(r#"listen = ["127.0.0.1:2", "[::1]:2"]"#).unwrap();
        assert_eq!(a.listen.as_slice(), ["127.0.0.1:2", "[::1]:2"]);
    }
}
