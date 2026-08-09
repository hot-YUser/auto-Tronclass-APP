//! Dev-only fake TronClass for the live skeleton demo. Bind 0.0.0.0 so an Android
//! emulator (via 10.0.2.2) or a LAN device can reach it. Build with:
//!   cargo run --features fakeserver --bin fake_tronclass -- [port]

use tronclass_core::fake;

#[tokio::main]
async fn main() {
    let port: u16 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(8779);

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .unwrap_or_else(|e| panic!("bind 0.0.0.0:{port}: {e}"));

    println!(
        "fake TronClass listening on http://0.0.0.0:{port}  (user={}, pass={})",
        fake::GOOD_USER,
        fake::GOOD_PASS
    );
    fake::serve(listener).await;
}
