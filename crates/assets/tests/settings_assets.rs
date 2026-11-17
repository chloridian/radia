use std::fs;

use assets::{EnvironmentDescription, EnvironmentSettings};

fn decode(name: &str) -> EnvironmentDescription {
    let path = format!("{}/tests/fixtures/settings/{name}.dat", env!("CARGO_MANIFEST_DIR"));
    let bytes = fs::read(&path).expect("read settings fixture");
    EnvironmentDescription::decode(&bytes).unwrap_or_else(|failure| panic!("{name}: {failure:?}"))
}

#[test]
fn library_presets_decode() {
    for (name, expected) in [
        ("Midday", "sky"),
        ("Sunrise", "sky"),
        ("Sunset", "sky"),
        ("Midnight", "sky"),
        ("DefaultSky", "sky"),
        ("DefaultWater", "water"),
        ("DefaultDaycycle", "day cycle"),
    ] {
        let decoded = match decode(name).settings {
            EnvironmentSettings::Sky(_) => "sky",
            EnvironmentSettings::Water(_) => "water",
            EnvironmentSettings::DayCycle(_) => "day cycle",
        };
        assert_eq!(decoded, expected, "{name}");
    }
}
