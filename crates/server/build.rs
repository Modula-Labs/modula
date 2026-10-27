fn main() {
    // The SPA is embedded at compile time; rebuild when the web build changes.
    println!("cargo:rerun-if-changed=../../web/dist");
}
