// Embeds the app icon (resource id 1, which GPUI loads for windows) and version info into the exe.
fn main() {
    println!("cargo:rerun-if-changed=assets/crabber.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/crabber.ico").set("ProductName", "Crabber").set("FileDescription", "Crabber");
        if let Err(e) = res.compile() {
            println!("cargo:warning=icon not embedded: {e}");
        }
    }
}
