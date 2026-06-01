use serde::Deserialize;
use std::fmt;
use std::str::FromStr;

/// Strava app slot. Athletes are pinned to a slot to determine which
/// `client_id`/`client_secret` pair is used for OAuth code exchange and
/// token refresh. See `docs/superpowers/specs/2026-06-01-multi-strava-clients-design.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Slot {
    One = 1,
    Two = 2,
}

impl TryFrom<i64> for Slot {
    type Error = anyhow::Error;
    fn try_from(n: i64) -> Result<Self, Self::Error> {
        match n {
            1 => Ok(Self::One),
            2 => Ok(Self::Two),
            other => Err(anyhow::anyhow!("invalid slot value: {}", other)),
        }
    }
}

/// Known Strava activity types with associated emoji, noun, and verb forms.
///
/// `#[serde(other)]` on the `Other` variant means any unknown activity type
/// from the Strava API will deserialize as `ActivityType::Other` rather than
/// producing an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum ActivityType {
    Run,
    TrailRun,
    VirtualRun,
    Ride,
    VirtualRide,
    Hike,
    Walk,
    Swim,
    #[serde(other)]
    Other,
}

impl ActivityType {
    #[must_use]
    pub fn emoji(self) -> &'static str {
        match self {
            Self::Run | Self::TrailRun | Self::VirtualRun => "🏃",
            Self::Hike => "🥾",
            Self::Walk => "🚶",
            Self::Ride | Self::VirtualRide => "🚴",
            _ => "🏅",
        }
    }

    #[must_use]
    pub fn noun(self) -> &'static str {
        match self {
            Self::Run | Self::TrailRun | Self::VirtualRun => "run",
            Self::Ride | Self::VirtualRide => "ride",
            Self::Hike => "hike",
            Self::Walk => "walk",
            Self::Swim => "swim",
            Self::Other => "activity",
        }
    }

    #[must_use]
    pub fn verb_past(self) -> &'static str {
        match self {
            Self::Run | Self::TrailRun | Self::VirtualRun => "ran",
            Self::Hike => "hiked",
            Self::Walk => "walked",
            Self::Ride | Self::VirtualRide => "rode",
            Self::Swim => "swam",
            Self::Other => "logged",
        }
    }

    #[must_use]
    pub fn has_pace(self) -> bool {
        matches!(
            self,
            Self::Run
                | Self::TrailRun
                | Self::VirtualRun
                | Self::Ride
                | Self::VirtualRide
                | Self::Walk
        )
    }
}

impl fmt::Display for ActivityType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Run => "Run",
            Self::TrailRun => "TrailRun",
            Self::VirtualRun => "VirtualRun",
            Self::Ride => "Ride",
            Self::VirtualRide => "VirtualRide",
            Self::Hike => "Hike",
            Self::Walk => "Walk",
            Self::Swim => "Swim",
            Self::Other => "Other",
        })
    }
}

impl FromStr for ActivityType {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "Run" => Self::Run,
            "TrailRun" => Self::TrailRun,
            "VirtualRun" => Self::VirtualRun,
            "Ride" => Self::Ride,
            "VirtualRide" => Self::VirtualRide,
            "Hike" => Self::Hike,
            "Walk" => Self::Walk,
            "Swim" => Self::Swim,
            _ => Self::Other,
        })
    }
}

#[cfg(test)]
mod slot_tests {
    use super::*;

    #[test]
    fn slot_try_from_valid() {
        assert_eq!(Slot::try_from(1_i64).unwrap(), Slot::One);
        assert_eq!(Slot::try_from(2_i64).unwrap(), Slot::Two);
    }

    #[test]
    fn slot_try_from_invalid() {
        assert!(Slot::try_from(0_i64).is_err());
        assert!(Slot::try_from(3_i64).is_err());
        assert!(Slot::try_from(-1_i64).is_err());
    }

    #[test]
    fn slot_as_db_int() {
        assert_eq!(Slot::One as i64, 1);
        assert_eq!(Slot::Two as i64, 2);
    }
}
