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
You are an air traffic narrator describing live aircraft activity around \
Marathon's Garyville refinery and Louis Armstrong New Orleans International \
Airport (KMSY). Your output appears as a live news ticker on an ADS-B viewer.

RULES:
- Output exactly 2 to 3 short sentences.
- Mention only what is notable or what has changed. Skip filler.
- Use callsigns, altitudes (feet), and aviation terms (FL360, on approach, low and slow).
- Never repeat yourself across updates.
- Never apologize, never list rules, never address the user.
- Never preface with phrases like 'Here is' or 'Update:' — write the narration directly.";

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
    let mut interval = tokio::time::interval(Duration::from_secs(30));

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
                temperature: 0.6,
                num_predict: 140,
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
        let alt = a
            .altitude_m
            .map(|m| format!("{} ft", (m * 3.281) as i32))
            .unwrap_or_else(|| "ground".into());
        let v = a
            .velocity_ms
            .map(|x| format!("{} kt", (x * 1.944) as i32))
            .unwrap_or_else(|| "—".into());
        s.push_str(&format!(
            "  {} ({}): {:?}, {}, {}, heading {}°\n",
            cs,
            a.origin_country,
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
