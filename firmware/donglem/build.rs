fn main() {
    embuild::espidf::sysenv::output();
    // The panic recorder (zrh-gateway, the source of firmware/hub/src/cpanic.rs)
    // records the last ESP-IDF panic before ESP-IDF handles it.
    println!("cargo:rustc-link-arg=-Wl,--wrap=esp_panic_handler");
}
