use std::fmt;

use serde::{Deserialize, Serialize};

use crate::FeatureId;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
pub struct ProtocolVersion {
    pub major: u16,
    pub minor: u16,
}

pub const PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion { major: 1, minor: 0 };

impl fmt::Display for ProtocolVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, thiserror::Error)]
#[error("incompatible protocol versions: ours {ours}, theirs {theirs}")]
pub struct IncompatibleVersion {
    pub ours: ProtocolVersion,
    pub theirs: ProtocolVersion,
}

/// Same major is required; the effective minor is the lower of the two.
pub fn check_compatibility(
    ours: ProtocolVersion,
    theirs: ProtocolVersion,
) -> Result<ProtocolVersion, IncompatibleVersion> {
    if ours.major == theirs.major {
        Ok(ProtocolVersion {
            major: ours.major,
            minor: ours.minor.min(theirs.minor),
        })
    } else {
        Err(IncompatibleVersion { ours, theirs })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct FeatureDescriptor {
    pub id: FeatureId,
    pub min_version: u16,
    pub max_version: u16,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct NegotiatedFeature {
    pub id: FeatureId,
    pub version: u16,
}

/// Highest common version of every feature both sides support, sorted by id.
/// Features without an overlapping range are left out.
pub fn negotiate_features(
    ours: &[FeatureDescriptor],
    theirs: &[FeatureDescriptor],
) -> Vec<NegotiatedFeature> {
    let mut out: Vec<NegotiatedFeature> = ours
        .iter()
        .filter_map(|o| {
            let t = theirs.iter().find(|t| t.id == o.id)?;
            let low = o.min_version.max(t.min_version);
            let high = o.max_version.min(t.max_version);
            (low <= high).then_some(NegotiatedFeature {
                id: o.id,
                version: high,
            })
        })
        .collect();
    out.sort_by_key(|f| f.id);
    out.dedup_by_key(|f| f.id);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(major: u16, minor: u16) -> ProtocolVersion {
        ProtocolVersion { major, minor }
    }

    fn fd(id: u16, min: u16, max: u16) -> FeatureDescriptor {
        FeatureDescriptor {
            id: FeatureId(id),
            min_version: min,
            max_version: max,
        }
    }

    #[test]
    fn same_major_is_compatible_with_lower_minor() {
        assert_eq!(check_compatibility(v(1, 3), v(1, 1)), Ok(v(1, 1)));
        assert_eq!(check_compatibility(v(1, 0), v(1, 7)), Ok(v(1, 0)));
    }

    #[test]
    fn different_major_is_rejected_with_both_versions() {
        let err = check_compatibility(v(1, 0), v(2, 0)).unwrap_err();
        assert_eq!(err.ours, v(1, 0));
        assert_eq!(err.theirs, v(2, 0));
        assert_eq!(
            err.to_string(),
            "incompatible protocol versions: ours 1.0, theirs 2.0"
        );
    }

    #[test]
    fn negotiation_picks_highest_common_version_and_drops_disjoint() {
        let ours = [fd(4, 1, 3), fd(1, 1, 2), fd(2, 2, 2), fd(9, 1, 1)];
        let theirs = [fd(1, 1, 5), fd(4, 2, 2), fd(2, 3, 4)];
        let got = negotiate_features(&ours, &theirs);
        assert_eq!(
            got,
            vec![
                NegotiatedFeature {
                    id: FeatureId(1),
                    version: 2
                },
                NegotiatedFeature {
                    id: FeatureId(4),
                    version: 2
                },
            ]
        );
    }

    #[test]
    fn negotiation_is_symmetric() {
        let a = [fd(1, 1, 4), fd(3, 2, 6), fd(5, 1, 1)];
        let b = [fd(3, 1, 3), fd(1, 2, 9), fd(5, 2, 2)];
        assert_eq!(negotiate_features(&a, &b), negotiate_features(&b, &a));
    }
}
