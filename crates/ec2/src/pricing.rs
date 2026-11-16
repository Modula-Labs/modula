//! EC2 prices: the AWS Price List API, the curated instance families, and
//! the monthly total a server really costs (compute + root disk + its
//! Elastic IP). All amounts in USD; hourly rates are held in nano-dollars
//! so totals never round through floats.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use futures::{StreamExt, TryStreamExt, stream};
use serde_json::{Value, json};

use crate::{Client, ProviderError};

/// The families offered: general purpose and compute, x86 and Arm, burstable
/// and not. Everything else (GPUs, storage, huge memory) stays out of the list.
pub const FAMILIES: [&str; 7] = ["t4g", "t3a", "t3", "m7g", "m7i", "c7g", "c7i"];

/// What the root disk defaults to when a server doesn't choose (GB).
pub const DEFAULT_ROOT_GB: u32 = 80;
pub const MIN_ROOT_GB: u32 = 20;
pub const MAX_ROOT_GB: u32 = 16_384;

/// Billed hours in a month, by AWS's convention.
pub const HOURS_PER_MONTH: u64 = 730;
/// gp3 storage, the us-east-1 list price (a default: Modula doesn't query
/// each region's EBS price).
pub const GP3_USD_PER_GB_MONTH: f64 = 0.08;
/// A public IPv4 address, in any state.
pub const EIP_USD_PER_HOUR: f64 = 0.005;

const NANOS_PER_USD: f64 = 1e9;
const NANOS_PER_CENT: u64 = 10_000_000;
const CACHE_TTL: Duration = Duration::from_secs(24 * 3600);
const CONCURRENCY: usize = 8;

/// `t3.small` to its family, `t3`.
pub fn is_curated(instance_type: &str) -> bool {
    instance_type
        .split_once('.')
        .is_some_and(|(family, size)| FAMILIES.contains(&family) && size != "metal")
}

/// `$0.0104` as nano-dollars.
pub fn parse_usd_nanos(s: &str) -> Option<u64> {
    let (whole, frac) = s.split_once('.').unwrap_or((s, ""));
    let whole: u64 = whole.parse().ok()?;
    let frac = format!("{frac:0<9}");
    let frac: u64 = frac.get(..9)?.parse().ok()?;
    whole.checked_mul(1_000_000_000)?.checked_add(frac)
}

/// The monthly cost of `disk_gb` of gp3, in nano-dollars.
pub fn gp3_monthly_nanos(disk_gb: u32) -> u64 {
    u64::from(disk_gb) * (GP3_USD_PER_GB_MONTH * NANOS_PER_USD).round() as u64
}

/// An Elastic IP's monthly cost, in nano-dollars.
pub fn eip_monthly_nanos() -> u64 {
    HOURS_PER_MONTH * (EIP_USD_PER_HOUR * NANOS_PER_USD).round() as u64
}

/// What a server costs per month, in USD cents: 730 hours of compute, its
/// root gp3 and its Elastic IP.
pub fn monthly_cents(hourly_nanos: u64, disk_gb: u32) -> i64 {
    let total = hourly_nanos * HOURS_PER_MONTH + gp3_monthly_nanos(disk_gb) + eip_monthly_nanos();
    ((total + NANOS_PER_CENT / 2) / NANOS_PER_CENT) as i64
}

/// Change in monthly cents between a root disk of `disk_gb` and the default.
pub fn disk_delta_cents(disk_gb: u32) -> i64 {
    let delta = gp3_monthly_nanos(disk_gb) as i64 - gp3_monthly_nanos(DEFAULT_ROOT_GB) as i64;
    delta / NANOS_PER_CENT as i64
}

/// (instance type, region code) to its on-demand Linux rate in nano-dollars.
pub type Prices = HashMap<(String, String), u64>;

/// One entry of `GetProducts`' `PriceList` (itself a JSON document).
pub fn parse_price_item(raw: &str) -> Option<((String, String), u64)> {
    let v: Value = serde_json::from_str(raw).ok()?;
    let attrs = &v["product"]["attributes"];
    let instance_type = attrs["instanceType"].as_str()?.to_owned();
    let region = attrs["regionCode"].as_str()?.to_owned();
    let terms = v["terms"]["OnDemand"].as_object()?;
    let rate = terms.values().find_map(|term| {
        term["priceDimensions"]
            .as_object()?
            .values()
            .find_map(|d| parse_usd_nanos(d["pricePerUnit"]["USD"].as_str()?))
    })?;
    (rate > 0).then_some(((instance_type, region), rate))
}

struct Cached {
    at: Instant,
    types: BTreeSet<String>,
    prices: Arc<Prices>,
}

/// Prices are public and the same for every account: cached per process and
/// endpoint (a mock in tests), though the provider is built per request.
static CACHE: LazyLock<Mutex<HashMap<String, Cached>>> = LazyLock::new(Default::default);

fn cached(key: &str, want: &BTreeSet<String>) -> Option<Arc<Prices>> {
    let cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let c = cache.get(key)?;
    (c.at.elapsed() < CACHE_TTL && want.is_subset(&c.types)).then(|| c.prices.clone())
}

fn stale(key: &str) -> Option<Arc<Prices>> {
    let cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    cache.get(key).map(|c| c.prices.clone())
}

impl Client {
    /// One region-independent query per instance type: its entries cover
    /// every region at once, so this costs about one request per type.
    async fn fetch_type_prices(&self, instance_type: &str) -> Result<Prices, ProviderError> {
        let term = |field: &str, value: &str| json!({"Type": "TERM_MATCH", "Field": field, "Value": value});
        let mut out = Prices::new();
        let mut token: Option<String> = None;
        loop {
            let mut body = json!({
                "ServiceCode": "AmazonEC2",
                "FormatVersion": "aws_v1",
                "MaxResults": 100,
                "Filters": [
                    term("instanceType", instance_type),
                    term("operatingSystem", "Linux"),
                    term("tenancy", "Shared"),
                    term("preInstalledSw", "NA"),
                    term("capacitystatus", "Used"),
                ],
            });
            if let Some(t) = &token {
                body["NextToken"] = json!(t);
            }
            let res = self.pricing_call(&body).await?;
            for item in res["PriceList"].as_array().into_iter().flatten() {
                if let Some((key, rate)) = item.as_str().and_then(parse_price_item) {
                    out.insert(key, rate);
                }
            }
            token = res["NextToken"].as_str().map(str::to_owned);
            if token.is_none() {
                return Ok(out);
            }
        }
    }

    /// Current prices for `types`: from the 24-hour cache when it holds
    /// them; when the API fails, whatever was last fetched; only with
    /// neither, the error.
    pub async fn prices(&self, types: &[String]) -> Result<Arc<Prices>, ProviderError> {
        let key = self.endpoint_key();
        let want: BTreeSet<String> = types.iter().cloned().collect();
        if let Some(p) = cached(&key, &want) {
            return Ok(p);
        }
        let fetched: Result<Vec<Prices>, ProviderError> = stream::iter(want.iter().cloned())
            .map(|t| async move { self.fetch_type_prices(&t).await })
            .buffer_unordered(CONCURRENCY)
            .try_collect()
            .await;
        match fetched {
            Ok(parts) => {
                let prices = Arc::new(parts.into_iter().flatten().collect::<Prices>());
                let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
                cache.insert(
                    key,
                    Cached {
                        at: Instant::now(),
                        types: want,
                        prices: prices.clone(),
                    },
                );
                Ok(prices)
            }
            Err(e) => match stale(&key) {
                Some(p) => {
                    tracing::warn!(error = %e, "AWS Pricing API failed; serving cached prices");
                    Ok(p)
                }
                None => Err(e),
            },
        }
    }

    /// A one-entry query, proving the credentials may read prices.
    pub async fn check_pricing(&self) -> Result<(), ProviderError> {
        let body = json!({"ServiceCode": "AmazonEC2", "MaxResults": 1});
        self.pricing_call(&body).await.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimal_rates() {
        assert_eq!(parse_usd_nanos("0.0084000000"), Some(8_400_000));
        assert_eq!(parse_usd_nanos("1.5"), Some(1_500_000_000));
        assert_eq!(parse_usd_nanos("2"), Some(2_000_000_000));
        assert_eq!(parse_usd_nanos("x"), None);
    }

    #[test]
    fn the_monthly_total_is_compute_plus_disk_plus_address() {
        // t4g.small in us-east-1: $0.0168/h. 730 h = $12.264; 80 GB gp3 = $6.40;
        // the address = $3.65.
        assert_eq!(monthly_cents(16_800_000, 80), 1226 + 640 + 365);
        assert_eq!(monthly_cents(0, 80), 1005);
        assert_eq!(monthly_cents(0, 100), 1165);
    }

    #[test]
    fn disk_defaults_named_from_constants() {
        assert_eq!(gp3_monthly_nanos(1), 80_000_000);
        assert_eq!(eip_monthly_nanos(), 3_650_000_000);
        assert_eq!(disk_delta_cents(DEFAULT_ROOT_GB), 0);
        assert_eq!(disk_delta_cents(180), 800);
        assert_eq!(disk_delta_cents(40), -320);
    }

    #[test]
    fn curated_families_only() {
        assert!(is_curated("t4g.nano"));
        assert!(is_curated("c7i.2xlarge"));
        assert!(!is_curated("m7g.metal"));
        assert!(!is_curated("p5.48xlarge"));
        assert!(!is_curated("t3"));
    }

    #[test]
    fn price_list_entry() {
        let raw = json!({
            "product": {"attributes": {"instanceType": "t3.small", "regionCode": "eu-west-1"}},
            "terms": {"OnDemand": {"X.JRTCKXETXF": {"priceDimensions": {"X.JRTCKXETXF.6YS6EN2CT7": {
                "unit": "Hrs", "pricePerUnit": {"USD": "0.0228000000"}}}}}}
        })
        .to_string();
        assert_eq!(
            parse_price_item(&raw),
            Some((("t3.small".to_owned(), "eu-west-1".to_owned()), 22_800_000))
        );
        // A $0 entry is a placeholder, not a free server.
        let zero = raw.replace("0.0228000000", "0.0000000000");
        assert_eq!(parse_price_item(&zero), None);
        assert_eq!(parse_price_item("{}"), None);
    }
}
