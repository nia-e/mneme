use mneme_config::{Config, ConfigDeclarations, PublishedCatalog, select_workspace};

const VALID: &str = "schema='mneme.config.v1'\n";

#[test]
fn toml_locations_are_one_based_character_positions() {
    // The error follows a multibyte character on the same line. Counting bytes
    // would report column 16 rather than the editor's character column 15.
    let text = "# another café\nschema='café' !";
    for actual in [
        Config::parse(text).unwrap_err().to_string(),
        ConfigDeclarations::parse(text).unwrap_err().to_string(),
    ] {
        assert_eq!(actual, "invalid Mneme TOML at line 2, column 15");
        assert!(!actual.contains("café"));
    }
}

#[test]
fn project_and_catalog_parsers_identify_the_document() {
    let text = "schema=42";
    for actual in [
        Config::parse_overlay(text).unwrap_err().to_string(),
        ConfigDeclarations::parse_overlay(text)
            .unwrap_err()
            .to_string(),
    ] {
        assert_eq!(actual, "invalid project TOML at line 1, column 8");
    }
    let actual = PublishedCatalog::parse_toml(text, "01M3CV5EZP4HWAK56YJCMZQG2Y")
        .unwrap_err()
        .to_string();
    assert_eq!(actual, "invalid discovery catalog TOML at line 1, column 8");
}

#[test]
fn unknown_fields_duplicate_keys_and_wrong_types_still_fail_without_source_echo() {
    for text in [
        format!("{VALID}private_credential='secret-value'"),
        format!("{VALID}schema='secret-value'"),
        format!("{VALID}[background]\nenabled='secret-value'"),
    ] {
        for actual in [
            Config::parse(&text).unwrap_err().to_string(),
            ConfigDeclarations::parse(&text).unwrap_err().to_string(),
        ] {
            assert!(actual.starts_with("invalid Mneme TOML at line "));
            assert!(!actual.contains("private_credential"));
            assert!(!actual.contains("secret-value"));
            assert!(!actual.contains('\n'));
        }
    }
}

#[test]
fn load_reports_the_requested_file_and_safe_location() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.toml");
    std::fs::write(&path, "schema=42").unwrap();
    assert_eq!(
        Config::load(&path).unwrap_err().to_string(),
        format!("{}: invalid Mneme TOML at line 1, column 8", path.display())
    );
    std::fs::write(&path, [0xff]).unwrap();
    assert_eq!(
        Config::load(&path).unwrap_err().to_string(),
        format!("{}: configuration must be UTF-8", path.display())
    );
}

#[test]
fn workspace_reader_attaches_user_path_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("user.toml");
    std::fs::write(&path, "schema=42").unwrap();
    assert_eq!(
        select_workspace(&path, dir.path(), None)
            .unwrap_err()
            .to_string(),
        format!(
            "user config {}: invalid Mneme TOML at line 1, column 8",
            path.display()
        )
    );
}

#[test]
fn workspace_reader_identifies_project_source_without_echoing_values() {
    let dir = tempfile::tempdir().unwrap();
    let user = dir.path().join("user.toml");
    std::fs::write(&user, VALID).unwrap();
    let project = dir.path().join("project");
    std::fs::create_dir_all(project.join(".mneme")).unwrap();
    let overlay = project.join(".mneme/memory.toml");
    std::fs::write(
        &overlay,
        format!("{VALID}private_credential='secret-value'"),
    )
    .unwrap();
    let actual = select_workspace(&user, &project, None)
        .unwrap_err()
        .to_string();
    // Name the logical source the operator selected, not its resolved target.
    assert_eq!(
        actual,
        format!(
            "project config {}: invalid project TOML at line 2, column 1",
            overlay.display()
        )
    );
    assert!(!actual.contains("private_credential"));
    assert!(!actual.contains("secret-value"));
}

#[test]
fn semantic_validation_still_reports_its_specific_reason() {
    assert_eq!(
        ConfigDeclarations::parse("schema='secret-value'")
            .unwrap_err()
            .to_string(),
        "unsupported configuration schema; expected mneme.config.v1"
    );
    assert_eq!(
        Config::parse("schema='secret-value'")
            .unwrap_err()
            .to_string(),
        "unsupported configuration schema; expected mneme.config.v1"
    );
}
