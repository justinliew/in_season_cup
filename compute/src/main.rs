//! Default Compute template program.

use chrono::{DateTime, Datelike, Duration, FixedOffset, TimeZone, Utc};
use fastly::http::{header, Method, StatusCode};
use fastly::kv_store;
use fastly::{mime, Error, Request, Response};
use serde::{Deserialize, Serialize};

#[derive(Deserialize, Debug)]
struct Team {
    abbrev: String,
    score: Option<u32>,
}

#[derive(Deserialize, Debug)]
struct Game {
    #[serde(rename = "gameState")]
    game_state: String,
    #[serde(rename = "awayTeam")]
    away_team: Team,
    #[serde(rename = "homeTeam")]
    home_team: Team,
}

#[derive(Deserialize, Debug)]
struct GameDay {
    date: String,
    games: Vec<Game>,
}

#[derive(Deserialize, Debug)]
struct NHLSchedule {
    #[serde(rename = "gameWeek")]
    game_week: Vec<GameDay>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct TeamCupHistory {
    team: String,
    cup_days: Vec<String>, // List of dates in "YYYY-MM-DD" format
}

#[derive(Serialize, Deserialize, Debug)]
struct CupHistory {
    teams: Vec<TeamCupHistory>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct AuditLogEntry {
    date: String,            // "YYYY-MM-DD"
    starting_owner: String,  // Team that started the day with the cup
    game_result: String,     // Description of what happened (won, lost, didn't play, error)
    ending_owner: String,    // Team that ended the day with the cup
    details: Option<String>, // Additional details (scores, error messages, etc.)
}

#[derive(Serialize, Deserialize, Debug)]
struct AuditLog {
    entries: Vec<AuditLogEntry>,
}

#[derive(Serialize, Deserialize, Debug)]
struct NextGame {
    date: String,              // "YYYY-MM-DD"
    time: String,              // Game time
    opponent: String,          // Opponent team abbreviation
    is_home: bool,            // True if cup holder is home team
    venue: String,            // Venue name
}

fn get_pst() -> FixedOffset {
    // PST is UTC-8
    FixedOffset::west_opt(8 * 3600).unwrap()
}

#[derive(Serialize, Deserialize, Debug)]
struct CupState {
    last_update_year: u32,
    last_update_month: u32,
    last_update_day: u32,
    current_owner: String, // Team abbreviation
}

impl CupState {
    fn default() -> Self {
        CupState {
            last_update_year: 2025,
            last_update_month: 10,
            last_update_day: 6,
            current_owner: "FLA".to_string(), // Default starting team
        }
    }

    fn get_last_update_date(&self) -> DateTime<FixedOffset> {
        let pst = get_pst();
        pst.with_ymd_and_hms(
            self.last_update_year as i32,
            self.last_update_month,
            self.last_update_day,
            0,
            0,
            0,
        )
        .unwrap()
    }
}

fn get_cup_state() -> Result<CupState, Error> {
    let store = kv_store::KVStore::open("in_season_cup")
        .expect("store")
        .unwrap();
    if let Ok(mut resp) = store.lookup("state") {
        let body = resp.take_body();
        let body_str = body.into_string();
        match serde_json::from_str(&body_str) {
            Ok(v) => {
                let res = v;
                return Ok(res);
            }
            Err(e) => {
                return Err(Error::msg(format!(
                    "Couldn't convert {} to json: {:?}",
                    body_str, e
                )));
            }
        }
    } else {
        // For now, return default state - we'll implement KV store later
        Ok(CupState::default())
    }
}

fn set_cup_state(state: &CupState) -> Result<(), Error> {
    let store = kv_store::KVStore::open("in_season_cup")
        .expect("store")
        .unwrap();
    let body = serde_json::to_string(state)
        .map_err(|e| Error::msg(format!("Couldn't convert state to json: {:?}", e)))?;
    store
        .insert("state", body)
        .map_err(|e| Error::msg(format!("Couldn't insert state into KV store: {:?}", e)))?;
    Ok(())
}

fn get_cup_history() -> Result<CupHistory, Error> {
    let store = kv_store::KVStore::open("in_season_cup")
        .expect("store")
        .unwrap();
    if let Ok(mut resp) = store.lookup("cup_history") {
        let body = resp.take_body();
        let body_str = body.into_string();
        match serde_json::from_str(&body_str) {
            Ok(v) => {
                let res = v;
                return Ok(res);
            }
            Err(e) => {
                return Err(Error::msg(format!(
                    "Couldn't convert {} to json: {:?}",
                    body_str, e
                )));
            }
        }
    } else {
        let players_json = get_players()?.into_body_str();
        // For now, initialize from players.json structure - later we'll load from KV store
        let players_data: serde_json::Value = serde_json::from_str(&players_json)
            .map_err(|e| Error::msg(format!("Failed to parse players.json: {:?}", e)))?;

        let mut teams = Vec::new();

        if let Some(players_array) = players_data["players"].as_array() {
            for player in players_array {
                if let Some(team_array) = player["teams"].as_array() {
                    for team in team_array {
                        if let Some(team_str) = team.as_str() {
                            // Check if we already have this team
                            if !teams.iter().any(|t: &TeamCupHistory| t.team == team_str) {
                                teams.push(TeamCupHistory {
                                    team: team_str.to_string(),
                                    cup_days: Vec::new(),
                                });
                            }
                        }
                    }
                }
            }
        }
        Ok(CupHistory { teams })
    }
}

fn set_cup_history(history: &CupHistory) -> Result<(), Error> {
    let store = kv_store::KVStore::open("in_season_cup")
        .expect("store")
        .unwrap();
    let body = serde_json::to_string(history)
        .map_err(|e| Error::msg(format!("Couldn't convert history to json: {:?}", e)))?;
    store
        .insert("cup_history", body)
        .map_err(|e| Error::msg(format!("Couldn't insert history into KV store: {:?}", e)))?;
    Ok(())
}

fn add_cup_day(history: &mut CupHistory, team: &str, date: &str) {
    // Find the team and add the date
    if let Some(team_history) = history.teams.iter_mut().find(|t| t.team == team) {
        // Only add if not already present
        if !team_history.cup_days.contains(&date.to_string()) {
            team_history.cup_days.push(date.to_string());
            team_history.cup_days.sort(); // Keep dates sorted
        }
    }
}

fn get_audit_log() -> Result<AuditLog, Error> {
    let store = kv_store::KVStore::open("in_season_cup")
        .expect("store")
        .unwrap();
    if let Ok(mut resp) = store.lookup("audit_log") {
        let body = resp.take_body();
        let body_str = body.into_string();
        match serde_json::from_str(&body_str) {
            Ok(v) => {
                let res = v;
                return Ok(res);
            }
            Err(e) => {
                return Err(Error::msg(format!(
                    "Couldn't convert {} to json: {:?}",
                    body_str, e
                )));
            }
        }
    } else {
        Ok(AuditLog {
            entries: Vec::new(),
        })
    }
}

fn set_audit_log(log: &AuditLog) -> Result<(), Error> {
    let store = kv_store::KVStore::open("in_season_cup")
        .expect("store")
        .unwrap();
    let body = serde_json::to_string(log)
        .map_err(|e| Error::msg(format!("Couldn't convert audit log to json: {:?}", e)))?;
    store
        .insert("audit_log", body)
        .map_err(|e| Error::msg(format!("Couldn't insert audit log into KV store: {:?}", e)))?;
    Ok(())
}

fn add_audit_entry(log: &mut AuditLog, entry: AuditLogEntry) {
    log.entries.push(entry);
}

fn get_players() -> Result<Response, Error> {
    let players_json = include_str!("players.json");
    Ok(Response::from_status(StatusCode::OK)
        .with_content_type(mime::APPLICATION_JSON)
        .with_header("Access-Control-Allow-Origin", "*")
        .with_body_text_plain(players_json))
}

fn find_next_game_for_cup_holder() -> Result<Option<NextGame>, Error> {
    // Get current cup state to find who has the cup
    let cup_state = get_cup_state()?;
    let cup_holder = cup_state.current_owner;
    
    let pst = get_pst();
    let today = Utc::now().with_timezone(&pst).date_naive();
    
    // Search up to 30 days ahead for the next game
    for days_ahead in 1..=30 {
        let check_date = today + Duration::days(days_ahead);
        let date_str = check_date.format("%Y-%m-%d").to_string();
        
        // Make request to NHL API for this date
        let url = format!("https://api-web.nhle.com/v1/schedule/{}", date_str);
        
        let req = Request::get(&url);
        
        match req.send("nhl_api") {
            Ok(resp) => {
                if resp.get_status() != StatusCode::OK {
                    continue; // Try next date
                }
                
                let body = resp.into_body_str();
                let schedule_data: serde_json::Value = match serde_json::from_str(&body) {
                    Ok(data) => data,
                    Err(_) => continue, // Try next date
                };
                
                // Look for games involving the cup holder
                if let Some(game_week) = schedule_data["gameWeek"].as_array() {
                    for day in game_week {
                        if let Some(games) = day["games"].as_array() {
                            for game in games {
                                let away_team = game["awayTeam"]["abbrev"].as_str().unwrap_or("");
                                let home_team = game["homeTeam"]["abbrev"].as_str().unwrap_or("");
                                
                                if away_team == cup_holder || home_team == cup_holder {
                                    let opponent = if away_team == cup_holder {
                                        home_team.to_string()
                                    } else {
                                        away_team.to_string()
                                    };
                                    
                                    let is_home = home_team == cup_holder;
                                    let start_time = game["startTimeUTC"].as_str().unwrap_or("TBD");
                                    let venue = game["venue"]["default"].as_str().unwrap_or("TBD");
                                    
                                    return Ok(Some(NextGame {
                                        date: date_str,
                                        time: start_time.to_string(),
                                        opponent,
                                        is_home,
                                        venue: venue.to_string(),
                                    }));
                                }
                            }
                        }
                    }
                }
            }
            Err(_) => continue, // Try next date on error
        }
    }
    
    Ok(None) // No game found in the next 30 days
}

fn get_next_game_endpoint() -> Result<Response, Error> {
    match find_next_game_for_cup_holder()? {
        Some(next_game) => {
            let game_json = serde_json::to_string(&next_game)
                .map_err(|e| Error::msg(format!("Failed to serialize next game: {:?}", e)))?;
            
            Ok(Response::from_status(StatusCode::OK)
                .with_content_type(mime::APPLICATION_JSON)
                .with_header("Access-Control-Allow-Origin", "*")
                .with_body_text_plain(&game_json))
        }
        None => {
            // No upcoming game found
            Ok(Response::from_status(StatusCode::NOT_FOUND)
                .with_content_type(mime::APPLICATION_JSON)
                .with_header("Access-Control-Allow-Origin", "*")
                .with_body_text_plain("{\"error\": \"No upcoming game found for cup holder in the next 30 days\"}"))
        }
    }
}

fn get_cup_history_endpoint() -> Result<Response, Error> {
    let cup_history = get_cup_history()?;
    let history_json = serde_json::to_string(&cup_history)
        .map_err(|e| Error::msg(format!("Failed to serialize cup history: {:?}", e)))?;

    Ok(Response::from_status(StatusCode::OK)
        .with_content_type(mime::APPLICATION_JSON)
        .with_header("Access-Control-Allow-Origin", "*")
        .with_body_text_plain(&history_json))
}

fn get_audit_log_endpoint() -> Result<Response, Error> {
    let audit_log = get_audit_log()?;
    let log_json = serde_json::to_string(&audit_log)
        .map_err(|e| Error::msg(format!("Failed to serialize audit log: {:?}", e)))?;

    Ok(Response::from_status(StatusCode::OK)
        .with_content_type(mime::APPLICATION_JSON)
        .with_header("Access-Control-Allow-Origin", "*")
        .with_body_text_plain(&log_json))
}

fn check_team_game_result(
    team_abbrev: &str,
    date: &str,
) -> Result<Option<(bool, Option<String>)>, Error> {
    // Construct NHL API URL
    let url = format!("https://api-web.nhle.com/v1/schedule/{}", date);

    // Make HTTP request
    let req = Request::get(&url);
    let mut resp = req.send("nhl_api")?;

    // Read response body
    let body = resp.take_body_str();

    // Parse JSON
    let schedule: NHLSchedule = serde_json::from_str(&body)
        .map_err(|e| Error::msg(format!("Failed to parse NHL API response: {:?}", e)))?;

    // Find the team's game for this date
    for game_day in schedule.game_week {
        if game_day.date == date {
            for game in game_day.games {
                // Only check completed games
                if game.game_state == "OFF" {
                    // Check if our team played
                    let is_away = game.away_team.abbrev == team_abbrev;
                    let is_home = game.home_team.abbrev == team_abbrev;

                    if is_away || is_home {
                        // Determine if the team won
                        let away_score = game.away_team.score.unwrap_or(0);
                        let home_score = game.home_team.score.unwrap_or(0);

                        let team_won = if is_away {
                            away_score > home_score
                        } else {
                            home_score > away_score
                        };

                        // Get opponent team abbreviation
                        let opponent = if is_away {
                            game.home_team.abbrev.clone()
                        } else {
                            game.away_team.abbrev.clone()
                        };

                        return Ok(Some((team_won, Some(opponent))));
                    }
                }
            }
        }
    }

    // Team didn't play on this date
    Ok(None)
}

fn update_in_season_cup() -> Result<Response, Error> {
    // Get current state, cup history, and audit log
    let mut current_state = get_cup_state()?;
    let mut cup_history = get_cup_history()?;
    let mut audit_log = get_audit_log()?;

    // Get current time in PST (UTC-8), but process up to yesterday since today's games might not be complete
    let today = Utc::now().with_timezone(&get_pst());
    let now = today - Duration::days(1); // Process up to yesterday

    // Get the last update date as timestamp
    let last_update = current_state.get_last_update_date();
    let duration: Duration = now - last_update;
    let days_to_process = duration.num_days() as u32;
    println!(
        "Days to process: {} {} {}",
        days_to_process, last_update, now
    );
    let mut days_processed = 0;
    let mut current_owner = current_state.current_owner.clone();

    // Process each day from last update to today
    for day_offset in 1..=days_to_process {
        let day_date = last_update + Duration::days(day_offset as i64);
        let day_date_str = day_date.format("%Y-%m-%d").to_string();
        let starting_owner = current_owner.clone();

        println!(
            "Processing day {}: {} (owner: {})",
            day_offset, day_date_str, current_owner
        );

        // Add this day to the current owner's cup history
        add_cup_day(&mut cup_history, &current_owner, &day_date_str);

        // Check if the current owner's team played and won their game that day
        let (game_result, details, ending_owner) =
            match check_team_game_result(&current_owner, &day_date_str) {
                Ok(Some((true, Some(opponent)))) => {
                    println!(
                        "  {} won their game against {} - keeps the cup!",
                        current_owner, opponent
                    );
                    (
                        format!("Won game vs {} - kept cup", opponent),
                        None,
                        current_owner.clone(),
                    )
                }
                Ok(Some((true, None))) => {
                    println!("  {} won their game - keeps the cup!", current_owner);
                    (
                        "Won game - kept cup".to_string(),
                        None,
                        current_owner.clone(),
                    )
                }
                Ok(Some((false, Some(opponent)))) => {
                    println!(
                        "  {} lost their game to {} - cup transfers to {}!",
                        current_owner, opponent, opponent
                    );
                    // Team lost, transfer cup to the winning team (opponent)
                    (
                        format!("Lost game to {} - cup transferred", opponent),
                        Some(format!(
                            "Cup transferred from {} to {}",
                            current_owner, opponent
                        )),
                        opponent,
                    )
                }
                Ok(Some((false, None))) => {
                    println!(
                        "  {} lost their game - cup transfers but opponent unknown!",
                        current_owner
                    );
                    (
                        "Lost game - cup should transfer".to_string(),
                        Some("Cup transfer failed: opponent team not found".to_string()),
                        current_owner.clone(),
                    )
                }
                Ok(None) => {
                    println!("  {} didn't play on {}", current_owner, day_date_str);
                    (
                        "Didn't play - kept cup".to_string(),
                        None,
                        current_owner.clone(),
                    )
                }
                Err(e) => {
                    let error_msg = format!("Error checking game result: {:?}", e);
                    println!("  {}", error_msg);
                    (
                        "API Error - kept cup".to_string(),
                        Some(error_msg),
                        current_owner.clone(),
                    )
                }
            };

        // Add audit log entry
        let audit_entry = AuditLogEntry {
            date: day_date_str.clone(),
            starting_owner,
            game_result,
            ending_owner: ending_owner.clone(),
            details,
        };
        add_audit_entry(&mut audit_log, audit_entry);

        // Update current owner (will be the same until we implement transfers)
        current_owner = ending_owner;

        days_processed += 1;
    }

    // Update the state with yesterday's date (since we process up to yesterday)
    current_state.last_update_year = now.year() as u32;
    current_state.last_update_month = now.month();
    current_state.last_update_day = now.day();
    current_state.current_owner = current_owner.clone();

    // Save state, cup history, and audit log
    set_cup_state(&current_state)?;
    set_cup_history(&cup_history)?;
    set_audit_log(&audit_log)?;

    let today_str = now.format("%Y-%m-%d").to_string();
    let response_json = format!(
        r#"{{"status": "success", "message": "In-season cup updated", "days_processed": {}, "current_date": "{}", "current_owner": "{}"}}"#,
        days_processed, today_str, current_state.current_owner
    );

    Ok(Response::from_status(StatusCode::OK)
        .with_content_type(mime::APPLICATION_JSON)
        .with_header("Access-Control-Allow-Origin", "*")
        .with_body_text_plain(&response_json))
}

/// The entry point for your application.
///
/// This function is triggered when your service receives a client request. It could be used to
/// route based on the request properties (such as method or path), send the request to a backend,
/// make completely new requests, and/or generate synthetic responses.
///
/// If `main` returns an error, a 500 error response will be delivered to the client.
#[fastly::main]
fn main(req: Request) -> Result<Response, Error> {
    // Log service version
    println!(
        "FASTLY_SERVICE_VERSION: {}",
        std::env::var("FASTLY_SERVICE_VERSION").unwrap_or_else(|_| String::new())
    );

    // Filter request methods...
    match req.get_method() {
        // Allow GET, HEAD, and POST (for update endpoint)
        &Method::GET | &Method::HEAD | &Method::POST => (),

        // Block requests with unexpected methods
        _ => {
            return Ok(Response::from_status(StatusCode::METHOD_NOT_ALLOWED)
                .with_header(header::ALLOW, "GET, HEAD, POST")
                .with_body_text_plain("This method is not allowed\n"))
        }
    };

    if req.get_method() == Method::OPTIONS {
        return Ok(Response::from_status(StatusCode::OK)
            .with_header("Access-Control-Allow-Origin", "*")
            .with_header("Access-Control-Allow-Headers", "*")
            .with_header("Vary", "Origin")
            .with_body_text_plain(""));
    }

    // Pattern match on the path...
    match req.get_path() {
        // If request is to the `/players` path...
        "/players" => get_players(),

        // If request is to the `/cup-history` path...
        "/cup-history" => get_cup_history_endpoint(),

        // If request is to the `/audit-log` path...
        "/audit-log" => get_audit_log_endpoint(),

        // If request is to the `/next-game` path...
        "/next-game" => get_next_game_endpoint(),

        // If request is to the `/update` path...
        "/update" => match req.get_method() {
            &Method::POST => update_in_season_cup(),
            _ => Ok(Response::from_status(StatusCode::METHOD_NOT_ALLOWED)
                .with_header(header::ALLOW, "POST")
                .with_header("Access-Control-Allow-Origin", "*")
                .with_body_text_plain("Only POST method is allowed for this endpoint\n")),
        },

        // Catch all other requests and return a 404.
        _ => Ok(Response::from_status(StatusCode::NOT_FOUND)
            .with_body_text_plain("The page you requested could not be found\n")),
    }
}
