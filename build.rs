fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-env-changed=LAGLESS_UPDATE_REPO");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.set("ProductName", "lagless");
        res.set("FileDescription", "lagless — low-latency audio streaming");
        if let Err(e) = res.compile() {
            println!("cargo:warning=could not embed Windows icon: {e}");
        }
    }
}
