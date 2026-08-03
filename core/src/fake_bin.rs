//! Standalone entry point for the dev/test-only fake TronClass server.

#[tokio::main]
async fn main() {
    let (port, listener) = tronclass_core::fake::bind_ephemeral().await;
    println!("fake TronClass listening on http://127.0.0.1:{port}");
    tronclass_core::fake::serve(listener).await;
}
