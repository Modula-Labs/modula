//! AWS Signature Version 4, as the EC2 Query and Pricing JSON APIs need it:
//! a request with a fixed set of headers and no query string.
//! <https://docs.aws.amazon.com/IAM/latest/UserGuide/create-signed-request.html>

use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use time::macros::format_description;

const ALGORITHM: &str = "AWS4-HMAC-SHA256";

/// What identifies a request's signing scope.
pub struct Scope<'a> {
    pub region: &'a str,
    pub service: &'a str,
}

/// `20150830T123600Z`, the `x-amz-date` value for `at`.
pub fn amz_date(at: OffsetDateTime) -> String {
    let fmt = format_description!("[year][month][day]T[hour][minute][second]Z");
    at.to_offset(time::UtcOffset::UTC)
        .format(&fmt)
        .expect("a UTC date always formats")
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

fn signing_key(secret: &str, date: &str, scope: &Scope<'_>) -> Vec<u8> {
    let k = hmac(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    let k = hmac(&k, scope.region.as_bytes());
    let k = hmac(&k, scope.service.as_bytes());
    hmac(&k, b"aws4_request")
}

fn canonical_request(method: &str, headers: &[(&str, &str)], payload: &[u8]) -> (String, String) {
    let mut sorted: Vec<(String, &str)> = headers
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v.trim()))
        .collect();
    sorted.sort();
    let signed = sorted
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");
    let canonical: String = sorted.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let hash = sha256_hex(payload);
    (
        format!("{method}\n/\n\n{canonical}\n{signed}\n{hash}"),
        signed,
    )
}

/// The `Authorization` header for a request to `/` with no query string.
/// `headers` must hold every header to sign, including `host` and
/// `x-amz-date` (whose value `amz_date` must be).
pub fn authorization(
    access_key: &str,
    secret: &str,
    scope: &Scope<'_>,
    amz_date: &str,
    method: &str,
    headers: &[(&str, &str)],
    payload: &[u8],
) -> String {
    let date = &amz_date[..8];
    let (canonical, signed) = canonical_request(method, headers, payload);
    let credential_scope = format!("{date}/{}/{}/aws4_request", scope.region, scope.service);
    let to_sign = format!(
        "{ALGORITHM}\n{amz_date}\n{credential_scope}\n{}",
        sha256_hex(canonical.as_bytes())
    );
    let signature = hex::encode(hmac(&signing_key(secret, date, scope), to_sign.as_bytes()));
    format!(
        "{ALGORITHM} Credential={access_key}/{credential_scope}, SignedHeaders={signed}, Signature={signature}"
    )
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::*;

    // The vectors of AWS's signing test suite, as published in
    // awslabs/aws-c-auth tests/aws-signing-test-suite/v4/.
    const KEY: &str = "AKIDEXAMPLE";
    const SECRET: &str = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";
    const DATE: &str = "20150830T123600Z";
    const SCOPE: Scope<'static> = Scope {
        region: "us-east-1",
        service: "service",
    };

    fn signature(method: &str, headers: &[(&str, &str)], payload: &[u8]) -> String {
        let auth = authorization(KEY, SECRET, &SCOPE, DATE, method, headers, payload);
        auth.rsplit("Signature=").next().unwrap().to_owned()
    }

    #[test]
    fn date_format() {
        assert_eq!(amz_date(datetime!(2015-08-30 12:36:00 UTC)), DATE);
    }

    #[test]
    fn get_vanilla() {
        let headers = [("Host", "example.amazonaws.com"), ("X-Amz-Date", DATE)];
        assert_eq!(
            signature("GET", &headers, b""),
            "5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
    }

    #[test]
    fn post_vanilla() {
        let headers = [("Host", "example.amazonaws.com"), ("X-Amz-Date", DATE)];
        assert_eq!(
            signature("POST", &headers, b""),
            "5da7c1a2acd57cee7505fc6676e4e544621c30862966e37dddb68e92efbe5d6b"
        );
    }

    #[test]
    fn post_form_body_matches_what_ec2_traffic_looks_like() {
        let body = b"Param1=value1";
        let hash = sha256_hex(body);
        assert_eq!(
            hash,
            "9095672bbd1f56dfc5b65f3e153adc8731a4a654192329106275f4c7b24d0b6e"
        );
        let headers = [
            ("Content-Length", "13"),
            ("Content-Type", "application/x-www-form-urlencoded"),
            ("Host", "example.amazonaws.com"),
            ("X-Amz-Content-Sha256", hash.as_str()),
            ("X-Amz-Date", DATE),
        ];
        let (canonical, signed) = canonical_request("POST", &headers, body);
        assert_eq!(
            signed,
            "content-length;content-type;host;x-amz-content-sha256;x-amz-date"
        );
        assert!(canonical.starts_with("POST\n/\n\ncontent-length:13\n"));
        assert_eq!(
            signature("POST", &headers, body),
            "d3875051da38690788ef43de4db0d8f280229d82040bfac253562e56c3f20e0b"
        );
    }

    #[test]
    fn the_header_carries_the_scope_but_never_the_secret() {
        let headers = [
            ("Host", "ec2.eu-west-1.amazonaws.com"),
            ("X-Amz-Date", DATE),
        ];
        let scope = Scope {
            region: "eu-west-1",
            service: "ec2",
        };
        let auth = authorization(KEY, SECRET, &scope, DATE, "POST", &headers, b"x");
        assert!(auth.starts_with(
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/eu-west-1/ec2/aws4_request, SignedHeaders=host;x-amz-date, Signature="
        ));
        assert!(!auth.contains(SECRET));
    }
}
