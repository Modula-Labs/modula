//! A small AWS EC2 client and [`ServerProvider`](modula_provider::ServerProvider):
//! just what Modula needs, shared by the control plane and the `modula` CLI.
//! EC2's Query API over hand-written SigV4; responses read as XML.
//!
//! The secret key only ever feeds the signature: it is kept in a
//! [`SecretString`] (zeroized on drop) and never logged or put in errors.

pub mod firewall;
pub mod pricing;
pub mod provider;
pub mod sigv4;
pub mod xml;

use std::collections::BTreeMap;
use std::time::Duration;

use base64::Engine;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use reqwest::StatusCode;
use secrecy::{ExposeSecret, SecretString};
use serde_json::Value;
use url::Url;

pub use modula_provider::ProviderError;

use self::xml::Node;

const API_VERSION: &str = "2016-11-15";
const USER_AGENT: &str = concat!("modula/", env!("CARGO_PKG_VERSION"));
/// Where requests not tied to a region go, and the Pricing API's region.
pub const HOME_REGION: &str = "us-east-1";
const PRICING_HOST: &str = "https://api.pricing.us-east-1.amazonaws.com/";
const PRICING_TARGET: &str = "AWSPriceListService.GetProducts";
const MAX_ATTEMPTS: u32 = 6;
const MAX_PAGES: u32 = 50;

/// The IAM policy Modula needs: nothing else is ever called. Shown when
/// connecting, and in guide/integrations.md.
pub const MINIMAL_IAM_POLICY: &str = r#"{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Action": [
        "ec2:AllocateAddress",
        "ec2:AssociateAddress",
        "ec2:AuthorizeSecurityGroupIngress",
        "ec2:CreateSecurityGroup",
        "ec2:CreateTags",
        "ec2:DescribeAddresses",
        "ec2:DescribeImages",
        "ec2:DescribeInstances",
        "ec2:DescribeInstanceTypeOfferings",
        "ec2:DescribeInstanceTypes",
        "ec2:DescribeKeyPairs",
        "ec2:DescribeRegions",
        "ec2:DescribeSecurityGroups",
        "ec2:DescribeSubnets",
        "ec2:DescribeVolumes",
        "ec2:DescribeVpcs",
        "ec2:DisassociateAddress",
        "ec2:ImportKeyPair",
        "ec2:ModifyInstanceAttribute",
        "ec2:RebootInstances",
        "ec2:ReleaseAddress",
        "ec2:RevokeSecurityGroupIngress",
        "ec2:RunInstances",
        "ec2:StartInstances",
        "ec2:StopInstances",
        "ec2:TerminateInstances",
        "pricing:GetProducts"
      ],
      "Resource": "*"
    }
  ]
}"#;

/// RFC 3986 unreserved stay as they are; what form bodies and SigV4 expect.
const ENCODE: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

fn encode(v: &str) -> String {
    utf8_percent_encode(v, ENCODE).to_string()
}

/// An IAM user's long-lived key. `Debug` is deliberately not derived.
pub struct Credentials {
    pub access_key_id: String,
    pub secret_access_key: SecretString,
}

/// Query API parameters, in the order given.
#[derive(Default, Clone)]
pub struct Params(Vec<(String, String)>);

impl Params {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.0.push((k.into(), v.into()));
        self
    }

    pub fn filters(mut self, filters: &[(&str, &[&str])]) -> Self {
        for (i, (name, values)) in filters.iter().enumerate() {
            let n = i + 1;
            self = self.set(format!("Filter.{n}.Name"), *name);
            for (j, v) in values.iter().enumerate() {
                self = self.set(format!("Filter.{n}.Value.{}", j + 1), *v);
            }
        }
        self
    }

    fn tag_spec(mut self, n: usize, resource: &str, tags: &BTreeMap<String, String>) -> Self {
        self = self.set(format!("TagSpecification.{n}.ResourceType"), resource);
        for (i, (k, v)) in tags.iter().enumerate() {
            let t = i + 1;
            self = self
                .set(format!("TagSpecification.{n}.Tag.{t}.Key"), k.as_str())
                .set(format!("TagSpecification.{n}.Tag.{t}.Value"), v.as_str());
        }
        self
    }

    pub fn tags(mut self, tags: &BTreeMap<String, String>) -> Self {
        for (i, (k, v)) in tags.iter().enumerate() {
            let t = i + 1;
            self = self
                .set(format!("Tag.{t}.Key"), k.as_str())
                .set(format!("Tag.{t}.Value"), v.as_str());
        }
        self
    }

    fn permission(mut self, rule: &IpRule) -> Self {
        let p = "IpPermissions.1";
        self = self
            .set(format!("{p}.IpProtocol"), rule.protocol.as_str())
            .set(format!("{p}.FromPort"), rule.from.to_string())
            .set(format!("{p}.ToPort"), rule.to.to_string());
        for (i, cidr) in rule.cidrs.iter().enumerate() {
            let r = i + 1;
            self = self.set(format!("{p}.IpRanges.{r}.CidrIp"), cidr.as_str());
            if let Some(d) = &rule.description {
                self = self.set(format!("{p}.IpRanges.{r}.Description"), d.as_str());
            }
        }
        self
    }

    fn body(&self, action: &str) -> String {
        let mut out = format!("Action={action}&Version={API_VERSION}");
        for (k, v) in &self.0 {
            out.push('&');
            out.push_str(&encode(k));
            out.push('=');
            out.push_str(&encode(v));
        }
        out
    }
}

/// An ingress rule: `protocol` on `from..=to` (-1 for all of ICMP) from `cidrs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IpRule {
    pub protocol: String,
    pub from: i32,
    pub to: i32,
    pub cidrs: Vec<String>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Instance {
    pub id: String,
    /// pending, running, stopping, stopped, shutting-down, terminated
    pub state: String,
    pub instance_type: String,
    pub public_ip: Option<String>,
    pub vpc_id: String,
    pub root_device: String,
    /// Volume ids by device name.
    pub volumes: BTreeMap<String, String>,
    pub groups: Vec<String>,
    pub tags: BTreeMap<String, String>,
    /// `(code, message)` when AWS says why it stopped or failed.
    pub state_reason: Option<(String, String)>,
}

impl Instance {
    pub fn tag(&self, key: &str) -> Option<&str> {
        self.tags.get(key).map(String::as_str)
    }

    fn from_node(n: &Node) -> Self {
        Self {
            id: n.string("instanceId"),
            state: n
                .child("instanceState")
                .map(|s| s.string("name"))
                .unwrap_or_default(),
            instance_type: n.string("instanceType"),
            public_ip: n.text_of("ipAddress").map(str::to_owned),
            vpc_id: n.string("vpcId"),
            root_device: n.string("rootDeviceName"),
            volumes: n
                .items("blockDeviceMapping")
                .filter_map(|b| {
                    let volume = b.child("ebs")?.text_of("volumeId")?;
                    Some((b.string("deviceName"), volume.to_owned()))
                })
                .collect(),
            groups: n.items("groupSet").map(|g| g.string("groupId")).collect(),
            tags: tags_of(n),
            state_reason: n
                .child("stateReason")
                .map(|r| (r.string("code"), r.string("message")))
                .filter(|(c, _)| !c.is_empty()),
        }
    }
}

fn tags_of(n: &Node) -> BTreeMap<String, String> {
    n.items("tagSet")
        .map(|t| (t.string("key"), t.string("value")))
        .collect()
}

#[derive(Debug, Clone, Default)]
pub struct Address {
    pub public_ip: String,
    pub allocation_id: String,
    pub association_id: Option<String>,
    pub instance_id: Option<String>,
    pub tags: BTreeMap<String, String>,
}

/// A permission as EC2 holds it. `other` counts its non-IPv4 sources, which
/// take up rule quota too.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Permission {
    pub protocol: String,
    pub from: i32,
    pub to: i32,
    pub ipv4: Vec<String>,
    pub other: usize,
}

#[derive(Debug, Clone, Default)]
pub struct SecurityGroup {
    pub id: String,
    pub name: String,
    pub vpc_id: String,
    pub tags: BTreeMap<String, String>,
    pub permissions: Vec<Permission>,
}

#[derive(Debug, Clone)]
pub struct InstanceTypeInfo {
    pub name: String,
    pub vcpus: u32,
    pub memory_mib: u64,
    /// `x86` or `arm`.
    pub arch: String,
    pub burstable: bool,
}

#[derive(Debug, Clone)]
pub struct KeyPair {
    pub name: String,
    pub fingerprint: String,
    /// OpenSSH format; empty when AWS doesn't return it.
    pub public_key: String,
}

#[derive(Debug, Clone)]
pub struct Image {
    pub id: String,
    pub root_device: String,
}

#[derive(Debug, Clone)]
pub struct Subnet {
    pub id: String,
    pub zone: String,
}

/// `RunInstances`' inputs. `Debug` leaves `user_data` out: it carries a join token.
#[derive(Clone)]
pub struct RunInstance {
    pub client_token: String,
    pub image_id: String,
    pub instance_type: String,
    pub subnet_id: String,
    pub security_groups: Vec<String>,
    pub key_name: Option<String>,
    pub root_device: String,
    pub root_gb: u32,
    pub user_data: String,
    pub tags: BTreeMap<String, String>,
}

impl std::fmt::Debug for RunInstance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunInstance")
            .field("instance_type", &self.instance_type)
            .field("subnet_id", &self.subnet_id)
            .field(
                "user_data",
                &format_args!("<redacted, {} bytes>", self.user_data.len()),
            )
            .finish_non_exhaustive()
    }
}

fn architecture(raw: &str) -> &'static str {
    if raw.contains("arm64") { "arm" } else { "x86" }
}

pub fn map_error(code: String, message: String) -> ProviderError {
    match code.as_str() {
        "AuthFailure"
        | "InvalidClientTokenId"
        | "SignatureDoesNotMatch"
        | "IncompleteSignature"
        | "MissingAuthenticationToken"
        | "UnrecognizedClientException"
        | "ExpiredToken" => ProviderError::Unauthorized,
        "UnauthorizedOperation" | "AccessDenied" | "AccessDeniedException" | "Blocked" => {
            ProviderError::Forbidden
        }
        "InvalidInstanceID.NotFound" | "InvalidInstanceID.Malformed" => ProviderError::NotFound,
        _ => ProviderError::Api { code, message },
    }
}

/// Throttled, or refused before anything happened: safe to repeat.
fn retryable(status: StatusCode, code: &str) -> bool {
    matches!(
        code,
        "RequestLimitExceeded" | "Throttling" | "ThrottlingException" | "TooManyRequestsException"
    ) || status == StatusCode::SERVICE_UNAVAILABLE
}

fn pricing_error(bytes: &[u8]) -> Option<(String, String)> {
    let v: Value = serde_json::from_slice(bytes).ok()?;
    let raw = v["__type"].as_str()?;
    let code = raw.rsplit('#').next()?.split(':').next()?.to_owned();
    let message = v["message"]
        .as_str()
        .or(v["Message"].as_str())
        .unwrap_or_default()
        .to_owned();
    Some((code, message))
}

fn without_url(e: reqwest::Error) -> String {
    e.without_url().to_string()
}

pub struct Client {
    http: reqwest::Client,
    /// Sends every request here (a mock in tests); None uses AWS's endpoints.
    endpoint: Option<Url>,
    credentials: Credentials,
    retry_base: Duration,
}

impl Client {
    pub fn new(endpoint: Option<Url>, credentials: Credentials) -> Result<Self, ProviderError> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| ProviderError::Transport(e.to_string()))?;
        Ok(Self {
            http,
            endpoint,
            credentials,
            retry_base: Duration::from_millis(200),
        })
    }

    /// The first retry's delay (tests shorten it).
    pub fn with_retry_base(mut self, base: Duration) -> Self {
        self.retry_base = base;
        self
    }

    pub(crate) fn retry_base(&self) -> Duration {
        self.retry_base
    }

    pub(crate) fn endpoint_key(&self) -> String {
        self.endpoint
            .as_ref()
            .map_or_else(|| "aws".to_owned(), Url::to_string)
    }

    fn url_for(&self, default: String) -> Result<Url, ProviderError> {
        match &self.endpoint {
            Some(u) => Ok(u.clone()),
            None => default
                .parse()
                .map_err(|e: url::ParseError| ProviderError::Transport(e.to_string())),
        }
    }

    /// Signs and sends one attempt: `(status, body)`.
    async fn send(
        &self,
        url: &Url,
        scope: &sigv4::Scope<'_>,
        content_type: &str,
        target: Option<&str>,
        body: &[u8],
    ) -> Result<(StatusCode, Vec<u8>), ProviderError> {
        let date = sigv4::amz_date(time::OffsetDateTime::now_utc());
        let host = match (url.host_str(), url.port()) {
            (Some(h), Some(p)) => format!("{h}:{p}"),
            (Some(h), None) => h.to_owned(),
            _ => return Err(ProviderError::Transport("the endpoint has no host".into())),
        };
        let mut headers: Vec<(&str, &str)> = vec![
            ("content-type", content_type),
            ("host", &host),
            ("x-amz-date", &date),
        ];
        if let Some(t) = target {
            headers.push(("x-amz-target", t));
        }
        let auth = sigv4::authorization(
            &self.credentials.access_key_id,
            self.credentials.secret_access_key.expose_secret(),
            scope,
            &date,
            "POST",
            &headers,
            body,
        );
        let mut req = self
            .http
            .post(url.clone())
            .header("authorization", auth)
            .header("content-type", content_type)
            .header("x-amz-date", &date)
            .body(body.to_vec());
        if let Some(t) = target {
            req = req.header("x-amz-target", t);
        }
        let res = req
            .send()
            .await
            .map_err(|e| ProviderError::Transport(without_url(e)))?;
        let status = res.status();
        let bytes = res
            .bytes()
            .await
            .map_err(|e| ProviderError::Transport(without_url(e)))?;
        Ok((status, bytes.to_vec()))
    }

    async fn sleep_before_retry(&self, attempt: u32) {
        let base = self.retry_base * 2u32.pow(attempt);
        let jitter = u64::from(time::OffsetDateTime::now_utc().nanosecond()) % 1000;
        tokio::time::sleep(base + base * jitter as u32 / 2000).await;
    }

    /// One EC2 action; throttled requests are repeated with backoff.
    pub(crate) async fn ec2(
        &self,
        region: &str,
        action: &str,
        params: &Params,
    ) -> Result<Node, ProviderError> {
        let url = self.url_for(format!("https://ec2.{region}.amazonaws.com/"))?;
        let scope = sigv4::Scope {
            region,
            service: "ec2",
        };
        let body = params.body(action);
        let mut attempt = 0;
        loop {
            let (status, bytes) = self
                .send(
                    &url,
                    &scope,
                    "application/x-www-form-urlencoded; charset=utf-8",
                    None,
                    body.as_bytes(),
                )
                .await?;
            if status.is_success() {
                return xml::parse(&bytes)
                    .map_err(|e| ProviderError::Transport(format!("unexpected response: {e}")));
            }
            let (code, message) = xml::error(&bytes).unwrap_or_else(|| {
                (
                    format!("http_{}", status.as_u16()),
                    status
                        .canonical_reason()
                        .unwrap_or("request failed")
                        .to_owned(),
                )
            });
            attempt += 1;
            if attempt < MAX_ATTEMPTS && retryable(status, &code) {
                self.sleep_before_retry(attempt - 1).await;
                continue;
            }
            return Err(map_error(code, message));
        }
    }

    /// Every page of an EC2 list action, each read by `read`.
    async fn ec2_paged<T>(
        &self,
        region: &str,
        action: &str,
        params: Params,
        read: impl Fn(&Node) -> Vec<T>,
    ) -> Result<Vec<T>, ProviderError> {
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let page = match &token {
                Some(t) => params.clone().set("NextToken", t.as_str()),
                None => params.clone(),
            };
            let root = self.ec2(region, action, &page).await?;
            out.extend(read(&root));
            token = root.text_of("nextToken").map(str::to_owned);
            if token.is_none() {
                break;
            }
        }
        Ok(out)
    }

    /// One Price List request.
    pub(crate) async fn pricing_call(&self, body: &Value) -> Result<Value, ProviderError> {
        let url = self.url_for(PRICING_HOST.to_owned())?;
        let scope = sigv4::Scope {
            region: HOME_REGION,
            service: "pricing",
        };
        let raw = body.to_string();
        let mut attempt = 0;
        loop {
            let (status, bytes) = self
                .send(
                    &url,
                    &scope,
                    "application/x-amz-json-1.1",
                    Some(PRICING_TARGET),
                    raw.as_bytes(),
                )
                .await?;
            if status.is_success() {
                return serde_json::from_slice(&bytes)
                    .map_err(|e| ProviderError::Transport(format!("unexpected response: {e}")));
            }
            let (code, message) = pricing_error(&bytes).unwrap_or_else(|| {
                (
                    format!("http_{}", status.as_u16()),
                    status
                        .canonical_reason()
                        .unwrap_or("request failed")
                        .to_owned(),
                )
            });
            attempt += 1;
            if attempt < MAX_ATTEMPTS && retryable(status, &code) {
                self.sleep_before_retry(attempt - 1).await;
                continue;
            }
            return Err(map_error(code, message));
        }
    }

    /// The regions this account has enabled.
    pub async fn regions(&self) -> Result<Vec<String>, ProviderError> {
        let root = self
            .ec2(HOME_REGION, "DescribeRegions", &Params::new())
            .await?;
        Ok(root
            .items("regionInfo")
            .map(|r| r.string("regionName"))
            .collect())
    }

    pub async fn default_vpc(&self, region: &str) -> Result<Option<String>, ProviderError> {
        let params = Params::new().filters(&[("isDefault", &["true"])]);
        let root = self.ec2(region, "DescribeVpcs", &params).await?;
        Ok(root.items("vpcSet").next().map(|v| v.string("vpcId")))
    }

    pub async fn default_subnets(
        &self,
        region: &str,
        vpc: &str,
    ) -> Result<Vec<Subnet>, ProviderError> {
        let params = Params::new().filters(&[("vpc-id", &[vpc]), ("default-for-az", &["true"])]);
        self.ec2_paged(region, "DescribeSubnets", params, |root| {
            root.items("subnetSet")
                .map(|s| Subnet {
                    id: s.string("subnetId"),
                    zone: s.string("availabilityZone"),
                })
                .collect()
        })
        .await
    }

    /// `patterns` may use wildcards (`t3.*`). Bare metal is left out.
    pub async fn instance_types(
        &self,
        region: &str,
        patterns: &[&str],
    ) -> Result<Vec<InstanceTypeInfo>, ProviderError> {
        let params =
            Params::new().filters(&[("instance-type", patterns), ("bare-metal", &["false"])]);
        self.ec2_paged(region, "DescribeInstanceTypes", params, |root| {
            root.items("instanceTypeSet")
                .map(|t| InstanceTypeInfo {
                    name: t.string("instanceType"),
                    vcpus: t
                        .child("vCpuInfo")
                        .and_then(|v| v.text_of("defaultVCpus"))
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0),
                    memory_mib: t
                        .child("memoryInfo")
                        .and_then(|v| v.text_of("sizeInMiB"))
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0),
                    arch: t
                        .child("processorInfo")
                        .map(|p| {
                            let arches = p
                                .items("supportedArchitectures")
                                .map(|a| a.text.clone())
                                .collect::<Vec<_>>()
                                .join(",");
                            architecture(&arches).to_owned()
                        })
                        .unwrap_or_default(),
                    burstable: t.text_of("burstablePerformanceSupported") == Some("true"),
                })
                .collect()
        })
        .await
    }

    /// Where `patterns` can be ordered: instance type to location, by `kind`
    /// (`region` or `availability-zone`).
    pub async fn offerings(
        &self,
        region: &str,
        kind: &str,
        patterns: &[&str],
    ) -> Result<Vec<(String, String)>, ProviderError> {
        let params = Params::new()
            .set("LocationType", kind)
            .filters(&[("instance-type", patterns)]);
        self.ec2_paged(region, "DescribeInstanceTypeOfferings", params, |root| {
            root.items("instanceTypeOfferingSet")
                .map(|o| (o.string("instanceType"), o.string("location")))
                .collect()
        })
        .await
    }

    /// Ubuntu 24.04's newest image for `arch` (`x86` or `arm`), from
    /// Canonical's own account.
    pub async fn latest_ubuntu(&self, region: &str, arch: &str) -> Result<Image, ProviderError> {
        let name = format!(
            "ubuntu/images/hvm-ssd*/ubuntu-noble-24.04-{}-server-*",
            if arch == "arm" { "arm64" } else { "amd64" }
        );
        let params = Params::new()
            .set("Owner.1", "099720109477")
            .filters(&[("name", &[name.as_str()]), ("state", &["available"])]);
        let root = self.ec2(region, "DescribeImages", &params).await?;
        root.items("imagesSet")
            .max_by(|a, b| a.string("creationDate").cmp(&b.string("creationDate")))
            .map(|i| Image {
                id: i.string("imageId"),
                root_device: i.string("rootDeviceName"),
            })
            .ok_or_else(|| ProviderError::Api {
                code: "no_image".into(),
                message: format!("no Ubuntu 24.04 image found in {region}"),
            })
    }

    pub async fn addresses(
        &self,
        region: &str,
        filters: &[(&str, &[&str])],
    ) -> Result<Vec<Address>, ProviderError> {
        let params = Params::new().filters(filters);
        let root = self.ec2(region, "DescribeAddresses", &params).await?;
        Ok(root
            .items("addressesSet")
            .map(|a| Address {
                public_ip: a.string("publicIp"),
                allocation_id: a.string("allocationId"),
                association_id: a.text_of("associationId").map(str::to_owned),
                instance_id: a.text_of("instanceId").map(str::to_owned),
                tags: tags_of(a),
            })
            .collect())
    }

    pub async fn allocate_address(
        &self,
        region: &str,
        tags: &BTreeMap<String, String>,
    ) -> Result<Address, ProviderError> {
        let params = Params::new()
            .set("Domain", "vpc")
            .tag_spec(1, "elastic-ip", tags);
        let root = self.ec2(region, "AllocateAddress", &params).await?;
        Ok(Address {
            public_ip: root.string("publicIp"),
            allocation_id: root.string("allocationId"),
            tags: tags.clone(),
            ..Default::default()
        })
    }

    pub async fn associate_address(
        &self,
        region: &str,
        allocation: &str,
        instance: &str,
    ) -> Result<(), ProviderError> {
        let params = Params::new()
            .set("AllocationId", allocation)
            .set("InstanceId", instance)
            .set("AllowReassociation", "false");
        self.ec2(region, "AssociateAddress", &params)
            .await
            .map(|_| ())
    }

    /// Succeeds when it is already disassociated.
    pub async fn disassociate_address(
        &self,
        region: &str,
        association: &str,
    ) -> Result<(), ProviderError> {
        let params = Params::new().set("AssociationId", association);
        match self.ec2(region, "DisassociateAddress", &params).await {
            Err(ProviderError::Api { code, .. }) if code.ends_with(".NotFound") => Ok(()),
            r => r.map(|_| ()),
        }
    }

    /// Succeeds when it is already released.
    pub async fn release_address(
        &self,
        region: &str,
        allocation: &str,
    ) -> Result<(), ProviderError> {
        let params = Params::new().set("AllocationId", allocation);
        match self.ec2(region, "ReleaseAddress", &params).await {
            Err(ProviderError::Api { code, .. }) if code.ends_with(".NotFound") => Ok(()),
            r => r.map(|_| ()),
        }
    }

    pub async fn create_tags(
        &self,
        region: &str,
        resource: &str,
        tags: &BTreeMap<String, String>,
    ) -> Result<(), ProviderError> {
        let params = Params::new().set("ResourceId.1", resource).tags(tags);
        self.ec2(region, "CreateTags", &params).await.map(|_| ())
    }

    pub async fn run_instance(
        &self,
        region: &str,
        run: &RunInstance,
    ) -> Result<Instance, ProviderError> {
        let mut params = Params::new()
            .set("ClientToken", run.client_token.as_str())
            .set("ImageId", run.image_id.as_str())
            .set("InstanceType", run.instance_type.as_str())
            .set("MinCount", "1")
            .set("MaxCount", "1")
            .set("SubnetId", run.subnet_id.as_str())
            .set("BlockDeviceMapping.1.DeviceName", run.root_device.as_str())
            .set(
                "BlockDeviceMapping.1.Ebs.VolumeSize",
                run.root_gb.to_string(),
            )
            .set("BlockDeviceMapping.1.Ebs.VolumeType", "gp3")
            .set("BlockDeviceMapping.1.Ebs.DeleteOnTermination", "true")
            .set("MetadataOptions.HttpTokens", "required")
            .set("MetadataOptions.HttpPutResponseHopLimit", "1")
            .set(
                "UserData",
                base64::engine::general_purpose::STANDARD.encode(&run.user_data),
            );
        for (i, g) in run.security_groups.iter().enumerate() {
            params = params.set(format!("SecurityGroupId.{}", i + 1), g.as_str());
        }
        if let Some(k) = &run.key_name {
            params = params.set("KeyName", k.as_str());
        }
        params = params
            .tag_spec(1, "instance", &run.tags)
            .tag_spec(2, "volume", &run.tags);
        let root = self.ec2(region, "RunInstances", &params).await?;
        root.items("instancesSet")
            .next()
            .map(Instance::from_node)
            .ok_or_else(|| ProviderError::Transport("RunInstances returned no instance".into()))
    }

    pub async fn instances(
        &self,
        region: &str,
        filters: &[(&str, &[&str])],
    ) -> Result<Vec<Instance>, ProviderError> {
        self.ec2_paged(
            region,
            "DescribeInstances",
            Params::new().filters(filters),
            read_instances,
        )
        .await
    }

    /// One instance; [`ProviderError::NotFound`] once AWS has forgotten it.
    pub async fn instance(&self, region: &str, id: &str) -> Result<Instance, ProviderError> {
        let params = Params::new().set("InstanceId.1", id);
        let root = self.ec2(region, "DescribeInstances", &params).await?;
        read_instances(&root)
            .into_iter()
            .next()
            .ok_or(ProviderError::NotFound)
    }

    /// Succeeds when it is already gone.
    pub async fn terminate(&self, region: &str, id: &str) -> Result<(), ProviderError> {
        let params = Params::new().set("InstanceId.1", id);
        match self.ec2(region, "TerminateInstances", &params).await {
            Err(ProviderError::NotFound) => Ok(()),
            r => r.map(|_| ()),
        }
    }

    pub async fn start(&self, region: &str, id: &str) -> Result<(), ProviderError> {
        let params = Params::new().set("InstanceId.1", id);
        self.ec2(region, "StartInstances", &params)
            .await
            .map(|_| ())
    }

    pub async fn stop(&self, region: &str, id: &str, force: bool) -> Result<(), ProviderError> {
        let mut params = Params::new().set("InstanceId.1", id);
        if force {
            params = params.set("Force", "true");
        }
        self.ec2(region, "StopInstances", &params).await.map(|_| ())
    }

    pub async fn reboot(&self, region: &str, id: &str) -> Result<(), ProviderError> {
        let params = Params::new().set("InstanceId.1", id);
        self.ec2(region, "RebootInstances", &params)
            .await
            .map(|_| ())
    }

    pub async fn modify_type(
        &self,
        region: &str,
        id: &str,
        instance_type: &str,
    ) -> Result<(), ProviderError> {
        let params = Params::new()
            .set("InstanceId", id)
            .set("InstanceType.Value", instance_type);
        self.ec2(region, "ModifyInstanceAttribute", &params)
            .await
            .map(|_| ())
    }

    /// Replaces the instance's security groups with `groups`.
    pub async fn modify_groups(
        &self,
        region: &str,
        id: &str,
        groups: &[String],
    ) -> Result<(), ProviderError> {
        let mut params = Params::new().set("InstanceId", id);
        for (i, g) in groups.iter().enumerate() {
            params = params.set(format!("GroupId.{}", i + 1), g.as_str());
        }
        self.ec2(region, "ModifyInstanceAttribute", &params)
            .await
            .map(|_| ())
    }

    pub async fn volume_size(
        &self,
        region: &str,
        volume: &str,
    ) -> Result<Option<u32>, ProviderError> {
        let params = Params::new().set("VolumeId.1", volume);
        let root = self.ec2(region, "DescribeVolumes", &params).await?;
        Ok(root
            .items("volumeSet")
            .next()
            .and_then(|v| v.text_of("size")?.parse().ok()))
    }

    /// Key pairs with their public halves, optionally just `name`.
    pub async fn key_pairs(
        &self,
        region: &str,
        name: Option<&str>,
    ) -> Result<Vec<KeyPair>, ProviderError> {
        let mut params = Params::new().set("IncludePublicKey", "true");
        if let Some(n) = name {
            params = params.set("KeyName.1", n);
        }
        let root = self.ec2(region, "DescribeKeyPairs", &params).await?;
        Ok(root
            .items("keySet")
            .map(|k| KeyPair {
                name: k.string("keyName"),
                fingerprint: k.string("keyFingerprint"),
                public_key: k.string("publicKey"),
            })
            .collect())
    }

    /// Uploads `public_key` (OpenSSH format) under `name`.
    pub async fn import_key(
        &self,
        region: &str,
        name: &str,
        public_key: &str,
    ) -> Result<(), ProviderError> {
        let params = Params::new().set("KeyName", name).set(
            "PublicKeyMaterial",
            base64::engine::general_purpose::STANDARD.encode(public_key),
        );
        self.ec2(region, "ImportKeyPair", &params).await.map(|_| ())
    }

    pub async fn security_groups(
        &self,
        region: &str,
        filters: &[(&str, &[&str])],
    ) -> Result<Vec<SecurityGroup>, ProviderError> {
        self.ec2_paged(
            region,
            "DescribeSecurityGroups",
            Params::new().filters(filters),
            |root| root.items("securityGroupInfo").map(read_group).collect(),
        )
        .await
    }

    pub async fn create_security_group(
        &self,
        region: &str,
        vpc: &str,
        name: &str,
        description: &str,
        tags: &BTreeMap<String, String>,
    ) -> Result<String, ProviderError> {
        let params = Params::new()
            .set("GroupName", name)
            .set("GroupDescription", description)
            .set("VpcId", vpc)
            .tag_spec(1, "security-group", tags);
        let root = self.ec2(region, "CreateSecurityGroup", &params).await?;
        Ok(root.string("groupId"))
    }

    /// Adds `rule`; a rule that already exists is not an error.
    pub async fn authorize(
        &self,
        region: &str,
        group: &str,
        rule: &IpRule,
    ) -> Result<(), ProviderError> {
        let params = Params::new().set("GroupId", group).permission(rule);
        match self
            .ec2(region, "AuthorizeSecurityGroupIngress", &params)
            .await
        {
            Err(ProviderError::Api { code, .. }) if code == "InvalidPermission.Duplicate" => Ok(()),
            r => r.map(|_| ()),
        }
    }

    /// Removes `rule`; one already gone is not an error.
    pub async fn revoke(
        &self,
        region: &str,
        group: &str,
        rule: &IpRule,
    ) -> Result<(), ProviderError> {
        let params = Params::new().set("GroupId", group).permission(&IpRule {
            description: None,
            ..rule.clone()
        });
        match self
            .ec2(region, "RevokeSecurityGroupIngress", &params)
            .await
        {
            Err(ProviderError::Api { code, .. }) if code == "InvalidPermission.NotFound" => Ok(()),
            r => r.map(|_| ()),
        }
    }
}

fn read_instances(root: &Node) -> Vec<Instance> {
    root.items("reservationSet")
        .flat_map(|r| r.items("instancesSet"))
        .map(Instance::from_node)
        .collect()
}

fn read_group(n: &Node) -> SecurityGroup {
    SecurityGroup {
        id: n.string("groupId"),
        name: n.string("groupName"),
        vpc_id: n.string("vpcId"),
        tags: tags_of(n),
        permissions: n
            .items("ipPermissions")
            .map(|p| Permission {
                protocol: p.string("ipProtocol"),
                from: p
                    .text_of("fromPort")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(-1),
                to: p
                    .text_of("toPort")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(-1),
                ipv4: p.items("ipRanges").map(|r| r.string("cidrIp")).collect(),
                other: p.items("ipv6Ranges").count()
                    + p.items("prefixListIds").count()
                    + p.items("groups").count(),
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_bodies_encode_per_rfc3986() {
        let body = Params::new()
            .set("UserData", "ab+/=")
            .set("Filter.1.Value.1", "ubuntu/images hvm*")
            .body("DescribeImages");
        assert_eq!(
            body,
            "Action=DescribeImages&Version=2016-11-15&UserData=ab%2B%2F%3D&Filter.1.Value.1=ubuntu%2Fimages%20hvm%2A"
        );
    }

    #[test]
    fn errors_map_to_provider_errors() {
        let m = |c: &str| map_error(c.to_owned(), "x".to_owned());
        assert!(matches!(m("AuthFailure"), ProviderError::Unauthorized));
        assert!(matches!(
            m("UnauthorizedOperation"),
            ProviderError::Forbidden
        ));
        assert!(matches!(
            m("InvalidInstanceID.NotFound"),
            ProviderError::NotFound
        ));
        assert!(matches!(
            m("AddressLimitExceeded"),
            ProviderError::Api { code, .. } if code == "AddressLimitExceeded"
        ));
    }

    #[test]
    fn throttling_is_retried_and_failures_are_not() {
        assert!(retryable(StatusCode::BAD_REQUEST, "RequestLimitExceeded"));
        assert!(retryable(StatusCode::SERVICE_UNAVAILABLE, "Unavailable"));
        assert!(!retryable(StatusCode::BAD_REQUEST, "InvalidParameterValue"));
    }

    #[test]
    fn describe_instances_reads_the_fields_used() {
        let doc = br#"<DescribeInstancesResponse xmlns="http://ec2.amazonaws.com/doc/2016-11-15/">
          <reservationSet><item><instancesSet><item>
            <instanceId>i-1234</instanceId>
            <instanceState><code>16</code><name>running</name></instanceState>
            <instanceType>t3.small</instanceType>
            <ipAddress>203.0.113.9</ipAddress>
            <vpcId>vpc-1</vpcId>
            <rootDeviceName>/dev/sda1</rootDeviceName>
            <blockDeviceMapping><item><deviceName>/dev/sda1</deviceName><ebs><volumeId>vol-1</volumeId></ebs></item></blockDeviceMapping>
            <groupSet><item><groupId>sg-1</groupId><groupName>a</groupName></item></groupSet>
            <tagSet><item><key>modula-id</key><value>abc</value></item></tagSet>
            <stateReason><code>Client.UserInitiatedShutdown</code><message>m</message></stateReason>
          </item></instancesSet></item></reservationSet>
        </DescribeInstancesResponse>"#;
        let root = xml::parse(doc).unwrap();
        let i = &read_instances(&root)[0];
        assert_eq!(i.id, "i-1234");
        assert_eq!(i.state, "running");
        assert_eq!(i.public_ip.as_deref(), Some("203.0.113.9"));
        assert_eq!(i.tag("modula-id"), Some("abc"));
        assert_eq!(i.volumes["/dev/sda1"], "vol-1");
        assert_eq!(i.groups, ["sg-1"]);
        assert_eq!(
            i.state_reason.as_ref().unwrap().0,
            "Client.UserInitiatedShutdown"
        );
    }

    #[test]
    fn security_group_permissions_count_every_source() {
        let doc = br#"<r><securityGroupInfo><item><groupId>sg-1</groupId><ipPermissions><item>
            <ipProtocol>udp</ipProtocol><fromPort>51820</fromPort><toPort>51820</toPort>
            <ipRanges><item><cidrIp>203.0.113.1/32</cidrIp></item><item><cidrIp>203.0.113.2/32</cidrIp></item></ipRanges>
            <ipv6Ranges><item><cidrIpv6>::/0</cidrIpv6></item></ipv6Ranges>
          </item></ipPermissions></item></securityGroupInfo></r>"#;
        let root = xml::parse(doc).unwrap();
        let g: Vec<_> = root.items("securityGroupInfo").map(read_group).collect();
        assert_eq!(g[0].permissions[0].ipv4.len(), 2);
        assert_eq!(g[0].permissions[0].other, 1);
    }

    #[test]
    fn iam_policy_grants_every_action_the_client_calls() {
        // Every call site is `self.ec2(…, "Action", …)` or `ec2_paged`.
        let source: String = include_str!("lib.rs").split_whitespace().collect();
        let policy: Value = serde_json::from_str(MINIMAL_IAM_POLICY).unwrap();
        let granted: Vec<&str> = policy["Statement"][0]["Action"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a.as_str().unwrap())
            .collect();
        assert!(granted.contains(&"pricing:GetProducts"));
        let mut checked = 0;
        for piece in source
            .split("region,\"")
            .skip(1)
            .chain(source.split("HOME_REGION,\"").skip(1))
        {
            let action = piece.split('"').next().unwrap();
            if action.chars().next().is_some_and(char::is_uppercase) {
                assert!(
                    granted.contains(&format!("ec2:{action}").as_str()),
                    "{action} is missing from the IAM policy"
                );
                checked += 1;
            }
        }
        assert!(checked >= 20, "only {checked} call sites found");
    }
}
