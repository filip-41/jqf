//! Markup names: namespace URI plus a nonempty local name.
//!
//! [`ExpandedName`] is the attribute identity. Format-specific validity stays with the decoder.

use alloc::string::String;
use core::fmt;
use jqf_resource::ResourceError;

use crate::identity::try_copy_str;

/// Markup attribute name: namespace URI plus a nonempty local name.
///
/// Format-specific validity stays with the decoder.
#[derive(Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ExpandedName {
    namespace_uri: String,
    local_name: String,
}

impl ExpandedName {
    /// Copy a namespace URI and nonempty local name. Does not apply format-specific name rules.
    pub fn try_new(namespace_uri: &str, local_name: &str) -> Result<Self, ExpandedNameError> {
        if local_name.is_empty() {
            return Err(ExpandedNameError::EmptyLocalName);
        }
        Ok(Self {
            namespace_uri: try_copy_str(namespace_uri).map_err(|_| ExpandedNameError::Allocation)?,
            local_name: try_copy_str(local_name).map_err(|_| ExpandedNameError::Allocation)?,
        })
    }

    /// Copies this exact expanded name into owned storage.
    pub fn try_clone_accounted(&self) -> Result<Self, ResourceError> {
        Ok(Self {
            namespace_uri: try_copy_str(self.namespace_uri())?,
            local_name: try_copy_str(self.local_name())?,
        })
    }

    /// Returns the exact namespace URI, including the empty no-namespace URI.
    #[must_use]
    pub fn namespace_uri(&self) -> &str {
        self.namespace_uri.as_str()
    }

    /// Returns the nonempty local name.
    #[must_use]
    pub fn local_name(&self) -> &str {
        self.local_name.as_str()
    }
}

/// Expanded-name construction failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExpandedNameError {
    /// The local name was empty.
    EmptyLocalName,
    /// Storage allocation failed.
    Allocation,
}

impl fmt::Display for ExpandedNameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::EmptyLocalName => "expanded-name local name must not be empty",
            Self::Allocation => "expanded-name allocation failed",
        })
    }
}

impl core::error::Error for ExpandedNameError {}
