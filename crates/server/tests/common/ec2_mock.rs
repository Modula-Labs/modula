// An in-process mock of the EC2 and Pricing APIs: `include!`d by the tests
// using it, so they keep their own imports.

pub const KEY_ID: &str = "AKIAIOSFODNN7EXAMPLE";
pub const SECRET: &str = "test-secret-0123456789abcdefghijklmnop";
pub const EIP: &str = "198.51.100.77";

#[derive(Clone, Default)]
pub struct Inst {
    region: String,
    id: String,
    pub state: String,
    instance_type: String,
    token: String,
    pub user_data: String,
    groups: Vec<String>,
    key: Option<String>,
    pub root_gb: u32,
    tags: BTreeMap<String, String>,
    public_ip: Option<String>,
}

#[derive(Clone, Default)]
pub struct Addr {
    region: String,
    allocation: String,
    ip: String,
    instance: Option<String>,
    tags: BTreeMap<String, String>,
}

/// (protocol, from, to, cidr)
pub type Rule = (String, i32, i32, String);

#[derive(Clone, Default)]
pub struct Group {
    region: String,
    id: String,
    name: String,
    vpc: String,
    tags: BTreeMap<String, String>,
    /// (protocol, from, to, cidr)
    rules: Vec<Rule>,
}

#[derive(Clone)]
pub struct Key {
    region: String,
    name: String,
    material: String,
}

pub type RunHook = Box<dyn FnMut(&str) + Send>;

#[derive(Default)]
pub struct Mock {
    pub instances: Vec<Inst>,
    addresses: Vec<Addr>,
    groups: Vec<Group>,
    keys: Vec<Key>,
    /// Regions without a default VPC.
    no_vpc: Vec<String>,
    fail_run: bool,
    /// Subnets whose zone is out of capacity.
    pub full_subnets: Vec<String>,
    fail_pricing: bool,
    /// Group creation loses a race: the group appears, the call fails.
    race_groups: bool,
    /// Answer this many EC2 requests with throttling first.
    throttle: u32,
    /// Actions, in order.
    calls: Vec<String>,
    /// The form of every RunInstances.
    run_forms: Vec<HashMap<String, String>>,
    /// Instances are running as soon as somebody looks at them.
    pub auto_run: bool,
    /// Called with each launched instance's user data.
    pub on_run: Option<RunHook>,
}

pub type Shared = Arc<Mutex<Mock>>;

fn form(body: &str) -> HashMap<String, String> {
    url::form_urlencoded::parse(body.as_bytes())
        .into_owned()
        .collect()
}

/// `Filter.N.*` as name to values.
fn filters(f: &HashMap<String, String>) -> BTreeMap<String, Vec<String>> {
    let mut out = BTreeMap::new();
    for n in 1.. {
        let Some(name) = f.get(&format!("Filter.{n}.Name")) else {
            break;
        };
        let mut values = Vec::new();
        for v in 1.. {
            match f.get(&format!("Filter.{n}.Value.{v}")) {
                Some(value) => values.push(value.clone()),
                None => break,
            }
        }
        out.insert(name.clone(), values);
    }
    out
}

fn tag_filters_match(
    filters: &BTreeMap<String, Vec<String>>,
    tags: &BTreeMap<String, String>,
) -> bool {
    filters
        .iter()
        .all(|(name, values)| match name.strip_prefix("tag:") {
            Some(key) => tags.get(key).is_some_and(|v| values.contains(v)),
            None => true,
        })
}

fn tags_xml(tags: &BTreeMap<String, String>) -> String {
    let items: String = tags
        .iter()
        .map(|(k, v)| format!("<item><key>{k}</key><value>{v}</value></item>"))
        .collect();
    format!("<tagSet>{items}</tagSet>")
}

fn ok(body: String) -> Response {
    (
        StatusCode::OK,
        format!(r#"<Reply xmlns="http://ec2.amazonaws.com/doc/2016-11-15/">{body}</Reply>"#),
    )
        .into_response()
}

fn err(code: &str, message: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        format!(
            "<Response><Errors><Error><Code>{code}</Code><Message>{message}</Message></Error></Errors><RequestID>r</RequestID></Response>"
        ),
    )
        .into_response()
}

fn instance_xml(i: &Inst) -> String {
    let groups: String = i
        .groups
        .iter()
        .map(|g| format!("<item><groupId>{g}</groupId></item>"))
        .collect();
    let ip = i
        .public_ip
        .as_ref()
        .map(|ip| format!("<ipAddress>{ip}</ipAddress>"))
        .unwrap_or_default();
    format!(
        "<item><instanceId>{id}</instanceId><instanceState><name>{state}</name></instanceState><instanceType>{ty}</instanceType>{ip}<vpcId>vpc-1</vpcId><rootDeviceName>/dev/sda1</rootDeviceName><blockDeviceMapping><item><deviceName>/dev/sda1</deviceName><ebs><volumeId>vol-{id}</volumeId></ebs></item></blockDeviceMapping><groupSet>{groups}</groupSet>{tags}</item>",
        id = i.id,
        state = i.state,
        ty = i.instance_type,
        tags = tags_xml(&i.tags),
    )
}

fn address_xml(a: &Addr) -> String {
    let assoc = a
        .instance
        .as_ref()
        .map(|i| format!("<instanceId>{i}</instanceId><associationId>assoc-{i}</associationId>"))
        .unwrap_or_default();
    format!(
        "<item><publicIp>{ip}</publicIp><allocationId>{al}</allocationId>{assoc}{tags}</item>",
        ip = a.ip,
        al = a.allocation,
        tags = tags_xml(&a.tags)
    )
}

fn group_xml(g: &Group) -> String {
    let perms: String = g
        .rules
        .iter()
        .map(|(p, from, to, cidr)| {
            format!(
                "<item><ipProtocol>{p}</ipProtocol><fromPort>{from}</fromPort><toPort>{to}</toPort><ipRanges><item><cidrIp>{cidr}</cidrIp></item></ipRanges></item>"
            )
        })
        .collect();
    format!(
        "<item><groupId>{id}</groupId><groupName>{name}</groupName><vpcId>{vpc}</vpcId>{tags}<ipPermissions>{perms}</ipPermissions></item>",
        id = g.id,
        name = g.name,
        vpc = g.vpc,
        tags = tags_xml(&g.tags)
    )
}

fn tag_spec(f: &HashMap<String, String>, n: usize) -> BTreeMap<String, String> {
    let mut tags = BTreeMap::new();
    for t in 1.. {
        let Some(k) = f.get(&format!("TagSpecification.{n}.Tag.{t}.Key")) else {
            break;
        };
        tags.insert(
            k.clone(),
            f[&format!("TagSpecification.{n}.Tag.{t}.Value")].clone(),
        );
    }
    tags
}

fn permissions(f: &HashMap<String, String>) -> Vec<(String, i32, i32, String)> {
    let proto = f["IpPermissions.1.IpProtocol"].clone();
    let from: i32 = f["IpPermissions.1.FromPort"].parse().unwrap();
    let to: i32 = f["IpPermissions.1.ToPort"].parse().unwrap();
    let mut out = Vec::new();
    for r in 1.. {
        match f.get(&format!("IpPermissions.1.IpRanges.{r}.CidrIp")) {
            Some(c) => out.push((proto.clone(), from, to, c.clone())),
            None => break,
        }
    }
    out
}

const TYPES: [(&str, u32, u64, &str, bool); 3] = [
    ("t3.small", 2, 2048, "x86_64", true),
    ("t3.large", 2, 8192, "x86_64", true),
    ("t4g.small", 2, 2048, "arm64", true),
];

fn price_for(ty: &str, region: &str) -> Option<&'static str> {
    match (ty, region) {
        ("t3.small", "us-east-1") => Some("0.0208000000"),
        ("t3.small", "eu-west-1") => Some("0.0228000000"),
        ("t3.large", "us-east-1") => Some("0.0832000000"),
        ("t4g.small", "us-east-1") => Some("0.0168000000"),
        _ => None,
    }
}

fn pricing(m: &Shared, body: &str) -> Response {
    m.lock().unwrap().calls.push("GetProducts".into());
    if m.lock().unwrap().fail_pricing {
        return (
            StatusCode::BAD_REQUEST,
            r##"{"__type":"com.amazon#InternalErrorException","message":"pricing is down"}"##,
        )
            .into_response();
    }
    let req: Value = serde_json::from_str(body).unwrap();
    let ty = req["Filters"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|f| f["Field"] == "instanceType")
        .and_then(|f| f["Value"].as_str());
    let list: Vec<String> = ["us-east-1", "eu-west-1"]
        .iter()
        .filter_map(|region| {
            let usd = price_for(ty?, region)?;
            Some(
                json!({
                    "product": {"attributes": {"instanceType": ty, "regionCode": region}},
                    "terms": {"OnDemand": {"a": {"priceDimensions": {"b": {"pricePerUnit": {"USD": usd}}}}}}
                })
                .to_string(),
            )
        })
        .collect();
    (StatusCode::OK, json!({"PriceList": list}).to_string()).into_response()
}

fn handle(m: &Shared, region: &str, f: HashMap<String, String>) -> Response {
    let action = f["Action"].clone();
    let mut mock = m.lock().unwrap();
    mock.calls.push(action.clone());
    if mock.throttle > 0 {
        mock.throttle -= 1;
        return err("RequestLimitExceeded", "slow down");
    }
    let filters = filters(&f);
    match action.as_str() {
        "DescribeRegions" => ok(
            "<regionInfo><item><regionName>us-east-1</regionName></item><item><regionName>eu-west-1</regionName></item></regionInfo>"
                .into(),
        ),
        "DescribeVpcs" => {
            if mock.no_vpc.iter().any(|r| r == region) {
                ok("<vpcSet/>".into())
            } else {
                ok("<vpcSet><item><vpcId>vpc-1</vpcId></item></vpcSet>".into())
            }
        }
        "DescribeSubnets" => ok(format!(
            "<subnetSet><item><subnetId>subnet-b</subnetId><availabilityZone>{region}b</availabilityZone></item><item><subnetId>subnet-a</subnetId><availabilityZone>{region}a</availabilityZone></item></subnetSet>"
        )),
        "DescribeInstanceTypes" => {
            let names = &filters["instance-type"];
            let matches = |name: &str| {
                names.iter().any(|p| match p.strip_suffix('*') {
                    Some(prefix) => name.starts_with(prefix),
                    None => p == name,
                })
            };
            // Two pages, to exercise NextToken.
            let start: usize = f.get("NextToken").map_or(0, |t| t.parse().unwrap());
            let hits: Vec<_> = TYPES.iter().filter(|t| matches(t.0)).collect();
            let items: String = hits
                .iter()
                .skip(start)
                .take(2)
                .map(|(n, cpu, mem, arch, burst)| format!(
                    "<item><instanceType>{n}</instanceType><vCpuInfo><defaultVCpus>{cpu}</defaultVCpus></vCpuInfo><memoryInfo><sizeInMiB>{mem}</sizeInMiB></memoryInfo><processorInfo><supportedArchitectures><item>{arch}</item></supportedArchitectures></processorInfo><burstablePerformanceSupported>{burst}</burstablePerformanceSupported></item>"
                ))
                .collect();
            let next = if start + 2 < hits.len() {
                format!("<nextToken>{}</nextToken>", start + 2)
            } else {
                String::new()
            };
            ok(format!("<instanceTypeSet>{items}</instanceTypeSet>{next}"))
        }
        "DescribeInstanceTypeOfferings" => {
            let az = f["LocationType"] == "availability-zone";
            let items: String = TYPES
                .iter()
                .filter(|t| price_for(t.0, region).is_some())
                .flat_map(|t| {
                    let locations = if az {
                        vec![format!("{region}a"), format!("{region}b")]
                    } else {
                        vec![region.to_owned()]
                    };
                    locations.into_iter().map(|location| {
                        format!(
                            "<item><instanceType>{}</instanceType><location>{location}</location></item>",
                            t.0
                        )
                    })
                })
                .collect();
            ok(format!("<instanceTypeOfferingSet>{items}</instanceTypeOfferingSet>"))
        }
        "DescribeImages" => ok(
            "<imagesSet><item><imageId>ami-old</imageId><creationDate>2024-01-01T00:00:00Z</creationDate><rootDeviceName>/dev/sda1</rootDeviceName></item><item><imageId>ami-new</imageId><creationDate>2026-01-01T00:00:00Z</creationDate><rootDeviceName>/dev/sda1</rootDeviceName></item></imagesSet>"
                .into(),
        ),
        "DescribeSecurityGroups" => {
            let items: String = mock
                .groups
                .iter()
                .filter(|g| g.region == region && tag_filters_match(&filters, &g.tags))
                .filter(|g| filters.get("vpc-id").is_none_or(|v| v.contains(&g.vpc)))
                .map(group_xml)
                .collect();
            ok(format!("<securityGroupInfo>{items}</securityGroupInfo>"))
        }
        "CreateSecurityGroup" => {
            let id = format!("sg-{}", mock.groups.len());
            mock.groups.push(Group {
                region: region.to_owned(),
                id: id.clone(),
                name: f["GroupName"].clone(),
                vpc: f["VpcId"].clone(),
                tags: tag_spec(&f, 1),
                rules: Vec::new(),
            });
            if mock.race_groups {
                return err("InvalidGroup.Duplicate", "the group already exists");
            }
            ok(format!("<groupId>{id}</groupId>"))
        }
        "AuthorizeSecurityGroupIngress" => {
            let add = permissions(&f);
            let g = mock.groups.iter_mut().find(|g| g.id == f["GroupId"]).unwrap();
            g.rules.extend(add);
            ok("<return>true</return>".into())
        }
        "RevokeSecurityGroupIngress" => {
            let remove = permissions(&f);
            let g = mock.groups.iter_mut().find(|g| g.id == f["GroupId"]).unwrap();
            g.rules.retain(|r| !remove.contains(r));
            ok("<return>true</return>".into())
        }
        "DescribeKeyPairs" => {
            let name = f.get("KeyName.1");
            let items: String = mock
                .keys
                .iter()
                .filter(|k| k.region == region && name.is_none_or(|n| *n == k.name))
                .map(|k| {
                    format!(
                        "<item><keyName>{}</keyName><keyFingerprint>fp</keyFingerprint><publicKey>{}</publicKey></item>",
                        k.name, k.material
                    )
                })
                .collect();
            ok(format!("<keySet>{items}</keySet>"))
        }
        "ImportKeyPair" => {
            let raw = base64::engine::general_purpose::STANDARD
                .decode(&f["PublicKeyMaterial"])
                .unwrap();
            mock.keys.push(Key {
                region: region.to_owned(),
                name: f["KeyName"].clone(),
                material: String::from_utf8(raw).unwrap(),
            });
            ok(format!("<keyName>{}</keyName>", f["KeyName"]))
        }
        "DescribeAddresses" => {
            let items: String = mock
                .addresses
                .iter()
                .filter(|a| a.region == region && tag_filters_match(&filters, &a.tags))
                .map(address_xml)
                .collect();
            ok(format!("<addressesSet>{items}</addressesSet>"))
        }
        "AllocateAddress" => {
            let allocation = format!("eipalloc-{}", mock.addresses.len());
            mock.addresses.push(Addr {
                region: region.to_owned(),
                allocation: allocation.clone(),
                ip: EIP.to_owned(),
                instance: None,
                tags: tag_spec(&f, 1),
            });
            ok(format!("<publicIp>{EIP}</publicIp><allocationId>{allocation}</allocationId>"))
        }
        "AssociateAddress" => {
            let instance = f["InstanceId"].clone();
            let Some(a) = mock.addresses.iter_mut().find(|a| a.allocation == f["AllocationId"]) else {
                return err("InvalidAllocationID.NotFound", "no such allocation");
            };
            a.instance = Some(instance.clone());
            let ip = a.ip.clone();
            if let Some(i) = mock.instances.iter_mut().find(|i| i.id == instance) {
                i.public_ip = Some(ip);
            }
            ok("<associationId>eipassoc-1</associationId>".into())
        }
        "DisassociateAddress" => {
            let instance = f["AssociationId"].strip_prefix("assoc-").unwrap_or_default().to_owned();
            if let Some(a) = mock.addresses.iter_mut().find(|a| a.instance.as_deref() == Some(&instance)) {
                a.instance = None;
            }
            ok("<return>true</return>".into())
        }
        "ReleaseAddress" => {
            mock.addresses.retain(|a| a.allocation != f["AllocationId"]);
            ok("<return>true</return>".into())
        }
        "CreateTags" => {
            let resource = f["ResourceId.1"].clone();
            let key = f["Tag.1.Key"].clone();
            let value = f["Tag.1.Value"].clone();
            if let Some(a) = mock.addresses.iter_mut().find(|a| a.allocation == resource) {
                a.tags.insert(key, value);
            }
            ok("<return>true</return>".into())
        }
        "RunInstances" => {
            mock.run_forms.push(f.clone());
            if mock.fail_run || mock.full_subnets.contains(&f["SubnetId"]) {
                return err("InsufficientInstanceCapacity", "no capacity right now");
            }
            let id = format!("i-{}", mock.instances.len());
            let user_data = base64::engine::general_purpose::STANDARD
                .decode(&f["UserData"])
                .unwrap();
            let mut groups = Vec::new();
            for n in 1.. {
                match f.get(&format!("SecurityGroupId.{n}")) {
                    Some(g) => groups.push(g.clone()),
                    None => break,
                }
            }
            let script = String::from_utf8(user_data).unwrap();
            if let Some(hook) = mock.on_run.as_mut() {
                hook(&script);
            }
            let inst = Inst {
                region: region.to_owned(),
                id,
                state: "pending".into(),
                instance_type: f["InstanceType"].clone(),
                token: f["ClientToken"].clone(),
                user_data: script,
                groups,
                key: f.get("KeyName").cloned(),
                root_gb: f["BlockDeviceMapping.1.Ebs.VolumeSize"].parse().unwrap(),
                tags: tag_spec(&f, 1),
                public_ip: Some("54.0.0.9".into()),
            };
            let xml = instance_xml(&inst);
            mock.instances.push(inst);
            ok(format!("<instancesSet>{xml}</instancesSet>"))
        }
        "DescribeInstances" => {
            if mock.auto_run {
                for i in mock.instances.iter_mut().filter(|i| i.state == "pending") {
                    i.state = "running".into();
                }
            }
            if let Some(id) = f.get("InstanceId.1") {
                return match mock.instances.iter().find(|i| i.id == *id && i.region == region) {
                    Some(i) => ok(format!(
                        "<reservationSet><item><instancesSet>{}</instancesSet></item></reservationSet>",
                        instance_xml(i)
                    )),
                    None => err("InvalidInstanceID.NotFound", "does not exist"),
                };
            }
            let items: String = mock
                .instances
                .iter()
                .filter(|i| i.region == region && tag_filters_match(&filters, &i.tags))
                .filter(|i| filters.get("instance-state-name").is_none_or(|v| v.contains(&i.state)))
                .filter(|i| {
                    filters.get("ip-address").is_none_or(|v| i.public_ip.as_ref().is_some_and(|ip| v.contains(ip)))
                })
                .map(instance_xml)
                .collect();
            ok(format!(
                "<reservationSet><item><instancesSet>{items}</instancesSet></item></reservationSet>"
            ))
        }
        "DescribeVolumes" => {
            let id = f["VolumeId.1"].strip_prefix("vol-").unwrap_or_default();
            match mock.instances.iter().find(|i| i.id == id) {
                Some(i) => ok(format!(
                    "<volumeSet><item><volumeId>vol-{id}</volumeId><size>{}</size></item></volumeSet>",
                    i.root_gb
                )),
                None => ok("<volumeSet/>".into()),
            }
        }
        "TerminateInstances" => {
            let id = f["InstanceId.1"].clone();
            match mock.instances.iter_mut().find(|i| i.id == id) {
                Some(i) => {
                    i.state = "shutting-down".into();
                    ok("<return>true</return>".into())
                }
                None => err("InvalidInstanceID.NotFound", "does not exist"),
            }
        }
        "StopInstances" | "StartInstances" | "RebootInstances" => {
            let id = f["InstanceId.1"].clone();
            let i = mock.instances.iter_mut().find(|i| i.id == id).unwrap();
            i.state = match action.as_str() {
                "StopInstances" => "stopped",
                _ => "running",
            }
            .into();
            ok("<return>true</return>".into())
        }
        "ModifyInstanceAttribute" => {
            let id = f["InstanceId"].clone();
            let i = mock.instances.iter_mut().find(|i| i.id == id).unwrap();
            if let Some(t) = f.get("InstanceType.Value") {
                i.instance_type = t.clone();
            }
            if f.contains_key("GroupId.1") {
                let mut groups = Vec::new();
                for n in 1.. {
                    match f.get(&format!("GroupId.{n}")) {
                        Some(g) => groups.push(g.clone()),
                        None => break,
                    }
                }
                i.groups = groups;
            }
            ok("<return>true</return>".into())
        }
        other => panic!("the mock doesn't implement {other}"),
    }
}

async fn entry(State(m): State<Shared>, headers: HeaderMap, body: String) -> Response {
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(auth.contains("SignedHeaders="), "every request is signed");
    assert!(auth.contains("host"), "the host is signed: {auth}");
    let scope = auth
        .split("Credential=")
        .nth(1)
        .and_then(|s| s.split(',').next())
        .unwrap_or_default();
    let mut parts = scope.split('/');
    let key = parts.next().unwrap_or_default();
    let (_, region, service) = (parts.next(), parts.next().unwrap_or_default(), parts.next());
    if key != KEY_ID {
        if headers.contains_key("x-amz-target") {
            return (
                StatusCode::BAD_REQUEST,
                r##"{"__type":"com.amazon#UnrecognizedClientException","message":"bad key"}"##,
            )
                .into_response();
        }
        return err(
            "AuthFailure",
            "AWS was not able to validate the credentials",
        );
    }
    if headers.contains_key("x-amz-target") {
        assert_eq!(service, Some("pricing"));
        return pricing(&m, &body);
    }
    assert_eq!(service, Some("ec2"));
    handle(&m, region, form(&body))
}

pub async fn mock_aws() -> (url::Url, Shared) {
    let m: Shared = Arc::default();
    let app = Router::new().route("/", post(entry)).with_state(m.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}/").parse().unwrap(), m)
}
