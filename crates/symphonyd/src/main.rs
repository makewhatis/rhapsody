//! symphonyd — the Rhapsody daemon binary. P0 stub: prints version and exits.
//! Wired up across P1–P6; binary name is load-bearing (desktop sidecar contract).

fn main() {
    println!("symphonyd (rhapsody) {}", env!("CARGO_PKG_VERSION"));
}
