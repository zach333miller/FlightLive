//! LLM-driven airspace narrator. Every 30 s, builds a structured prompt from
//! the latest snapshot + recent narrations, asks Ollama for a 2-3 sentence
//! commentary, and broadcasts it to all subscribed WebSocket clients.

use crate::opensky::now_ms;
use crate::types::*;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, RwLock};

#[derive(Serialize)]
struct OllamaRequest<'a> {
    model: &'a str,
    prompt: String,
    system: &'a str,
    stream: bool,
    options: OllamaOptions,
}

#[derive(Serialize)]
struct OllamaOptions {
    temperature: f64,
    num_predict: i32,
}

#[derive(Deserialize)]
struct OllamaResponse {
    response: String,
}

const SYSTEM_PROMPT: &str = "\
You are an air traffic narrator for a live ADS-B map of the airspace around \
Marathon's Garyville refinery and Louis Armstrong New Orleans International \
Airport (KMSY).

OUTPUT RULES:
- Exactly 2 to 3 short sentences. No more.
- Use the formatted altitude verbatim from the data (e.g. 'FL360', '8,500 ft'). \
  Never invent or recompute flight levels — copy them exactly as given.
- Use the operator class verbatim. An 'N'-numbered callsign (like N800CU) is a \
  general-aviation aircraft, NOT an airline flight. Never say 'United flight N800CU'.
- Never repeat any sentence from RECENT NARRATIONS. If the airspace hasn't changed, \
  comment on something else — acoustics, traffic density, or a single specific aircraft.
- No prefaces. No 'Update:', 'Currently,', 'Here is'. Just the narration.
- No apologies. No addressing the user.";

pub async fn narrator_task(
    cache: Arc<RwLock<Option<Snapshot>>>,
    tx: broadcast::Sender<Narration>,
    recent: Arc<RwLock<VecDeque<String>>>,
    ollama_url: String,
    model: String,
) {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(45))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("failed to build reqwest client: {e}");
            return;
        }
    };

    // Wait 15 s before first narration so the cache has real history.
    tokio::time::sleep(Duration::from_secs(15)).await;
    let mut interval = tokio::time::interval(Duration::from_secs(45));

    loop {
        interval.tick().await;

        let snap = match cache.read().await.clone() {
            Some(s) => s,
            None => continue,
        };

        let recent_lines: Vec<String> = recent.read().await.iter().cloned().collect();
        let prompt = build_prompt(&snap, &recent_lines);

        let req = OllamaRequest {
            model: &model,
            prompt,
            system: SYSTEM_PROMPT,
            stream: false,
            options: OllamaOptions {
                temperature: 0.7,
                num_predict: 160,
            },
        };

        let url = format!("{ollama_url}/api/generate");
        match client.post(&url).json(&req).send().await {
            Ok(resp) => match resp.json::<OllamaResponse>().await {
                Ok(parsed) => {
                    let text = clean_output(&parsed.response);
                    if text.is_empty() {
                        continue;
                    }
                    // Drop the generation if it's too similar to the most-recent
                    // narration (bag-of-words overlap > 60%). The model
                    // sometimes ignores the "don't repeat" rule.
                    if is_too_similar(&text, recent_lines.last()) {
                        tracing::info!("narration suppressed (too similar to previous)");
                        continue;
                    }
                    {
                        let mut r = recent.write().await;
                        r.push_back(text.clone());
                        while r.len() > RECENT_NARRATIONS_MAX {
                            r.pop_front();
                        }
                    }
                    let narration = Narration {
                        at_ms: now_ms(),
                        text,
                        aircraft_count: snap.aircraft.len(),
                    };
                    tracing::info!("narration: {}", narration.text);
                    let _ = tx.send(narration);
                }
                Err(e) => tracing::warn!("ollama parse failed: {e}"),
            },
            Err(e) => tracing::warn!("ollama request failed: {e}"),
        }
    }
}

/// Format an aircraft's altitude the way ATC actually writes it.
/// FL{n} above 18,000 ft (the transition altitude in the US), else "{n} ft".
fn format_altitude(a: &Aircraft) -> String {
    if a.on_ground {
        return "ground".to_string();
    }
    let Some(m) = a.altitude_m else {
        return "unknown".to_string();
    };
    let ft = (m * 3.281) as i32;
    if ft >= 18_000 {
        format!("FL{}", ft / 100)
    } else if ft <= 0 {
        "near surface".to_string()
    } else {
        format!("{} ft", ft)
    }
}

/// Classify the operator from the callsign so the LLM doesn't guess.
/// Returns a human-friendly label for use in the prompt.
fn classify_operator(callsign: Option<&str>) -> &'static str {
    let Some(cs) = callsign else {
        return "Unknown";
    };

    // N-numbered (FAA US registry) = general aviation / private.
    // Pattern: starts with N, length 2-7, rest is alphanumeric.
    if let Some(rest) = cs.strip_prefix('N') {
        if !rest.is_empty()
            && rest.len() <= 6
            && rest.starts_with(|c: char| c.is_ascii_digit())
            && rest.chars().all(|c| c.is_ascii_alphanumeric())
        {
            return "GA (private)";
        }
    }

    // Common 3-letter airline / operator ICAO prefixes — extend as needed.
    let airline_prefixes: &[(&str, &str)] = &[
        ("UAL", "United Airlines"),
        ("AAL", "American Airlines"),
        ("DAL", "Delta Air Lines"),
        ("SWA", "Southwest Airlines"),
        ("JBU", "JetBlue"),
        ("ASA", "Alaska Airlines"),
        ("FFT", "Frontier Airlines"),
        ("NKS", "Spirit Airlines"),
        ("VOI", "Volaris"),
        ("AMX", "Aeromexico"),
        ("ACA", "Air Canada"),
        ("UCA", "Air Canada Jazz"),
        ("FDX", "FedEx"),
        ("UPS", "UPS"),
        ("EJA", "NetJets"),
        ("LXJ", "Flexjet"),
        ("WJA", "WestJet"),
        ("EDV", "Endeavor"),
        ("ENY", "Envoy / American Eagle"),
        ("RPA", "Republic / American Eagle"),
        ("GJS", "GoJet / United Express"),
        ("SKW", "SkyWest"),
        ("PDT", "Piedmont / American Eagle"),
    ];
    for (prefix, name) in airline_prefixes {
        if cs.starts_with(prefix) {
            return name;
        }
    }
    "Commercial / charter"
}

/// Crude bag-of-words similarity to suppress repeated narrations.
fn is_too_similar(new: &str, prev: Option<&String>) -> bool {
    let Some(prev) = prev else {
        return false;
    };
    use std::collections::HashSet;
    let normalize = |s: &str| -> HashSet<String> {
        s.split_whitespace()
            .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase())
            .filter(|w| w.len() > 3)
            .collect()
    };
    let new_words = normalize(new);
    let prev_words = normalize(prev);
    if new_words.is_empty() || prev_words.is_empty() {
        return false;
    }
    let inter = new_words.intersection(&prev_words).count();
    let overlap = inter as f64 / new_words.len() as f64;
    overlap > 0.6
}

fn clean_output(s: &str) -> String {
    // Strip common LLM preface patterns.
    let mut t = s.trim().to_string();
    for prefix in [
        "Update:",
        "Here is",
        "Here's",
        "Currently,",
        "At the moment,",
    ] {
        if let Some(stripped) = t.strip_prefix(prefix) {
            t = stripped.trim().to_string();
        }
    }
    // Trim quotes that some models like to wrap output in.
    t = t.trim_matches(|c| c == '"' || c == '\'').to_string();
    t
}

fn build_prompt(snap: &Snapshot, recent: &[String]) -> String {
    let mut s = String::with_capacity(1024);

    // Aircraft summary
    s.push_str(&format!(
        "AIRSPACE STATE — {} aircraft tracked.\n\n",
        snap.aircraft.len()
    ));

    let mut by_behavior: HashMap<&Behavior, usize> = HashMap::new();
    for a in &snap.aircraft {
        *by_behavior.entry(&a.behavior).or_insert(0) += 1;
    }
    if !by_behavior.is_empty() {
        s.push_str("BEHAVIOR BREAKDOWN:\n");
        for (b, count) in by_behavior {
            s.push_str(&format!("  {} × {:?}\n", count, b));
        }
        s.push('\n');
    }

    // Top 8 aircraft for detail
    s.push_str("AIRCRAFT (notable):\n");
    let mut sorted: Vec<&Aircraft> = snap.aircraft.iter().collect();
    sorted.sort_by(|a, b| {
        let pri = |x: &Aircraft| match x.behavior {
            Behavior::Hovering | Behavior::Holding => 0,
            Behavior::Approach => 1,
            Behavior::Climbing | Behavior::Descending => 2,
            Behavior::Cruise => 4,
            Behavior::Taxiing => 5,
            Behavior::Enroute => 3,
        };
        pri(a).cmp(&pri(b))
    });
    for a in sorted.iter().take(8) {
        let cs = a.callsign.clone().unwrap_or_else(|| a.icao24.clone());
        let alt = format_altitude(a); // pre-formatted: FL360 or "8,500 ft"
        let op = classify_operator(a.callsign.as_deref());
        let v = a
            .velocity_ms
            .map(|x| format!("{} kt", (x * 1.944) as i32))
            .unwrap_or_else(|| "—".into());
        s.push_str(&format!(
            "  {} [{}] [{:?}]: {}, {}, heading {}°\n",
            cs,
            op,
            a.behavior,
            alt,
            v,
            a.heading.unwrap_or(0.0) as i32
        ));
    }
    s.push('\n');

    // Conflicts
    if !snap.conflicts.is_empty() {
        s.push_str(&format!("CONFLICTS: {} pair(s) inside 3 NM / 1000 ft.\n", snap.conflicts.len()));
        for c in snap.conflicts.iter().take(3) {
            let a = c.a_callsign.clone().unwrap_or_else(|| c.a_icao.clone());
            let b = c.b_callsign.clone().unwrap_or_else(|| c.b_icao.clone());
            s.push_str(&format!(
                "  {} ↔ {}: {:.1} NM, {:.0} ft, in {:.0} s\n",
                a, b, c.horizontal_nm, c.vertical_ft, c.seconds_from_now
            ));
        }
        s.push('\n');
    }

    // Audibility events
    if !snap.audible.is_empty() {
        s.push_str(&format!(
            "AUDIBLE AT REFINERY: {} aircraft within 4 min.\n",
            snap.audible.len()
        ));
        for ev in snap.audible.iter().take(3) {
            let cs = ev.callsign.clone().unwrap_or_else(|| ev.icao24.clone());
            s.push_str(&format!(
                "  {}: closest in {} s at {:.1} NM ({:.0} dB)\n",
                cs, ev.closest_approach_in_s as i32, ev.closest_distance_nm, ev.estimated_db
            ));
        }
        s.push('\n');
    }

    if !recent.is_empty() {
        s.push_str("RECENT NARRATIONS (do NOT repeat these):\n");
        for r in recent {
            s.push_str(&format!("  - \"{}\"\n", r));
        }
        s.push('\n');
    }

    s.push_str("Write 2-3 short sentences narrating the most notable activity right now.");
    s
}
