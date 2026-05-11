//! Behavioral classifier, conflict detection, and acoustic prediction.
//!
//! All three are pure functions of the current aircraft list (and, for the
//! classifier, the per-aircraft history buffer). They run on every fetch.

use crate::types::*;
use std::collections::VecDeque;

// ---------- Geometry helpers ----------

pub const EARTH_R_M: f64 = 6_371_000.0;
pub const NM_TO_M: f64 = 1852.0;
pub const M_TO_FT: f64 = 3.281;
pub const M_PER_DEG_LAT: f64 = 111_111.0;

/// Great-circle distance in meters.
pub fn haversine_m(lat1: f64, lng1: f64, lat2: f64, lng2: f64) -> f64 {
    let r = std::f64::consts::PI / 180.0;
    let dlat = (lat2 - lat1) * r;
    let dlng = (lng2 - lng1) * r;
    let a = (dlat / 2.0).sin().powi(2)
        + (lat1 * r).cos() * (lat2 * r).cos() * (dlng / 2.0).sin().powi(2);
    let c = 2.0 * a.sqrt().asin();
    EARTH_R_M * c
}

/// Project an (lng, lat) forward by `dt_s` seconds at given heading and speed.
pub fn dead_reckon(lng: f64, lat: f64, heading_deg: f64, speed_ms: f64, dt_s: f64) -> (f64, f64) {
    let h = heading_deg.to_radians();
    let m_north = speed_ms * h.cos() * dt_s;
    let m_east = speed_ms * h.sin() * dt_s;
    let d_lat = m_north / M_PER_DEG_LAT;
    let d_lng = m_east / (M_PER_DEG_LAT * lat.to_radians().cos());
    (lng + d_lng, lat + d_lat)
}

// ---------- Behavioral classifier ----------

/// Classify an aircraft based on its recent history.
/// History is oldest-first; the last element is the current observation.
pub fn classify(history: &VecDeque<TrackPoint>) -> Behavior {
    let Some(latest) = history.back() else {
        return Behavior::Enroute;
    };

    if latest.on_ground {
        return if latest.velocity_ms.unwrap_or(0.0) > 1.0 {
            Behavior::Taxiing
        } else {
            Behavior::Enroute // stationary on ground; not visually interesting
        };
    }

    let v = latest.velocity_ms.unwrap_or(0.0);
    let alt = latest.altitude_m.unwrap_or(0.0);

    // Hovering: slow, low, and small spatial drift across recent points.
    // Helicopter on station near a refinery / pipeline / emergency scene.
    // Tight thresholds so commercial jets on final approach (also slow + low,
    // but moving forward) don't trip this.
    if v < 6.0 && alt < 800.0 && history.len() >= 3 && spatial_drift_m(history) < 250.0 {
        return Behavior::Hovering;
    }

    // Holding pattern: latest position is close to one of the older points.
    // Captures racetrack circuits common around busy terminals.
    if history.len() >= 8 && is_holding(history) {
        return Behavior::Holding;
    }

    // Climb / descent rates based on recent altitude change.
    let rate = altitude_rate_mps(history);

    // Approach: descending and below 3000 m (≈10,000 ft).
    if rate < -2.0 && alt < 3000.0 {
        return Behavior::Approach;
    }
    if rate > 4.0 && alt < 6000.0 {
        return Behavior::Climbing;
    }
    if rate < -4.0 {
        return Behavior::Descending;
    }

    // Cruise: above 6 km and reasonably fast.
    if alt > 6000.0 && v > 100.0 {
        return Behavior::Cruise;
    }

    Behavior::Enroute
}

fn spatial_drift_m(history: &VecDeque<TrackPoint>) -> f64 {
    // Max pairwise haversine distance over the last 5 points.
    let recent: Vec<&TrackPoint> = history.iter().rev().take(5).collect();
    let mut max_d = 0.0_f64;
    for i in 0..recent.len() {
        for j in (i + 1)..recent.len() {
            let d = haversine_m(recent[i].lat, recent[i].lng, recent[j].lat, recent[j].lng);
            if d > max_d {
                max_d = d;
            }
        }
    }
    max_d
}

fn altitude_rate_mps(history: &VecDeque<TrackPoint>) -> f64 {
    // Slope between newest and a point ~3 samples back.
    let n = history.len();
    if n < 2 {
        return 0.0;
    }
    let newest = &history[n - 1];
    let older = &history[n.saturating_sub(3)];
    let dt = (newest.time_ms as f64 - older.time_ms as f64) / 1000.0;
    if dt < 0.5 {
        return 0.0;
    }
    let dh = newest.altitude_m.unwrap_or(0.0) - older.altitude_m.unwrap_or(0.0);
    dh / dt
}

fn is_holding(history: &VecDeque<TrackPoint>) -> bool {
    // Latest within 2 km of any point 4+ samples ago.
    let n = history.len();
    if n < 8 {
        return false;
    }
    let newest = &history[n - 1];
    for i in 0..(n - 4) {
        let old = &history[i];
        if haversine_m(newest.lat, newest.lng, old.lat, old.lng) < 2000.0 {
            return true;
        }
    }
    false
}

// ---------- Conflict detection ----------
//
// Standard terminal separation: 3 NM horizontal, 1000 ft vertical.
// We check at t=0 (current) and t=horizon (predicted) so the demo flags
// pairs that are about to come too close, not just pairs already close.

pub const SEPARATION_HORIZ_NM: f64 = 3.0;
pub const SEPARATION_VERT_FT: f64 = 1000.0;

pub fn detect_conflicts(aircraft: &[Aircraft], horizon_s: f64) -> Vec<Conflict> {
    let mut out = Vec::new();
    for &dt in &[0.0, horizon_s] {
        for i in 0..aircraft.len() {
            for j in (i + 1)..aircraft.len() {
                if let Some(c) = check_pair(&aircraft[i], &aircraft[j], dt) {
                    out.push(c);
                }
            }
        }
    }
    // De-dupe by (a, b) keeping the earliest conflict.
    out.sort_by(|a, b| {
        (&a.a_icao, &a.b_icao, a.seconds_from_now as i64).cmp(&(
            &b.a_icao,
            &b.b_icao,
            b.seconds_from_now as i64,
        ))
    });
    out.dedup_by(|a, b| a.a_icao == b.a_icao && a.b_icao == b.b_icao);
    out
}

fn check_pair(a: &Aircraft, b: &Aircraft, dt_s: f64) -> Option<Conflict> {
    if a.on_ground || b.on_ground {
        return None;
    }
    let (a_lng, a_lat) = if dt_s > 0.0 {
        dead_reckon(
            a.longitude,
            a.latitude,
            a.heading.unwrap_or(0.0),
            a.velocity_ms.unwrap_or(0.0),
            dt_s,
        )
    } else {
        (a.longitude, a.latitude)
    };
    let (b_lng, b_lat) = if dt_s > 0.0 {
        dead_reckon(
            b.longitude,
            b.latitude,
            b.heading.unwrap_or(0.0),
            b.velocity_ms.unwrap_or(0.0),
            dt_s,
        )
    } else {
        (b.longitude, b.latitude)
    };

    let dist_m = haversine_m(a_lat, a_lng, b_lat, b_lng);
    let dist_nm = dist_m / NM_TO_M;
    let vert_ft = (a.altitude_m.unwrap_or(0.0) - b.altitude_m.unwrap_or(0.0)).abs() * M_TO_FT;

    if dist_nm < SEPARATION_HORIZ_NM && vert_ft < SEPARATION_VERT_FT {
        Some(Conflict {
            a_icao: a.icao24.clone(),
            b_icao: b.icao24.clone(),
            a_callsign: a.callsign.clone(),
            b_callsign: b.callsign.clone(),
            horizontal_nm: dist_nm,
            vertical_ft: vert_ft,
            seconds_from_now: dt_s,
            at_lng: (a_lng + b_lng) / 2.0,
            at_lat: (a_lat + b_lat) / 2.0,
        })
    } else {
        None
    }
}

// ---------- Acoustic prediction ----------
//
// Per-aircraft: when will it pass closest to the listener (refinery), and will
// it be loud enough to actually hear? Estimate by:
//   1. dead-reckon forward at 1-second steps for the horizon
//   2. find the moment of minimum slant distance (lateral + vertical)
//   3. compute estimated dB at listener using 20·log10 attenuation from a
//      1 km reference, less ambient threshold

pub const ACOUSTIC_HORIZON_S: f64 = 240.0;
const AMBIENT_DB: f64 = 50.0; // industrial-ish background
const AUDIBLE_THRESHOLD_DB: f64 = AMBIENT_DB + 5.0;

fn source_db_at_1km(a: &Aircraft) -> f64 {
    let alt = a.altitude_m.unwrap_or(10_000.0);
    let v = a.velocity_ms.unwrap_or(0.0);
    // Heuristic class detection from speed + altitude + callsign pattern.
    if v < 30.0 && alt < 1500.0 {
        70.0 // helicopter
    } else if a
        .callsign
        .as_ref()
        .is_some_and(|c| c.starts_with('N') && c.len() <= 7)
        && alt < 3000.0
    {
        62.0 // small GA
    } else {
        78.0 // jet
    }
}

pub fn predict_audible(
    aircraft: &[Aircraft],
    listener_lng: f64,
    listener_lat: f64,
    horizon_s: f64,
) -> Vec<AudibleEvent> {
    let mut events = Vec::new();

    for a in aircraft {
        if a.on_ground {
            continue;
        }
        let v = a.velocity_ms.unwrap_or(0.0);
        if v < 1.0 {
            continue;
        }
        let heading = a.heading.unwrap_or(0.0);
        let alt = a.altitude_m.unwrap_or(0.0);

        // Sample slant distance every 5 s for speed.
        let mut best_slant = f64::MAX;
        let mut best_horiz = f64::MAX;
        let mut best_t = 0.0_f64;
        let mut step = 0.0_f64;
        while step <= horizon_s {
            let (lng, lat) = dead_reckon(a.longitude, a.latitude, heading, v, step);
            let horiz_m = haversine_m(lat, lng, listener_lat, listener_lng);
            let slant = (horiz_m * horiz_m + alt * alt).sqrt();
            if slant < best_slant {
                best_slant = slant;
                best_horiz = horiz_m;
                best_t = step;
            }
            step += 5.0;
        }

        // Skip if closest approach is at the very end of the horizon — those
        // aircraft are flying *away* and never get closer, so the projection
        // is unreliable. (Cheap proxy: skip if best_t >= horizon - 5.)
        if best_t >= horizon_s - 5.0 {
            continue;
        }

        let src = source_db_at_1km(a);
        let atten = 20.0 * (best_slant.max(100.0) / 1000.0).log10();
        let estimated_db = src - atten;

        if estimated_db >= AUDIBLE_THRESHOLD_DB {
            events.push(AudibleEvent {
                icao24: a.icao24.clone(),
                callsign: a.callsign.clone(),
                closest_approach_in_s: best_t,
                closest_distance_nm: best_horiz / NM_TO_M,
                closest_slant_m: best_slant,
                estimated_db,
            });
        }
    }

    events.sort_by(|a, b| {
        a.closest_approach_in_s
            .partial_cmp(&b.closest_approach_in_s)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    events
}
