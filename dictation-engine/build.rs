fn main() {
    println!("cargo::rustc-check-cfg=cfg(with_pipewire)");
    println!("cargo::rustc-check-cfg=cfg(pipewire_disabled)");
    let linux = std::env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "linux");
    let pipewire = std::env::var_os("CARGO_FEATURE_PIPEWIRE").is_some();
    match (linux, pipewire) {
        (true, true) => println!("cargo::rustc-cfg=with_pipewire"),
        (true, false) => println!("cargo::rustc-cfg=pipewire_disabled"),
        _ => {}
    }
}
