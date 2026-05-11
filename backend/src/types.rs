//! Shared data types used across modules and serialized to the wire.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TrackPoint {
    pub time_ms: u64,
    pub lng: f64,
    pub lat: f64,
    pub altitude_m: Option<f64>,
    pub velocity_ms: Option<f64>,
    pub heading: Option<f64>,
    pub on_ground: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
#[serde(rename_all = "UPPERCASE")]
pub enum Behavior {
    Cruise,
    Approach,
    Holding,
    Hovering,
    Climbing,
    Descending,
    Taxiing,
    Enroute,
}

#[derive(Serialize, Debug, Clone)]
pub struct Aircraft {
    pub icao24: String,
    pub callsign: Option<String>,
    pub origin_country: String,
    pub longitude: f64,
    pub latitude: f64,
    pub altitude_m: Option<f64>,
    pub velocity_ms: Option<f64>,
    pub heading: Option<f64>,
    pub on_ground: bool,
    pub behavior: Behavior,
    /// `[lng, lat]` history points, oldest first; capped at HISTORY_MAX.
    pub trail: Vec<[f64; 2]>,
}

#[derive(Serialize, Debug, Clone)]
pub struct Conflict {
    pub a_icao: String,
    pub b_icao: String,
    pub a_callsign: Option<String>,
    pub b_callsign: Option<String>,
    pub horizontal_nm: f64,
    pub vertical_ft: f64,
    pub seconds_from_now: f64,
    /// Midpoint of the two aircraft at the conflict moment, for visualization.
    pub at_lng: f64,
    pub at_lat: f64,
}

#[derive(Serialize, Debug, Clone)]
pub struct AudibleEvent {
    pub icao24: String,
    pub callsign: Option<String>,
    pub closest_approach_in_s: f64,
    pub closest_distance_nm: f64,
    pub closest_slant_m: f64,
    pub estimated_db: f64,
}

#[derive(Serialize, Debug, Clone)]
pub struct Snapshot {
    pub time: i64,
    pub fetched_at_ms: u128,
    pub aircraft: Vec<Aircraft>,
    pub conflicts: Vec<Conflict>,
    pub audible: Vec<AudibleEvent>,
    /// `[lng, lat]` of the listener used for audibility (refinery).
    pub listener: [f64; 2],
}

#[derive(Serialize, Clone, Debug)]
pub struct Narration {
    pub at_ms: u128,
    pub text: String,
    pub aircraft_count: usize,
}

pub type HistoryMap = HashMap<String, VecDeque<TrackPoint>>;

pub const HISTORY_MAX: usize = 36; // last ~6 min at 10 s intervals
pub const RECENT_NARRATIONS_MAX: usize = 5;

/// Marathon Garyville refinery — used as the acoustic listener and the
/// "where Zach flies drones from" reference point.
pub const LISTENER_LNG: f64 = -90.628;
pub const LISTENER_LAT: f64 = 30.063;
