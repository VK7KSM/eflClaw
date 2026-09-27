//! Public data feeds for the local weather / traffic / emergency jobs, and
//! their parsers. Every parser is a pure function over the decoded JSON so it
//! can be tested against captured payloads; fetching lives in the callers.
//!
//! Sources (all measured 2026-09-27):
//! - BOM, the Bureau of Meteorology — the JSON API the official BOM app uses.
//!   Undocumented, so parsing is lenient; hourly entries carry BOM's own gust
//!   forecast, which is what the antenna thresholds are checked against.
//! - Open-Meteo — free, no key. Used only for hail (WMO codes 96/99), which
//!   BOM's hourly icons do not distinguish from a plain storm.
//! - LiveTraffic NSW — the public site's hazard feed (incidents with category,
//!   road, suburb, coordinates).
//! - NSW RFS major incidents — GeoJSON with an alert level per fire.
//! - TomTom Calculate Route — travel time with live traffic. Needs a key; 2,500
//!   free requests a day, over that the request is refused rather than billed.

use chrono::{DateTime, NaiveDateTime, Utc};
use serde_json::Value;

pub const BOM_API: &str = "https://api.weather.bom.gov.au/v1/locations";
pub const LIVETRAFFIC_INCIDENTS: &str = "https://www.livetraffic.com/traffic/hazards/incident.json";
/// The official TfNSW Open Data endpoint for the same data (same GeoJSON
/// shape, measured 2026-09-27). Needs a free API token, sent as
/// `Authorization: apikey <token>`.
pub const TFNSW_INCIDENTS: &str = "https://api.transport.nsw.gov.au/v1/live/hazards/incident/open";
pub const RFS_MAJOR_INCIDENTS: &str = "https://www.rfs.nsw.gov.au/feeds/majorIncidents.json";
pub const HOLIDAYS_API: &str = "https://date.nager.at/api/v3/PublicHolidays";

pub fn bom_url(geohash: &str, what: &str) -> String {
    format!("{BOM_API}/{geohash}/{what}")
}

pub fn open_meteo_url(lat: f64, lon: f64, tz: &str) -> String {
    format!(
        "https://api.open-meteo.com/v1/forecast?latitude={lat:.4}&longitude={lon:.4}\
         &hourly=weather_code,wind_gusts_10m&timezone={}&forecast_days=2",
        tz.replace('/', "%2F")
    )
}

/// Great-circle distance in kilometres.
pub fn km(a: (f64, f64), b: (f64, f64)) -> f64 {
    let (lat1, lon1) = (a.0.to_radians(), a.1.to_radians());
    let (lat2, lon2) = (b.0.to_radians(), b.1.to_radians());
    let h = ((lat2 - lat1) / 2.0).sin().powi(2)
        + lat1.cos() * lat2.cos() * ((lon2 - lon1) / 2.0).sin().powi(2);
    2.0 * 6371.0 * h.sqrt().asin()
}

fn f64_at(v: &Value, path: &[&str]) -> Option<f64> {
    let mut cur = v;
    for k in path {
        cur = cur.get(*k)?;
    }
    cur.as_f64()
}

fn str_at<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn utc(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

// ── BOM ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Default)]
pub struct DayForecast {
    pub icon: String,
    pub temp_min: Option<f64>,
    pub temp_max: Option<f64>,
    pub rain_chance: Option<f64>,
    pub rain_min: Option<f64>,
    pub rain_max: Option<f64>,
    pub uv_max: Option<f64>,
}

/// `forecasts/daily` → today's entry (the first one).
pub fn parse_bom_daily(v: &Value) -> Option<DayForecast> {
    let d = v.get("data")?.as_array()?.first()?;
    Some(DayForecast {
        icon: str_at(d, "icon_descriptor").to_string(),
        temp_min: d.get("temp_min").and_then(Value::as_f64),
        temp_max: d.get("temp_max").and_then(Value::as_f64),
        rain_chance: f64_at(d, &["rain", "chance"]),
        rain_min: f64_at(d, &["rain", "amount", "min"]),
        rain_max: f64_at(d, &["rain", "amount", "max"]),
        uv_max: f64_at(d, &["uv", "max_index"]),
    })
}

#[derive(Debug, Clone, PartialEq)]
pub struct HourForecast {
    pub time: DateTime<Utc>,
    pub icon: String,
    pub gust_kmh: Option<f64>,
    pub rain_chance: Option<f64>,
    pub rain_max: Option<f64>,
}

/// `forecasts/hourly` → one entry per hour.
pub fn parse_bom_hourly(v: &Value) -> Vec<HourForecast> {
    v.get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|h| {
            Some(HourForecast {
                time: utc(str_at(h, "time"))?,
                icon: str_at(h, "icon_descriptor").to_string(),
                gust_kmh: f64_at(h, &["wind", "gust_speed_kilometre"]),
                rain_chance: f64_at(h, &["rain", "chance"]),
                rain_max: f64_at(h, &["rain", "amount", "max"]),
            })
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq)]
pub struct Warning {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub phase: String,
    pub expiry: Option<DateTime<Utc>>,
}

/// `warnings` → the warnings that cover the location.
pub fn parse_bom_warnings(v: &Value) -> Vec<Warning> {
    v.get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|w| {
            let id = str_at(w, "id");
            (!id.is_empty()).then(|| Warning {
                id: id.to_string(),
                kind: str_at(w, "type").to_string(),
                title: str_at(w, "title").to_string(),
                phase: str_at(w, "phase").to_string(),
                expiry: utc(str_at(w, "expiry_time")),
            })
        })
        .collect()
}

/// BOM's icon descriptors, in Chinese. Unknown values pass through unchanged.
pub fn icon_zh(icon: &str) -> String {
    match icon {
        "sunny" | "clear" => "晴",
        "mostly_sunny" => "大致晴",
        "partly_cloudy" => "晴间多云",
        "cloudy" => "多云",
        "hazy" => "有霾",
        "fog" => "有雾",
        "light_shower" => "小阵雨",
        "shower" => "阵雨",
        "heavy_shower" => "强阵雨",
        "light_rain" => "小雨",
        "rain" => "有雨",
        "storm" => "雷雨",
        "windy" => "大风",
        "dusty" => "沙尘",
        "frost" => "有霜",
        "snow" => "有雪",
        "cyclone" => "气旋",
        other => other,
    }
    .to_string()
}

// ── Open-Meteo ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub struct MeteoHour {
    /// Local time (the request asks for the reader's timezone).
    pub time: NaiveDateTime,
    pub code: u32,
    pub gust_kmh: Option<f64>,
}

impl MeteoHour {
    /// WMO 96 / 99: thunderstorm with hail.
    pub fn hail(&self) -> bool {
        matches!(self.code, 96 | 99)
    }
    /// WMO 95 / 96 / 99: any thunderstorm.
    pub fn storm(&self) -> bool {
        matches!(self.code, 95 | 96 | 99)
    }
}

pub fn parse_open_meteo(v: &Value) -> Vec<MeteoHour> {
    let Some(h) = v.get("hourly") else {
        return Vec::new();
    };
    let arr = |k: &str| {
        h.get(k)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    let (times, codes, gusts) = (arr("time"), arr("weather_code"), arr("wind_gusts_10m"));
    times
        .iter()
        .enumerate()
        .filter_map(|(i, t)| {
            Some(MeteoHour {
                time: NaiveDateTime::parse_from_str(t.as_str()?, "%Y-%m-%dT%H:%M").ok()?,
                code: codes
                    .get(i)
                    .and_then(Value::as_u64)
                    .and_then(|c| u32::try_from(c).ok())
                    .unwrap_or(0),
                gust_kmh: gusts.get(i).and_then(Value::as_f64),
            })
        })
        .collect()
}

// ── LiveTraffic ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub struct Incident {
    pub id: String,
    pub category: String,
    pub is_major: bool,
    pub headline: String,
    pub street: String,
    pub cross_street: String,
    pub suburb: String,
    pub lanes: usize,
    pub lat: f64,
    pub lon: f64,
}

pub fn parse_incidents(v: &Value) -> Vec<Incident> {
    v.get("features")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|f| {
            let p = f.get("properties")?;
            if p.get("ended").and_then(Value::as_bool) == Some(true) {
                return None;
            }
            let c = f.get("geometry")?.get("coordinates")?.as_array()?;
            let road = p
                .get("roads")
                .and_then(Value::as_array)
                .and_then(|r| r.first())
                .cloned()
                .unwrap_or(Value::Null);
            let id = match f.get("id") {
                Some(Value::String(s)) => s.clone(),
                Some(n) if !n.is_null() => n.to_string(),
                _ => return None,
            };
            Some(Incident {
                id,
                category: str_at(p, "mainCategory").trim().to_string(),
                is_major: p.get("isMajor").and_then(Value::as_bool).unwrap_or(false),
                headline: str_at(p, "headline").trim().to_string(),
                street: str_at(&road, "mainStreet").trim().to_string(),
                cross_street: str_at(&road, "crossStreet").trim().to_string(),
                suburb: str_at(&road, "suburb").trim().to_string(),
                lanes: road
                    .get("impactedLanes")
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len),
                lat: c.get(1)?.as_f64()?,
                lon: c.first()?.as_f64()?,
            })
        })
        .collect()
}

/// LiveTraffic's category names, in Chinese.
pub fn incident_zh(category: &str) -> String {
    match category.to_ascii_uppercase().as_str() {
        "CRASH" => "车祸",
        "BREAKDOWN" => "故障车",
        "HAZARD" => "路面危险",
        "FIRE" => "火情",
        "FLOODING" | "FLOOD" => "积水",
        "TRAFFIC LIGHTS BLACKED OUT" => "红绿灯停电",
        "TRAFFIC LIGHTS FLASHING YELLOW" => "红绿灯故障",
        "EMERGENCY ROADWORK" => "抢修施工",
        "CHANGED TRAFFIC CONDITIONS" => "交通管制",
        "ROAD CLOSED" | "ROAD CLOSURE" => "封路",
        _ => "路况事件",
    }
    .to_string()
}

// ── RFS ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub struct Fire {
    pub guid: String,
    pub title: String,
    /// "Advice", "Watch and Act" or "Emergency Warning".
    pub level: String,
    pub location: String,
    pub status: String,
    pub lat: f64,
    pub lon: f64,
}

/// A field from RFS's `description` ("ALERT LEVEL: Advice <br />LOCATION: …").
fn rfs_field(description: &str, name: &str) -> String {
    description
        .split("<br />")
        .find_map(|part| {
            let (k, v) = part.split_once(':')?;
            (k.trim().eq_ignore_ascii_case(name)).then(|| v.trim().to_string())
        })
        .unwrap_or_default()
}

pub fn parse_rfs(v: &Value) -> Vec<Fire> {
    v.get("features")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|f| {
            let p = f.get("properties")?;
            let g = f.get("geometry")?;
            // Point, or a GeometryCollection whose first member is the point.
            let point = if str_at(g, "type") == "Point" {
                g
            } else {
                g.get("geometries")?
                    .as_array()?
                    .iter()
                    .find(|x| str_at(x, "type") == "Point")?
            };
            let c = point.get("coordinates")?.as_array()?;
            let description = str_at(p, "description");
            Some(Fire {
                guid: str_at(p, "guid").to_string(),
                title: str_at(p, "title").to_string(),
                level: str_at(p, "category").to_string(),
                location: rfs_field(description, "LOCATION"),
                status: rfs_field(description, "STATUS"),
                lat: c.get(1)?.as_f64()?,
                lon: c.first()?.as_f64()?,
            })
        })
        .collect()
}

// ── TomTom ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub struct RouteEta {
    pub seconds: i64,
    /// Extra time caused by traffic right now.
    pub delay_seconds: i64,
    pub length_m: i64,
}

/// Calculate Route for `points` (origin, forced waypoints, destination),
/// departing at `depart` (RFC 3339 with offset).
pub fn tomtom_url(key: &str, points: &[(f64, f64)], depart: &str) -> String {
    let locations = points
        .iter()
        .map(|(lat, lon)| format!("{lat:.6},{lon:.6}"))
        .collect::<Vec<_>>()
        .join(":");
    format!(
        "https://api.tomtom.com/routing/1/calculateRoute/{locations}/json\
         ?key={key}&traffic=true&travelMode=car&routeType=fastest&departAt={}",
        depart.replace('+', "%2B").replace(':', "%3A")
    )
}

pub fn parse_tomtom(v: &Value) -> Option<RouteEta> {
    let s = v.get("routes")?.as_array()?.first()?.get("summary")?;
    Some(RouteEta {
        seconds: s.get("travelTimeInSeconds")?.as_i64()?,
        delay_seconds: s
            .get("trafficDelayInSeconds")
            .and_then(Value::as_i64)
            .unwrap_or(0),
        length_m: s.get("lengthInMeters").and_then(Value::as_i64).unwrap_or(0),
    })
}

// ── public holidays ──────────────────────────────────────────────────────

/// Nager.Date holidays → the dates that apply in NSW (national ones have no
/// `counties`).
pub fn parse_nsw_holidays(v: &Value) -> Vec<chrono::NaiveDate> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter(|h| match h.get("counties").and_then(Value::as_array) {
            None => true,
            Some(c) => c.iter().any(|x| x.as_str() == Some("AU-NSW")),
        })
        .filter_map(|h| chrono::NaiveDate::parse_from_str(str_at(h, "date"), "%Y-%m-%d").ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn km_matches_known_distances() {
        // Two fixture points about 5.6 km apart in a straight line.
        let d = km((-33.8497, 150.9447), (-33.8245, 151.0014));
        assert!((5.0..6.2).contains(&d), "{d}");
        assert!(km((-33.8, 150.963), (-33.8, 150.963)) < 1e-9);
    }

    #[test]
    fn bom_daily_takes_today_and_tolerates_missing_fields() {
        let v = json!({"data": [{
            "icon_descriptor": "shower", "temp_max": 17, "temp_min": null,
            "rain": {"chance": 95, "amount": {"min": 4, "max": 6}},
            "uv": {"max_index": 7}
        }, {"icon_descriptor": "sunny"}]});
        let d = parse_bom_daily(&v).unwrap();
        assert_eq!(d.icon, "shower");
        assert_eq!((d.temp_min, d.temp_max), (None, Some(17.0)));
        assert_eq!(
            (d.rain_chance, d.rain_min, d.rain_max),
            (Some(95.0), Some(4.0), Some(6.0))
        );
        assert_eq!(d.uv_max, Some(7.0));
        assert!(parse_bom_daily(&json!({})).is_none());
    }

    #[test]
    fn bom_hourly_reads_the_official_gust_forecast() {
        let v = json!({"data": [
            {"time": "2026-09-27T21:00:00Z", "icon_descriptor": "storm",
             "wind": {"gust_speed_kilometre": 83}, "rain": {"chance": 80, "amount": {"max": 5}}},
            {"time": "not a time"}
        ]});
        let h = parse_bom_hourly(&v);
        assert_eq!(h.len(), 1);
        assert_eq!((h[0].icon.as_str(), h[0].gust_kmh), ("storm", Some(83.0)));
    }

    #[test]
    fn bom_warnings_keep_id_type_and_phase() {
        let v = json!({"data": [{"id": "NSW_TS001", "type": "severe_thunderstorm_warning",
            "title": "Severe Thunderstorm Warning for damaging winds", "phase": "new",
            "expiry_time": "2026-09-27T14:00:00Z"}, {"type": "no id"}]});
        let w = parse_bom_warnings(&v);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].kind, "severe_thunderstorm_warning");
        assert!(w[0].expiry.is_some());
    }

    #[test]
    fn open_meteo_flags_storms_and_hail() {
        let v = json!({"hourly": {
            "time": ["2026-09-28T15:00", "2026-09-28T16:00", "2026-09-28T17:00"],
            "weather_code": [3, 95, 96],
            "wind_gusts_10m": [30.0, 55.0, 72.0]}});
        let h = parse_open_meteo(&v);
        assert_eq!(h.len(), 3);
        assert!(!h[0].storm() && h[1].storm() && !h[1].hail() && h[2].hail());
        assert_eq!(h[2].gust_kmh, Some(72.0));
    }

    #[test]
    fn incidents_skip_ended_ones_and_read_the_first_road() {
        let v = json!({"features": [
            {"id": 95130, "geometry": {"coordinates": [150.963, -33.84]},
             "properties": {"mainCategory": "CRASH", "isMajor": false, "headline": "CRASH 2 cars",
                            "ended": false, "roads": [{"mainStreet": "Alpha Road",
                            "crossStreet": "Beta Street", "suburb": "Testville", "impactedLanes": [{}]}]}},
            {"id": 1, "geometry": {"coordinates": [150.963, -33.8]},
             "properties": {"mainCategory": "CRASH", "ended": true}}
        ]});
        let i = parse_incidents(&v);
        assert_eq!(i.len(), 1);
        assert_eq!(i[0].id, "95130");
        assert_eq!(
            (i[0].street.as_str(), i[0].suburb.as_str(), i[0].lanes),
            ("Alpha Road", "Testville", 1)
        );
        assert_eq!((i[0].lat, i[0].lon), (-33.84, 150.963));
        assert_eq!(incident_zh("CRASH"), "车祸");
    }

    #[test]
    fn rfs_reads_the_point_out_of_a_geometry_collection() {
        let v = json!({"features": [{
            "geometry": {"type": "GeometryCollection", "geometries": [
                {"type": "Point", "coordinates": [150.913, -33.87]},
                {"type": "Polygon", "coordinates": []}]},
            "properties": {"guid": "g1", "title": "Bardwell Valley", "category": "Watch and Act",
                "description": "ALERT LEVEL: Watch and Act <br />LOCATION: Wolli Creek Reserve <br />STATUS: Out of control"}
        }]});
        let f = parse_rfs(&v);
        assert_eq!(f[0].level, "Watch and Act");
        assert_eq!(f[0].location, "Wolli Creek Reserve");
        assert_eq!(f[0].status, "Out of control");
        assert_eq!((f[0].lat, f[0].lon), (-33.87, 150.913));
    }

    #[test]
    fn tomtom_url_escapes_the_departure_time_and_parse_reads_the_summary() {
        let url = tomtom_url(
            "KEY",
            &[(-33.8497, 150.9447), (-33.8245, 151.0014)],
            "2026-09-28T08:25:00+10:00",
        );
        assert!(url.contains("-33.849700,150.944700:-33.824500,151.001400"));
        assert!(
            url.contains("departAt=2026-09-28T08%3A25%3A00%2B10%3A00"),
            "{url}"
        );
        let v = json!({"routes": [{"summary": {"travelTimeInSeconds": 720,
            "trafficDelayInSeconds": 120, "lengthInMeters": 6900}}]});
        assert_eq!(
            parse_tomtom(&v),
            Some(RouteEta {
                seconds: 720,
                delay_seconds: 120,
                length_m: 6900
            })
        );
        assert_eq!(parse_tomtom(&json!({"routes": []})), None);
    }

    #[test]
    fn nsw_holidays_include_national_and_nsw_only() {
        let v = json!([
            {"date": "2026-10-05", "localName": "Labour Day", "counties": ["AU-NSW", "AU-ACT"]},
            {"date": "2026-12-25", "localName": "Christmas Day", "counties": null},
            {"date": "2026-03-09", "localName": "Labour Day", "counties": ["AU-VIC"]}
        ]);
        let h = parse_nsw_holidays(&v);
        assert_eq!(h.len(), 2);
        assert!(!h.iter().any(|d| d.to_string() == "2026-03-09"));
    }
}
