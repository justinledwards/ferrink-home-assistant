use std::collections::HashMap;
use std::process::Command;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Local, NaiveDate};
use serde::Deserialize;
use serde_json::json;
use slint::{ModelRc, SharedString, Timer, TimerMode, VecModel};

slint::include_modules!();

#[cfg(all(target_arch = "arm", target_os = "linux", target_env = "musl"))]
static LIBERATION_SANS: &[u8] = include_bytes!("../../assets/LiberationSans-Regular.ttf");

const POLL_INTERVAL: Duration = Duration::from_secs(120);
const DEFAULT_SLEEP_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(12);
const LEGACY_BATTERY_CAPACITY_PATH: &str =
    "/sys/devices/system/yoshi_battery/yoshi_battery0/battery_capacity";
const LEGACY_BATTERY_CHARGING_PATH: &str = "/sys/devices/platform/aplite_charger.0/charging";
const POWER_SUPPLY_PATH: &str = "/sys/class/power_supply";
const LIVING_ROOM_FAN: &str = "fan.living_room";

const THERMOSTATS: [(&str, &str); 5] = [
    ("climate.office", "Office"),
    ("climate.guest_room", "Guest"),
    ("climate.living_room", "Living Room"),
    ("climate.bedroom", "Bedroom"),
    ("climate.upstairs", "Upstairs"),
];

const LIGHTS: [(&str, &str); 20] = [
    ("light.office_lamp", "Office Lamp"),
    ("light.backyard_flood", "Backyard Flood"),
    ("light.dining_room", "Dining"),
    ("light.entryway", "Entryway"),
    ("light.garage", "Garage"),
    ("light.front_porch", "Front Porch"),
    ("light.bedroom_hall", "Bedroom Hall"),
    ("light.shower", "Shower"),
    ("light.gallery", "Gallery"),
    ("light.kitchen_island", "Kitchen Island"),
    ("light.kitchen", "Kitchen"),
    ("light.mudroom", "Mud Room"),
    ("light.office", "Office"),
    ("light.laundry", "Laundry"),
    ("light.living_room", "Living Room"),
    ("light.backyard", "Backyard"),
    ("light.garage_outside", "Garage Outside"),
    ("light.living_room_accent", "Living Room Accent"),
    ("light.bathroom_vanity", "Bathroom Vanity"),
    ("light.bedroom_fan_light", "Bedroom Fan"),
];

#[derive(Clone)]
struct HaClient {
    agent: ureq::Agent,
    base_url: String,
    token: String,
}

#[derive(Deserialize)]
struct HaState {
    entity_id: String,
    state: String,
    #[serde(default)]
    attributes: HaAttributes,
}

#[derive(Default, Deserialize)]
struct HaAttributes {
    current_temperature: Option<f64>,
    temperature: Option<f64>,
    hvac_action: Option<String>,
    #[serde(default)]
    hvac_modes: Vec<String>,
    fan_mode: Option<String>,
    #[serde(default)]
    fan_modes: Vec<String>,
}

#[derive(Deserialize)]
struct HaForecastServiceResponse {
    service_response: HashMap<String, HaForecast>,
}

#[derive(Deserialize)]
struct HaForecast {
    #[serde(default)]
    forecast: Vec<HaForecastItem>,
}

#[derive(Deserialize)]
struct HaForecastItem {
    temperature: Option<f64>,
}

#[derive(Deserialize)]
struct HaCalendar {
    entity_id: String,
    name: String,
}

#[derive(Deserialize)]
struct HaCalendarEvent {
    summary: String,
    start: HaCalendarTime,
}

#[derive(Deserialize)]
struct HaCalendarTime {
    #[serde(rename = "dateTime")]
    date_time: Option<String>,
    date: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
struct ClimateState {
    entity_id: String,
    name: String,
    current: Option<f64>,
    target: Option<f64>,
    mode: String,
    action: String,
    available: bool,
    supports_heat: bool,
    supports_cool: bool,
    fan_mode: String,
    fan_modes: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
struct FanState {
    on: bool,
    available: bool,
}

#[derive(Clone, Debug, PartialEq)]
struct LightState {
    entity_id: String,
    name: String,
    on: bool,
    available: bool,
}

#[derive(Clone, Debug, PartialEq)]
struct CalendarEvent {
    summary: String,
    date: String,
    time: String,
    calendar: String,
    sort_epoch: i64,
}

#[derive(Clone, Debug, PartialEq)]
struct BatteryState {
    level: u8,
    kind: &'static str,
}

#[derive(Clone, Debug, PartialEq)]
struct DashboardSnapshot {
    outside_temperature: Option<f64>,
    outside_high: Option<f64>,
    outside_condition: String,
    weather_kind: String,
    thermostats: Vec<ClimateState>,
    living_fan: FanState,
    lights: Vec<LightState>,
    calendar_events: Vec<CalendarEvent>,
}

impl DashboardSnapshot {
    fn loading() -> Self {
        Self {
            outside_temperature: None,
            outside_high: None,
            outside_condition: "Connecting".into(),
            weather_kind: "cloud".into(),
            thermostats: THERMOSTATS
                .iter()
                .map(|(entity_id, name)| ClimateState {
                    entity_id: (*entity_id).into(),
                    name: (*name).into(),
                    current: None,
                    target: None,
                    mode: "unknown".into(),
                    action: "unknown".into(),
                    available: false,
                    supports_heat: false,
                    supports_cool: false,
                    fan_mode: String::new(),
                    fan_modes: Vec::new(),
                })
                .collect(),
            living_fan: FanState {
                on: false,
                available: false,
            },
            lights: LIGHTS
                .iter()
                .map(|(entity_id, name)| LightState {
                    entity_id: (*entity_id).into(),
                    name: (*name).into(),
                    on: false,
                    available: false,
                })
                .collect(),
            calendar_events: Vec::new(),
        }
    }

    fn demo() -> Self {
        let temperatures = [
            (78.0, 66.0, "cool", "Auto"),
            (74.0, 66.0, "cool", "High"),
            (74.0, 66.0, "cool", "High"),
            (73.0, 66.0, "cool", "High"),
            (75.0, 72.0, "cool", ""),
        ];
        Self {
            outside_temperature: Some(77.0),
            outside_high: Some(89.0),
            outside_condition: "partlycloudy".into(),
            weather_kind: "cloud-sun".into(),
            thermostats: THERMOSTATS
                .iter()
                .zip(temperatures)
                .map(
                    |((entity_id, name), (current, target, mode, fan_mode))| ClimateState {
                        entity_id: (*entity_id).into(),
                        name: (*name).into(),
                        current: Some(current),
                        target: Some(target),
                        mode: mode.into(),
                        action: mode.into(),
                        available: true,
                        supports_heat: true,
                        supports_cool: true,
                        fan_mode: fan_mode.into(),
                        fan_modes: if fan_mode.is_empty() {
                            Vec::new()
                        } else {
                            ["Auto", "Low", "Med", "High"]
                                .into_iter()
                                .map(str::to_string)
                                .collect()
                        },
                    },
                )
                .collect(),
            living_fan: FanState {
                on: true,
                available: true,
            },
            lights: LIGHTS
                .iter()
                .enumerate()
                .map(|(index, (entity_id, name))| LightState {
                    entity_id: (*entity_id).into(),
                    name: (*name).into(),
                    on: index % 6 == 0,
                    available: true,
                })
                .collect(),
            calendar_events: vec![CalendarEvent {
                summary: "Dinner with friends".into(),
                date: "SUN JUL 19".into(),
                time: "6:30 PM".into(),
                calendar: "Family".into(),
                sort_epoch: 0,
            }],
        }
    }
}

impl HaClient {
    fn from_environment() -> Result<Self, String> {
        let base_url = std::env::var("HASS_URL")
            .map_err(|_| "HASS_URL is not configured".to_string())?
            .trim_end_matches('/')
            .to_string();
        let token =
            std::env::var("HASS_TOKEN").map_err(|_| "HASS_TOKEN is not configured".to_string())?;
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(REQUEST_TIMEOUT)
            .timeout_read(REQUEST_TIMEOUT)
            .timeout_write(REQUEST_TIMEOUT)
            .build();
        Ok(Self {
            agent,
            base_url,
            token,
        })
    }

    fn get_all_states(&self) -> Result<Vec<HaState>, String> {
        self.agent
            .get(&format!("{}/api/states", self.base_url))
            .set("Authorization", &format!("Bearer {}", self.token))
            .set("Accept", "application/json")
            .call()
            .map_err(|error| format!("states: {error}"))?
            .into_json()
            .map_err(|error| format!("states: invalid response: {error}"))
    }

    fn get_calendars(&self) -> Result<Vec<HaCalendar>, String> {
        self.agent
            .get(&format!("{}/api/calendars", self.base_url))
            .set("Authorization", &format!("Bearer {}", self.token))
            .set("Accept", "application/json")
            .call()
            .map_err(|error| format!("calendars: {error}"))?
            .into_json()
            .map_err(|error| format!("calendars: invalid response: {error}"))
    }

    fn get_calendar_events(
        &self,
        entity_id: &str,
        start: &str,
        end: &str,
    ) -> Result<Vec<HaCalendarEvent>, String> {
        self.agent
            .get(&format!("{}/api/calendars/{entity_id}", self.base_url))
            .query("start", start)
            .query("end", end)
            .set("Authorization", &format!("Bearer {}", self.token))
            .set("Accept", "application/json")
            .call()
            .map_err(|error| format!("{entity_id}: {error}"))?
            .into_json()
            .map_err(|error| format!("{entity_id}: invalid response: {error}"))
    }

    fn fetch_calendar_events(&self) -> Result<Vec<CalendarEvent>, String> {
        let now = Local::now();
        let start = now.to_rfc3339();
        let end = (now + chrono::Duration::days(30)).to_rfc3339();
        let mut upcoming = Vec::new();

        for calendar in self.get_calendars()? {
            let Ok(events) = self.get_calendar_events(&calendar.entity_id, &start, &end) else {
                continue;
            };
            for event in events {
                let Some((sort_epoch, date, time)) = format_calendar_time(&event.start) else {
                    continue;
                };
                upcoming.push(CalendarEvent {
                    summary: event.summary,
                    date,
                    time,
                    calendar: calendar.name.clone(),
                    sort_epoch,
                });
            }
        }
        upcoming.sort_by_key(|event| event.sort_epoch);
        upcoming.truncate(7);
        Ok(upcoming)
    }

    fn call_service(
        &self,
        domain: &str,
        service: &str,
        body: serde_json::Value,
    ) -> Result<(), String> {
        self.agent
            .post(&format!(
                "{}/api/services/{domain}/{service}",
                self.base_url
            ))
            .set("Authorization", &format!("Bearer {}", self.token))
            .set("Accept", "application/json")
            .send_json(body)
            .map_err(|error| format!("{domain}.{service}: {error}"))?;
        Ok(())
    }

    fn get_daily_high(&self, entity_id: &str) -> Result<Option<f64>, String> {
        let response: HaForecastServiceResponse = self
            .agent
            .post(&format!(
                "{}/api/services/weather/get_forecasts?return_response",
                self.base_url
            ))
            .set("Authorization", &format!("Bearer {}", self.token))
            .set("Accept", "application/json")
            .send_json(json!({ "entity_id": entity_id, "type": "daily" }))
            .map_err(|error| format!("weather.get_forecasts: {error}"))?
            .into_json()
            .map_err(|error| format!("weather.get_forecasts: invalid response: {error}"))?;

        Ok(response
            .service_response
            .get(entity_id)
            .and_then(|weather| weather.forecast.first())
            .and_then(|forecast| forecast.temperature))
    }

    fn set_temperature(&self, entity_id: &str, temperature: f64) -> Result<(), String> {
        self.call_service(
            "climate",
            "set_temperature",
            json!({ "entity_id": entity_id, "temperature": temperature }),
        )
    }

    fn set_mode(&self, entity_id: &str, mode: &str) -> Result<(), String> {
        self.call_service(
            "climate",
            "set_hvac_mode",
            json!({ "entity_id": entity_id, "hvac_mode": mode }),
        )
    }

    fn set_fan_mode(&self, entity_id: &str, fan_mode: &str) -> Result<(), String> {
        self.call_service(
            "climate",
            "set_fan_mode",
            json!({ "entity_id": entity_id, "fan_mode": fan_mode }),
        )
    }

    fn set_fan(&self, on: bool) -> Result<(), String> {
        self.call_service(
            "fan",
            if on { "turn_on" } else { "turn_off" },
            json!({ "entity_id": LIVING_ROOM_FAN }),
        )
    }

    fn set_light(&self, entity_id: &str, on: bool) -> Result<(), String> {
        self.call_service(
            "light",
            if on { "turn_on" } else { "turn_off" },
            json!({ "entity_id": entity_id }),
        )
    }

    fn fetch_snapshot(&self) -> Result<DashboardSnapshot, String> {
        let states: HashMap<String, HaState> = self
            .get_all_states()?
            .into_iter()
            .map(|state| (state.entity_id.clone(), state))
            .collect();

        let weather = states
            .get("weather.forecast_home")
            .or_else(|| states.get("weather.kmco"))
            .ok_or_else(|| "weather entity unavailable".to_string())?;
        let is_night = states
            .get("sun.sun")
            .is_some_and(|sun| sun.state == "below_horizon");
        let outside_high = self
            .get_daily_high(&weather.entity_id)
            .unwrap_or_else(|error| {
                eprintln!("Daily forecast refresh failed: {error}");
                None
            });

        let thermostats = THERMOSTATS
            .iter()
            .map(|(entity_id, name)| match states.get(*entity_id) {
                Some(state) if state.state != "unavailable" && state.state != "unknown" => {
                    let action = state
                        .attributes
                        .hvac_action
                        .clone()
                        .unwrap_or_else(|| state.state.clone());
                    ClimateState {
                        entity_id: (*entity_id).into(),
                        name: (*name).into(),
                        current: state.attributes.current_temperature,
                        target: state.attributes.temperature,
                        mode: state.state.clone(),
                        action,
                        available: true,
                        supports_heat: state
                            .attributes
                            .hvac_modes
                            .iter()
                            .any(|mode| mode == "heat"),
                        supports_cool: state
                            .attributes
                            .hvac_modes
                            .iter()
                            .any(|mode| mode == "cool"),
                        fan_mode: state.attributes.fan_mode.clone().unwrap_or_default(),
                        fan_modes: state.attributes.fan_modes.clone(),
                    }
                }
                _ => ClimateState {
                    entity_id: (*entity_id).into(),
                    name: (*name).into(),
                    current: None,
                    target: None,
                    mode: "unavailable".into(),
                    action: "unavailable".into(),
                    available: false,
                    supports_heat: false,
                    supports_cool: false,
                    fan_mode: String::new(),
                    fan_modes: Vec::new(),
                },
            })
            .collect();

        let living_fan = match states.get(LIVING_ROOM_FAN) {
            Some(state) if state.state != "unavailable" && state.state != "unknown" => FanState {
                on: state.state == "on",
                available: true,
            },
            _ => FanState {
                on: false,
                available: false,
            },
        };

        let lights = LIGHTS
            .iter()
            .map(|(entity_id, name)| match states.get(*entity_id) {
                Some(state) if state.state != "unavailable" && state.state != "unknown" => {
                    LightState {
                        entity_id: (*entity_id).into(),
                        name: (*name).into(),
                        on: state.state == "on",
                        available: true,
                    }
                }
                _ => LightState {
                    entity_id: (*entity_id).into(),
                    name: (*name).into(),
                    on: false,
                    available: false,
                },
            })
            .collect();

        let calendar_events = self.fetch_calendar_events().unwrap_or_else(|error| {
            eprintln!("Calendar refresh failed: {error}");
            Vec::new()
        });

        Ok(DashboardSnapshot {
            outside_temperature: weather.attributes.temperature,
            outside_high,
            outside_condition: weather.state.clone(),
            weather_kind: weather_kind(&weather.state, is_night).into(),
            thermostats,
            living_fan,
            lights,
            calendar_events,
        })
    }
}

fn format_calendar_time(value: &HaCalendarTime) -> Option<(i64, String, String)> {
    if let Some(date_time) = value.date_time.as_deref() {
        let parsed = DateTime::parse_from_rfc3339(date_time).ok()?;
        return Some((
            parsed.timestamp(),
            parsed.format("%a %b %-d").to_string().to_uppercase(),
            parsed.format("%-I:%M %p").to_string().to_uppercase(),
        ));
    }
    let date = NaiveDate::parse_from_str(value.date.as_deref()?, "%Y-%m-%d").ok()?;
    let sort_epoch = date.and_hms_opt(0, 0, 0)?.and_utc().timestamp();
    Some((
        sort_epoch,
        date.format("%a %b %-d").to_string().to_uppercase(),
        "ALL DAY".into(),
    ))
}

fn weather_kind(condition: &str, is_night: bool) -> &'static str {
    match condition {
        "sunny" => "sun",
        "clear-night" => "moon",
        "partlycloudy" if is_night => "cloud-moon",
        "partlycloudy" => "cloud-sun",
        "rainy" => "cloud-rain",
        "pouring" | "windy" | "windy-variant" => "cloud-rain-wind",
        "lightning" | "lightning-rainy" => "cloud-lightning",
        "snowy" | "snowy-rainy" | "hail" => "snowflake",
        "fog" => "cloud-fog",
        _ => "cloud",
    }
}

fn pretty_label(value: &str) -> String {
    let value = match value {
        "partlycloudy" => "partly cloudy",
        other => other,
    };
    value
        .split(['_', '-', ' '])
        .filter(|word| !word.is_empty())
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn format_temperature(value: Option<f64>) -> String {
    value
        .map(|temperature| format!("{temperature:.0}°"))
        .unwrap_or_else(|| "--".into())
}

fn thermostat_model(snapshot: &DashboardSnapshot) -> ModelRc<ThermostatData> {
    let rows = snapshot
        .thermostats
        .iter()
        .map(|thermostat| ThermostatData {
            name: SharedString::from(thermostat.name.as_str()),
            current: SharedString::from(format_temperature(thermostat.current)),
            target: SharedString::from(format_temperature(thermostat.target)),
            state: SharedString::from(pretty_label(&thermostat.action).to_uppercase()),
            available: thermostat.available,
        })
        .collect::<Vec<_>>();
    ModelRc::new(VecModel::from(rows))
}

fn light_model(snapshot: &DashboardSnapshot) -> ModelRc<LightData> {
    let rows = snapshot
        .lights
        .iter()
        .map(|light| LightData {
            name: SharedString::from(light.name.as_str()),
            on: light.on,
            available: light.available,
        })
        .collect::<Vec<_>>();
    ModelRc::new(VecModel::from(rows))
}

fn calendar_model(snapshot: &DashboardSnapshot) -> ModelRc<CalendarData> {
    let rows = snapshot
        .calendar_events
        .iter()
        .map(|event| CalendarData {
            date: SharedString::from(event.date.as_str()),
            time: SharedString::from(event.time.as_str()),
            summary: SharedString::from(event.summary.as_str()),
            calendar: SharedString::from(event.calendar.as_str()),
        })
        .collect::<Vec<_>>();
    ModelRc::new(VecModel::from(rows))
}

fn apply_selected(app: &AppWindow, snapshot: &DashboardSnapshot) {
    let index = app.get_selected_index().max(0) as usize;
    let Some(thermostat) = snapshot.thermostats.get(index) else {
        return;
    };
    app.set_selected_name(thermostat.name.as_str().into());
    app.set_selected_target(format_temperature(thermostat.target).into());
    app.set_selected_mode(pretty_label(&thermostat.mode).to_uppercase().into());
    app.set_selected_supports_heat(thermostat.supports_heat);
    app.set_selected_supports_cool(thermostat.supports_cool);
    app.set_selected_supports_fan(!thermostat.fan_modes.is_empty());
    app.set_selected_fan_mode(pretty_label(&thermostat.fan_mode).to_uppercase().into());
}

fn apply_snapshot(app: &AppWindow, snapshot: &DashboardSnapshot, updated_text: Option<String>) {
    app.set_outside_temperature(format_temperature(snapshot.outside_temperature).into());
    app.set_outside_high(
        snapshot
            .outside_high
            .map(|high| format!("HIGH {}", format_temperature(Some(high))))
            .unwrap_or_else(|| "HIGH --".into())
            .into(),
    );
    app.set_outside_condition(
        pretty_label(&snapshot.outside_condition)
            .to_uppercase()
            .into(),
    );
    app.set_weather_kind(snapshot.weather_kind.as_str().into());
    app.set_living_fan_on(snapshot.living_fan.on);
    app.set_living_fan_available(snapshot.living_fan.available);
    if let Some(event) = snapshot.calendar_events.first() {
        app.set_next_event_label(
            format!("NEXT / {} / {}", event.date, event.calendar.to_uppercase()).into(),
        );
        app.set_next_event_title(event.summary.as_str().into());
        app.set_next_event_time(event.time.as_str().into());
    } else {
        app.set_next_event_label("NEXT EVENT".into());
        app.set_next_event_title("NO UPCOMING EVENTS".into());
        app.set_next_event_time("".into());
    }

    app.set_thermostats(thermostat_model(snapshot));
    app.set_lights(light_model(snapshot));
    app.set_calendar_events(calendar_model(snapshot));
    let on_count = snapshot.lights.iter().filter(|light| light.on).count();
    let available = snapshot
        .lights
        .iter()
        .filter(|light| light.available)
        .count();
    app.set_lights_summary(format!("{on_count} ON / {available} AVAILABLE").into());
    apply_selected(app, snapshot);

    if let Some(updated_text) = updated_text {
        app.set_updated_text(updated_text.clone().into());
        app.set_action_status(updated_text.into());
    }
}

fn set_action_status(weak: &slint::Weak<AppWindow>, message: String) {
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(app) = weak.upgrade() {
            app.set_action_status(message.into());
        }
    });
}

fn fetch_once(
    client: &HaClient,
    state: &Arc<Mutex<DashboardSnapshot>>,
    weak: &slint::Weak<AppWindow>,
) {
    match client.fetch_snapshot() {
        Ok(snapshot) => {
            let changed = {
                let mut current = state.lock().expect("dashboard state poisoned");
                if *current == snapshot {
                    false
                } else {
                    *current = snapshot.clone();
                    true
                }
            };
            if changed {
                let weak = weak.clone();
                let updated = format!("UPDATED {}", Local::now().format("%-I:%M %p"));
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(app) = weak.upgrade() {
                        apply_snapshot(&app, &snapshot, Some(updated));
                    }
                });
            }
        }
        Err(error) => {
            eprintln!("Home Assistant refresh failed: {error}");
            set_action_status(weak, "HOME ASSISTANT UNAVAILABLE".into());
        }
    }
}

fn spawn_poll_loop(
    client: HaClient,
    state: Arc<Mutex<DashboardSnapshot>>,
    weak: slint::Weak<AppWindow>,
) {
    std::thread::spawn(move || {
        loop {
            fetch_once(&client, &state, &weak);
            std::thread::sleep(POLL_INTERVAL);
        }
    });
}

fn parse_battery_level(capacity: &str) -> Option<u8> {
    Some(
        capacity
            .trim()
            .trim_end_matches('%')
            .parse::<u8>()
            .ok()?
            .min(100),
    )
}

fn read_power_supply_battery_property(property: &str) -> Option<String> {
    for entry in std::fs::read_dir(POWER_SUPPLY_PATH).ok()?.flatten() {
        let path = entry.path();
        let name_is_battery = entry
            .file_name()
            .to_string_lossy()
            .to_ascii_lowercase()
            .contains("battery");
        let type_is_battery = std::fs::read_to_string(path.join("type"))
            .map(|value| value.trim().eq_ignore_ascii_case("battery"))
            .unwrap_or(false);
        if !name_is_battery && !type_is_battery {
            continue;
        }
        if let Ok(value) = std::fs::read_to_string(path.join(property)) {
            return Some(value.trim().to_owned());
        }
    }
    None
}

fn read_powerd_property(property: &str) -> Option<String> {
    Command::new("lipc-get-prop")
        .args(["com.lab126.powerd", property])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn battery_kind(level: u8, charging: bool, plugged_full: bool) -> &'static str {
    if plugged_full {
        "plug"
    } else if charging {
        "battery-charging"
    } else if level >= 80 {
        "battery-full"
    } else if level >= 40 {
        "battery-medium"
    } else if level >= 15 {
        "battery-low"
    } else {
        "battery"
    }
}

fn read_battery_state() -> Option<BatteryState> {
    let capacity = std::fs::read_to_string(LEGACY_BATTERY_CAPACITY_PATH)
        .ok()
        .or_else(|| read_power_supply_battery_property("capacity"))
        .or_else(|| read_powerd_property("battLevel"))?;
    let level = parse_battery_level(&capacity)?;

    let battery_status = read_power_supply_battery_property("status");
    let powerd_charging = read_powerd_property("isCharging")
        .map(|value| value == "1")
        .unwrap_or(false);
    let legacy_charging = std::fs::read_to_string(LEGACY_BATTERY_CHARGING_PATH)
        .map(|value| value.trim() == "1")
        .unwrap_or(false);
    let status_charging = battery_status
        .as_deref()
        .map(|status| status.eq_ignore_ascii_case("charging"))
        .unwrap_or(false);
    let status_full = battery_status
        .as_deref()
        .map(|status| status.eq_ignore_ascii_case("full"))
        .unwrap_or(false);

    // The PW1 charger sysfs flag goes back to zero after charging completes.
    // Powerd retains a separate full-charge flag, which lets us show the plug
    // state instead of incorrectly claiming that a topped-off device is still
    // charging.
    let powerd_full = read_powerd_property("status")
        .map(|status| status.contains("batt_full=1"))
        .unwrap_or(false);

    let full = status_full || powerd_full;
    let charging = legacy_charging || status_charging || (powerd_charging && !full);
    let kind = battery_kind(level, charging, powerd_charging && full);

    Some(BatteryState { level, kind })
}

fn spawn_battery_loop(weak: slint::Weak<AppWindow>) {
    std::thread::spawn(move || {
        let mut displayed: Option<BatteryState> = None;
        loop {
            if let Some(current) = read_battery_state()
                && displayed.as_ref() != Some(&current)
            {
                let next = current.clone();
                let weak = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(app) = weak.upgrade() {
                        app.set_battery_level(format!("{}%", next.level).into());
                        app.set_battery_kind(next.kind.into());
                    }
                });
                displayed = Some(current);
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    });
}

fn sleep_timeout() -> Duration {
    std::env::var("SLEEP_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_SLEEP_TIMEOUT)
}

fn arm_sleep_timer(timer: &Timer, weak: slint::Weak<AppWindow>, timeout: Duration) {
    timer.start(TimerMode::SingleShot, timeout, move || {
        let Some(app) = weak.upgrade() else { return };
        app.set_sleeping(true);
        app.invoke_enter_sleep();
    });
}

fn main() {
    #[cfg(all(target_arch = "arm", target_os = "linux", target_env = "musl"))]
    let kindle_backend = {
        let backend = slint_backend_kindle::install(LIBERATION_SANS)
            .expect("failed to install Kindle backend");
        backend.set_black_and_white(true);
        Rc::new(backend)
    };

    let app = AppWindow::new().expect("failed to create window");

    #[cfg(all(target_arch = "arm", target_os = "linux", target_env = "musl"))]
    app.on_clean_screen({
        let weak = app.as_weak();
        let kindle_backend = Rc::clone(&kindle_backend);
        move || {
            if let Some(app) = weak.upgrade() {
                app.set_action_status("CLEARING E-INK ARTIFACTS".into());
            }
            kindle_backend.request_full_refresh();
        }
    });

    #[cfg(all(target_arch = "arm", target_os = "linux", target_env = "musl"))]
    app.on_enter_sleep({
        let kindle_backend = Rc::clone(&kindle_backend);
        move || {
            kindle_backend.set_black_and_white(false);
            kindle_backend.request_full_refresh();
        }
    });

    #[cfg(all(target_arch = "arm", target_os = "linux", target_env = "musl"))]
    app.on_wake_from_sleep({
        let kindle_backend = Rc::clone(&kindle_backend);
        move || {
            kindle_backend.set_black_and_white(true);
            kindle_backend.request_full_refresh();
        }
    });

    #[cfg(not(all(target_arch = "arm", target_os = "linux", target_env = "musl")))]
    app.on_clean_screen({
        let weak = app.as_weak();
        move || {
            if let Some(app) = weak.upgrade() {
                app.set_action_status("FULL REFRESH IS KINDLE-ONLY".into());
            }
        }
    });

    app.on_quit(|| std::process::exit(0));
    spawn_battery_loop(app.as_weak());

    let idle_timer = Rc::new(Timer::default());
    let idle_timeout = sleep_timeout();
    app.on_user_activity({
        let idle_timer = Rc::clone(&idle_timer);
        let weak = app.as_weak();
        move || arm_sleep_timer(&idle_timer, weak.clone(), idle_timeout)
    });
    arm_sleep_timer(&idle_timer, app.as_weak(), idle_timeout);

    let state = Arc::new(Mutex::new(DashboardSnapshot::loading()));
    apply_snapshot(&app, &state.lock().expect("dashboard state poisoned"), None);

    app.on_select_room({
        let weak = app.as_weak();
        let state = state.clone();
        move |index| {
            let Some(app) = weak.upgrade() else { return };
            app.set_selected_index(index);
            let snapshot = state.lock().expect("dashboard state poisoned");
            apply_selected(&app, &snapshot);
            app.set_action_status("USE - / + OR SELECT A MODE".into());
        }
    });

    let demo_mode = std::env::var("SLINT_DASHBOARD_DEMO").as_deref() == Ok("1");
    if demo_mode {
        let demo = DashboardSnapshot::demo();
        *state.lock().expect("dashboard state poisoned") = demo.clone();
        apply_snapshot(&app, &demo, Some("MCP DEMO DATA".into()));
    } else {
        match HaClient::from_environment() {
            Ok(client) => {
                app.on_change_target({
                    let weak = app.as_weak();
                    let state = state.clone();
                    let client = client.clone();
                    move |delta| {
                        let Some(app) = weak.upgrade() else { return };
                        let index = app.get_selected_index().max(0) as usize;
                        let (entity_id, target, snapshot) = {
                            let mut snapshot = state.lock().expect("dashboard state poisoned");
                            let Some(thermostat) = snapshot.thermostats.get_mut(index) else {
                                return;
                            };
                            if !thermostat.available {
                                app.set_action_status("THERMOSTAT UNAVAILABLE".into());
                                return;
                            }
                            let target = thermostat.target.or(thermostat.current).unwrap_or(68.0)
                                + f64::from(delta);
                            thermostat.target = Some(target);
                            (thermostat.entity_id.clone(), target, snapshot.clone())
                        };
                        apply_snapshot(&app, &snapshot, None);
                        app.set_action_status("SAVING SET POINT".into());

                        let weak = weak.clone();
                        let client = client.clone();
                        std::thread::spawn(move || {
                            match client.set_temperature(&entity_id, target) {
                                Ok(()) => set_action_status(&weak, "SET POINT SAVED".into()),
                                Err(error) => {
                                    eprintln!("Set temperature failed: {error}");
                                    set_action_status(&weak, "SET POINT FAILED".into());
                                }
                            }
                        });
                    }
                });

                app.on_set_mode({
                    let weak = app.as_weak();
                    let state = state.clone();
                    let client = client.clone();
                    move |mode| {
                        let Some(app) = weak.upgrade() else { return };
                        let index = app.get_selected_index().max(0) as usize;
                        let (entity_id, mode_text, snapshot) = {
                            let mut snapshot = state.lock().expect("dashboard state poisoned");
                            let Some(thermostat) = snapshot.thermostats.get_mut(index) else {
                                return;
                            };
                            if !thermostat.available {
                                app.set_action_status("THERMOSTAT UNAVAILABLE".into());
                                return;
                            }
                            let mode_text = mode.to_string();
                            let supported = mode_text == "off"
                                || (mode_text == "heat" && thermostat.supports_heat)
                                || (mode_text == "cool" && thermostat.supports_cool);
                            if !supported {
                                app.set_action_status("MODE NOT SUPPORTED".into());
                                return;
                            }
                            thermostat.mode = mode_text.clone();
                            thermostat.action = mode_text.clone();
                            (thermostat.entity_id.clone(), mode_text, snapshot.clone())
                        };
                        apply_snapshot(&app, &snapshot, None);
                        app.set_action_status("SAVING MODE".into());

                        let weak = weak.clone();
                        let client = client.clone();
                        std::thread::spawn(move || match client.set_mode(&entity_id, &mode_text) {
                            Ok(()) => set_action_status(&weak, "MODE SAVED".into()),
                            Err(error) => {
                                eprintln!("Set mode failed: {error}");
                                set_action_status(&weak, "MODE FAILED".into());
                            }
                        });
                    }
                });

                app.on_set_fan_mode({
                    let weak = app.as_weak();
                    let state = state.clone();
                    let client = client.clone();
                    move |requested_mode| {
                        let Some(app) = weak.upgrade() else { return };
                        let index = app.get_selected_index().max(0) as usize;
                        let (entity_id, fan_mode, snapshot) = {
                            let mut snapshot = state.lock().expect("dashboard state poisoned");
                            let Some(thermostat) = snapshot.thermostats.get_mut(index) else {
                                return;
                            };
                            if !thermostat.available {
                                app.set_action_status("THERMOSTAT UNAVAILABLE".into());
                                return;
                            }
                            let Some(fan_mode) = thermostat
                                .fan_modes
                                .iter()
                                .find(|mode| mode.eq_ignore_ascii_case(requested_mode.as_str()))
                                .cloned()
                            else {
                                app.set_action_status("FAN SPEED NOT SUPPORTED".into());
                                return;
                            };
                            thermostat.fan_mode = fan_mode.clone();
                            (thermostat.entity_id.clone(), fan_mode, snapshot.clone())
                        };
                        apply_snapshot(&app, &snapshot, None);
                        app.set_action_status("SAVING FAN SPEED".into());

                        let weak = weak.clone();
                        let client = client.clone();
                        std::thread::spawn(move || {
                            match client.set_fan_mode(&entity_id, &fan_mode) {
                                Ok(()) => set_action_status(&weak, "FAN SPEED SAVED".into()),
                                Err(error) => {
                                    eprintln!("Set fan mode failed: {error}");
                                    set_action_status(&weak, "FAN SPEED FAILED".into());
                                }
                            }
                        });
                    }
                });

                app.on_toggle_living_fan({
                    let weak = app.as_weak();
                    let state = state.clone();
                    let client = client.clone();
                    move || {
                        let (turn_on, snapshot) = {
                            let mut snapshot = state.lock().expect("dashboard state poisoned");
                            if !snapshot.living_fan.available {
                                if let Some(app) = weak.upgrade() {
                                    app.set_action_status("LIVING ROOM FAN UNAVAILABLE".into());
                                }
                                return;
                            }
                            snapshot.living_fan.on = !snapshot.living_fan.on;
                            (snapshot.living_fan.on, snapshot.clone())
                        };
                        if let Some(app) = weak.upgrade() {
                            apply_snapshot(&app, &snapshot, None);
                            app.set_action_status("UPDATING LIVING ROOM FAN".into());
                        }

                        let weak = weak.clone();
                        let client = client.clone();
                        std::thread::spawn(move || match client.set_fan(turn_on) {
                            Ok(()) => set_action_status(&weak, "LIVING ROOM FAN UPDATED".into()),
                            Err(error) => {
                                eprintln!("Set living room fan failed: {error}");
                                set_action_status(&weak, "LIVING ROOM FAN FAILED".into());
                            }
                        });
                    }
                });

                app.on_toggle_light({
                    let weak = app.as_weak();
                    let state = state.clone();
                    let client = client.clone();
                    move |index| {
                        let index = index.max(0) as usize;
                        let (entity_id, turn_on, snapshot) = {
                            let mut snapshot = state.lock().expect("dashboard state poisoned");
                            let Some(light) = snapshot.lights.get_mut(index) else {
                                return;
                            };
                            if !light.available {
                                return;
                            }
                            light.on = !light.on;
                            (light.entity_id.clone(), light.on, snapshot.clone())
                        };
                        if let Some(app) = weak.upgrade() {
                            apply_snapshot(&app, &snapshot, None);
                        }

                        let weak = weak.clone();
                        let client = client.clone();
                        std::thread::spawn(move || match client.set_light(&entity_id, turn_on) {
                            Ok(()) => set_action_status(&weak, "LIGHT UPDATED".into()),
                            Err(error) => {
                                eprintln!("Set light failed: {error}");
                                set_action_status(&weak, "LIGHT UPDATE FAILED".into());
                            }
                        });
                    }
                });

                app.on_refresh({
                    let weak = app.as_weak();
                    let state = state.clone();
                    let client = client.clone();
                    move || {
                        let Some(app) = weak.upgrade() else { return };
                        app.set_action_status("REFRESHING".into());
                        let weak = weak.clone();
                        let state = state.clone();
                        let client = client.clone();
                        std::thread::spawn(move || fetch_once(&client, &state, &weak));
                    }
                });

                spawn_poll_loop(client, state, app.as_weak());
            }
            Err(error) => {
                eprintln!("Home Assistant configuration error: {error}");
                app.set_action_status("HOME ASSISTANT CONFIG MISSING".into());
            }
        }
    }

    app.run().expect("event loop error");
}

#[cfg(test)]
mod battery_tests {
    use super::{battery_kind, parse_battery_level};

    #[test]
    fn capacity_is_parsed_and_clamped() {
        assert_eq!(parse_battery_level("87%\n"), Some(87));
        assert_eq!(parse_battery_level("101\n"), Some(100));
    }

    #[test]
    fn a_full_plugged_in_device_uses_the_plug_icon() {
        assert_eq!(battery_kind(100, false, true), "plug");
        assert_eq!(battery_kind(64, true, false), "battery-charging");
    }
}
