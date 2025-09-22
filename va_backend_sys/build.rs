use std::env;
use std::path::PathBuf;

fn main() {
    let bindings = bindgen::Builder::default()
        .header("wrapper.h")
        // Wrap unsafe operations as this prevents warnings in the 2024 edition
        .wrap_unsafe_ops(true)
        // Only generate bindings for actual VA-API items
        // .allowlist_file(r".*/va/va.*\.h")
        // .allowlist_type("VA.*")
        .allowlist_var("VA_STATUS_.*")
        .allowlist_type("VABufferID")
        .allowlist_type("VABufferType")
        .allowlist_type("VAConfigAttrib")
        .allowlist_type("VAConfigID")
        .allowlist_type("VAContextID")
        .allowlist_type("VADisplayAttribute")
        .allowlist_type("VADriverContextP")
        .allowlist_type("VADriverInit")
        .allowlist_type("VADriverVTable")
        .allowlist_type("VAEntrypoint")
        .allowlist_type("VAImage")
        .allowlist_type("VAImageFormat")
        .allowlist_type("VAImageID")
        .allowlist_type("VAProfile")
        .allowlist_type("VAStatus")
        .allowlist_type("VASubpictureID")
        .allowlist_type("VASurfaceID")
        .allowlist_type("VASurfaceStatus")
        .allowlist_type("drm_state")
        .allowlist_var("VaProfile.*")
        // The backend doesn't actually link to libva, so we can ignore functions
        .ignore_functions()
        .ignore_methods()
        // Disable layout tests to reduce build time
        // TODO: re-enable?
        .layout_tests(false)
        // Tell cargo to invalidate the built crate whenever any of the
        // included header files changed.
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .generate()
        .expect("Unable to generate bindings");

    // Write the bindings to the $OUT_DIR/bindings.rs file.
    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out_path.join("bindings.rs"))
        .expect("Couldn't write bindings!");
}
