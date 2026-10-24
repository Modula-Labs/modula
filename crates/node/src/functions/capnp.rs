//! workerd's config, in Cap'n Proto text: one service and one loopback
//! socket per function, every function's outbound traffic through one
//! network service that only reaches the public internet.

use std::fmt::Write as _;

/// What the config says about one function. Its modules are files in the
/// config's directory, under `<dir>/` (see [`super::Layout`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Service<'a> {
    /// The service and socket name, and the module directory.
    pub name: &'a str,
    pub dir: &'a str,
    pub port: u16,
    pub compatibility_date: &'a str,
    /// Text bindings (the deployment's variables), by name.
    pub bindings: &'a [(String, String)],
}

/// The main module: tags the console first, then is the user's module.
pub const ENTRY_MODULE: &str = "__modula_entry.js";
pub const CONSOLE_MODULE: &str = "__modula_console.js";
pub const USER_MODULE: &str = modula_proto::node::v1::FUNCTION_MODULE;

/// Addresses no function may connect to: everything that isn't the public
/// internet (this server, its LAN and the private network between servers,
/// link-local addresses with the cloud's metadata service, carrier-grade NAT
/// ranges VPNs use). workerd's own categories, plus the ranges spelled out.
const DENY: &[&str] = &[
    "private",
    "local",
    "unix",
    "unix-abstract",
    "10.0.0.0/8",
    "100.64.0.0/10",
    "127.0.0.0/8",
    "169.254.0.0/16",
    "172.16.0.0/12",
    "192.168.0.0/16",
    "fc00::/7",
    "fe80::/10",
    "::1/128",
];

/// The whole config file. `services` in order (callers sort them).
pub fn render(services: &[Service<'_>]) -> String {
    let mut out = String::from(
        "# Rendered by modula-node; rewritten whenever its functions change.\n\
         using Workerd = import \"/workerd/workerd.capnp\";\n\n\
         const config :Workerd.Config = (\n  services = [\n",
    );
    for s in services {
        let _ = writeln!(out, "    (name = {}, worker = (", quote(s.name));
        out.push_str("      modules = [\n");
        for m in [ENTRY_MODULE, CONSOLE_MODULE, USER_MODULE] {
            let _ = writeln!(
                out,
                "        (name = {}, esModule = embed {}),",
                quote(m),
                quote(&format!("{}/{m}", s.dir))
            );
        }
        out.push_str("      ],\n");
        let _ = writeln!(
            out,
            "      compatibilityDate = {},",
            quote(s.compatibility_date)
        );
        out.push_str("      bindings = [\n");
        for (name, text) in s.bindings {
            let _ = writeln!(
                out,
                "        (name = {}, text = {}),",
                quote(name),
                quote(text)
            );
        }
        out.push_str("      ],\n      globalOutbound = \"internet\",\n    )),\n");
    }
    out.push_str(
        "    (name = \"internet\", network = (\n      allow = [\"public\"],\n      deny = [",
    );
    let deny: Vec<String> = DENY.iter().map(|d| quote(d)).collect();
    out.push_str(&deny.join(", "));
    out.push_str(
        "],\n      tlsOptions = (trustBrowserCas = true),\n    )),\n  ],\n  sockets = [\n",
    );
    for s in services {
        let _ = writeln!(
            out,
            "    (name = {}, address = {}, http = (), service = {}),",
            quote(s.name),
            quote(&format!("127.0.0.1:{}", s.port)),
            quote(s.name)
        );
    }
    out.push_str("  ],\n);\n");
    out
}

/// A Cap'n Proto text literal: printable ASCII as is, everything else
/// escaped (bytes of non-ASCII characters as `\xHH`).
pub fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for b in s.bytes() {
        match b {
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            0x20..=0x7e => out.push(b as char),
            _ => {
                let _ = write!(out, "\\x{b:02x}");
            }
        }
    }
    out.push('"');
    out
}

/// The console module of `deployment`: prefixes its output with the
/// deployment's tag (each service is its own isolate, so this console is
/// the function's alone). See [`super::parse_line`].
pub fn console_module(deployment: &str) -> String {
    format!(
        "// Tags this function's console output for modula-node.\n\
         const tag = \"\\u001e{deployment}\\u001f\";\n\
         for (const level of [\"log\", \"info\", \"warn\", \"error\", \"debug\", \"trace\"]) {{\n  \
           const write = console[level].bind(console);\n  \
           console[level] = (...args) => write(tag, ...args);\n\
         }}\n"
    )
}

/// The main module: the console module first (imports run in order), then
/// everything the user's module exports.
pub const ENTRY_SOURCE: &str = "import \"./__modula_console.js\";\n\
export * from \"./worker.js\";\n\
export { default } from \"./worker.js\";\n";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_one_service_and_socket_per_function() {
        let a = [
            ("API_URL".to_owned(), "https://api.example.com".to_owned()),
            (
                "TRICKY".to_owned(),
                "a \"quoted\" \\ back\nslash\ttab é".to_owned(),
            ),
        ];
        let rendered = render(&[
            Service {
                name: "fn-a",
                dir: "a",
                port: 47100,
                compatibility_date: "2026-10-01",
                bindings: &a,
            },
            Service {
                name: "fn-b",
                dir: "b",
                port: 47101,
                compatibility_date: "2025-01-01",
                bindings: &[],
            },
        ]);
        let want = r#"# Rendered by modula-node; rewritten whenever its functions change.
using Workerd = import "/workerd/workerd.capnp";

const config :Workerd.Config = (
  services = [
    (name = "fn-a", worker = (
      modules = [
        (name = "__modula_entry.js", esModule = embed "a/__modula_entry.js"),
        (name = "__modula_console.js", esModule = embed "a/__modula_console.js"),
        (name = "worker.js", esModule = embed "a/worker.js"),
      ],
      compatibilityDate = "2026-10-01",
      bindings = [
        (name = "API_URL", text = "https://api.example.com"),
        (name = "TRICKY", text = "a \"quoted\" \\ back\nslash\ttab \xc3\xa9"),
      ],
      globalOutbound = "internet",
    )),
    (name = "fn-b", worker = (
      modules = [
        (name = "__modula_entry.js", esModule = embed "b/__modula_entry.js"),
        (name = "__modula_console.js", esModule = embed "b/__modula_console.js"),
        (name = "worker.js", esModule = embed "b/worker.js"),
      ],
      compatibilityDate = "2025-01-01",
      bindings = [
      ],
      globalOutbound = "internet",
    )),
    (name = "internet", network = (
      allow = ["public"],
      deny = ["private", "local", "unix", "unix-abstract", "10.0.0.0/8", "100.64.0.0/10", "127.0.0.0/8", "169.254.0.0/16", "172.16.0.0/12", "192.168.0.0/16", "fc00::/7", "fe80::/10", "::1/128"],
      tlsOptions = (trustBrowserCas = true),
    )),
  ],
  sockets = [
    (name = "fn-a", address = "127.0.0.1:47100", http = (), service = "fn-a"),
    (name = "fn-b", address = "127.0.0.1:47101", http = (), service = "fn-b"),
  ],
);
"#;
        assert_eq!(rendered, want);
    }

    #[test]
    fn quoting_escapes_everything_but_printable_ascii() {
        assert_eq!(quote("plain"), "\"plain\"");
        assert_eq!(quote("\"); evil = ("), "\"\\\"); evil = (\"");
        assert_eq!(quote("\u{0}\u{7f}"), "\"\\x00\\x7f\"");
        assert_eq!(quote("ü"), "\"\\xc3\\xbc\"");
    }

    #[test]
    fn the_console_module_tags_its_output() {
        let m = console_module("d1");
        assert!(m.contains("const tag = \"\\u001ed1\\u001f\";"), "{m}");
        assert!(m.contains("console[level] = (...args) => write(tag, ...args);"));
    }
}
