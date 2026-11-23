fn main() {
    embuild::espidf::sysenv::output();
    println!("cargo::rustc-check-cfg=cfg(esp_idf_spiram_mode_oct)");
    // The image's features for `resources::BUILD` (default and app are implied).
    let mut features: Vec<String> = std::env::vars()
        .filter_map(|(k, _)| k.strip_prefix("CARGO_FEATURE_").map(|f| f.to_ascii_lowercase().replace('_', "-")))
        .filter(|f| f != "default" && f != "app")
        .collect();
    features.sort();
    println!("cargo:rustc-env=TBR_HUB_FEATURES={}", features.join(","));
}
