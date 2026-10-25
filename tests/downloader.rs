//! Tests for the download task generators.

use std::collections::HashMap;
use std::path::PathBuf;

use mdt::downloader::{
    build_tasks_from_asset_index, build_tasks_from_version_json, AssetIndex, AssetObject,
    VersionJson,
};

#[test]
fn test_build_tasks_from_version_json() {
    let json_str = r#"
    {
        "id": "1.20.1",
        "downloads": {
            "client": {
                "sha1": "client_sha1_mock",
                "size": 1000,
                "url": "https://piston-data.mojang.com/v1/objects/client.jar"
            }
        },
        "libraries": [
            {
                "name": "net.java.jinput:jinput:2.0.5",
                "downloads": {
                    "artifact": {
                        "path": "net/java/jinput/jinput/2.0.5/jinput-2.0.5.jar",
                        "sha1": "lib_sha1_mock",
                        "size": 500,
                        "url": "https://libraries.minecraft.net/net/java/jinput/jinput/2.0.5/jinput-2.0.5.jar"
                    }
                }
            },
            {
                "name": "org.lwjgl:lwjgl:3.3.1",
                "downloads": {
                    "classifiers": {
                        "natives-linux": {
                            "path": "org/lwjgl/lwjgl/3.3.1/lwjgl-3.3.1-natives-linux.jar",
                            "sha1": "native_linux_mock",
                            "size": 200,
                            "url": "https://libraries.minecraft.net/natives-linux.jar"
                        },
                        "natives-windows": {
                            "path": "org/lwjgl/lwjgl/3.3.1/lwjgl-3.3.1-natives-windows.jar",
                            "sha1": "native_win_mock",
                            "size": 300,
                            "url": "https://libraries.minecraft.net/natives-windows.jar"
                        }
                    }
                }
            }
        ],
        "assetIndex": {
            "id": "5",
            "sha1": "asset_sha1_mock",
            "size": 20000,
            "url": "https://piston-meta.mojang.com/v1/packages/asset_index.json"
        }
    }
    "#;

    let version_json: VersionJson = serde_json::from_str(json_str).unwrap();

    let root_dir = PathBuf::from(".minecraft");
    let tasks = build_tasks_from_version_json(&version_json, &root_dir);

    let current_os = std::env::consts::OS;
    // client.jar + the jinput library + (natives for the current OS). The asset index is no longer
    // part of this list — it is fetched separately via the global index cache.
    let expected_count = 2 + if current_os == "windows" || current_os == "linux" || current_os == "macos" { 1 } else { 0 };

    assert_eq!(tasks.len(), expected_count, "任务数量不对");

    assert!(tasks.iter().any(|t| t.dest_path.ends_with("versions/1.20.1/1.20.1.jar")));

    assert!(tasks.iter().any(|t| t.dest_path.ends_with("libraries/net/java/jinput/jinput/2.0.5/jinput-2.0.5.jar")));

    let native_task = tasks.iter().find(|t| t.extract_to.is_some());
    if current_os == "windows" {
        assert!(native_task.is_some(), "Windows 下应该找到 Natives 任务");
        assert!(native_task.unwrap().dest_path.to_str().unwrap().contains("natives-windows"));
    } else if current_os == "linux" {
        assert!(native_task.is_some(), "Linux 下应该找到 Natives 任务");
        assert!(native_task.unwrap().dest_path.to_str().unwrap().contains("natives-linux"));
    }
}

#[test]
fn test_build_tasks_from_asset_index() {
    let mut objects = HashMap::new();
    objects.insert(
        "icons/icon_16x16.png".to_string(),
        AssetObject {
            hash: "bdf48ef6b5d0d23bbb02e17d04865216179f510a".to_string(),
            size: 3665,
        },
    );

    let asset_index = AssetIndex { objects };
    let root_dir = PathBuf::from(".minecraft");
    let tasks = build_tasks_from_asset_index(&asset_index, &root_dir);

    assert_eq!(tasks.len(), 1);
    assert_eq!(
        tasks[0].url,
        "https://resources.download.minecraft.net/bd/bdf48ef6b5d0d23bbb02e17d04865216179f510a"
    );
    assert!(tasks[0].dest_path.ends_with("assets/objects/bd/bdf48ef6b5d0d23bbb02e17d04865216179f510a"));
}
