use std::{
    collections::HashMap,
    env, fs,
    io::ErrorKind,
    path::{Path, PathBuf},
};

const REQUIRED_KEYS: [&str; 5] = [
    "BADGE_WIFI_SSID",
    "BADGE_WIFI_PASS",
    "TEMPORAL_ADDRESS",
    "TEMPORAL_NAMESPACE",
    "TEMPORAL_API_KEY",
];

fn read_optional_env(path: &Path) -> HashMap<String, String> {
    match fs::read_to_string(path) {
        Ok(content) => temporal_trivia_shared::parse_env(&content)
            .unwrap_or_else(|error| panic!("invalid {}: {error}", path.display())),
        Err(error) if error.kind() == ErrorKind::NotFound => HashMap::new(),
        Err(error) => panic!("could not read {}: {error}", path.display()),
    }
}

/// Resolves one configuration file: an explicit environment override wins and
/// must exist, otherwise the first candidate that does. Falls back to the last
/// candidate so a missing file is reported under its documented name.
fn config_path(variable: &str, candidates: &[PathBuf]) -> PathBuf {
    println!("cargo:rerun-if-env-changed={variable}");
    if let Some(path) = env::var_os(variable).map(PathBuf::from) {
        assert!(
            path.is_file(),
            "{variable} points to missing file {}",
            path.display()
        );
        return path;
    }
    let last = candidates.last().expect("at least one candidate path");
    candidates
        .iter()
        .find(|candidate| candidate.is_file())
        .unwrap_or(last)
        .clone()
}

fn main() {
    embuild::espidf::sysenv::output();
    println!("cargo:rerun-if-env-changed=BADGE_BUILD_UNIX_EPOCH");
    for key in REQUIRED_KEYS {
        println!("cargo:rerun-if-env-changed={key}");
    }

    let firmware = Path::new(env!("CARGO_MANIFEST_DIR"));
    let project = firmware
        .parent()
        .expect("firmware is inside the repository");
    let wifi_path = config_path("BADGE_WIFI_ENV_FILE", &[firmware.join(".env.wifi")]);
    // The repo's own .env, else .env.temporal. There is deliberately no
    // fallback outside the repository: an absolute path belongs in
    // TEMPORAL_ENV_FILE, which config_path asserts actually exists.
    let temporal_path = config_path(
        "TEMPORAL_ENV_FILE",
        &[project.join(".env"), project.join(".env.temporal")],
    );
    println!("cargo:rerun-if-changed={}", wifi_path.display());
    println!("cargo:rerun-if-changed={}", temporal_path.display());

    let mut values = read_optional_env(&wifi_path);
    values.extend(read_optional_env(&temporal_path));
    values.entry("BADGE_WIFI_PASS".to_owned()).or_default();
    for key in REQUIRED_KEYS {
        if let Ok(value) = env::var(key)
            && !value.is_empty()
        {
            values.insert(key.to_owned(), value);
        }
        assert!(
            values.get(key).is_some_and(|value| !value.is_empty()) || key == "BADGE_WIFI_PASS",
            "missing {key}; set it in the environment or the documented .env file"
        );
    }

    let build_epoch = env::var("BADGE_BUILD_UNIX_EPOCH").unwrap_or_else(|_| "0".to_owned());
    let generated = format!(
        "const WIFI_SSID: &str = {wifi_ssid:?};\n\
         const WIFI_PASS: &str = {wifi_pass:?};\n\
         const TEMPORAL_ADDRESS: &str = {temporal_address:?};\n\
         const TEMPORAL_NAMESPACE: &str = {temporal_namespace:?};\n\
         const TEMPORAL_API_KEY: &str = {temporal_api_key:?};\n\
         const BUILD_UNIX_EPOCH: &str = {build_epoch:?};\n",
        wifi_ssid = values["BADGE_WIFI_SSID"],
        wifi_pass = values["BADGE_WIFI_PASS"],
        temporal_address = values["TEMPORAL_ADDRESS"],
        temporal_namespace = values["TEMPORAL_NAMESPACE"],
        temporal_api_key = values["TEMPORAL_API_KEY"],
    );
    let output = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo sets OUT_DIR"))
        .join("firmware_config.rs");
    fs::write(&output, generated)
        .unwrap_or_else(|error| panic!("could not write {}: {error}", output.display()));
}
