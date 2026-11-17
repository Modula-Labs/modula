//! EC2 behind [`ServerProvider`]: one Elastic IP and one set of security
//! groups per server, tags instead of labels, ids of the form `region/i-abc`.

use std::collections::{BTreeMap, BTreeSet};

use futures::future::BoxFuture;
use futures::{FutureExt, StreamExt, stream};
use modula_provider::locations::{self, Location};
use modula_provider::{
    ActionState, CreatedServer, LocationPrice, MeshSources, NewServer, Phase, Power, ProviderError,
    ProviderLocation, ProviderServer, ProviderSshKey, ServerProvider, ServerState, ServerTypeOffer,
};
use tokio::sync::OnceCell;

use crate::firewall::{self, GROUPS_PER_INSTANCE, Slot, WG_PORT};
use crate::pricing::{self, DEFAULT_ROOT_GB, MAX_ROOT_GB, MIN_ROOT_GB};
use crate::{Address, Client, Instance, IpRule, Permission, RunInstance, SecurityGroup, sigv4};

pub const KIND: &str = "aws";
/// AWS's limit on user data, before base64.
pub const USER_DATA_LIMIT: usize = 16 * 1024;
/// How long a server waits for its Elastic IP (the join token lasts 30 minutes).
const EIP_WAIT_ATTEMPTS: u32 = 500;
const CONCURRENCY: usize = 8;
const SLOT_TAG: &str = "modula-slot";
const ID_TAG: &str = "modula-id";
const INSTANCE_TAG: &str = "modula-instance";
/// [`ServerProvider::resize`] happens at once on EC2: it returns this id.
const DONE_ACTION: &str = "done";
const LIVE_STATES: &[&str] = &["pending", "running", "stopping", "stopped"];
/// Release retries while AWS still counts the address as in use.
const RELEASE_ATTEMPTS: u32 = 5;
/// EC2's error when a zone has none of the instance type free.
const NO_CAPACITY: &str = "InsufficientInstanceCapacity";

fn api(code: &str, message: impl Into<String>) -> ProviderError {
    ProviderError::Api {
        code: code.to_owned(),
        message: message.into(),
    }
}

fn split_id(id: &str) -> Result<(&str, &str), ProviderError> {
    id.split_once('/').ok_or(ProviderError::NotFound)
}

fn phase(state: &str) -> Phase {
    match state {
        "running" => Phase::Running,
        "stopping" => Phase::Stopping,
        "stopped" => Phase::Off,
        "shutting-down" | "terminated" => Phase::Gone,
        _ => Phase::Booting,
    }
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// `script` with, right after its shebang, what must happen before anything
/// else: install `extra_keys` (EC2 launches with one key pair) and wait
/// until the instance's public address is `eip`, so it never registers the
/// temporary one. Exits on timeout rather than join with the wrong address.
pub fn user_data_for(script: &str, eip: &str, extra_keys: &[String]) -> String {
    let (shebang, rest) = match script.split_once('\n') {
        Some((first, rest)) if first.starts_with("#!") => (first, rest),
        _ => ("#!/bin/bash", script),
    };
    let mut keys = String::new();
    if !extra_keys.is_empty() {
        keys.push_str("install -d -m 700 -o ubuntu -g ubuntu /home/ubuntu/.ssh\n");
        for key in extra_keys {
            keys.push_str(&format!(
                "printf '%s\\n' {} >> /home/ubuntu/.ssh/authorized_keys\n",
                shell_quote(key)
            ));
        }
        keys.push_str("chown ubuntu:ubuntu /home/ubuntu/.ssh/authorized_keys\n");
    }
    let eip = shell_quote(eip);
    format!(
        r#"{shebang}
{keys}# Wait for this server's Elastic IP before doing anything else.
eip={eip}
imds=http://169.254.169.254/latest
ip=
for _ in $(seq 1 {EIP_WAIT_ATTEMPTS}); do
  t=$(curl -fsS -m 2 -X PUT -H 'X-aws-ec2-metadata-token-ttl-seconds: 60' "$imds/api/token" || true)
  ip=$(curl -fsS -m 2 -H "X-aws-ec2-metadata-token: $t" "$imds/meta-data/public-ipv4" || true)
  [ "$ip" = "$eip" ] && break
  sleep 3
done
if [ "$ip" != "$eip" ]; then echo "Elastic IP $eip was never attached (saw '$ip')" >&2; exit 1; fi
{rest}"#
    )
}

/// The first two fields of an OpenSSH public key, without its comment.
fn key_material(public_key: &str) -> String {
    public_key
        .split_whitespace()
        .take(2)
        .collect::<Vec<_>>()
        .join(" ")
}

/// What identifies one firewall's groups, and how they are made.
struct GroupSet {
    tags: BTreeMap<String, String>,
    name: String,
    fixed: Vec<IpRule>,
}

impl GroupSet {
    fn filters(&self) -> Vec<(String, String)> {
        self.tags
            .iter()
            .map(|(k, v)| (format!("tag:{k}"), v.clone()))
            .collect()
    }
}

/// The non-WireGuard rule count and WireGuard sources of a group.
fn slot_of(group: &SecurityGroup) -> Slot {
    let mut slot = Slot::default();
    for p in &group.permissions {
        if is_mesh(p) {
            slot.mesh.extend(p.ipv4.iter().cloned());
            slot.others += p.other;
        } else {
            slot.others += p.ipv4.len() + p.other;
        }
    }
    slot
}

fn is_mesh(p: &Permission) -> bool {
    p.protocol == "udp" && p.from == WG_PORT && p.to == WG_PORT
}

fn has_rule(group: &SecurityGroup, rule: &IpRule) -> bool {
    group.permissions.iter().any(|p| {
        p.protocol == rule.protocol
            && p.from == rule.from
            && p.to == rule.to
            && rule.cidrs.iter().all(|c| p.ipv4.contains(c))
    })
}

fn slot_number(group: &SecurityGroup) -> Option<usize> {
    group.tags.get(SLOT_TAG)?.parse().ok()
}

pub struct Ec2Provider {
    client: Client,
    workspace: String,
    /// Public IPs allowed to the WireGuard port on a firewall being made.
    mesh_sources: Vec<String>,
    regions: OnceCell<Vec<String>>,
}

impl Ec2Provider {
    pub fn new(client: Client, workspace: impl Into<String>, mesh_sources: Vec<String>) -> Self {
        Self {
            client,
            workspace: workspace.into(),
            mesh_sources,
            regions: OnceCell::new(),
        }
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    /// Enabled regions Modula has in its catalog.
    async fn regions(&self) -> Result<&Vec<String>, ProviderError> {
        self.regions
            .get_or_try_init(|| async {
                Ok(self
                    .client
                    .regions()
                    .await?
                    .into_iter()
                    .filter(|r| location_of(r).is_some())
                    .collect())
            })
            .await
    }

    fn worker_set(&self, ssh: bool) -> GroupSet {
        let ssh_label = if ssh { "1" } else { "0" };
        GroupSet {
            tags: BTreeMap::from([
                ("modula".to_owned(), "1".to_owned()),
                ("workspace".to_owned(), self.workspace.clone()),
                ("ssh".to_owned(), ssh_label.to_owned()),
            ]),
            name: format!("modula-{}{}", self.workspace, if ssh { "-ssh" } else { "" }),
            fixed: firewall::worker_rules(ssh),
        }
    }

    fn main_set(name: &str, port: u16) -> GroupSet {
        GroupSet {
            tags: BTreeMap::from([
                ("modula".to_owned(), "1".to_owned()),
                ("role".to_owned(), "main".to_owned()),
                ("server".to_owned(), name.to_owned()),
            ]),
            name: format!("modula-main-{name}"),
            fixed: firewall::main_rules(port),
        }
    }

    /// Finds or creates every group of `set`, in slot order, and brings its
    /// fixed rules and WireGuard sources up to date.
    async fn ensure_groups(
        &self,
        region: &str,
        vpc: &str,
        set: &GroupSet,
    ) -> Result<Vec<SecurityGroup>, ProviderError> {
        let filters = set.filters();
        let mut query: Vec<(&str, Vec<&str>)> = filters
            .iter()
            .map(|(k, v)| (k.as_str(), vec![v.as_str()]))
            .collect();
        query.push(("vpc-id", vec![vpc]));
        let query: Vec<(&str, &[&str])> = query.iter().map(|(k, v)| (*k, v.as_slice())).collect();

        let mut slots: BTreeMap<usize, SecurityGroup> = self
            .client
            .security_groups(region, &query)
            .await?
            .into_iter()
            .filter_map(|g| Some((slot_number(&g)?, g)))
            .collect();
        for slot in 0..GROUPS_PER_INSTANCE {
            if slots.contains_key(&slot) {
                continue;
            }
            let mut tags = set.tags.clone();
            tags.insert(SLOT_TAG.to_owned(), slot.to_string());
            let name = format!("{}-{slot}", set.name);
            let description = format!("Modula: {name}");
            let created = self
                .client
                .create_security_group(region, vpc, &name, &description, &tags)
                .await;
            let id = match created {
                Ok(id) => id,
                // Another server being created in this workspace made it first.
                Err(ProviderError::Api { code, .. }) if code == "InvalidGroup.Duplicate" => {
                    let raced = self
                        .client
                        .security_groups(region, &query)
                        .await?
                        .into_iter()
                        .find(|g| slot_number(g) == Some(slot));
                    let Some(group) = raced else {
                        return Err(api(
                            "security_group_name_taken",
                            format!(
                                "a security group named {name} already exists in {vpc} and isn't Modula's; rename or delete it"
                            ),
                        ));
                    };
                    slots.insert(slot, group);
                    continue;
                }
                Err(e) => return Err(e),
            };
            slots.insert(
                slot,
                SecurityGroup {
                    id,
                    name,
                    vpc_id: vpc.to_owned(),
                    tags,
                    permissions: Vec::new(),
                },
            );
        }
        let mut groups: Vec<SecurityGroup> = slots.into_values().collect();
        for rule in &set.fixed {
            if !has_rule(&groups[0], rule) {
                self.client.authorize(region, &groups[0].id, rule).await?;
                groups[0].permissions.push(Permission {
                    protocol: rule.protocol.clone(),
                    from: rule.from,
                    to: rule.to,
                    ipv4: rule.cidrs.clone(),
                    other: 0,
                });
            }
        }
        self.reconcile_mesh(region, &groups, &self.mesh_sources)
            .await?;
        Ok(groups)
    }

    /// Applies the difference between `groups`' WireGuard rules and `sources`.
    async fn reconcile_mesh(
        &self,
        region: &str,
        groups: &[SecurityGroup],
        sources: &[String],
    ) -> Result<(), ProviderError> {
        let slots: Vec<Slot> = groups.iter().map(slot_of).collect();
        let desired = firewall::mesh_cidrs(sources);
        let plan = firewall::plan_mesh(&slots, &desired).map_err(|fits| {
            api(
                "security_group_capacity",
                format!(
                    "this firewall needs {} WireGuard sources but its {} security groups hold only {fits} (AWS allows 60 rules per group and 5 groups per instance); ask AWS to raise those quotas or remove servers",
                    desired.len(),
                    groups.len(),
                ),
            )
        })?;
        for (group, change) in groups.iter().zip(plan) {
            if !change.revoke.is_empty() {
                self.client
                    .revoke(region, &group.id, &firewall::mesh_rule(change.revoke))
                    .await?;
            }
            if !change.authorize.is_empty() {
                self.client
                    .authorize(region, &group.id, &firewall::mesh_rule(change.authorize))
                    .await?;
            }
        }
        Ok(())
    }

    /// Runs `f` for every enabled region, a few at a time.
    async fn each_region<'a, T, Fut>(
        &'a self,
        f: impl Fn(String) -> Fut,
    ) -> Result<Vec<(String, Result<T, ProviderError>)>, ProviderError>
    where
        Fut: Future<Output = Result<T, ProviderError>> + 'a,
    {
        let regions = self.regions().await?;
        Ok(stream::iter(regions.iter().cloned())
            .map(|r| {
                let call = f(r.clone());
                async move { (r, call.await) }
            })
            .buffer_unordered(CONCURRENCY)
            .collect()
            .await)
    }

    /// The default subnets whose zone offers `instance_type`, in zone order:
    /// a launch tries the next when a zone is out of capacity.
    async fn subnets_for(
        &self,
        region: &str,
        vpc: &str,
        instance_type: &str,
    ) -> Result<Vec<String>, ProviderError> {
        let mut subnets = self.client.default_subnets(region, vpc).await?;
        if subnets.is_empty() {
            return Err(no_default_vpc(region));
        }
        let zones: BTreeSet<String> = self
            .client
            .offerings(region, "availability-zone", &[instance_type])
            .await?
            .into_iter()
            .map(|(_, zone)| zone)
            .collect();
        subnets.sort_by(|a, b| a.zone.cmp(&b.zone));
        let usable: Vec<String> = subnets
            .into_iter()
            .filter(|s| zones.contains(&s.zone))
            .map(|s| s.id)
            .collect();
        if usable.is_empty() {
            return Err(api(
                "instance_type_unavailable",
                format!("{instance_type} isn't offered in any default subnet of {region}"),
            ));
        }
        Ok(usable)
    }

    /// Launch in the first of `subnets` whose zone has capacity. A zone out
    /// of capacity launched nothing, so the next is safe to try; each try
    /// gets its own client token, as EC2 refuses a token reused with other
    /// parameters, and the tokens are the same on every call so a repeated
    /// create still finds what an earlier one launched.
    async fn launch_in_any_zone(
        &self,
        region: &str,
        run: &mut RunInstance,
        subnets: &[String],
    ) -> Result<Instance, ProviderError> {
        let base = run.client_token.clone();
        for (i, subnet) in subnets.iter().enumerate() {
            run.subnet_id = subnet.clone();
            run.client_token = if i == 0 {
                base.clone()
            } else {
                format!("{base}-{i}")
            };
            match self.client.run_instance(region, run).await {
                Err(ProviderError::Api { code, .. }) if code == NO_CAPACITY => {
                    tracing::info!(region, subnet = %subnet, "zone out of capacity, trying the next");
                }
                other => return other,
            }
        }
        Err(api(
            NO_CAPACITY,
            format!(
                "AWS has no {} capacity in any zone of {region} right now; try again later, or another type or region",
                run.instance_type
            ),
        ))
    }

    /// The key pair to launch with in `region` (importing it when it only
    /// exists elsewhere) and the public halves of the others.
    async fn resolve_keys(
        &self,
        region: &str,
        ids: &[String],
    ) -> Result<(Option<String>, Vec<String>), ProviderError> {
        let Some((first, others)) = ids.split_first() else {
            return Ok((None, Vec::new()));
        };
        let mut extra = Vec::new();
        for id in others {
            extra.push(self.public_key(id).await?);
        }
        let (source, name) = split_key_id(first)?;
        if source == region {
            return Ok((Some(name.to_owned()), extra));
        }
        let public_key = self.public_key(first).await?;
        let here = self.client.key_pairs(region, None).await?;
        let material = key_material(&public_key);
        if let Some(k) = here
            .iter()
            .find(|k| key_material(&k.public_key) == material)
        {
            return Ok((Some(k.name.clone()), extra));
        }
        let name = if here.iter().any(|k| k.name == name) {
            format!("{name}-{}", &sigv4::sha256_hex(material.as_bytes())[..8])
        } else {
            name.to_owned()
        };
        self.client.import_key(region, &name, &public_key).await?;
        Ok((Some(name), extra))
    }

    async fn public_key(&self, id: &str) -> Result<String, ProviderError> {
        let (source, name) = split_key_id(id)?;
        self.client
            .key_pairs(source, Some(name))
            .await?
            .into_iter()
            .next()
            .map(|k| k.public_key)
            .filter(|k| !k.is_empty())
            .ok_or_else(|| {
                api(
                    "invalid_ssh_key",
                    format!("{id:?} has no public key at AWS"),
                )
            })
    }

    async fn find_or_allocate(
        &self,
        region: &str,
        modula_id: &str,
        tags: &BTreeMap<String, String>,
    ) -> Result<Address, ProviderError> {
        let existing = self
            .client
            .addresses(region, &[(&format!("tag:{ID_TAG}"), &[modula_id])])
            .await?;
        if let Some(a) = existing.into_iter().next() {
            return Ok(a);
        }
        self.client.allocate_address(region, tags).await.map_err(|e| match e {
            ProviderError::Api { code, .. } if code == "AddressLimitExceeded" => api(
                "AddressLimitExceeded",
                format!(
                    "the Elastic IP quota of {region} is used up (AWS's default is 5 per region); release unused addresses or request an increase, then try again"
                ),
            ),
            e => e,
        })
    }

    async fn release(&self, region: &str, address: &Address) -> Result<(), ProviderError> {
        if let Some(association) = &address.association_id {
            self.client
                .disassociate_address(region, association)
                .await?;
        }
        let mut attempt = 0;
        loop {
            match self
                .client
                .release_address(region, &address.allocation_id)
                .await
            {
                Err(ProviderError::Api { code, .. })
                    if code == "InvalidIPAddress.InUse" && attempt + 1 < RELEASE_ATTEMPTS =>
                {
                    attempt += 1;
                    tokio::time::sleep(self.client.retry_base() * 2u32.pow(attempt)).await;
                }
                r => return r,
            }
        }
    }

    /// Releases the Elastic IPs of a server that is terminating or gone:
    /// found by the instance recorded on them, and by the server's id.
    async fn release_addresses_of(
        &self,
        region: &str,
        instance: &str,
        modula_id: Option<&str>,
    ) -> Result<(), ProviderError> {
        let mut found: BTreeMap<String, Address> = BTreeMap::new();
        let by_instance = self
            .client
            .addresses(region, &[(&format!("tag:{INSTANCE_TAG}"), &[instance])])
            .await?;
        found.extend(
            by_instance
                .into_iter()
                .map(|a| (a.allocation_id.clone(), a)),
        );
        if let Some(id) = modula_id {
            let by_id = self
                .client
                .addresses(region, &[(&format!("tag:{ID_TAG}"), &[id])])
                .await?;
            found.extend(by_id.into_iter().map(|a| (a.allocation_id.clone(), a)));
        }
        for address in found.values() {
            self.release(region, address).await?;
        }
        Ok(())
    }

    /// Attaches the server's Elastic IP when it is running and not yet
    /// attached. Returns its address once attached, or why it can't be.
    async fn settle_address(
        &self,
        region: &str,
        instance: &Instance,
    ) -> Result<(Option<String>, Option<String>), ProviderError> {
        let Some(modula_id) = instance.tag(ID_TAG) else {
            return Ok((instance.public_ip.clone(), None));
        };
        let addresses = self
            .client
            .addresses(region, &[(&format!("tag:{ID_TAG}"), &[modula_id])])
            .await?;
        let Some(address) = addresses.into_iter().next() else {
            return Ok((
                None,
                Some("the server's Elastic IP no longer exists at AWS".to_owned()),
            ));
        };
        match address.instance_id.as_deref() {
            Some(id) if id == instance.id => return Ok((Some(address.public_ip), None)),
            Some(other) => {
                return Ok((
                    None,
                    Some(format!(
                        "the server's Elastic IP is attached to {other} instead"
                    )),
                ));
            }
            None => {}
        }
        if instance.state != "running" {
            return Ok((None, None));
        }
        self.client
            .associate_address(region, &address.allocation_id, &instance.id)
            .await?;
        Ok((Some(address.public_ip), None))
    }

    async fn describe_instance(
        &self,
        region: &str,
        instance: &Instance,
    ) -> Result<ProviderServer, ProviderError> {
        let info = self
            .client
            .instance_types(region, &[instance.instance_type.as_str()])
            .await?
            .into_iter()
            .next();
        let disk_gb = match instance.volumes.get(&instance.root_device) {
            Some(volume) => self.client.volume_size(region, volume).await?.unwrap_or(0),
            None => 0,
        };
        Ok(ProviderServer {
            id: format!("{region}/{}", instance.id),
            name: instance.tag("Name").unwrap_or_default().to_owned(),
            ipv4: instance.public_ip.clone(),
            phase: phase(&instance.state),
            location: location_of(region),
            server_type: instance.instance_type.clone(),
            cores: info.as_ref().map_or(0, |i| i.vcpus),
            memory_gb: info.as_ref().map_or(0.0, |i| i.memory_mib as f64 / 1024.0),
            disk_gb,
            arch: info.as_ref().map(|i| i.arch.clone()).unwrap_or_default(),
            storage_type: "network".to_owned(),
            role: instance.tag("role").map(str::to_owned),
        })
    }

    /// Offers in `regions`: curated types priced where they can be ordered.
    async fn offers(&self, regions: &[String]) -> Result<Vec<ServerTypeOffer>, ProviderError> {
        let Some(home) = regions
            .iter()
            .find(|r| *r == crate::HOME_REGION)
            .or(regions.first())
        else {
            return Ok(Vec::new());
        };
        let patterns: Vec<String> = pricing::FAMILIES.iter().map(|f| format!("{f}.*")).collect();
        let patterns: Vec<&str> = patterns.iter().map(String::as_str).collect();
        let infos: Vec<_> = self
            .client
            .instance_types(home, &patterns)
            .await?
            .into_iter()
            .filter(|t| pricing::is_curated(&t.name))
            .collect();
        let names: Vec<String> = infos.iter().map(|i| i.name.clone()).collect();
        let prices = self.client.prices(&names).await?;
        let patterns = &patterns;
        let available: Vec<(String, String)> = stream::iter(regions.iter().cloned())
            .map(|region| async move { self.client.offerings(&region, "region", patterns).await })
            .buffer_unordered(CONCURRENCY)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .flatten()
            .collect();
        let mut offers: Vec<ServerTypeOffer> = infos
            .into_iter()
            .filter_map(|t| {
                let mut prices: Vec<LocationPrice> = available
                    .iter()
                    .filter(|(name, _)| *name == t.name)
                    .filter_map(|(_, region)| {
                        let rate = prices.get(&(t.name.clone(), region.clone()))?;
                        Some(LocationPrice {
                            location: location_of(region)?.id,
                            monthly_cents: pricing::monthly_cents(*rate, DEFAULT_ROOT_GB),
                        })
                    })
                    .collect();
                prices.sort_by_key(|p| p.location);
                (!prices.is_empty()).then(|| ServerTypeOffer {
                    description: t.name.clone(),
                    cores: t.vcpus,
                    memory_gb: t.memory_mib as f64 / 1024.0,
                    disk_gb: DEFAULT_ROOT_GB,
                    arch: t.arch,
                    cpu_type: if t.burstable { "shared" } else { "dedicated" }.to_owned(),
                    storage_type: "network".to_owned(),
                    name: t.name,
                    prices,
                })
            })
            .collect();
        offers.sort_by_key(|o| {
            (
                o.prices
                    .iter()
                    .map(|p| p.monthly_cents)
                    .min()
                    .unwrap_or(i64::MAX),
                o.name.clone(),
            )
        });
        Ok(offers)
    }
}

fn location_of(region: &str) -> Option<&'static Location> {
    locations::resolve(KIND, region, "").filter(|l| l.provider == KIND)
}

fn split_key_id(id: &str) -> Result<(&str, &str), ProviderError> {
    id.split_once('/')
        .ok_or_else(|| api("invalid_ssh_key", format!("{id:?} is not an AWS key id")))
}

fn no_default_vpc(region: &str) -> ProviderError {
    api(
        "no_default_vpc",
        format!(
            "AWS region {region} has no default VPC. Create one with `aws ec2 create-default-vpc --region {region}` (or in the VPC console: Actions, Create default VPC), then try again"
        ),
    )
}

/// The total for a root disk of `disk_gb` given the price at the default
/// size, or why that disk isn't allowed.
pub fn price_with_disk(monthly_cents: i64, disk_gb: u32) -> Result<i64, String> {
    if !(MIN_ROOT_GB..=MAX_ROOT_GB).contains(&disk_gb) {
        return Err(format!(
            "the root disk must be {MIN_ROOT_GB}–{MAX_ROOT_GB} GB"
        ));
    }
    Ok(monthly_cents + pricing::disk_delta_cents(disk_gb))
}

impl ServerProvider for Ec2Provider {
    fn kind(&self) -> &'static str {
        KIND
    }

    fn locations(&self) -> BoxFuture<'_, Result<Vec<ProviderLocation>, ProviderError>> {
        async move {
            Ok(self
                .regions()
                .await?
                .iter()
                .filter_map(|r| {
                    Some(ProviderLocation {
                        region: r.clone(),
                        location: location_of(r)?,
                    })
                })
                .collect())
        }
        .boxed()
    }

    fn server_types(&self) -> BoxFuture<'_, Result<Vec<ServerTypeOffer>, ProviderError>> {
        async move {
            let regions = self.regions().await?;
            self.offers(regions).await
        }
        .boxed()
    }

    fn server_types_at<'a>(
        &'a self,
        location_id: &'a str,
    ) -> BoxFuture<'a, Result<Vec<ServerTypeOffer>, ProviderError>> {
        async move {
            let Some(location) = locations::get(location_id).filter(|l| l.provider == KIND) else {
                return Ok(Vec::new());
            };
            let region = location.region.to_owned();
            if !self.regions().await?.contains(&region) {
                return Ok(Vec::new());
            }
            self.offers(std::slice::from_ref(&region)).await
        }
        .boxed()
    }

    fn create<'a>(
        &'a self,
        req: &'a NewServer,
    ) -> BoxFuture<'a, Result<CreatedServer, ProviderError>> {
        async move {
            let region = req.region.as_str();
            let modula_id = req
                .labels
                .get(ID_TAG)
                .ok_or_else(|| api("invalid_request", "a server needs a modula-id label"))?
                .clone();
            let root_gb = req.disk_gb.unwrap_or(DEFAULT_ROOT_GB);
            if !(MIN_ROOT_GB..=MAX_ROOT_GB).contains(&root_gb) {
                return Err(api(
                    "invalid_disk",
                    format!("the root disk must be {MIN_ROOT_GB}–{MAX_ROOT_GB} GB"),
                ));
            }
            let vpc = self
                .client
                .default_vpc(region)
                .await?
                .ok_or_else(|| no_default_vpc(region))?;
            let info = self
                .client
                .instance_types(region, &[req.server_type.as_str()])
                .await?
                .into_iter()
                .next()
                .ok_or_else(|| {
                    api(
                        "instance_type_unavailable",
                        format!("{} isn't offered in {region}", req.server_type),
                    )
                })?;
            let image = self.client.latest_ubuntu(region, &info.arch).await?;
            let subnets = self.subnets_for(region, &vpc, &req.server_type).await?;
            let groups = self
                .ensure_groups(region, &vpc, &self.worker_set(req.ssh))
                .await?;
            let (key_name, extra_keys) = self.resolve_keys(region, &req.ssh_key_ids).await?;

            // Everything that can reasonably fail has: only now take an
            // address, which bills from the moment it exists.
            let mut tags = req.labels.clone();
            tags.insert("Name".to_owned(), req.name.clone());
            let address = self.find_or_allocate(region, &modula_id, &tags).await?;

            let user_data = user_data_for(&req.user_data, &address.public_ip, &extra_keys);
            let mut run = RunInstance {
                client_token: modula_id.clone(),
                image_id: image.id,
                instance_type: req.server_type.clone(),
                subnet_id: subnets[0].clone(),
                security_groups: groups.iter().map(|g| g.id.clone()).collect(),
                key_name,
                root_device: image.root_device,
                root_gb,
                user_data,
                tags,
            };
            let launched = if run.user_data.len() > USER_DATA_LIMIT {
                Err(api(
                    "user_data_too_large",
                    format!(
                        "the boot script is {} bytes, over EC2's {USER_DATA_LIMIT}",
                        run.user_data.len()
                    ),
                ))
            } else {
                self.launch_in_any_zone(region, &mut run, &subnets).await
            };
            let instance = match launched {
                Ok(i) => i,
                Err(e) => {
                    // A lost response may still have launched it.
                    let live = self
                        .client
                        .instances(
                            region,
                            &[
                                (format!("tag:{ID_TAG}").as_str(), &[modula_id.as_str()]),
                                ("instance-state-name", LIVE_STATES),
                            ],
                        )
                        .await;
                    match live {
                        Ok(found) if !found.is_empty() => found.into_iter().next().ok_or(e)?,
                        Ok(_) => {
                            if let Err(release) = self.release(region, &address).await {
                                tracing::warn!(error = %release, "cannot release the Elastic IP of a failed server");
                            }
                            return Err(e);
                        }
                        // Unknown state: `find` releases it later if unused.
                        Err(_) => return Err(e),
                    }
                }
            };
            let record = BTreeMap::from([(INSTANCE_TAG.to_owned(), instance.id.clone())]);
            if let Err(e) = self
                .client
                .create_tags(region, &address.allocation_id, &record)
                .await
            {
                // Still found by its `modula-id` tag.
                tracing::warn!(error = %e, "cannot record the instance on its Elastic IP");
            }
            Ok(CreatedServer {
                id: format!("{region}/{}", instance.id),
                firewall_id: Some(groups[0].id.clone()),
                action_id: None,
                // Reported by `status` once attached.
                ipv4: None,
            })
        }
        .boxed()
    }

    fn find<'a>(
        &'a self,
        modula_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<CreatedServer>, ProviderError>> {
        async move {
            let results = self
                .each_region(|region| async move {
                    let region = region.as_str();
                    self.client
                        .instances(
                            region,
                            &[
                                (format!("tag:{ID_TAG}").as_str(), &[modula_id]),
                                ("instance-state-name", LIVE_STATES),
                            ],
                        )
                        .await
                })
                .await?;
            let mut hits = Vec::new();
            for (region, result) in results {
                hits.extend(result?.into_iter().map(|i| (region.clone(), i)));
            }
            if let Some((region, instance)) = hits.into_iter().next() {
                return Ok(Some(CreatedServer {
                    id: format!("{region}/{}", instance.id),
                    firewall_id: None,
                    action_id: None,
                    ipv4: None,
                }));
            }
            // No server, so any address tagged for it is orphaned by a crash
            // between allocating and launching; it would bill forever.
            let orphans = self
                .each_region(|region| async move {
                    let region = region.as_str();
                    self.client
                        .addresses(region, &[(format!("tag:{ID_TAG}").as_str(), &[modula_id])])
                        .await
                })
                .await?;
            for (region, addresses) in orphans {
                for address in addresses? {
                    if address.association_id.is_none() {
                        self.release(&region, &address).await?;
                    }
                }
            }
            Ok(None)
        }
        .boxed()
    }

    fn delete<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), ProviderError>> {
        async move {
            let (region, instance) = split_id(id)?;
            let modula_id = match self.client.instance(region, instance).await {
                Ok(i) => i.tag(ID_TAG).map(str::to_owned),
                Err(ProviderError::NotFound) => None,
                Err(e) => return Err(e),
            };
            self.client.terminate(region, instance).await?;
            self.release_addresses_of(region, instance, modula_id.as_deref())
                .await
        }
        .boxed()
    }

    fn power<'a>(&'a self, id: &'a str, op: Power) -> BoxFuture<'a, Result<(), ProviderError>> {
        async move {
            let (region, instance) = split_id(id)?;
            match op {
                Power::On => self.client.start(region, instance).await,
                Power::Off => self.client.stop(region, instance, false).await,
                Power::ForceOff => self.client.stop(region, instance, true).await,
                Power::Reboot => self.client.reboot(region, instance).await,
            }
        }
        .boxed()
    }

    fn status<'a>(
        &'a self,
        id: &'a str,
        _action_id: Option<&'a str>,
    ) -> BoxFuture<'a, Result<ServerState, ProviderError>> {
        async move {
            let (region, instance_id) = split_id(id)?;
            let gone = ServerState {
                phase: Phase::Gone,
                ipv4: None,
                error: None,
            };
            let instance = match self.client.instance(region, instance_id).await {
                Ok(i) => i,
                Err(ProviderError::NotFound) => {
                    if let Err(e) = self.release_addresses_of(region, instance_id, None).await {
                        tracing::warn!(error = %e, "cannot release the Elastic IP of a gone server");
                    }
                    return Ok(gone);
                }
                Err(e) => return Err(e),
            };
            let phase = phase(&instance.state);
            if phase == Phase::Gone {
                // The server isn't coming back: don't keep billing its address.
                if let Err(e) = self
                    .release_addresses_of(region, instance_id, instance.tag(ID_TAG))
                    .await
                {
                    tracing::warn!(error = %e, "cannot release the Elastic IP of a gone server");
                }
                let error = instance
                    .state_reason
                    .filter(|(code, _)| code != "Client.UserInitiatedShutdown")
                    .map(|(code, message)| format!("{message} ({code})"));
                return Ok(ServerState { error, ..gone });
            }
            let (ipv4, error) = self.settle_address(region, &instance).await?;
            Ok(ServerState { phase, ipv4, error })
        }
        .boxed()
    }

    fn describe<'a>(
        &'a self,
        id: &'a str,
    ) -> BoxFuture<'a, Result<Option<ProviderServer>, ProviderError>> {
        async move {
            let (region, instance) = split_id(id)?;
            match self.client.instance(region, instance).await {
                Ok(i) if i.state == "terminated" => Ok(None),
                Ok(i) => Ok(Some(self.describe_instance(region, &i).await?)),
                Err(ProviderError::NotFound) => Ok(None),
                Err(e) => Err(e),
            }
        }
        .boxed()
    }

    fn find_by_address<'a>(
        &'a self,
        ipv4: &'a str,
    ) -> BoxFuture<'a, Result<Option<ProviderServer>, ProviderError>> {
        async move {
            let results = self
                .each_region(|region| async move {
                    let region = region.as_str();
                    self.client
                        .instances(
                            region,
                            &[
                                ("tag:modula", &["1"]),
                                ("ip-address", &[ipv4]),
                                ("instance-state-name", LIVE_STATES),
                            ],
                        )
                        .await
                })
                .await?;
            let mut found = Vec::new();
            for (region, result) in results {
                found.extend(result?.into_iter().map(|i| (region.clone(), i)));
            }
            found.sort_by_key(|(_, i)| i.tag("role") != Some("main"));
            match found.first() {
                Some((region, i)) => Ok(Some(self.describe_instance(region, i).await?)),
                None => Ok(None),
            }
        }
        .boxed()
    }

    fn resize_blocker(&self, server: &ProviderServer, to: &ServerTypeOffer) -> Option<String> {
        (!server.arch.is_empty() && !to.arch.is_empty() && server.arch != to.arch).then(|| {
            format!(
                "{} is {}, this server is {}: the operating system can't move between architectures",
                to.name,
                arch_name(&to.arch),
                arch_name(&server.arch)
            )
        })
    }

    fn resize<'a>(
        &'a self,
        id: &'a str,
        server_type: &'a str,
    ) -> BoxFuture<'a, Result<String, ProviderError>> {
        async move {
            let (region, instance) = split_id(id)?;
            self.client
                .modify_type(region, instance, server_type)
                .await?;
            Ok(DONE_ACTION.to_owned())
        }
        .boxed()
    }

    fn action<'a>(
        &'a self,
        action_id: &'a str,
    ) -> BoxFuture<'a, Result<ActionState, ProviderError>> {
        async move {
            if action_id == DONE_ACTION {
                Ok(ActionState::Done)
            } else {
                Err(ProviderError::NotFound)
            }
        }
        .boxed()
    }

    fn ssh_keys(&self) -> BoxFuture<'_, Result<Vec<ProviderSshKey>, ProviderError>> {
        async move {
            let results = self
                .each_region(|region| async move { self.client.key_pairs(&region, None).await })
                .await?;
            let mut by_region: Vec<(String, Vec<_>)> = results
                .into_iter()
                .map(|(r, keys)| Ok((r, keys?)))
                .collect::<Result<_, ProviderError>>()?;
            by_region.sort_by(|(a, _), (b, _)| a.cmp(b));
            // The same key imported into several regions is listed once.
            let mut seen = BTreeSet::new();
            let mut out = Vec::new();
            for (region, keys) in by_region {
                for k in keys {
                    if seen.insert((k.name.clone(), key_material(&k.public_key))) {
                        out.push(ProviderSshKey {
                            id: format!("{region}/{}", k.name),
                            name: k.name,
                            fingerprint: k.fingerprint,
                            public_key: Some(k.public_key),
                        });
                    }
                }
            }
            Ok(out)
        }
        .boxed()
    }

    fn sync_mesh<'a>(
        &'a self,
        sources: &'a MeshSources,
    ) -> BoxFuture<'a, Result<(), ProviderError>> {
        async move {
            let results = self
                .each_region(|region| async move {
                    let region = region.as_str();
                    let groups = self
                        .client
                        .security_groups(region, &[("tag:modula", &["1"])])
                        .await?;
                    let mut sets: BTreeMap<(String, String), BTreeMap<usize, SecurityGroup>> =
                        BTreeMap::new();
                    for g in groups {
                        let Some(slot) = slot_number(&g) else {
                            continue;
                        };
                        let set = if g.tags.get("role").map(String::as_str) == Some("main") {
                            format!("main-{}", g.tags.get("server").cloned().unwrap_or_default())
                        } else if g.tags.get("workspace") == Some(&self.workspace) {
                            format!("worker-{}", g.tags.get("ssh").cloned().unwrap_or_default())
                        } else {
                            continue;
                        };
                        sets.entry((g.vpc_id.clone(), set))
                            .or_default()
                            .insert(slot, g);
                    }
                    // Groups on our own main servers: the account may hold
                    // another instance's main server, tagged `role=main` too.
                    let mains: Vec<&str> = sources.mains.iter().map(String::as_str).collect();
                    let ours: BTreeSet<String> =
                        if mains.is_empty() || !sets.keys().any(|(_, s)| s.starts_with("main-")) {
                            BTreeSet::new()
                        } else {
                            self.client
                                .instances(
                                    region,
                                    &[("ip-address", &mains), ("instance-state-name", LIVE_STATES)],
                                )
                                .await?
                                .into_iter()
                                .flat_map(|i| i.groups)
                                .collect()
                        };
                    let mut failed = Vec::new();
                    for ((_, set), groups) in sets {
                        let wanted = if set.starts_with("main-") {
                            if !groups.values().any(|g| ours.contains(&g.id)) {
                                continue;
                            }
                            &sources.everyone
                        } else {
                            &sources.workspace
                        };
                        let groups: Vec<SecurityGroup> = groups.into_values().collect();
                        if let Err(e) = self.reconcile_mesh(region, &groups, wanted).await {
                            failed.push(format!("{region} {set}: {e}"));
                        }
                    }
                    Ok(failed)
                })
                .await?;
            let mut failed = Vec::new();
            for (region, result) in results {
                match result {
                    Ok(f) => failed.extend(f),
                    Err(e) => failed.push(format!("{region}: {e}")),
                }
            }
            if failed.is_empty() {
                Ok(())
            } else {
                Err(api("firewall_sync_failed", failed.join("; ")))
            }
        }
        .boxed()
    }

    fn open_node_port<'a>(
        &'a self,
        id: &'a str,
        name: &'a str,
        port: u16,
    ) -> BoxFuture<'a, Result<(), ProviderError>> {
        async move {
            let (region, instance_id) = split_id(id)?;
            let instance = self.client.instance(region, instance_id).await?;
            let groups = self
                .ensure_groups(region, &instance.vpc_id, &Self::main_set(name, port))
                .await?;
            // The main set covers everything the worker set allowed, and an
            // instance can't carry both within the five-group limit.
            let ids: Vec<String> = groups.iter().map(|g| g.id.clone()).collect();
            let current: BTreeSet<&String> = instance.groups.iter().collect();
            if current == ids.iter().collect::<BTreeSet<_>>() {
                return Ok(());
            }
            self.client.modify_groups(region, instance_id, &ids).await
        }
        .boxed()
    }

    fn price_with_disk(&self, monthly_cents: i64, disk_gb: Option<u32>) -> Result<i64, String> {
        match disk_gb {
            Some(d) => price_with_disk(monthly_cents, d),
            None => Ok(monthly_cents),
        }
    }
}

fn arch_name(arch: &str) -> &str {
    match arch {
        "x86" => "x86 (Intel/AMD)",
        "arm" => "Arm",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wait_goes_after_the_shebang() {
        let s = user_data_for(
            "#!/bin/bash\nset -euo pipefail\necho hi\n",
            "198.51.100.7",
            &[],
        );
        assert!(s.starts_with("#!/bin/bash\n"));
        let wait = s.find("public-ipv4").unwrap();
        assert!(wait < s.find("set -euo pipefail").unwrap());
        assert!(s.contains("eip='198.51.100.7'"));
        assert!(s.contains("X-aws-ec2-metadata-token-ttl-seconds"), "IMDSv2");
        assert!(s.contains("exit 1"));
    }

    #[test]
    fn a_script_without_shebang_gets_one() {
        assert!(user_data_for("echo hi\n", "1.2.3.4", &[]).starts_with("#!/bin/bash\n"));
    }

    #[test]
    fn extra_keys_are_quoted() {
        let s = user_data_for("#!/bin/bash\n", "1.2.3.4", &["ssh-ed25519 AAA it's".into()]);
        assert!(s.contains(r"printf '%s\n' 'ssh-ed25519 AAA it'\''s' >>"));
    }

    #[test]
    fn phases() {
        assert_eq!(phase("pending"), Phase::Booting);
        assert_eq!(phase("running"), Phase::Running);
        assert_eq!(phase("stopping"), Phase::Stopping);
        assert_eq!(phase("stopped"), Phase::Off);
        assert_eq!(phase("shutting-down"), Phase::Gone);
        assert_eq!(phase("terminated"), Phase::Gone);
    }

    #[test]
    fn disk_changes_the_price() {
        assert_eq!(price_with_disk(1000, 80), Ok(1000));
        assert_eq!(price_with_disk(1000, 100), Ok(1160));
        assert!(price_with_disk(1000, 10).is_err());
    }

    #[test]
    fn rules_count_every_source() {
        let group = SecurityGroup {
            permissions: vec![
                Permission {
                    protocol: "udp".into(),
                    from: WG_PORT,
                    to: WG_PORT,
                    ipv4: vec!["203.0.113.1/32".into(), "203.0.113.2/32".into()],
                    other: 0,
                },
                Permission {
                    protocol: "tcp".into(),
                    from: 80,
                    to: 80,
                    ipv4: vec!["0.0.0.0/0".into()],
                    other: 1,
                },
            ],
            ..Default::default()
        };
        let s = slot_of(&group);
        assert_eq!(s.mesh.len(), 2);
        assert_eq!(s.others, 2);
    }
}
