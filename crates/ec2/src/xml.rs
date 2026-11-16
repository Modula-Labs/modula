//! EC2 answers in XML. Responses become a small tree that the client reads
//! by element name, tolerating elements it doesn't know.

use quick_xml::Reader;
use quick_xml::escape::resolve_predefined_entity;
use quick_xml::events::Event;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Node {
    pub name: String,
    pub text: String,
    pub children: Vec<Node>,
}

impl Node {
    pub fn child(&self, name: &str) -> Option<&Node> {
        self.children.iter().find(|c| c.name == name)
    }

    /// The text of the child `name`, when present and not empty.
    pub fn text_of(&self, name: &str) -> Option<&str> {
        self.child(name)
            .map(|c| c.text.as_str())
            .filter(|t| !t.is_empty())
    }

    pub fn string(&self, name: &str) -> String {
        self.text_of(name).unwrap_or_default().to_owned()
    }

    /// The `<item>`s of the set `name` (`<fooSet><item/>…</fooSet>`).
    pub fn items<'a>(&'a self, name: &str) -> impl Iterator<Item = &'a Node> + use<'a> {
        self.child(name)
            .into_iter()
            .flat_map(|set| set.children.iter().filter(|c| c.name == "item"))
    }
}

fn local(name: &str) -> String {
    name.rsplit(':').next().unwrap_or_default().to_owned()
}

/// Parses a whole document; the root element is returned.
pub fn parse(bytes: &[u8]) -> Result<Node, String> {
    let mut reader = Reader::from_reader(bytes);
    let mut buf = Vec::new();
    let mut stack: Vec<Node> = Vec::new();
    let mut root = None;
    loop {
        let event = reader
            .read_event_into(&mut buf)
            .map_err(|e| e.to_string())?;
        match event {
            Event::Start(e) => stack.push(Node {
                name: local(e.name().as_ref()),
                ..Default::default()
            }),
            Event::Empty(e) => {
                let node = Node {
                    name: local(e.name().as_ref()),
                    ..Default::default()
                };
                match stack.last_mut() {
                    Some(parent) => parent.children.push(node),
                    None => root = Some(node),
                }
            }
            Event::Text(t) => {
                if let Some(top) = stack.last_mut() {
                    top.text.push_str(&t.xml10_content());
                }
            }
            Event::CData(t) => {
                if let Some(top) = stack.last_mut() {
                    top.text.push_str(&t.xml10_content());
                }
            }
            Event::GeneralRef(r) => {
                if let Some(top) = stack.last_mut() {
                    if let Some(c) = r.resolve_char_ref().map_err(|e| e.to_string())? {
                        top.text.push(c);
                    } else {
                        let name = r.xml10_content();
                        let entity = resolve_predefined_entity(&name)
                            .ok_or_else(|| format!("unknown entity {name}"))?;
                        top.text.push_str(entity);
                    }
                }
            }
            Event::End(_) => {
                let done = stack.pop().ok_or("unbalanced document")?;
                match stack.last_mut() {
                    Some(parent) => parent.children.push(done),
                    None => root = Some(done),
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    root.ok_or_else(|| "empty document".to_owned())
}

/// EC2's error envelope: the code and message of the first error.
pub fn error(bytes: &[u8]) -> Option<(String, String)> {
    let root = parse(bytes).ok()?;
    let errors = root.child("Errors")?;
    let e = errors.child("Error")?;
    Some((e.string("Code"), e.string("Message")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describe_addresses_sample_from_the_api_reference() {
        let doc = br#"<DescribeAddressesResponse xmlns="http://ec2.amazonaws.com/doc/2016-11-15/">
   <requestId>f7de5e98-491a-4c19-a92d-908d6EXAMPLE</requestId>
   <addressesSet>
     <item>
       <publicIp>203.0.113.41</publicIp>
       <allocationId>eipalloc-08229861</allocationId>
       <domain>vpc</domain>
       <instanceId>i-0598c7d356eba48d7</instanceId>
       <associationId>eipassoc-f0229899</associationId>
       <networkInterfaceId>eni-ef229886</networkInterfaceId>
       <networkInterfaceOwnerId>053230519467</networkInterfaceOwnerId>
       <privateIpAddress>10.0.0.228</privateIpAddress>
     </item>
   </addressesSet>
</DescribeAddressesResponse>"#;
        let root = parse(doc).unwrap();
        assert_eq!(root.name, "DescribeAddressesResponse");
        let items: Vec<_> = root.items("addressesSet").collect();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text_of("publicIp"), Some("203.0.113.41"));
        assert_eq!(items[0].text_of("associationId"), Some("eipassoc-f0229899"));
        assert_eq!(items[0].text_of("missing"), None);
    }

    #[test]
    fn error_envelope() {
        let doc = br#"<Response><Errors><Error><Code>InvalidInstanceID.NotFound</Code><Message>The instance ID &apos;i-1a2b3c4d&apos; does not exist &#38; never did</Message></Error></Errors><RequestID>ea966190</RequestID></Response>"#;
        let (code, message) = error(doc).unwrap();
        assert_eq!(code, "InvalidInstanceID.NotFound");
        assert_eq!(
            message,
            "The instance ID 'i-1a2b3c4d' does not exist & never did"
        );
    }

    #[test]
    fn empty_elements_and_garbage() {
        let root = parse(b"<a><b/><c>x</c></a>").unwrap();
        assert_eq!(root.text_of("b"), None);
        assert_eq!(root.string("c"), "x");
        assert!(error(b"not xml at all").is_none());
    }
}
