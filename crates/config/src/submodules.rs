use eyre::bail;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};
use tracing::{debug, info};

/// Subsystem allowing for the configuration of storage submodules via a handy TOML file
///
/// Storage submodule path mappings are governed by a [`SUBMODULES_CONFIG_FILE_NAME`] file,
/// within the instance/base directory (typically `./.irys`).
///
/// This file is automatically created if it does not exist when the node starts, and is
/// populated with `submodule_paths` set to a default configuration of 3 storage modules
/// (This should be the same as the minimum required configuration to initiate a network genesis)
///
/// The `submodule_paths` items are expected to be paths to directories that can be
/// mounted as submodules. The number of paths in `submodule_paths` must exactly
/// match the number of expected submodules based on the current storage config,
/// or an error will be thrown and the process will abort.
/// During storage initialization - if `is_using_hardcoded_paths` is false -
/// symlinks will be created within the `storage_modules` directory,
/// linking the folder to each path in the config sequentially.
/// Otherwise, the paths in the config will be used as-is.
///
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct StorageSubmodulesConfig {
    #[serde(default)]
    pub is_using_hardcoded_paths: bool, // Defaults to false with default trait
    pub submodule_paths: Vec<PathBuf>,
}

pub const SUBMODULES_CONFIG_FILE_NAME: &str = ".irys_submodules.toml";

impl StorageSubmodulesConfig {
    /// Loads the [`StorageSubmodulesConfig`] from a TOML file at the given path
    pub fn from_toml(path: impl AsRef<Path>) -> eyre::Result<Self> {
        let contents = fs::read_to_string(path)?;
        let config: Self = toml::from_str(&contents)?;

        let submodule_count = config.submodule_paths.len();
        if submodule_count < 3 {
            bail!(
                "Insufficient submodules: found {}, but minimum of 3 required in .irys_submodules.toml for chain initialization",
                submodule_count
            );
        }

        Ok(config)
    }

    pub fn load_for_test(instance_dir: PathBuf, num_submodules: usize) -> eyre::Result<Self> {
        let config_path_local = Path::new(&instance_dir).join(SUBMODULES_CONFIG_FILE_NAME);

        tracing::info!(
            "Creating test config at {:?} with {} submodule paths",
            config_path_local,
            num_submodules
        );

        let mut submodule_paths = Vec::new();
        for i in 0..num_submodules {
            submodule_paths
                .push(Path::new(&instance_dir).join(format!("storage_modules/submodule_{}", i)));
        }

        let config = Self {
            is_using_hardcoded_paths: true,
            submodule_paths,
        };

        fs::create_dir_all(instance_dir).expect(".irys config dir can be created");
        let toml = toml::to_string(&config).expect("Able to serialize config");
        fs::write(&config_path_local, toml).unwrap_or_else(|_| {
            panic!("Failed to write config to {}", config_path_local.display())
        });

        for path in &config.submodule_paths {
            fs::create_dir_all(path).expect("to create submodule dir");
        }

        Self::from_toml(config_path_local)
    }

    pub fn load(instance_dir: PathBuf) -> eyre::Result<Self> {
        let config_path_local = Path::new(&instance_dir).join(SUBMODULES_CONFIG_FILE_NAME);

        let base_path = instance_dir.join("storage_modules");
        fs::create_dir_all(base_path.clone()).expect("to create storage_modules directory"); // clone: needed below in multiple branches

        fs::read_dir(base_path.clone()) // clone: needed below in symlink creation
            .expect("to read storage_modules dir")
            .filter_map(std::result::Result::ok)
            .filter(|e| e.file_type().map(|t| t.is_symlink()).unwrap_or(false))
            .for_each(|e| {
                debug!("removing symlink {:?}", e.path());
                fs::remove_dir_all(e.path()).unwrap()
            });

        if config_path_local.exists() {
            let config = Self::from_toml(config_path_local).expect("To load the submodule config");
            if config.is_using_hardcoded_paths {
                return Ok(config);
            };
            let submodule_paths = &config.submodule_paths;
            for idx in 0..submodule_paths.len() {
                let dest = submodule_paths.get(idx).unwrap();
                if let Some(filename) = dest.components().next_back() {
                    let sm_path = base_path.join(filename.as_os_str());

                    if sm_path.exists() && sm_path.is_dir() && !sm_path.is_symlink() {
                        panic!(
                            "Found unexpected folder {:?} in storage submodule path {:?} - please remove this folder, or set `is_using_hardcoded_paths` to `true`",
                            &sm_path, &base_path
                        )
                    }

                    info!("Creating symlink from {:?} to {:?}", sm_path, dest);
                    debug_assert!(dest.exists());

                    #[cfg(unix)]
                    std::os::unix::fs::symlink(dest, &sm_path).expect("to create symlink");
                    #[cfg(windows)]
                    std::os::windows::fs::symlink_dir(&dest, &sm_path).expect("to create symlink");
                }
            }

            Ok(config)
        } else {
            tracing::info!("Creating default config at {:?}", config_path_local);
            let config = Self {
                is_using_hardcoded_paths: true,
                submodule_paths: vec![
                    Path::new(&instance_dir).join("storage_modules/submodule_0"),
                    Path::new(&instance_dir).join("storage_modules/submodule_1"),
                    Path::new(&instance_dir).join("storage_modules/submodule_2"),
                ],
            };

            fs::create_dir_all(instance_dir).expect(".irys config dir can be created");
            let toml = toml::to_string(&config).expect("Able to serialize config");
            fs::write(&config_path_local, toml).unwrap_or_else(|_| {
                panic!("Failed to write config to {}", config_path_local.display())
            });

            for path in &config.submodule_paths {
                fs::create_dir_all(path).expect("to create submodule dir");
            }

            Self::from_toml(config_path_local)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use rstest::rstest;
    use tempfile::tempdir;

    fn arb_storage_submodules_config() -> impl Strategy<Value = StorageSubmodulesConfig> {
        (
            any::<bool>(),
            prop::collection::vec("[a-zA-Z0-9_/]{1,50}".prop_map(PathBuf::from), 0..10),
        )
            .prop_map(|(is_using_hardcoded_paths, submodule_paths)| {
                StorageSubmodulesConfig {
                    is_using_hardcoded_paths,
                    submodule_paths,
                }
            })
    }

    proptest! {
        #[test]
        fn serde_roundtrip(config in arb_storage_submodules_config()) {
            let toml_str = toml::to_string(&config)?;
            let decoded: StorageSubmodulesConfig = toml::from_str(&toml_str)?;
            prop_assert_eq!(config, decoded);
        }
    }

    proptest! {
        #[test]
        fn from_toml_never_panics_on_arbitrary_input(input in "\\PC{0,200}") {
            let dir = tempdir()?;
            let path = dir.path().join("test.toml");
            fs::write(&path, &input)?;
            let _result = StorageSubmodulesConfig::from_toml(&path);
        }
    }

    fn write_config_toml(dir: &Path, count: usize) -> eyre::Result<PathBuf> {
        let paths: Vec<PathBuf> = (0..count).map(|i| dir.join(format!("sub_{}", i))).collect();
        let config = StorageSubmodulesConfig {
            is_using_hardcoded_paths: true,
            submodule_paths: paths,
        };
        let toml_str = toml::to_string(&config)?;
        let path = dir.join("test.toml");
        fs::write(&path, &toml_str)?;
        Ok(path)
    }

    #[rstest]
    #[case(0, true)]
    #[case(1, true)]
    #[case(2, true)]
    #[case(3, false)]
    #[case(10, false)]
    fn from_toml_submodule_count_boundary(
        #[case] count: usize,
        #[case] should_err: bool,
    ) -> eyre::Result<()> {
        let dir = tempdir()?;
        let path = write_config_toml(dir.path(), count)?;
        let result = StorageSubmodulesConfig::from_toml(&path);
        assert_eq!(result.is_err(), should_err);
        if should_err {
            let err_msg = result.unwrap_err().to_string();
            assert!(
                err_msg.contains("Insufficient submodules"),
                "expected 'Insufficient submodules' in error, got: {err_msg}"
            );
        }
        Ok(())
    }

    #[test]
    fn load_creates_default_config_when_none_exists() -> eyre::Result<()> {
        let dir = tempdir()?;
        let config = StorageSubmodulesConfig::load(dir.path().to_path_buf())?;
        assert_eq!(config.submodule_paths.len(), 3);
        assert!(config.is_using_hardcoded_paths);
        let config_path = dir.path().join(SUBMODULES_CONFIG_FILE_NAME);
        assert!(config_path.exists());
        Ok(())
    }

    #[test]
    fn load_reads_existing_config() -> eyre::Result<()> {
        let dir = tempdir()?;
        let first = StorageSubmodulesConfig::load(dir.path().to_path_buf())?;
        let second = StorageSubmodulesConfig::load(dir.path().to_path_buf())?;
        assert_eq!(first, second);
        assert_eq!(second.submodule_paths.len(), 3);
        Ok(())
    }
}
