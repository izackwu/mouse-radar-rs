use serde::Deserialize;
use std::fmt;
use std::str::FromStr;

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
