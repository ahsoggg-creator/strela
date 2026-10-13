fn main() {
    println!("cargo:rerun-if-changed=assets/app.rc");
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed=assets/app.manifest");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let v = |k: &str| {
            format!(
                "{k}={}",
                std::env::var(format!("CARGO_PKG_VERSION_{}", &k[3..])).unwrap()
            )
        };
        let macros = [v("ST_MAJOR"), v("ST_MINOR"), v("ST_PATCH")];
        embed_resource::compile("assets/app.rc", &macros)
            .manifest_optional()
            .unwrap();
    }
}
