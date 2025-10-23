use std::fmt::{self, Display};
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

/// A validated URL for a node endpoint
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NodeUrl(Arc<str>);

impl NodeUrl {
    /// Creates a new NodeUrl from a string, validating that it's a valid HTTP(S) URL
    pub fn new(url: impl Into<String>) -> Result<Self, String> {
        let url_str = url.into();
        let parsed = url::Url::parse(&url_str).map_err(|e| e.to_string())?;

        if parsed.scheme() != "http" && parsed.scheme() != "https" {
            return Err("URL must use http or https scheme".to_string());
        }

        Ok(Self(Arc::from(url_str.as_str())))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_arc(self) -> Arc<str> {
        self.0
    }

    pub fn arc_clone(&self) -> Arc<str> {
        Arc::clone(&self.0)
    }
}

impl Display for NodeUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl AsRef<str> for NodeUrl {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl From<NodeUrl> for String {
    fn from(url: NodeUrl) -> Self {
        url.0.to_string()
    }
}

/// A unique identifier for a node
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId(NonZeroU32);

impl NodeId {
    /// Creates a new NodeId from a non-zero u32
    pub fn new(id: u32) -> Option<Self> {
        NonZeroU32::new(id).map(Self)
    }

    /// Returns the inner value as a u32
    pub fn get(self) -> u32 {
        self.0.get()
    }
}

impl Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Node#{}", self.0)
    }
}

/// A validated refresh interval
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefreshInterval(Duration);

impl RefreshInterval {
    /// Creates a new RefreshInterval from seconds
    pub fn from_secs(secs: u64) -> Self {
        Self(Duration::from_secs(secs))
    }

    /// Returns the interval as a Duration
    pub fn as_duration(&self) -> Duration {
        self.0
    }

    /// Increases the interval by a factor
    pub fn increase(&mut self) {
        let new_secs = (self.0.as_secs() as f64 * 1.5).min(300.0) as u64;
        self.0 = Duration::from_secs(new_secs);
    }

    /// Decreases the interval by a factor
    pub fn decrease(&mut self) {
        let new_secs = (self.0.as_secs() as f64 / 1.5).max(1.0) as u64;
        self.0 = Duration::from_secs(new_secs);
    }
}

impl Default for RefreshInterval {
    fn default() -> Self {
        Self(Duration::from_secs(5))
    }
}

/// A validated node alias
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeAlias(Arc<str>);

impl NodeAlias {
    /// Creates a new NodeAlias, validating it's not empty
    pub fn new(alias: impl Into<String>) -> Option<Self> {
        let alias = alias.into();
        if alias.trim().is_empty() {
            None
        } else {
            Some(Self(Arc::from(alias.as_str())))
        }
    }

    /// Returns the alias as a string slice
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns a clone of the inner `Arc<str>`
    pub fn arc_clone(&self) -> Arc<str> {
        Arc::clone(&self.0)
    }
}

impl Display for NodeAlias {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl AsRef<str> for NodeAlias {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Marker type indicating recording is active
#[derive(Debug, Clone, Copy)]
pub struct Recording;

/// Marker type indicating recording is inactive
#[derive(Debug, Clone, Copy)]
pub struct NotRecording;
