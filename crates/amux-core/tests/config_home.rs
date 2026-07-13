//! End-to-end check that a real `config.toml` on disk relocates the amux home. Runs in its own
//! test binary and sets `XDG_CONFIG_HOME`, so it doesn't race the in-crate unit tests.

use std::path::PathBuf;

#[test]
fn configured_root_relocates_the_whole_amux_home() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg_dir = tmp.path().join("config").join("amux");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::write(cfg_dir.join("config.toml"), "root = \"/data/relocated-amux\"\n").unwrap();

    // BaseDirs reads XDG_CONFIG_HOME when constructed, so set it before resolving.
    std::env::set_var("XDG_CONFIG_HOME", tmp.path().join("config"));

    assert_eq!(
        amux_core::paths::amux_home().unwrap(),
        PathBuf::from("/data/relocated-amux"),
        "config.toml root should become the amux home"
    );
    assert_eq!(
        amux_core::paths::state_file().unwrap(),
        PathBuf::from("/data/relocated-amux/state.json"),
        "state.json should live under the relocated home"
    );
}
