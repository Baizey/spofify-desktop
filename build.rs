fn main() {
    for key in [
        "SLINT_EMIT_DEBUG_INFO",
        "SPOFIFY_UPDATE_REPO",
        "SPOFIFY_UPDATE_PUBLIC_KEY",
    ] {
        println!("cargo:rerun-if-env-changed={key}");
    }
    slint_build::compile_with_config(
        "ui/app.slint",
        slint_build::CompilerConfiguration::new().with_style("fluent-dark".into()),
    )
    .expect("compile native UI");
}
