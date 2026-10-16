//! Manual check that the D-Bus/BlueZ path works from inside the container:
//! `./scripts/build.sh build --examples -p backchat-ble` then run it with the
//! run.sh mounts (see scripts/ — needs --userns=keep-id for D-Bus auth).

#[tokio::main(flavor = "current_thread")]
async fn main() -> bluer::Result<()> {
    let session = bluer::Session::new().await?;
    println!("session ok; adapters: {:?}", session.adapter_names().await?);
    let adapter = session.default_adapter().await?;
    let address = adapter.address().await?;
    println!(
        "default adapter: {} addr={} powered={}",
        adapter.name(),
        address,
        adapter.is_powered().await?
    );
    println!("bluer path OK — D-Bus auth and BlueZ reachable");
    Ok(())
}
