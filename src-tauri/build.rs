fn main() {
    // Declaring the commands gives the ACL a manifest for them (`allow-engine_status`
    // and friends). Without it Tauri cannot resolve an app command coming from a
    // capability at all and answers "Plugin not found" — which is what the engine
    // page, a *remote* origin, hit when its toolbar first called one.
    tauri_build::try_build(
        tauri_build::Attributes::new().app_manifest(
            tauri_build::AppManifest::new().commands(&[
                "startup_status",
                "engine_status",
                "engine_update",
                "toolbar_ready",
            ]),
        ),
    )
    .expect("failed to run tauri-build");
}
