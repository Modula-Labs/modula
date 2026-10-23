//! Where servers run: a catalog of cloud regions, and matching a server to one.
//!
//! A node reports hints at connect time: its cloud provider and region from
//! the provider's metadata service when it has one, otherwise the Cloudflare
//! edge it reaches (`colo`, an airport code) and its country. An exact
//! provider region wins; otherwise the closest catalog entry to that edge.
//! Admins can always override the result.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Location {
    /// `<provider>:<region>`, e.g. `hetzner:fsn1`.
    pub id: &'static str,
    pub provider: &'static str,
    /// The provider's own region code.
    #[serde(skip)]
    pub region: &'static str,
    pub label: &'static str,
    pub city: &'static str,
    /// ISO 3166-1 alpha-2.
    pub country_code: &'static str,
    #[serde(skip)]
    pub lat: f64,
    #[serde(skip)]
    pub lon: f64,
}

macro_rules! loc {
    ($provider:literal, $region:literal, $label:literal, $city:literal, $cc:literal, $lat:expr, $lon:expr) => {
        Location {
            id: concat!($provider, ":", $region),
            provider: $provider,
            region: $region,
            label: $label,
            city: $city,
            country_code: $cc,
            lat: $lat,
            lon: $lon,
        }
    };
}

#[rustfmt::skip]
pub const CATALOG: &[Location] = &[
    // Hetzner
    loc!("hetzner", "fsn1", "Falkenstein", "Falkenstein", "DE", 50.48, 12.37),
    loc!("hetzner", "nbg1", "Nuremberg", "Nuremberg", "DE", 49.45, 11.08),
    loc!("hetzner", "hel1", "Helsinki", "Helsinki", "FI", 60.17, 24.94),
    loc!("hetzner", "ash", "Ashburn, VA", "Ashburn", "US", 39.04, -77.49),
    loc!("hetzner", "hil", "Hillsboro, OR", "Hillsboro", "US", 45.52, -122.99),
    loc!("hetzner", "sin", "Singapore", "Singapore", "SG", 1.35, 103.82),
    // AWS
    loc!("aws", "us-east-1", "N. Virginia", "Ashburn", "US", 39.04, -77.49),
    loc!("aws", "us-east-2", "Ohio", "Columbus", "US", 39.96, -83.00),
    loc!("aws", "us-west-1", "N. California", "San Jose", "US", 37.34, -121.89),
    loc!("aws", "us-west-2", "Oregon", "Boardman", "US", 45.84, -119.70),
    loc!("aws", "ca-central-1", "Canada Central", "Montreal", "CA", 45.50, -73.57),
    loc!("aws", "sa-east-1", "São Paulo", "São Paulo", "BR", -23.55, -46.63),
    loc!("aws", "eu-west-1", "Ireland", "Dublin", "IE", 53.35, -6.26),
    loc!("aws", "eu-west-2", "London", "London", "GB", 51.51, -0.13),
    loc!("aws", "eu-west-3", "Paris", "Paris", "FR", 48.86, 2.35),
    loc!("aws", "eu-central-1", "Frankfurt", "Frankfurt", "DE", 50.11, 8.68),
    loc!("aws", "eu-north-1", "Stockholm", "Stockholm", "SE", 59.33, 18.07),
    loc!("aws", "eu-south-1", "Milan", "Milan", "IT", 45.46, 9.19),
    loc!("aws", "me-central-1", "UAE", "Dubai", "AE", 25.20, 55.27),
    loc!("aws", "af-south-1", "Cape Town", "Cape Town", "ZA", -33.92, 18.42),
    loc!("aws", "ap-south-1", "Mumbai", "Mumbai", "IN", 19.08, 72.88),
    loc!("aws", "ap-southeast-1", "Singapore", "Singapore", "SG", 1.35, 103.82),
    loc!("aws", "ap-southeast-2", "Sydney", "Sydney", "AU", -33.87, 151.21),
    loc!("aws", "ap-northeast-1", "Tokyo", "Tokyo", "JP", 35.68, 139.69),
    loc!("aws", "ap-northeast-2", "Seoul", "Seoul", "KR", 37.57, 126.98),
    loc!("aws", "ap-east-1", "Hong Kong", "Hong Kong", "HK", 22.32, 114.17),
    // Google Cloud
    loc!("gcp", "us-central1", "Iowa", "Council Bluffs", "US", 41.26, -95.86),
    loc!("gcp", "us-east1", "South Carolina", "Moncks Corner", "US", 33.20, -80.01),
    loc!("gcp", "us-east4", "N. Virginia", "Ashburn", "US", 39.04, -77.49),
    loc!("gcp", "us-west1", "Oregon", "The Dalles", "US", 45.59, -121.18),
    loc!("gcp", "europe-west1", "Belgium", "St. Ghislain", "BE", 50.45, 3.82),
    loc!("gcp", "europe-west2", "London", "London", "GB", 51.51, -0.13),
    loc!("gcp", "europe-west3", "Frankfurt", "Frankfurt", "DE", 50.11, 8.68),
    loc!("gcp", "europe-west4", "Netherlands", "Eemshaven", "NL", 53.44, 6.83),
    loc!("gcp", "asia-southeast1", "Singapore", "Singapore", "SG", 1.35, 103.82),
    loc!("gcp", "asia-northeast1", "Tokyo", "Tokyo", "JP", 35.68, 139.69),
    loc!("gcp", "asia-south1", "Mumbai", "Mumbai", "IN", 19.08, 72.88),
    loc!("gcp", "australia-southeast1", "Sydney", "Sydney", "AU", -33.87, 151.21),
    // DigitalOcean
    loc!("digitalocean", "nyc", "New York", "New York", "US", 40.71, -74.01),
    loc!("digitalocean", "sfo", "San Francisco", "San Francisco", "US", 37.77, -122.42),
    loc!("digitalocean", "tor", "Toronto", "Toronto", "CA", 43.65, -79.38),
    loc!("digitalocean", "ams", "Amsterdam", "Amsterdam", "NL", 52.37, 4.90),
    loc!("digitalocean", "lon", "London", "London", "GB", 51.51, -0.13),
    loc!("digitalocean", "fra", "Frankfurt", "Frankfurt", "DE", 50.11, 8.68),
    loc!("digitalocean", "blr", "Bangalore", "Bangalore", "IN", 12.97, 77.59),
    loc!("digitalocean", "sgp", "Singapore", "Singapore", "SG", 1.35, 103.82),
    loc!("digitalocean", "syd", "Sydney", "Sydney", "AU", -33.87, 151.21),
    // Vultr
    loc!("vultr", "ewr", "New Jersey", "Piscataway", "US", 40.55, -74.46),
    loc!("vultr", "ord", "Chicago", "Chicago", "US", 41.88, -87.63),
    loc!("vultr", "lax", "Los Angeles", "Los Angeles", "US", 34.05, -118.24),
    loc!("vultr", "ams", "Amsterdam", "Amsterdam", "NL", 52.37, 4.90),
    loc!("vultr", "fra", "Frankfurt", "Frankfurt", "DE", 50.11, 8.68),
    loc!("vultr", "lhr", "London", "London", "GB", 51.51, -0.13),
    loc!("vultr", "nrt", "Tokyo", "Tokyo", "JP", 35.68, 139.69),
    loc!("vultr", "sgp", "Singapore", "Singapore", "SG", 1.35, 103.82),
    loc!("vultr", "syd", "Sydney", "Sydney", "AU", -33.87, 151.21),
    // Akamai (Linode)
    loc!("linode", "us-east", "Newark, NJ", "Newark", "US", 40.74, -74.17),
    loc!("linode", "us-central", "Dallas, TX", "Dallas", "US", 32.78, -96.80),
    loc!("linode", "eu-central", "Frankfurt", "Frankfurt", "DE", 50.11, 8.68),
    loc!("linode", "eu-west", "London", "London", "GB", 51.51, -0.13),
    loc!("linode", "ap-south", "Singapore", "Singapore", "SG", 1.35, 103.82),
    // OVHcloud
    loc!("ovh", "gra", "Gravelines", "Gravelines", "FR", 50.99, 2.13),
    loc!("ovh", "sbg", "Strasbourg", "Strasbourg", "FR", 48.57, 7.75),
    loc!("ovh", "bhs", "Beauharnois", "Beauharnois", "CA", 45.31, -73.87),
    // Azure
    loc!("azure", "eastus", "East US", "Virginia", "US", 37.43, -78.66),
    loc!("azure", "westus2", "West US 2", "Washington", "US", 47.23, -119.85),
    loc!("azure", "westeurope", "West Europe", "Amsterdam", "NL", 52.37, 4.90),
    loc!("azure", "northeurope", "North Europe", "Dublin", "IE", 53.35, -6.26),
    loc!("azure", "southeastasia", "Southeast Asia", "Singapore", "SG", 1.35, 103.82),
];

/// Cloudflare edge locations (IATA codes) → coordinates, for picking the
/// nearest catalog entry when a server has no cloud metadata.
#[rustfmt::skip]
const COLOS: &[(&str, f64, f64)] = &[
    ("IAD", 38.95, -77.46), ("EWR", 40.69, -74.17), ("JFK", 40.64, -73.78), ("BOS", 42.36, -71.01),
    ("ORD", 41.98, -87.90), ("DFW", 32.90, -97.04), ("ATL", 33.64, -84.43), ("MIA", 25.79, -80.29),
    ("DEN", 39.86, -104.67), ("SEA", 47.45, -122.31), ("SJC", 37.36, -121.93), ("SFO", 37.62, -122.38),
    ("LAX", 33.94, -118.41), ("PDX", 45.59, -122.60), ("YYZ", 43.68, -79.63), ("YUL", 45.47, -73.74),
    ("GRU", -23.43, -46.47), ("LHR", 51.47, -0.45), ("MAN", 53.35, -2.27), ("DUB", 53.42, -6.27),
    ("CDG", 49.01, 2.55), ("MRS", 43.44, 5.22), ("AMS", 52.31, 4.76), ("BRU", 50.90, 4.48),
    ("FRA", 50.04, 8.56), ("DUS", 51.29, 6.77), ("HAM", 53.63, 9.99), ("MUC", 48.35, 11.79),
    ("TXL", 52.56, 13.29), ("BER", 52.37, 13.50), ("PRG", 50.10, 14.26), ("VIE", 48.11, 16.57),
    ("ZRH", 47.46, 8.55), ("MXP", 45.63, 8.72), ("MAD", 40.47, -3.56), ("LIS", 38.77, -9.13),
    ("CPH", 55.62, 12.65), ("ARN", 59.65, 17.92), ("OSL", 60.19, 11.10), ("HEL", 60.32, 24.96),
    ("WAW", 52.17, 20.97), ("DXB", 25.25, 55.36), ("JNB", -26.14, 28.24), ("CPT", -33.97, 18.60),
    ("BOM", 19.09, 72.87), ("DEL", 28.56, 77.10), ("BLR", 13.20, 77.71), ("SIN", 1.36, 103.99),
    ("HKG", 22.31, 113.91), ("NRT", 35.77, 140.39), ("KIX", 34.43, 135.24), ("ICN", 37.46, 126.44),
    ("SYD", -33.95, 151.18), ("MEL", -37.67, 144.84),
    ("MAA", 12.99, 80.17), ("HYD", 17.24, 78.43), ("CCU", 22.65, 88.45), ("AMD", 23.07, 72.63),
    ("COK", 10.15, 76.40), ("CJB", 11.03, 77.04), ("NAG", 21.09, 79.05), ("CMB", 7.18, 79.88),
    ("KUL", 2.74, 101.71), ("BKK", 13.69, 100.75), ("CGK", -6.13, 106.66), ("MNL", 14.51, 121.02),
    ("TPE", 25.08, 121.23), ("DOH", 25.27, 51.61), ("TLV", 32.01, 34.89), ("IST", 41.26, 28.74),
    ("CAI", 30.12, 31.41), ("LOS", 6.58, 3.32), ("NBO", -1.32, 36.93), ("SCL", -33.39, -70.79),
    ("EZE", -34.82, -58.54), ("BOG", 4.70, -74.15), ("LIM", -12.02, -77.11), ("QRO", 20.62, -100.19),
    ("AKL", -37.01, 174.79), ("PER", -31.94, 115.97), ("BNE", -27.38, 153.12),
];

pub fn get(id: &str) -> Option<&'static Location> {
    CATALOG.iter().find(|l| l.id == id)
}

/// The best catalog entry for a server's hints.
pub fn resolve(provider: &str, region: &str, colo: &str) -> Option<&'static Location> {
    let provider = provider.trim().to_ascii_lowercase();
    let region = region.trim().to_ascii_lowercase();
    if !provider.is_empty() && !region.is_empty() {
        // Exact region, or a datacenter inside it (`fsn1-dc14`, `nyc3`).
        let exact = CATALOG
            .iter()
            .filter(|l| l.provider == provider)
            .find(|l| region == l.region || region.starts_with(l.region));
        if exact.is_some() {
            return exact;
        }
    }
    let colo = colo.trim().to_ascii_uppercase();
    let &(_, lat, lon) = COLOS.iter().find(|(code, ..)| *code == colo)?;
    // Prefer the server's own provider among equally near entries.
    CATALOG.iter().min_by(|a, b| {
        let da =
            distance_km(lat, lon, a.lat, a.lon) + if a.provider == provider { 0.0 } else { 1.0 };
        let db =
            distance_km(lat, lon, b.lat, b.lon) + if b.provider == provider { 0.0 } else { 1.0 };
        da.total_cmp(&db)
    })
}

fn distance_km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dp = (lat2 - lat1).to_radians();
    let dl = (lon2 - lon1).to_radians();
    let a = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    6371.0 * 2.0 * a.sqrt().asin()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_regions_and_nearest_edges() {
        assert_eq!(
            resolve("hetzner", "fsn1-dc14", "").unwrap().id,
            "hetzner:fsn1"
        );
        assert_eq!(
            resolve("aws", "eu-central-1", "FRA").unwrap().id,
            "aws:eu-central-1"
        );
        assert_eq!(
            resolve("digitalocean", "nyc3", "").unwrap().id,
            "digitalocean:nyc"
        );
        // No metadata: nearest to the edge, preferring the same provider.
        assert_eq!(resolve("hetzner", "", "HEL").unwrap().id, "hetzner:hel1");
        assert_eq!(resolve("", "", "SIN").unwrap().city, "Singapore");
        assert!(resolve("", "", "XXX").is_none());
        let ids: std::collections::HashSet<_> = CATALOG.iter().map(|l| l.id).collect();
        assert_eq!(ids.len(), CATALOG.len(), "ids are unique");
    }
}
