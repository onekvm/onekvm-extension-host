fn main() {
    println!("cargo:rerun-if-changed=native/bus.c");
    let libsystemd = pkg_config::Config::new()
        .probe("libsystemd")
        .expect("libsystemd is required");
    let mut build = cc::Build::new();
    build.file("native/bus.c").warnings(true);
    for path in &libsystemd.include_paths {
        build.include(path);
    }
    build.compile("onekvm-extension-bus");
    println!("cargo:rustc-link-lib=systemd");
}
