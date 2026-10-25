//! What can be ordered where: Hetzner's locations and server
//! types boiled down to "these locations, each with these types at these
//! prices", plus the pick we recommend for a main server. Pure functions, so
//! the wizard, `server options` and the tests share them.

use modula_hetzner::{Location, ServerType, price_cents};
use serde::Serialize;

/// Smaller than this can't run the dashboard, Postgres and Redis.
pub const MIN_MEMORY_GB: f64 = 2.0;
/// Below this the dashboard runs, with little room for apps.
pub const COMFORTABLE_MEMORY_GB: f64 = 4.0;
/// The recommended class (cx33 / cax21): room for the dashboard, Postgres
/// and a few apps.
pub const RECOMMENDED_MEMORY_GB: f64 = 8.0;
pub const RECOMMENDED_CORES: u32 = 4;

pub const RECOMMENDATION: &str = "the dashboard, its Postgres and a few apps";

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LocationOption {
    /// Hetzner's name, e.g. `fsn1` (what `--location` takes).
    pub name: String,
    pub city: String,
    /// ISO 3166 code, e.g. `DE`.
    pub country: String,
    pub network_zone: String,
    /// Orderable here now, cheapest first.
    pub types: Vec<TypeOption>,
    /// The type we suggest here (`--type`).
    pub recommended: Option<String>,
}

impl LocationOption {
    /// `🇩🇪 Falkenstein (fsn1)`
    pub fn label(&self) -> String {
        format!("{} {} ({})", flag(&self.country), self.city, self.name)
    }

    pub fn find_type(&self, name: &str) -> Option<&TypeOption> {
        self.types
            .iter()
            .find(|t| t.name.eq_ignore_ascii_case(name))
    }

    /// The cheapest price here, for the location list.
    pub fn from_cents(&self) -> Option<i64> {
        self.cheapest().map(|t| t.monthly_cents)
    }

    /// The cheapest type here (types are sorted cheapest first).
    pub fn cheapest(&self) -> Option<&TypeOption> {
        self.types.first()
    }

    /// `€6.49+  2 vCPU  4 GB RAM  40 GB disk arm`: the cheapest server here.
    pub fn offer(&self) -> String {
        let Some(t) = self.cheapest() else {
            return String::new();
        };
        format!(
            "{:>7}+  {} vCPU {:>2} GB RAM {:>3} GB disk {}",
            t.monthly,
            t.cores,
            format_gb(t.memory_gb),
            t.disk_gb,
            if t.arch == "arm64" { "arm" } else { "x86" }
        )
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TypeOption {
    pub name: String,
    pub cores: u32,
    pub memory_gb: f64,
    pub disk_gb: u32,
    /// `x86` or `arm64`.
    pub arch: String,
    /// `shared` or `dedicated`.
    pub cpu_type: String,
    /// Gross (VAT included) euro cents per month in this location.
    pub monthly_cents: i64,
    /// `€8.21`.
    pub monthly: String,
    pub recommended: bool,
}

impl TypeOption {
    /// `cx33   €8.21  4 vCPU  8 GB  80 GB disk x86  ★ recommended`
    pub fn label(&self) -> String {
        let mut text = format!(
            "{:<6} {:>7}  {:>2} vCPU {:>3} GB {:>4} GB disk {:<3}",
            self.name,
            self.monthly,
            self.cores,
            format_gb(self.memory_gb),
            self.disk_gb,
            if self.arch == "arm64" { "arm" } else { "x86" }
        );
        if self.cpu_type == "dedicated" {
            text.push_str(" dedicated");
        }
        if self.recommended {
            text.push_str("  ★ recommended");
        } else if self.memory_gb < COMFORTABLE_MEMORY_GB {
            text.push_str("  (tight)");
        }
        text
    }

    /// `cx33 (4 vCPU, 8 GB RAM, 80 GB disk, x86)`
    pub fn summary(&self) -> String {
        format!(
            "{} ({} vCPU, {} GB RAM, {} GB disk, {})",
            self.name,
            self.cores,
            format_gb(self.memory_gb),
            self.disk_gb,
            self.arch
        )
    }
}

/// `4` for 4.0, `0.5` for 0.5.
fn format_gb(gb: f64) -> String {
    if gb.fract() == 0.0 {
        format!("{gb:.0}")
    } else {
        format!("{gb}")
    }
}

/// `"de"` → 🇩🇪 (regional indicator symbols); anything else → 🌐.
pub fn flag(country: &str) -> String {
    let c = country.trim().to_ascii_uppercase();
    if c.len() != 2 || !c.bytes().all(|b| b.is_ascii_uppercase()) {
        return "🌐".into();
    }
    c.chars()
        .filter_map(|ch| char::from_u32(0x1F1E6 + (ch as u32 - 'A' as u32)))
        .collect()
}

/// 821 → `€8.21`.
pub fn format_eur(cents: i64) -> String {
    format!("€{}.{:02}", cents / 100, cents % 100)
}

/// Whether a type suits a main server at all (enough memory, still sold).
fn suitable(t: &ServerType) -> bool {
    !t.is_deprecated() && t.memory >= MIN_MEMORY_GB
}

/// Locations that can create at least one suitable type right now, each
/// with those types (cheapest first) and our recommendation.
///
/// A type is offered in a location when it is priced there and neither
/// retired nor sold out (`server_types[].locations[].available`).
pub fn build(locations: &[Location], types: &[ServerType]) -> Vec<LocationOption> {
    let mut out: Vec<LocationOption> = locations
        .iter()
        .filter_map(|loc| {
            let mut offered: Vec<TypeOption> = types
                .iter()
                .filter(|t| suitable(t) && t.orderable_in(&loc.name))
                .filter_map(|t| {
                    let cents = price_cents(&t.monthly_price_in(&loc.name)?.gross)?;
                    Some(TypeOption {
                        name: t.name.clone(),
                        cores: t.cores,
                        memory_gb: t.memory,
                        disk_gb: t.disk,
                        arch: if t.architecture == "arm" {
                            "arm64".into()
                        } else {
                            t.architecture.clone()
                        },
                        cpu_type: t.cpu_type.clone(),
                        monthly_cents: cents,
                        monthly: format_eur(cents),
                        recommended: false,
                    })
                })
                .collect();
            if offered.is_empty() {
                return None;
            }
            offered.sort_by(|a, b| {
                (a.monthly_cents, a.cores, &a.name).cmp(&(b.monthly_cents, b.cores, &b.name))
            });
            let recommended = recommend(&offered).map(|i| {
                offered[i].recommended = true;
                offered[i].name.clone()
            });
            Some(LocationOption {
                name: loc.name.clone(),
                city: if loc.city.is_empty() {
                    loc.description.clone()
                } else {
                    loc.city.clone()
                },
                country: loc.country.clone(),
                network_zone: loc.network_zone.clone(),
                types: offered,
                recommended,
            })
        })
        .collect();
    // Cheapest first, then by name.
    out.sort_by(|a, b| (a.from_cents(), &a.name).cmp(&(b.from_cents(), &b.name)));
    out
}

/// The cheapest shared-vCPU type of the cx33 / cax21 class (4 vCPU, 8 GB),
/// else the cheapest with at least 4 GB, else the cheapest. `types` is
/// sorted cheapest first.
pub fn recommend(types: &[TypeOption]) -> Option<usize> {
    let shared = |t: &&TypeOption| t.cpu_type != "dedicated";
    let pick = |pred: &dyn Fn(&TypeOption) -> bool| {
        types
            .iter()
            .enumerate()
            .filter(|(_, t)| shared(t) && pred(t))
            .min_by_key(|(_, t)| t.monthly_cents)
            .map(|(i, _)| i)
    };
    pick(&|t| t.memory_gb >= RECOMMENDED_MEMORY_GB && t.cores >= RECOMMENDED_CORES)
        .or_else(|| pick(&|t| t.memory_gb >= COMFORTABLE_MEMORY_GB))
        .or_else(|| (!types.is_empty()).then_some(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn types() -> Vec<ServerType> {
        let at = |loc: &str, available: bool| json!({"name": loc, "available": available});
        let price = |loc: &str, gross: &str| {
            json!({"location": loc, "price_hourly": {"net": "0", "gross": "0"},
                   "price_monthly": {"net": "1", "gross": gross},
                   "included_traffic": if loc == "ash" { 1_099_511_627_776_u64 } else { 21_990_232_555_520 }})
        };
        serde_json::from_value(json!([
            {"id": 1, "name": "cx23", "cores": 2, "memory": 4.0, "disk": 40, "cpu_type": "shared", "architecture": "x86",
             "prices": [price("fsn1", "4.5101"), price("hel1", "4.5101"), price("ash", "5.99")],
             // Sold out in fsn1; not listed in ash.
             "locations": [at("fsn1", false), at("hel1", true), at("sin", true)]},
            {"id": 2, "name": "cx33", "cores": 4, "memory": 8.0, "disk": 80, "cpu_type": "shared", "architecture": "x86",
             "prices": [price("fsn1", "7.4900"), price("hel1", "7.4900")],
             // Sold out in hel1.
             "locations": [at("fsn1", true), at("hel1", false), at("sin", true)]},
            {"id": 3, "name": "cax21", "cores": 4, "memory": 8.0, "disk": 80, "cpu_type": "shared", "architecture": "arm",
             "prices": [price("fsn1", "7.2500")]},
            {"id": 4, "name": "ccx13", "cores": 2, "memory": 8.0, "disk": 80, "cpu_type": "dedicated", "architecture": "x86",
             "prices": [price("fsn1", "14.86")]},
            {"id": 5, "name": "cx11", "cores": 1, "memory": 2.0, "disk": 20, "cpu_type": "shared", "architecture": "x86",
             "deprecation": {"announced": "2024-06-06T00:00:00Z"}, "prices": [price("fsn1", "1.00")]},
            {"id": 6, "name": "tiny", "cores": 1, "memory": 1.0, "disk": 10, "cpu_type": "shared", "architecture": "x86",
             "prices": [price("fsn1", "0.50")]},
            {"id": 7, "name": "cpx11", "cores": 2, "memory": 2.0, "disk": 40, "cpu_type": "shared", "architecture": "x86",
             "prices": [price("ash", "4.99")]}
        ]))
        .unwrap()
    }

    fn locations() -> Vec<Location> {
        serde_json::from_value(json!([
            {"name": "fsn1", "city": "Falkenstein", "country": "DE", "network_zone": "eu-central"},
            {"name": "hel1", "city": "Helsinki", "country": "FI", "network_zone": "eu-central"},
            {"name": "ash", "city": "Ashburn, VA", "country": "US", "network_zone": "us-east"},
            {"name": "sin", "city": "Singapore", "country": "SG", "network_zone": "ap-southeast"}
        ]))
        .unwrap()
    }

    #[test]
    fn only_available_suitable_types_per_location() {
        let c = build(&locations(), &types());
        let names: Vec<&str> = c.iter().map(|l| l.name.as_str()).collect();
        // No suitable type is sold in sin; cheapest first.
        assert_eq!(names, ["hel1", "ash", "fsn1"]);
        let at = |name: &str| c.iter().find(|l| l.name == name).unwrap();

        let fsn1 = at("fsn1");
        let types: Vec<&str> = fsn1.types.iter().map(|t| t.name.as_str()).collect();
        // cx23 is sold out, cx11 deprecated, tiny too small; cheapest first.
        assert_eq!(types, ["cax21", "cx33", "ccx13"]);
        assert_eq!(fsn1.types[0].arch, "arm64");
        assert_eq!(fsn1.types[0].monthly, "€7.25");
        assert_eq!(fsn1.recommended.as_deref(), Some("cax21"));
        assert_eq!(fsn1.offer(), "  €7.25+  4 vCPU  8 GB RAM  80 GB disk arm");

        let hel1 = at("hel1");
        assert_eq!(hel1.types.len(), 1);
        assert_eq!(hel1.types[0].name, "cx23");
        // No 8 GB type there: the cheapest with 4 GB.
        assert_eq!(hel1.recommended.as_deref(), Some("cx23"));
        // Prices round up to the cent.
        assert_eq!(hel1.types[0].monthly_cents, 452);

        // Only a 2 GB type: still offered, flagged as tight, recommended as the only one.
        let ash = at("ash");
        assert_eq!(ash.recommended.as_deref(), Some("cpx11"));
        assert!(ash.label().starts_with("🇺🇸 Ashburn, VA (ash)"));
    }

    #[test]
    fn without_location_data_prices_decide() {
        let mut t = types();
        t.iter_mut().for_each(|t| t.locations.clear());
        let c = build(&locations(), &t);
        let fsn1 = c.iter().find(|l| l.name == "fsn1").unwrap();
        assert!(fsn1.find_type("CX23").is_some());
        assert_eq!(fsn1.recommended.as_deref(), Some("cax21"));
    }

    #[test]
    fn recommendation_prefers_shared_8gb_then_4gb() {
        let t = |name: &str, cores, mem, cents, cpu: &str| TypeOption {
            name: name.into(),
            cores,
            memory_gb: mem,
            disk_gb: 40,
            arch: "x86".into(),
            cpu_type: cpu.into(),
            monthly_cents: cents,
            monthly: format_eur(cents),
            recommended: false,
        };
        let list = vec![
            t("small", 2, 4.0, 400, "shared"),
            t("big", 4, 8.0, 800, "shared"),
            t("ded", 4, 8.0, 700, "dedicated"),
        ];
        assert_eq!(recommend(&list), Some(1));
        assert_eq!(recommend(&list[..1]), Some(0));
        assert_eq!(recommend(&[]), None);
    }

    #[test]
    fn formatting() {
        assert_eq!(format_eur(821), "€8.21");
        assert_eq!(format_eur(1200), "€12.00");
        assert_eq!(format_eur(5), "€0.05");
        assert_eq!(flag("de"), "🇩🇪");
        assert_eq!(flag("FI"), "🇫🇮");
        assert_eq!(flag("??"), "🌐");
        assert_eq!(flag(""), "🌐");
        let c = build(&locations(), &types());
        let fsn1 = c.iter().find(|l| l.name == "fsn1").unwrap();
        let label = fsn1.types[1].label();
        assert_eq!(label, "cx33     €7.49   4 vCPU   8 GB   80 GB disk x86");
        assert!(fsn1.types[0].label().ends_with("★ recommended"));
        assert_eq!(
            fsn1.types[1].summary(),
            "cx33 (4 vCPU, 8 GB RAM, 80 GB disk, x86)"
        );
    }
}
