use std::collections::HashMap;
use std::env;
use std::fs::{self, File};
use std::io::{self, Cursor, Read};
use std::process::Command;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};

use reqwest::blocking::Client;
use scraper::{Html, Selector};
use serde::Deserialize;
use serde_json::Value;
use zip::ZipArchive;
// Removed: use walkdir::WalkDir; // This import is not used
use dirs;

use eframe::egui;

use tar::Archive;
use flate2::read::GzDecoder;
use xz2::read::XzDecoder;

// Temurin API response structure
#[derive(Deserialize)]
struct TemurinAsset {
    binary: Binary,
}

#[derive(Deserialize)]
struct Binary {
    package: Package,
}

#[derive(Deserialize)]
struct Package {
    name: String,
    link: String,
}

/// Detects the operating system and architecture.
/// Returns a tuple of `(os_name, arch)` or `None` if unsupported.
/// The `arch` value is adjusted for different vendor APIs (e.g., "x86_64" becomes "x64" for Azul, "arm64" for Node.js).
fn detect_platform() -> Option<(&'static str, &'static str)> {
    let os = env::consts::OS;
    let arch = env::consts::ARCH; // Use raw arch and map later based on vendor needs

    match os {
        "windows" => Some(("windows", arch)),
        "macos"   => Some(("darwin",   arch)), // Node.js expects darwin
        "linux"   => Some(("linux",   arch)),
        _         => None,
    }
}

/// Helper function to compare versions. Supports "==" and ">=".
/// Performs a simple string comparison. For more complex version specifiers (e.g., "~=", "^"),
/// a dedicated version parsing library would be required.
fn is_version_compatible(installed_version: &str, required_specifier: &str) -> bool {
    if required_specifier.contains("==") {
        let parts: Vec<&str> = required_specifier.split("==").collect();
        if parts.len() == 2 {
            return installed_version == parts[1].trim();
        }
    } else if required_specifier.contains(">=") {
        let parts: Vec<&str> = required_specifier.split(">=").collect();
        if parts.len() == 2 {
            let required_version_str = parts[1].trim();
            // Simple string comparison for now. This assumes lexicographical comparison works for
            // simple cases (e.g., "3.9.1" >= "3.9.0") but might fail for complex ones
            // (e.g., "1.10.0" vs "1.2.0").
            return installed_version >= required_version_str;
        }
    } else {
        // If no specifier, assume exact match or general compatibility
        return installed_version == required_specifier;
    }
    false
}

/// Fetches the latest stable Python 3.x version from python.org.
fn get_latest_python_version() -> Result<String, String> {
    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("Python version check HTTP client failed: {}", e))?;

    let resp = client.get("https://www.python.org/downloads/")
        .send().map_err(|e| format!("Failed to reach python.org: {}", e))?
        .text().map_err(|e| format!("Failed to read python.org HTML: {}", e))?;

    let document = Html::parse_document(&resp);
    // Selector for the latest stable Python 3 release link.
    // This selector targets the link with class 'release-download-v3' within the 'download-for-current-os' section.
    let selector = Selector::parse(".download-for-current-os .release-download-v3").map_err(|e| format!("Failed to parse selector for Python version: {:?}", e))?;

    if let Some(element) = document.select(&selector).next() {
        if let Some(href) = element.value().attr("href") {
            // Example: /ftp/python/3.12.4/Python-3.12.4.tgz or /ftp/python/3.12.4/python-3.12.4-embed-amd64.zip
            let parts: Vec<&str> = href.split('/').collect();
            if let Some(filename) = parts.last() {
                // Extract version from filename like "Python-3.12.4.tgz" or "python-3.12.4-embed-amd64.zip"
                if filename.contains("Python-") {
                    let version_part = filename.replace("Python-", "").replace(".tgz", "").replace(".zip", "").replace("-embed-amd64", "");
                    return Ok(version_part);
                } else if filename.contains("python-") { // For embeddable zips
                    let version_part = filename.replace("python-", "").replace("-embed-amd64.zip", "");
                    return Ok(version_part);
                }
            }
        }
    }
    Err("Could not find the latest Python 3.x version on python.org. Please try a specific version.".to_string())
}

/// Fetches the latest stable Go version from go.dev/dl/.
fn get_latest_go_version(os_name: &str, arch: &str) -> Result<(String, String, bool), String> {
    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("Go version check HTTP client failed: {}", e))?;

    let resp = client.get("https://go.dev/dl/")
        .send().map_err(|e| format!("Failed to reach go.dev/dl/: {}", e))?
        .text().map_err(|e| format!("Failed to read go.dev/dl/ HTML: {}", e))?;

    let document = Html::parse_document(&resp);
    let toggle_button_selector = Selector::parse(".toggleButton").map_err(|e| format!("Failed to parse toggleButton selector for Go version: {:?}", e))?;
    let download_table_selector = Selector::parse(".downloadTable a").map_err(|e| format!("Failed to parse downloadTable selector for Go version: {:?}", e))?;

    let mut latest_go_version: Option<String> = None;

    // Find the latest version from the toggle buttons
    for element in document.select(&toggle_button_selector) {
        let text = element.text().collect::<String>();
        if text.contains("(latest)") {
            latest_go_version = text.split_whitespace().next().map(|s| s.to_string());
            break;
        }
    }

    let go_version = latest_go_version.ok_or("Could not find the latest Go version on go.dev/dl/.".to_string())?;

    let go_arch = match arch {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        _ => return Err(format!("Unsupported architecture for Go: {}", arch)),
    };

    let file_extension = if os_name == "windows" { ".zip" } else { ".tar.gz" };
    let expected_link_part = format!("{}-{}{}", os_name, go_arch, file_extension);

    for element in document.select(&download_table_selector) {
        if let Some(href) = element.value().attr("href") {
            if href.contains(&go_version) && href.contains(&expected_link_part) {
                let download_url = format!("https://go.dev{}", href);
                let pkg_name = href.split('/').last().unwrap_or("go_package").to_string();
                let is_zip = file_extension == ".zip";
                return Ok((download_url, pkg_name, is_zip));
            }
        }
    }

    Err(format!("Could not find Go download link for version {} on {}/{}", go_version, os_name, go_arch))
}


/// Core installation logic, refactored to take a mutable String for logging.
/// Returns Ok(()) on success, Err(String) on failure.
fn run_installation_logic(
    vendor: &str,
    version: &str,
    install_latest_flag: bool,
    python_libraries: &str, // New parameter for Python libraries
    log_output: Arc<Mutex<String>>, // Changed to Arc<Mutex<String>>
    ctx: egui::Context, // Pass context to update UI from thread
    app_state_id: egui::Id, // Pass ID to access app state in context
    cancel_requested: Arc<AtomicBool>, // Cancellation flag
) -> Result<(), String> {
    // Helper to update app state and request repaint
    let update_app_state = |
        ctx: &egui::Context,
        app_state_id: egui::Id,
        vendor_name: &str, // New parameter to identify which language's state to update
        status: Option<String>,
        download_progress: Option<f32>,
        extract_progress: Option<f32>,
    | {
        if let Some(app_state_arc) = ctx.data(|d| d.get_temp::<Arc<Mutex<JdkInstallerApp>>>(app_state_id)) {
            let mut app_state = app_state_arc.lock().expect("Failed to acquire app state lock in update_app_state");
            if let Some(lang_state) = app_state.language_states.get_mut(vendor_name) {
                if let Some(s) = status {
                    lang_state.current_status = s;
                }
                if let Some(dp) = download_progress {
                    lang_state.download_progress = dp;
                }
                if let Some(ep) = extract_progress {
                    lang_state.extract_progress = ep;
                }
            }
            drop(app_state); // Release the lock
            ctx.request_repaint();
            // Add a small sleep to make the progress visually apparent
            // This sleep is acceptable as this function runs on a separate thread.
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    };

    let mut current_log = log_output.lock().expect("Failed to acquire log mutex at start of run_installation_logic");
    current_log.push_str(&format!("Checking system information...\n"));
    drop(current_log);

    let (os_name_raw, arch_raw) = detect_platform().ok_or_else(|| {
        "Current system is not supported.".to_string()
    })?;

    let mut current_log = log_output.lock().expect("Failed to acquire log mutex after platform detect");
    current_log.push_str(&format!("OS: {}, ARCH: {}\n", os_name_raw, arch_raw));
    drop(current_log);

    let install_root = dirs::home_dir().ok_or("Could not find home directory.".to_string())?.join("jdkm");
    
    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(300)) // Increased timeout for potentially larger downloads and installations
        .build()
        .map_err(|e| format!("HTTP client creation failed: {}", e))?;

    // Determine download URL and actual version *before* idempotency check
    let (download_url, _pkg_name, is_zip, actual_download_version) = match vendor {
        "azul" => {
            let os_name = os_name_raw;
            let arch = match arch_raw {
                "x86_64" => "x64",
                "aarch64" => "aarch64",
                _ => arch_raw, // Fallback
            };
            let api;
            let display_version = if install_latest_flag { "latest" } else { version };
            update_app_state(&ctx, app_state_id, vendor, Some(format!("Preparing Azul Zulu JDK {} installation...", display_version)), None, None);
            let mut current_log = log_output.lock().expect("Failed to acquire log mutex for Azul start");
            current_log.push_str(&format!("Preparing Azul Zulu JDK {}...\n", display_version));
            drop(current_log);

            if install_latest_flag {
                api = format!(
                    "https://api.azul.com/metadata/v1/zulu/packages?latest=true&availability_types=ca&os={}&arch={}&package_type=jdk",
                    os_name, arch
                );
            } else {
                api = format!(
                    "https://api.azul.com/metadata/v1/zulu/packages?java_version={}&os={}&arch={}&package_type=jdk&latest=true&availability_types=ca",
                    version, os_name, arch
                );
            }

            let resp = client.get(&api)
                .send().map_err(|e| format!("Azul API call failed: {}", e))?;
            let json: Value = resp.json().map_err(|e| format!("Failed to parse Azul JSON: {}", e))?;

            let package_info_vec: Vec<&Value> = json.as_array()
                .ok_or_else(|| "Azul API response is not an array.".to_string())?
                .iter()
                .filter(|pkg| {
                    pkg.get("name")
                        .and_then(Value::as_str)
                        .map_or(false, |name| {
                            name.contains("-jdk") && name.ends_with(".zip")
                        })
                })
                .collect();

            let selected_package = package_info_vec.iter()
                .find(|&&pkg| {
                    pkg.get("name")
                        .and_then(Value::as_str)
                        .map_or(false, |name| {
                            !name.contains("crac") && !name.contains("fx")
                        })
                })
                .map(|&pkg| pkg)
                .or_else(|| {
                    package_info_vec.iter().find(|&&pkg| {
                        pkg.get("name")
                            .and_then(Value::as_str)
                            .map_or(false, |name| {
                                !name.contains("crac")
                            })
                    })
                    .map(|&pkg| pkg)
                })
                .or_else(|| {
                    package_info_vec.first().map(|&pkg| pkg)
                })
                .ok_or_else(|| "No suitable Azul JDK package (zip) found for the specified criteria.".to_string())?;

            let download_url = selected_package.get("download_url")
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| "Download link not found in Azul package info".to_string())?;

            let pkg_name_derived = selected_package.get("name")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| {
                    download_url.split('/').last()
                        .unwrap_or("zulu-jdk.zip")
                        .replace(".zip", "")
                });
            
            let version_from_api = selected_package.get("java_version")
                .and_then(Value::as_i64)
                .map(|v| v.to_string())
                .unwrap_or_else(|| version.to_string()); // Fallback to requested version

            (download_url, pkg_name_derived, true, version_from_api) // Azul usually provides zips
        }

        "temurin" => {
            let os_name = os_name_raw;
            let arch = match arch_raw {
                "x86_64" => "x64",
                "aarch64" => "aarch64",
                _ => arch_raw, // Fallback
            };
            let api;
            let display_version = if install_latest_flag { "latest" } else { version };
            update_app_state(&ctx, app_state_id, vendor, Some(format!("Preparing Temurin JDK {} installation...", display_version)), None, None);
            let mut current_log = log_output.lock().expect("Failed to acquire log mutex for Temurin start");
            current_log.push_str(&format!("Preparing Temurin JDK {}...\n", display_version));
            drop(current_log);

            if install_latest_flag {
                api = format!(
                    "https://api.adoptium.net/v3/assets/latest/all/hotspot?os={}&architecture={}&image_type=jdk",
                    os_name, arch
                );
            } else {
                api = format!(
                    "https://api.adoptium.net/v3/assets/latest/{}/hotspot?os={}&architecture={}&image_type=jdk",
                    version, os_name, arch
                );
            }

            let assets: Vec<TemurinAsset> = client.get(&api)
                .send().map_err(|e| format!("Temurin API call failed: {}", e))?
                .json().map_err(|e| format!("Failed to parse Temurin JSON: {}", e))?;
            let pkg = assets.into_iter().next().ok_or_else(|| "Temurin package not found".to_string())?;
            
            let is_zip_file = pkg.binary.package.name.ends_with(".zip");
            let version_from_api = version.to_string(); // Temurin API doesn't easily give exact version from asset list
            (pkg.binary.package.link, pkg.binary.package.name, is_zip_file, version_from_api)
        }

        "openjdk" => {
            let os_name = os_name_raw;
            if install_latest_flag {
                return Err("Latest version not supported for OpenJDK. Please specify a version number.".to_string());
            }
            update_app_state(&ctx, app_state_id, vendor, Some(format!("Preparing OpenJDK {} installation...", version)), None, None);
            let mut current_log = log_output.lock().expect("Failed to acquire log mutex for OpenJDK start");
            current_log.push_str(&format!("Preparing OpenJDK {}...\n", version));
            drop(current_log);
            let page = format!("https://jdk.java.net/{}", version);
            let html = client.get(&page)
                .send().map_err(|e| format!("Failed to request OpenJDK page: {}", e))?
                .text().map_err(|e| format!("Failed to read HTML: {}", e))?;

            let document = Html::parse_document(&html);
            let selector = Selector::parse("a").map_err(|e| format!("Failed to parse selector: {:?}", e))?;
            let link = document.select(&selector)
                .filter_map(|a| a.value().attr("href"))
                .find(|l| l.contains(os_name) && l.ends_with(".zip"))
                .ok_or_else(|| "OpenJDK ZIP link not found".to_string())?;
            let pkg_name_derived = link.split('/').last()
                .unwrap_or("openjdk.zip")
                .replace(".zip", "");
            (link.to_string(), pkg_name_derived, true, version.to_string()) // OpenJDK usually provides zips
        }

        "python" => {
            let os_name = os_name_raw;
            let python_version_to_download = if install_latest_flag {
                update_app_state(&ctx, app_state_id, vendor, Some("Finding latest Python version...".to_string()), None, None);
                let mut current_log = log_output.lock().expect("Failed to acquire log mutex for Python version search");
                current_log.push_str("Searching for latest Python 3.x version...\n");
                drop(current_log);
                let latest_version = get_latest_python_version()?; // Call the new function
                let mut current_log = log_output.lock().expect("Failed to acquire log mutex after Python version search");
                current_log.push_str(&format!("Found latest Python version: {}\n", latest_version));
                drop(current_log);
                latest_version
            } else {
                version.to_string()
            };

            let (url, is_zip_file) = match os_name {
                "windows" => {
                    // Prefer embeddable zip for Windows
                    (format!("https://www.python.org/ftp/python/{}/python-{}-embed-amd64.zip", python_version_to_download, python_version_to_download), true)
                },
                "darwin" | "linux" => { // macOS and Linux
                    // Prefer gzipped tarball for macOS/Linux
                    (format!("https://www.python.org/ftp/python/{}/Python-{}.tgz", python_version_to_download, python_version_to_download), false)
                },
                _ => return Err(format!("Python installation not supported for OS: {}", os_name)),
            };
            
            let pkg_name_derived = url.split('/').last()
                .unwrap_or("python_package")
                .to_string();

            update_app_state(&ctx, app_state_id, vendor, Some(format!("Preparing Python {} installation...", python_version_to_download)), None, None);
            let mut current_log = log_output.lock().expect("Failed to acquire log mutex for Python start");
            current_log.push_str(&format!("Preparing Python {}...\n", python_version_to_download));
            drop(current_log);

            (url, pkg_name_derived, is_zip_file, python_version_to_download) // Pass the actual version to be used for path
        }
        "c_cpp" => {
            let os_name = os_name_raw;
            update_app_state(&ctx, app_state_id, vendor, Some("Preparing C/C++ (MinGW-w64) installation...".to_string()), None, None);
            let mut current_log = log_output.lock().expect("Failed to acquire log mutex for C/C++ start");
            current_log.push_str("Preparing C/C++ (MinGW-w64)...\n");
            drop(current_log);

            if os_name != "windows" {
                return Err("C/C++ (MinGW-w64) installation via this installer is only supported on Windows. For Linux/macOS, please use your system's package manager (e.g., for GCC/Clang: `sudo apt install build-essential` on Debian/Ubuntu, `xcode-select --install` / `brew install gcc` on macOS).".to_string());
            }
            // For simplicity, hardcode a common MinGW-w64 build for x64 Windows.
            // A more robust solution would involve parsing SourceForge or similar.
            let url = "https://sourceforge.net/projects/mingw-w64/files/mingw-w64/mingw-w64-release/mingw-w64-v11.0.0.zip/download"; // Fixed URL for MinGW-w64 v11.0.0
            let pkg_name_derived = "mingw-w64-v11.0.0.zip".to_string();
            let is_zip_file = true;
            let actual_version = "11.0.0".to_string(); // Placeholder for MinGW version

            (url.to_string(), pkg_name_derived, is_zip_file, actual_version)
        }
        "rust" => {
            let os_name = os_name_raw;
            update_app_state(&ctx, app_state_id, vendor, Some("Preparing Rust installation...".to_string()), None, None);
            let mut current_log = log_output.lock().expect("Failed to acquire log mutex for Rust start");
            current_log.push_str("Preparing Rust via rustup...\n");
            drop(current_log);
            let (url, is_zip_file) = match os_name {
                "windows" => ("https://win.rustup.rs/x86_64".to_string(), false), // rustup-init.exe is not a zip
                "darwin" | "linux" => ("https://sh.rustup.rs".to_string(), false), // rustup-init.sh is not a zip
                _ => return Err(format!("Rust installation not supported for OS: {}", os_name)),
            };
            let pkg_name_derived = if os_name == "windows" { "rustup-init.exe".to_string() } else { "rustup-init.sh".to_string() };
            let actual_version = "stable".to_string(); // rustup installs stable by default
            (url, pkg_name_derived, is_zip_file, actual_version)
        }
        "nodejs" => {
            let os_name = os_name_raw;
            let arch = match arch_raw {
                "x86_64" => "x64",
                "aarch64" => "arm64",
                _ => arch_raw, // Fallback
            };
            update_app_state(&ctx, app_state_id, vendor, Some("Preparing Node.js LTS installation...".to_string()), None, None);
            let mut current_log = log_output.lock().expect("Failed to acquire log mutex for Node.js start");
            current_log.push_str("Preparing Node.js LTS...\n");
            drop(current_log);

            let base_url = "https://nodejs.org/dist/latest-lts/";
            let resp = client.get(base_url)
                .send().map_err(|e| format!("Failed to reach nodejs.org: {}", e))?
                .text().map_err(|e| format!("Failed to read nodejs.org HTML: {}", e))?;
            
            let document = Html::parse_document(&resp);
            let selector = Selector::parse("a").map_err(|e| format!("Failed to parse selector for Node.js version: {:?}", e))?;

            let mut node_version = "unknown".to_string();
            let mut download_link = None;
            let mut is_zip_file_node = false; // Renamed to avoid conflict

            // Find the latest LTS version link
            for element in document.select(&selector) {
                if let Some(href) = element.value().attr("href") {
                    if href.starts_with("v") && href.ends_with("/") && href.contains("lts") { // Look for LTS versions
                        node_version = href.trim_start_matches('v').trim_end_matches('/').to_string();
                        // Now search for the correct file within this version's directory
                        let expected_filename_part = format!("{}-{}", os_name, arch);
                        
                        // Construct the full URL for the specific OS/arch
                        let full_version_url = format!("{}{}", base_url, href);
                        let version_resp = client.get(&full_version_url)
                            .send().map_err(|e| format!("Failed to reach Node.js version page: {}", e))?
                            .text().map_err(|e| format!("Failed to read Node.js version HTML: {}", e))?;
                        let version_document = Html::parse_document(&version_resp);
                        let version_selector = Selector::parse("a").map_err(|e| format!("Failed to parse selector for Node.js file: {:?}", e))?;

                        for file_element in version_document.select(&version_selector) {
                            if let Some(file_href) = file_element.value().attr("href") {
                                if file_href.contains(&expected_filename_part) {
                                    if os_name == "windows" && file_href.ends_with(".zip") {
                                        download_link = Some(format!("{}{}", full_version_url, file_href));
                                        is_zip_file_node = true;
                                        break;
                                    } else if (os_name == "darwin" || os_name == "linux") && (file_href.ends_with(".tar.gz") || file_href.ends_with(".tar.xz")) {
                                        download_link = Some(format!("{}{}", full_version_url, file_href));
                                        is_zip_file_node = false;
                                        break;
                                    }
                                }
                            }
                        }
                        if download_link.is_some() {
                            break; // Found the download link for the latest LTS
                        }
                    }
                }
            }

            let final_download_url = download_link.ok_or_else(|| {
                format!("Could not find Node.js LTS download for {}/{}", os_name, arch)
            })?;
            let pkg_name_derived = final_download_url.split('/').last().unwrap_or("nodejs_package").to_string();
            (final_download_url, pkg_name_derived, is_zip_file_node, node_version)
        }
        "go" => {
            let os_name = os_name_raw;
            update_app_state(&ctx, app_state_id, vendor, Some("Preparing Go installation...".to_string()), None, None);
            let mut current_log = log_output.lock().expect("Failed to acquire log mutex for Go start");
            current_log.push_str("Preparing Go...\n");
            drop(current_log);

            let (download_url_go, pkg_name_go, is_zip_go) = get_latest_go_version(os_name, arch_raw)?;
            let actual_version_go = pkg_name_go.split('.').next().unwrap_or("unknown").trim_start_matches("go").to_string();

            (download_url_go, pkg_name_go, is_zip_go, actual_version_go)
        }
        other => {
            return Err(format!("Unsupported vendor: {}", other));
        }
    };

    // Determine the expected final installation path for idempotency check
    let expected_final_sdk_path = if vendor == "rust" {
        dirs::home_dir().ok_or_else(|| "Could not find home directory for .cargo path.".to_string())?.join(".cargo")
    } else {
        install_root.join(format!("{}_versions", vendor)).join(format!("{}-{}", vendor, actual_download_version))
    };

    // --- Idempotency Check ---
    update_app_state(&ctx, app_state_id, vendor, Some("Checking for existing installations...".to_string()), None, None);
    let mut current_log = log_output.lock().expect("Failed to acquire log mutex for existing installations check");
    
    let mut is_already_installed = false;
    if expected_final_sdk_path.exists() {
        let (verification_command_path, version_arg) = match vendor {
            "python" => (expected_final_sdk_path.join(if os_name_raw == "windows" { "python.exe" } else { "bin/python3" }), "--version"),
            "c_cpp" => (expected_final_sdk_path.join(if os_name_raw == "windows" { "bin/gcc.exe" } else { "bin/gcc" }), "--version"),
            "rust" => (expected_final_sdk_path.join("bin/rustc"), "--version"), // .cargo/bin/rustc
            "nodejs" => (expected_final_sdk_path.join(if os_name_raw == "windows" { "node.exe" } else { "bin/node" }), "--version"),
            "go" => (expected_final_sdk_path.join(if os_name_raw == "windows" { "bin/go.exe" } else { "bin/go" }), "version"),
            _ => (expected_final_sdk_path.join(if os_name_raw == "windows" { "bin/java.exe" } else { "bin/java" }), "-version"), // Java
        };

        if verification_command_path.exists() {
            let output = Command::new(&verification_command_path)
                .arg(version_arg)
                .output();
            
            if let Ok(output) = output {
                let installed_version_str = if vendor == "python" {
                    String::from_utf8_lossy(&output.stdout).trim().replace("Python ", "").to_string()
                } else if vendor == "rust" {
                    String::from_utf8_lossy(&output.stdout).lines().next()
                        .unwrap_or("unknown rustc version").replace("rustc ", "").split(' ').next().unwrap_or("unknown").to_string()
                }
                else if vendor == "c_cpp" {
                    String::from_utf8_lossy(&output.stdout).lines().next()
                        .unwrap_or("unknown gcc version").split(' ').nth(2).unwrap_or("unknown").to_string()
                }
                else if vendor == "nodejs" {
                    String::from_utf8_lossy(&output.stdout).trim().replace("v", "").to_string()
                }
                else if vendor == "go" {
                    String::from_utf8_lossy(&output.stdout).trim().replace("go version go", "").split_whitespace().next().unwrap_or("unknown").to_string()
                }
                else { // Java vendors
                    let stderr_str = String::from_utf8_lossy(&output.stderr);
                    stderr_str.lines().find(|line| line.contains("version"))
                        .map(|line| line.replace("openjdk version \"", "").replace("java version \"", "").trim_end_matches('"').to_string())
                        .unwrap_or_else(|| "unknown".to_string())
                };

                // Compare installed version with requested version/latest logic
                let target_version_for_check = if install_latest_flag {
                    actual_download_version.clone() // Check against the version we *would* download
                } else {
                    version.to_string() // Check against the explicitly requested version
                };

                if is_version_compatible(&installed_version_str, &target_version_for_check) {
                    current_log.push_str(&format!("{} version {} is already installed at {}.\n", vendor, installed_version_str, expected_final_sdk_path.display()));
                    is_already_installed = true;
                } else {
                    current_log.push_str(&format!("Existing {} version {} at {} is not compatible with requested version {}. Proceeding with new installation.\n", vendor, installed_version_str, expected_final_sdk_path.display(), target_version_for_check));
                }
            } else {
                current_log.push_str(&format!("Failed to verify existing {} installation at {}. Proceeding with new installation.\n", vendor, expected_final_sdk_path.display()));
            }
        } else {
            current_log.push_str(&format!("Executable not found for existing {} installation at {}. Proceeding with new installation.\n", vendor, expected_final_sdk_path.display()));
        }
    } else {
        current_log.push_str(&format!("No existing {} installation found at {}. Proceeding with new installation.\n", vendor, expected_final_sdk_path.display()));
    }
    drop(current_log);

    if is_already_installed {
        update_app_state(&ctx, app_state_id, vendor, Some(format!("{} is already installed.", vendor)), Some(1.0), Some(1.0));
        return Ok(());
    }
    // --- End Idempotency Check ---

    // Proceed with download and installation if not already installed
    update_app_state(&ctx, app_state_id, vendor, Some(format!("Downloading {}...", vendor)), Some(0.0), Some(0.0));
    let mut current_log = log_output.lock().expect("Failed to acquire log mutex for download start");
    current_log.push_str(&format!("Downloading: {}\n", download_url));
    drop(current_log);
    
    let mut response = client.get(&download_url)
        .send().map_err(|e| format!("Failed to download from {}: {}", download_url, e))?;

    let total_size = response.content_length().unwrap_or(0);
    let mut downloaded_bytes: u64 = 0;
    let mut buffer = Vec::new(); // Use a buffer to accumulate bytes

    // Read the response body in chunks and update progress
    loop {
        if cancel_requested.load(Ordering::SeqCst) {
            let mut current_log = log_output.lock().expect("Failed to acquire log mutex for cancellation during download");
            current_log.push_str("Installation cancelled during download.\n");
            drop(current_log);
            update_app_state(&ctx, app_state_id, vendor, Some("Installation cancelled.".to_string()), None, None);
            return Err("Installation cancelled by user.".to_string());
        }
        let mut chunk = vec![0; 8192]; // Read in 8KB chunks
        let bytes_read = match response.read(&mut chunk) {
            Ok(0) => break, // End of stream
            Ok(n) => n,
            Err(e) => return Err(format!("Failed to read download stream: {}", e)),
        };
        buffer.extend(chunk.iter().take(bytes_read));
        downloaded_bytes += bytes_read as u64;

        let progress = if total_size > 0 {
            downloaded_bytes as f32 / total_size as f32
        } else {
            0.0
        };
        update_app_state(&ctx, app_state_id, vendor, Some(format!("Downloading... {:.0}%", progress * 100.0)), Some(progress), None);
        let mut current_log = log_output.lock().expect("Failed to acquire log mutex for download progress");
        current_log.push_str(&format!("Download progress: {:.2}%\n", progress * 100.0));
        drop(current_log);
    }

    let mut bytes_cursor = Cursor::new(buffer);

    // Create the base directory for versions if it doesn't exist
    let vendor_versions_path = install_root.join(format!("{}_versions", vendor));
    fs::create_dir_all(&vendor_versions_path).map_err(|e| format!("Failed to create vendor versions directory {}: {}", vendor_versions_path.display(), e))?;

    let mut extracted_top_level_dir_name: Option<String> = None;
    let current_install_target_path = expected_final_sdk_path.clone(); // Use the pre-determined path

    if vendor == "rust" {
        // Rustup handles its own installation path, typically ~/.cargo
        // We just need to execute the downloaded rustup-init.
        let rustup_init_path = if os_name_raw == "windows" {
            install_root.join("rustup-init.exe") // Place init in jdkm root for temp use
        } else {
            install_root.join("rustup-init.sh")
        };

        let mut rustup_file = File::create(&rustup_init_path)
            .map_err(|e| format!("Failed to create rustup-init file: {}", e))?;
        io::copy(&mut bytes_cursor, &mut rustup_file)
            .map_err(|e| format!("Failed to write rustup-init file: {}", e))?;
        
        if os_name_raw != "windows" {
            Command::new("chmod")
                .arg("+x")
                .arg(&rustup_init_path)
                .output()
                .map_err(|e| format!("Failed to make rustup-init.sh executable: {}", e))?;
        }

        update_app_state(&ctx, app_state_id, vendor, Some("Running rustup installer...".to_string()), None, Some(0.0));
        let mut current_log = log_output.lock().expect("Failed to acquire log mutex for rustup-init run");
        current_log.push_str("Running rustup-init...\n");
        drop(current_log);

        let mut command = Command::new(&rustup_init_path);
        command.arg("--default-toolchain").arg("stable").arg("-y");
        
        let rustup_output = command
            .output()
            .map_err(|e| format!("Failed to run rustup-init: {}", e))?;

        let mut current_log = log_output.lock().expect("Failed to acquire log mutex for rustup-init output");
        current_log.push_str(&format!("{}", String::from_utf8_lossy(&rustup_output.stdout)));
        current_log.push_str(&format!("{}", String::from_utf8_lossy(&rustup_output.stderr)));
        drop(current_log);

        if rustup_output.status.success() {
            let mut current_log = log_output.lock().expect("Failed to acquire log mutex for Rust success");
            current_log.push_str("Rust installed successfully via rustup.\n");
            drop(current_log);
            // The actual_sdk_root for Rust is ~/.cargo, which was already set in expected_final_sdk_path
            let mut current_log = log_output.lock().expect("Failed to acquire log mutex for Rust cargo home info");
            current_log.push_str(&format!("Rust's cargo home: {}\n", expected_final_sdk_path.display()));
            current_log.push_str(&format!("Rust's PATH has been automatically configured by rustup for persistent use in new terminal sessions.\n"));
            drop(current_log);
        } else {
            let mut current_log = log_output.lock().expect("Failed to acquire log mutex for Rust failure");
            current_log.push_str("Rust installation failed.\n");
            drop(current_log);
            return Err("Rust installation failed.".to_string());
        }

        fs::remove_file(&rustup_init_path)
            .map_err(|e| format!("Failed to remove rustup-init: {}", e))?;
        let mut current_log = log_output.lock().expect("Failed to acquire log mutex for rustup-init cleanup");
        current_log.push_str("Cleaned up rustup-init.\n");
        drop(current_log);

    } else { // Handle ZIP and Tarball extractions for other vendors
        if is_zip {
            let mut archive = ZipArchive::new(bytes_cursor)
                .map_err(|e| format!("Failed to parse ZIP archive: {}", e))?;
            let total_files = archive.len();
            update_app_state(&ctx, app_state_id, vendor, Some("Extracting files, almost there...".to_string()), None, Some(0.0));

            for i in 0..total_files {
                if cancel_requested.load(Ordering::SeqCst) {
                    let mut current_log = log_output.lock().expect("Failed to acquire log mutex for cancellation during extraction");
                    current_log.push_str("Installation cancelled during extraction.\n");
                    drop(current_log);
                    update_app_state(&ctx, app_state_id, vendor, Some("Installation cancelled.".to_string()), None, None);
                    return Err("Installation cancelled by user.".to_string());
                }
                let mut file = archive.by_index(i).map_err(|e| format!("Failed to get file from archive at index {}: {}", i, e))?;
                let file_path_in_zip = PathBuf::from(file.name());

                if extracted_top_level_dir_name.is_none() && file.is_dir() {
                    if let Some(top_level_component) = file_path_in_zip.components().next().and_then(|c| c.as_os_str().to_str()) {
                        extracted_top_level_dir_name = Some(top_level_component.to_string());
                    }
                }

                let out_path = current_install_target_path.join(file.name());

                if (*file.name()).ends_with('/') {
                    fs::create_dir_all(&out_path).map_err(|e| format!("Failed to create directory {}: {}", out_path.display(), e))?;
                } else {
                    if let Some(p) = out_path.parent() {
                        fs::create_dir_all(p).map_err(|e| format!("Failed to create parent directory {}: {}", p.display(), e))?;
                    }
                    let mut outfile = File::create(&out_path).map_err(|e| format!("Failed to create file {}: {}", out_path.display(), e))?;
                    io::copy(&mut file, &mut outfile).map_err(|e| format!("Failed to copy data to file {}: {}", out_path.display(), e))?;
                }
                let progress = (i + 1) as f32 / total_files as f32;
                update_app_state(&ctx, app_state_id, vendor, Some(format!("Extracting... {:.0}%", progress * 100.0)), None, Some(progress));
                let mut current_log = log_output.lock().expect("Failed to acquire log mutex for extraction progress");
                current_log.push_str(&format!("Extraction progress: {:.2}%\n", progress * 100.0));
                drop(current_log);
            }
        } else { // Handle tarballs (.tgz, .tar.xz)
            let decoder: Box<dyn Read> = if _pkg_name.ends_with(".tgz") || _pkg_name.ends_with(".tar.gz") {
                Box::new(GzDecoder::new(bytes_cursor))
            } else if _pkg_name.ends_with(".tar.xz") {
                Box::new(XzDecoder::new(bytes_cursor))
            } else {
                return Err(format!("Unsupported archive format: {}", _pkg_name));
            };

            let mut archive = Archive::new(decoder);
            
            let mut entries_processed = 0;
            // A rough estimate for total entries in a tarball for progress, or could count first pass
            // For now, using a large number to ensure progress bar moves.
            let total_tar_entries_estimate = 1000.0; 
            update_app_state(&ctx, app_state_id, vendor, Some("Extracting files, almost there...".to_string()), None, Some(0.0));

            for entry_result in archive.entries().map_err(|e| format!("Failed to read tar archive entries: {}", e))? {
                if cancel_requested.load(Ordering::SeqCst) {
                    let mut current_log = log_output.lock().expect("Failed to acquire log mutex for cancellation during tar extraction");
                    current_log.push_str("Installation cancelled during extraction.\n");
                    drop(current_log);
                    update_app_state(&ctx, app_state_id, vendor, Some("Installation cancelled.".to_string()), None, None);
                    return Err("Installation cancelled by user.".to_string());
                }
                let mut entry = entry_result.map_err(|e| format!("Failed to get tar entry: {}", e))?;
                let entry_path = entry.path().map_err(|e| format!("Failed to get tar entry path: {}", e))?;

                if extracted_top_level_dir_name.is_none() && entry.header().entry_type().is_dir() {
                    if let Some(top_level_component) = entry_path.components().next().and_then(|c| c.as_os_str().to_str()) {
                        extracted_top_level_dir_name = Some(top_level_component.to_string());
                    }
                }
                
                let out_path = current_install_target_path.join(&entry_path);

                if entry.header().entry_type().is_dir() {
                    fs::create_dir_all(&out_path).map_err(|e| format!("Failed to create directory {}: {}", out_path.display(), e))?;
                } else {
                    if let Some(p) = out_path.parent() {
                        fs::create_dir_all(p).map_err(|e| format!("Failed to create parent directory {}: {}", p.display(), e))?;
                    }
                    let mut outfile = File::create(&out_path).map_err(|e| format!("Failed to create file {}: {}", out_path.display(), e))?;
                    io::copy(&mut entry, &mut outfile).map_err(|e| format!("Failed to copy data to file {}: {}", out_path.display(), e))?;
                }
                entries_processed += 1;
                let progress = (entries_processed as f32 / total_tar_entries_estimate).min(1.0);
                update_app_state(&ctx, app_state_id, vendor, Some(format!("Extracting... {:.0}%", progress * 100.0)), None, Some(progress));
                let mut current_log = log_output.lock().expect("Failed to acquire log mutex for tar extraction progress");
                current_log.push_str(&format!("Extraction progress: {:.2}%\n", progress * 100.0));
                drop(current_log);
            }
            update_app_state(&ctx, app_state_id, vendor, None, None, Some(1.0));
        }
        let mut current_log = log_output.lock().expect("Failed to acquire log mutex after extraction");
        current_log.push_str("Extraction complete.\n");
        drop(current_log);

        // For non-rust installations, the actual SDK root is the current_install_target_path
        // which now contains the extracted content.
        // If there was a top-level directory in the archive, append it.
        if let Some(dir_name) = extracted_top_level_dir_name {
            // If the extracted content is within a single top-level directory,
            // move the contents of that directory up to `current_install_target_path`
            // and then remove the now-empty top-level directory.
            let temp_extracted_path = current_install_target_path.join(&dir_name);
            if temp_extracted_path.exists() && temp_extracted_path.is_dir() {
                let mut current_log = log_output.lock().expect("Failed to acquire log mutex for dir move");
                current_log.push_str(&format!("Moving contents from {} to {}...\n", temp_extracted_path.display(), current_install_target_path.display()));
                drop(current_log);

                // Move contents
                for entry in fs::read_dir(&temp_extracted_path).map_err(|e| format!("Failed to read temp extracted dir: {}", e))? {
                    let entry = entry.map_err(|e| format!("Failed to read entry in temp extracted dir: {}", e))?;
                    let original_path = entry.path();
                    let target_path = current_install_target_path.join(entry.file_name());
                    fs::rename(&original_path, &target_path).map_err(|e| format!("Failed to move {:?} to {:?}: {}", original_path, target_path, e))?;
                }
                // Remove the empty top-level directory
                fs::remove_dir(&temp_extracted_path).map_err(|e| format!("Failed to remove temp extracted dir {}: {}", temp_extracted_path.display(), e))?;
                let mut current_log = log_output.lock().expect("Failed to acquire log mutex for dir move complete");
                current_log.push_str("Contents moved.\n");
                drop(current_log);
            }
        }
    }


    // Set JAVA_HOME or PYTHON_HOME or PATH for C/C++/Rust/Node.js/Go
    // Use expected_final_sdk_path as the actual_sdk_root after successful installation
    let actual_sdk_root_final = expected_final_sdk_path;

    if vendor == "python" {
        std::env::set_var("PYTHON_HOME", &actual_sdk_root_final);
        let mut current_log = log_output.lock().expect("Failed to acquire log mutex for PYTHON_HOME set");
        current_log.push_str(&format!("PYTHON_HOME={}\n", actual_sdk_root_final.display()));
        current_log.push_str(&format!("For persistent use across new terminal sessions, you will need to manually add `{}` to your system's PATH environment variable. This typically requires administrative privileges.\n", actual_sdk_root_final.display()));
        drop(current_log);
    } else if vendor == "c_cpp" {
        let mingw_bin_path = actual_sdk_root_final.join("bin");
        let current_path = env::var("PATH").unwrap_or_default();
        env::set_var("PATH", format!("{};{}", mingw_bin_path.display(), current_path));
        let mut current_log = log_output.lock().expect("Failed to acquire log mutex for C/C++ PATH update");
        current_log.push_str(&format!("PATH updated for current session: {}\n", mingw_bin_path.display()));
        current_log.push_str(&format!("For persistent use across new terminal sessions, you will need to manually add `{}` to your system's PATH environment variable. This typically requires administrative privileges.\n", mingw_bin_path.display()));
        drop(current_log);
    } else if vendor == "nodejs" {
        let node_bin_path = if os_name_raw == "windows" {
            actual_sdk_root_final.clone() // Node.js on Windows has node.exe directly in root
        } else {
            actual_sdk_root_final.join("bin")
        };
        let current_path = env::var("PATH").unwrap_or_default();
        env::set_var("PATH", format!("{};{}", node_bin_path.display(), current_path));
        let mut current_log = log_output.lock().expect("Failed to acquire log mutex for Node.js PATH update");
        current_log.push_str(&format!("PATH updated for current session: {}\n", node_bin_path.display()));
        current_log.push_str(&format!("For persistent use across new terminal sessions, you will need to manually add `{}` to your system's PATH environment variable. This typically requires administrative privileges.\n", node_bin_path.display()));
        drop(current_log);
    } else if vendor == "go" {
        std::env::set_var("GOROOT", &actual_sdk_root_final);
        let go_bin_path = actual_sdk_root_final.join("bin");
        let current_path = env::var("PATH").unwrap_or_default();
        env::set_var("PATH", format!("{};{}", go_bin_path.display(), current_path));
        let mut current_log = log_output.lock().expect("Failed to acquire log mutex for Go GOROOT/PATH update");
        current_log.push_str(&format!("GOROOT={}\n", actual_sdk_root_final.display()));
        current_log.push_str(&format!("PATH updated for current session: {}\n", go_bin_path.display()));
        current_log.push_str(&format!("For persistent use across new terminal sessions, you will need to manually add `{}` to your system's PATH environment variable. This typically requires administrative privileges.\n", go_bin_path.display()));
        drop(current_log);
    }
    else if vendor != "rust" { // Java vendors
        std::env::set_var("JAVA_HOME", &actual_sdk_root_final);
        let mut current_log = log_output.lock().expect("Failed to acquire log mutex for JAVA_HOME set");
        current_log.push_str(&format!("JAVA_HOME={}\n", actual_sdk_root_final.display()));
        current_log.push_str(&format!("For persistent use across new terminal sessions, you will need to manually add `{}` to your system's PATH environment variable. This typically requires administrative privileges.\n", actual_sdk_root_final.join("bin").display()));
        drop(current_log);
    }


    // Verification step
    update_app_state(&ctx, app_state_id, vendor, Some(format!("Verifying {} installation...", vendor)), None, None);
    let mut current_log = log_output.lock().expect("Failed to acquire log mutex for verification start");
    current_log.push_str(&format!("Verifying {} version...\n", vendor));
    drop(current_log);

    let (verification_command_path, version_arg) = match vendor {
        "python" => {
            let path = if os_name_raw == "windows" {
                actual_sdk_root_final.join("python.exe")
            } else {
                actual_sdk_root_final.join("bin").join("python3")
            };
            (path, "--version")
        },
        "c_cpp" => {
            let path = if os_name_raw == "windows" {
                actual_sdk_root_final.join("bin").join("gcc.exe")
            } else {
                actual_sdk_root_final.join("bin").join("gcc")
            };
            (path, "--version")
        },
        "rust" => {
            let path = dirs::home_dir().ok_or_else(|| "Could not find home directory for .cargo path.".to_string())?.join(".cargo").join("bin").join("rustc");
            (path, "--version")
        },
        "nodejs" => {
            let path = if os_name_raw == "windows" {
                actual_sdk_root_final.join("node.exe")
            } else {
                actual_sdk_root_final.join("bin").join("node")
            };
            (path, "--version")
        },
        "go" => {
            let path = if os_name_raw == "windows" {
                actual_sdk_root_final.join("bin").join("go.exe")
            } else {
                actual_sdk_root_final.join("bin").join("go")
            };
            (path, "version") // Go uses "go version" not "go --version"
        },
        _ => { // Java vendors
            let path = if os_name_raw == "windows" {
                actual_sdk_root_final.join("bin").join("java.exe")
            } else {
                actual_sdk_root_final.join("bin").join("java")
            };
            (path, "-version")
        }
    };

    let output = Command::new(&verification_command_path)
        .arg(version_arg)
        .output()
        .map_err(|e| format!("Failed to execute {} verification command: {}", vendor, e))?;
    
    let mut current_log = log_output.lock().expect("Failed to acquire log mutex for verification output");
    current_log.push_str(&format!("{}", String::from_utf8_lossy(&output.stderr)));
    current_log.push_str(&format!("{}", String::from_utf8_lossy(&output.stdout))); // Python/Rust/Node.js/Go outputs to stdout
    drop(current_log);

    if output.status.success() {
        let installed_version_str = if vendor == "python" {
            String::from_utf8_lossy(&output.stdout).trim().replace("Python ", "").to_string()
        } else if vendor == "rust" {
            String::from_utf8_lossy(&output.stdout).lines().next()
                .unwrap_or("unknown rustc version").replace("rustc ", "").split(' ').next().unwrap_or("unknown").to_string()
        }
        else if vendor == "c_cpp" {
            String::from_utf8_lossy(&output.stdout).lines().next()
                .unwrap_or("unknown gcc version").split(' ').nth(2).unwrap_or("unknown").to_string()
        }
        else if vendor == "nodejs" {
            String::from_utf8_lossy(&output.stdout).trim().replace("v", "").to_string()
        }
        else if vendor == "go" {
            String::from_utf8_lossy(&output.stdout).trim().replace("go version go", "").split_whitespace().next().unwrap_or("unknown").to_string()
        }
        else {
            // Parse Java version from stderr (e.g., "openjdk version "21.0.2"")
            let stderr_str = String::from_utf8_lossy(&output.stderr);
            stderr_str.lines().find(|line| line.contains("version"))
                .map(|line| line.replace("openjdk version \"", "").replace("java version \"", "").trim_end_matches('"').to_string())
                .unwrap_or_else(|| "unknown".to_string())
        };

        let mut current_log = log_output.lock().expect("Failed to acquire log mutex for successful verification");
        current_log.push_str(&format!("{} version {} installed.\n", vendor, installed_version_str));
        drop(current_log);
        
        // Check specific version compatibility for Python (and potentially others in the future)
        if vendor == "python" {
            // Use the version from the GUI input for compatibility check, as that's what the user *requested*
            let mut current_log = log_output.lock().expect("Failed to acquire log mutex for Python compatibility check");
            current_log.push_str(&format!("Checking Python version compatibility: Installed '{}' vs Required '{}'.\n", installed_version_str, version));
            drop(current_log);
            if !is_version_compatible(&installed_version_str, version) {
                let mut current_log = log_output.lock().expect("Failed to acquire log mutex for Python version mismatch");
                current_log.push_str(&format!("Installed Python version {} does not match required version {}.\n", installed_version_str, version));
                drop(current_log);
                update_app_state(&ctx, app_state_id, vendor, Some(format!("Python version mismatch: Expected {}, got {}.", version, installed_version_str)), None, None);
                return Err(format!("Python version mismatch: Expected {}, got {}.", version, installed_version_str));
            } else {
                let mut current_log = log_output.lock().expect("Failed to acquire log mutex for Python version match");
                current_log.push_str(&format!("Installed Python version {} matches required version {}.\n", installed_version_str, version));
                drop(current_log);
            }

            // --- START: PIP BOOTSTRAP AND LIBRARY INSTALLATION ---
            let python_exe_path = if os_name_raw == "windows" {
                actual_sdk_root_final.join("python.exe")
            } else {
                actual_sdk_root_final.join("bin").join("python3")
            };

            // Determine pip executable path based on OS
            let pip_exe_path = if os_name_raw == "windows" {
                actual_sdk_root_final.join("Scripts").join("pip.exe")
            } else {
                python_exe_path.clone() // Used with -m pip
            };

            // Step 1: Bootstrap pip if it's missing (common for embedded zips).
            if os_name_raw == "windows" {
                update_app_state(&ctx, app_state_id, vendor, Some("Downloading pip installer...".to_string()), None, None);
                let mut current_log = log_output.lock().expect("Failed to acquire log mutex for get-pip.py download start");
                current_log.push_str("Downloading get-pip.py...\n");
                drop(current_log);
                let get_pip_url = "https://bootstrap.pypa.io/get-pip.py";
                let mut get_pip_response = client.get(get_pip_url)
                    .send().map_err(|e| format!("Failed to download get-pip.py: {}", e))?;
                
                let get_pip_path = actual_sdk_root_final.join("get-pip.py");
                let mut get_pip_file = File::create(&get_pip_path)
                    .map_err(|e| format!("Failed to create get-pip.py file: {}", e))?;
                io::copy(&mut get_pip_response, &mut get_pip_file)
                    .map_err(|e| format!("Failed to save get-pip.py: {}", e))?;
                let mut current_log = log_output.lock().expect("Failed to acquire log mutex for get-pip.py download complete");
                current_log.push_str("get-pip.py download complete.\n");
                drop(current_log);

                update_app_state(&ctx, app_state_id, vendor, Some("Installing pip...".to_string()), None, None);
                let mut current_log = log_output.lock().expect("Failed to acquire log mutex for pip install start");
                current_log.push_str("Running get-pip.py to install pip...\n");
                drop(current_log);
                let pip_install_output = Command::new(&python_exe_path)
                    .arg(&get_pip_path)
                    .output()
                    .map_err(|e| format!("Failed to execute get-pip.py: {}", e))?;
                
                let mut current_log = log_output.lock().expect("Failed to acquire log mutex for pip install output");
                current_log.push_str(&format!("{}", String::from_utf8_lossy(&pip_install_output.stdout)));
                current_log.push_str(&format!("{}", String::from_utf8_lossy(&pip_install_output.stderr)));
                drop(current_log);

                if pip_install_output.status.success() {
                    let mut current_log = log_output.lock().expect("Failed to acquire log mutex for pip install success");
                    current_log.push_str("pip installed successfully.\n");
                    drop(current_log);
                } else {
                    let mut current_log = log_output.lock().expect("Failed to acquire log mutex for pip install failure");
                    current_log.push_str("Failed to install pip using get-pip.py.\n");
                    drop(current_log);
                    return Err("pip installation failed. Cannot proceed with library installation.".to_string());
                }

                // Clean up get-pip.py
                fs::remove_file(&get_pip_path)
                    .map_err(|e| format!("Failed to remove get-pip.py: {}", e))?;
                let mut current_log = log_output.lock().expect("Failed to acquire log mutex for get-pip.py cleanup");
                current_log.push_str("Cleaned up get-pip.py.\n");
                drop(current_log);

            } else { // Attempt ensurepip for non-Windows
                update_app_state(&ctx, app_state_id, vendor, Some("Checking pip availability...".to_string()), None, None);
                let mut current_log = log_output.lock().expect("Failed to acquire log mutex for ensurepip start");
                current_log.push_str("Checking pip availability...\n");
                drop(current_log);
                let ensurepip_output = Command::new(&python_exe_path)
                    .arg("-m")
                    .arg("ensurepip")
                    .arg("--default-pip")
                    .output()
                    .map_err(|e| format!("Failed to bootstrap pip: {}", e))?;

                let mut current_log = log_output.lock().expect("Failed to acquire log mutex for ensurepip output");
                current_log.push_str(&format!("{}", String::from_utf8_lossy(&ensurepip_output.stdout)));
                current_log.push_str(&format!("{}", String::from_utf8_lossy(&ensurepip_output.stderr)));
                drop(current_log);

                if ensurepip_output.status.success() {
                    let mut current_log = log_output.lock().expect("Failed to acquire log mutex for ensurepip success");
                    current_log.push_str("pip is now available.\n");
                    drop(current_log);
                } else {
                    let mut current_log = log_output.lock().expect("Failed to acquire log mutex for ensurepip failure");
                    current_log.push_str("Failed to ensure pip is available. Library installation might fail.\n");
                    drop(current_log);
                    // Do not return Err here, allow library installation to proceed and report its own errors.
                }
            }


            // Step 2: Install Python libraries
            let libraries: Vec<&str> = python_libraries.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
            if !libraries.is_empty() {
                update_app_state(&ctx, app_state_id, vendor, Some("Installing Python libraries...".to_string()), None, None);
                let mut current_log = log_output.lock().expect("Failed to acquire log mutex for Python library install start");
                current_log.push_str("Installing specified Python libraries...\n");
                drop(current_log);

                for lib_spec in libraries {
                    let mut current_log = log_output.lock().expect("Failed to acquire log mutex for library install attempt");
                    current_log.push_str(&format!("Attempting to install: {}\n", lib_spec));
                    drop(current_log);
                    let pip_install_output = if os_name_raw == "windows" {
                        // For Windows, call pip.exe directly.
                        Command::new(&pip_exe_path)
                            .arg("install")
                            .arg(lib_spec)
                            .output()
                            .map_err(|e| format!("Failed to execute pip install for {}: {}", lib_spec, e))?
                    } else {
                        // For non-Windows, use python -m pip
                        Command::new(&python_exe_path)
                            .arg("-m")
                            .arg("pip")
                            .arg("install")
                            .arg(lib_spec)
                            .output()
                            .map_err(|e| format!("Failed to execute pip install for {}: {}", lib_spec, e))?
                    };
                    
                    let mut current_log = log_output.lock().expect("Failed to acquire log mutex for pip install output");
                    current_log.push_str(&format!("{}", String::from_utf8_lossy(&pip_install_output.stdout)));
                    current_log.push_str(&format!("{}", String::from_utf8_lossy(&pip_install_output.stderr)));
                    drop(current_log);

                    if pip_install_output.status.success() {
                        let mut current_log = log_output.lock().expect("Failed to acquire log mutex for library install success");
                        current_log.push_str(&format!("Successfully installed: {}\n", lib_spec));
                        drop(current_log);
                        
                        // Verify installed library version
                        let lib_name = lib_spec.split_once(&['=', '>', '<', '~'][..]).map_or(lib_spec, |(name, _)| name);
                        let pip_show_output = if os_name_raw == "windows" {
                            // For Windows, call pip.exe directly.
                            Command::new(&pip_exe_path)
                                .arg("show")
                                .arg(lib_name)
                                .output()
                                .map_err(|e| format!("Failed to execute pip show for {}: {}", lib_name, e))?
                        } else {
                            // For non-Windows, use python -m pip
                            Command::new(&python_exe_path)
                                .arg("-m")
                                .arg("pip")
                                .arg("show")
                                .arg(lib_name)
                                .output()
                                .map_err(|e| format!("Failed to execute pip show for {}: {}", lib_name, e))?
                        };
                        
                        let pip_show_str = String::from_utf8_lossy(&pip_show_output.stdout);
                        let installed_lib_version = pip_show_str.lines()
                            .find(|line| line.starts_with("Version:"))
                            .and_then(|line| line.split(':').nth(1))
                            .map_or("unknown", |s| s.trim());

                        let mut current_log = log_output.lock().expect("Failed to acquire log mutex for library compatibility check");
                        current_log.push_str(&format!("Checking library compatibility for {}: Installed '{}' vs Required '{}'.\n", lib_name, installed_lib_version, lib_spec));
                        drop(current_log);
                        if !is_version_compatible(installed_lib_version, lib_spec) {
                            let mut current_log = log_output.lock().expect("Failed to acquire log mutex for library version mismatch");
                            current_log.push_str(&format!("Installed version of {} ({}) does not meet requirement {}.\n", lib_name, installed_lib_version, lib_spec));
                            drop(current_log);
                            update_app_state(&ctx, app_state_id, vendor, Some(format!("Library compatibility issue for {}: Expected {}, got {}.", lib_name, lib_spec, installed_lib_version)), None, None);
                            return Err(format!("Library compatibility issue for {}: Expected {}, got {}.", lib_name, lib_spec, installed_lib_version));
                        } else {
                            let mut current_log = log_output.lock().expect("Failed to acquire log mutex for library version match");
                            current_log.push_str(&format!("{} version verified: {} (meets requirement {}).\n", lib_name, installed_lib_version, lib_spec));
                            drop(current_log);
                        }

                    } else {
                        let mut current_log = log_output.lock().expect("Failed to acquire log mutex for library install failure");
                        current_log.push_str(&format!("Failed to install: {}\n", lib_spec));
                        drop(current_log);
                        update_app_state(&ctx, app_state_id, vendor, Some(format!("Python library installation failed: {}.", lib_spec)), None, None);
                        return Err(format!("Python library installation failed: {}.", lib_spec));
                    }
                }
            }
            // --- END: PIP BOOTSTRAP AND LIBRARY INSTALLATION ---
        }
        update_app_state(&ctx, app_state_id, vendor, Some(format!("{} installation complete!", vendor)), Some(1.0), Some(1.0));
    } else {
        let mut current_log = log_output.lock().expect("Failed to acquire log mutex for verification failure");
        current_log.push_str(&format!("{} verification failed.", vendor));
        drop(current_log);
        update_app_state(&ctx, app_state_id, vendor, Some(format!("{} verification failed.", vendor)), None, None);
        return Err(format!("{} verification failed.", vendor));
    }
    Ok(())
}

/// Represents the configuration for a specific language installation.
struct LanguageConfig {
    vendor: String,
    version: String,
    install_latest: bool,
    python_libraries_input: String, // Specific to Python.
}

impl Default for LanguageConfig {
    fn default() -> Self {
        LanguageConfig {
            vendor: "azul".to_owned(), // Default to Java Azul.
            version: "21".to_owned(),
            install_latest: false,
            python_libraries_input: "".to_owned(),
        }
    }
}

/// Represents the runtime state of a specific language installation.
struct LanguageState {
    output_log: Arc<Mutex<String>>, // Shared state for logging
    is_installing: bool,
    install_result: Option<Result<(), String>>,
    download_progress: f32, // 0.0 to 1.0
    extract_progress: f32,  // 0.0 to 1.0
    current_status: String,
    cancel_requested: Arc<AtomicBool>,
}

impl Default for LanguageState {
    fn default() -> Self {
        LanguageState {
            output_log: Arc::new(Mutex::new(String::new())),
            is_installing: false,
            install_result: None,
            download_progress: 0.0,
            extract_progress: 0.0,
            current_status: "Ready for installation".to_string(),
            cancel_requested: Arc::new(AtomicBool::new(false)),
        }
    }
}


// Main GUI application structure
struct JdkInstallerApp {
    language_configs: HashMap<String, LanguageConfig>,
    language_states: HashMap<String, LanguageState>,
    selected_vendor: String, // Current active "tab"
    font_size: f32,
    show_cancel_confirmation: bool,
    show_exit_confirmation: bool, // New field for exit confirmation
}

impl eframe::App for JdkInstallerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Apply font size
        let mut style = (*ctx.style()).clone();
        for (_text_style, font_id) in style.text_styles.iter_mut() {
            font_id.size = self.font_size;
        }
        
        // --- START: Aesthetic improvements ---
        let mut visuals = egui::Visuals::dark(); // Start with a dark theme
        visuals.widgets.noninteractive.bg_fill = egui::Color32::from_rgb(30, 30, 30); // Darker background for panels
        visuals.widgets.inactive.bg_fill = egui::Color32::from_rgb(40, 40, 40); // Slightly brighter for inactive widgets
        visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(60, 60, 60); // Hovered buttons
        visuals.widgets.active.bg_fill = egui::Color32::from_rgb(80, 80, 80); // Active buttons

        // Accent color for selected items and progress bars
        let accent_color = egui::Color32::from_rgb(100, 149, 237); // Cornflower Blue
        visuals.selection.bg_fill = accent_color.linear_multiply(0.2); // Lighter background for selection
        visuals.selection.stroke = egui::Stroke::new(1.0, accent_color); // Border for selection - corrected: Convert Color32 to Stroke

        visuals.widgets.active.fg_stroke.color = egui::Color32::WHITE; // White text for active buttons
        visuals.widgets.hovered.fg_stroke.color = egui::Color32::WHITE; // White text for hovered buttons
        visuals.widgets.inactive.fg_stroke.color = egui::Color32::LIGHT_GRAY; // Light gray text for inactive buttons

        // Progress bar colors
        visuals.override_text_color = Some(egui::Color32::WHITE); // Default text color to white
        visuals.hyperlink_color = accent_color; // Hyperlinks (if any)

        // Rounded corners
        visuals.widgets.noninteractive.rounding = egui::Rounding::same(5.0);
        visuals.widgets.inactive.rounding = egui::Rounding::same(5.0);
        visuals.widgets.hovered.rounding = egui::Rounding::same(5.0);
        visuals.widgets.active.rounding = egui::Rounding::same(5.0);
        visuals.window_rounding = egui::Rounding::same(8.0);

        // Shadows - corrected: Use direct Shadow constructor and add 'spread' field
        visuals.window_shadow = egui::Shadow {
            offset: egui::Vec2::new(1.0, 1.0),
            blur: 5.0,
            spread: 1.0, // Added missing 'spread' field
            color: egui::Color32::from_black_alpha(150),
        };
        visuals.popup_shadow = egui::Shadow {
            offset: egui::Vec2::new(1.0, 1.0),
            blur: 5.0,
            spread: 1.0, // Added missing 'spread' field
            color: egui::Color32::from_black_alpha(150),
        };

        style.visuals = visuals;
        // --- END: Aesthetic improvements ---

        ctx.set_style(style); 

        // Top panel for main application title
        egui::TopBottomPanel::top("top_panel").show(ctx, |ui| {
            ui.add_space(5.0);
            egui::menu::bar(ui, |ui| {
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.heading("Multi-Language Installer"); // Updated title
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Exit").clicked() {
                        self.show_exit_confirmation = true;
                    }
                });
            });
            ui.add_space(5.0);
        });

        // Side panel for language selection (vertical tabs)
        egui::SidePanel::left("side_panel").resizable(true).show(ctx, |ui| {
            ui.vertical_centered_justified(|ui| {
                ui.add_space(10.0);
                ui.heading("Installation Options");
                ui.add_space(10.0);
            });

            ui.separator();
            ui.add_space(10.0);

            // Language selection buttons
            ui.vertical(|ui| {
                ui.selectable_value(&mut self.selected_vendor, "azul".to_owned(), "Java (Azul Zulu)");
                ui.selectable_value(&mut self.selected_vendor, "temurin".to_owned(), "Java (Temurin)");
                ui.selectable_value(&mut self.selected_vendor, "openjdk".to_owned(), "Java (OpenJDK)");
                ui.selectable_value(&mut self.selected_vendor, "python".to_owned(), "Python");
                ui.selectable_value(&mut self.selected_vendor, "c_cpp".to_owned(), "C/C++ (MinGW-w64)");
                ui.selectable_value(&mut self.selected_vendor, "rust".to_owned(), "Rust");
                ui.selectable_value(&mut self.selected_vendor, "nodejs".to_owned(), "Node.js (LTS)");
                ui.selectable_value(&mut self.selected_vendor, "go".to_owned(), "Go");
            });

            ui.add_space(20.0);
            ui.add(egui::Slider::new(&mut self.font_size, 10.0..=24.0).text("Font Size"));
            ui.add_space(10.0);
        });

        // Central panel for selected language's configuration, status, and output log
        egui::CentralPanel::default().show(ctx, |ui| {
            let current_config = self.language_configs.get_mut(&self.selected_vendor).unwrap();
            let current_state = self.language_states.get_mut(&self.selected_vendor).unwrap();

            // Defensively reset is_installing if it got stuck true after an installation attempt.
            if current_state.install_result.is_some() && current_state.is_installing {
                current_state.is_installing = false;
            }

            ui.vertical(|ui| {
                ui.add_space(10.0);
                ui.heading(format!("{} Configuration", match self.selected_vendor.as_str() {
                    "azul" => "Java (Azul Zulu)",
                    "temurin" => "Java (Temurin)",
                    "openjdk" => "Java (OpenJDK)",
                    "python" => "Python",
                    "c_cpp" => "C/C++",
                    "rust" => "Rust",
                    "nodejs" => "Node.js",
                    "go" => "Go",
                    _ => "Unknown Language",
                }));
                ui.add_space(10.0);

                // Only Java and Python allow version input.
                if self.selected_vendor == "python" || self.selected_vendor.starts_with("java") {
                    ui.checkbox(&mut current_config.install_latest, "Install Latest Version");
                    ui.add_enabled_ui(!current_config.install_latest, |ui| {
                        ui.label("Version:");
                        ui.text_edit_singleline(&mut current_config.version);
                    });
                } else {
                    // For C/C++, Rust, Node.js, Go, do not provide version selection via text input.
                    ui.label("Version:");
                    ui.add_enabled(false, egui::TextEdit::singleline(&mut current_config.version).hint_text("Latest supported version"));
                    ui.label(format!("(This installer attempts to install the latest supported {} version.)", match self.selected_vendor.as_str() {
                        "c_cpp" => "MinGW-w64",
                        "rust" => "Rust (stable)",
                        "nodejs" => "Node.js (LTS)",
                        "go" => "Go",
                        _ => "",
                    }));
                    current_config.install_latest = true; // Ensure this is always true in these cases.
                }


                // Python specific options
                if self.selected_vendor == "python" {
                    ui.add_space(10.0);
                    ui.label("Python Libraries (e.g., 'numpy==1.20.0, pandas>=1.3.0'):");
                    ui.text_edit_singleline(&mut current_config.python_libraries_input);
                }

                ui.add_space(20.0);

                ui.with_layout(egui::Layout::bottom_up(egui::Align::Center), |ui| {
                    if ui.add_enabled(!current_state.is_installing, egui::Button::new("Install")).clicked() {
                        *current_state.output_log.lock().expect("Failed to acquire log mutex to clear log") = String::new(); // Corrected line
                        current_state.is_installing = true;
                        current_state.install_result = None;
                        current_state.download_progress = 0.0;
                        current_state.extract_progress = 0.0;
                        current_state.current_status = "Starting installation process...".to_string();
                        current_state.cancel_requested.store(false, Ordering::SeqCst);

                        let vendor_clone = self.selected_vendor.clone();
                        let version_clone = current_config.version.clone();
                        let install_latest_clone = current_config.install_latest;
                        let python_libraries_clone = current_config.python_libraries_input.clone();
                        let output_log_clone = current_state.output_log.clone();
                        let ctx_clone = ctx.clone();
                        let app_state_id_clone = egui::Id::new("JdkInstallerAppState"); // Still use one global ID for app state
                        let cancel_requested_clone = current_state.cancel_requested.clone();

                        std::thread::spawn(move || {
                            let result = run_installation_logic(
                                &vendor_clone,
                                &version_clone,
                                install_latest_clone,
                                &python_libraries_clone,
                                output_log_clone.clone(), // Pass Arc<Mutex<String>> directly
                                ctx_clone.clone(),
                                app_state_id_clone,
                                cancel_requested_clone,
                            );
                            
                            if let Some(app_state_arc) = ctx_clone.data(|d| d.get_temp::<Arc<Mutex<JdkInstallerApp>>>(app_state_id_clone)) {
                                let mut app_state = app_state_arc.lock().expect("Failed to acquire app state mutex in spawned thread");
                                if let Some(lang_state) = app_state.language_states.get_mut(&vendor_clone) {
                                    lang_state.is_installing = false;
                                    // Also push error to log if there was one.
                                    if let Err(ref e) = result {
                                        let mut log = lang_state.output_log.lock().expect("Failed to acquire log mutex to append error");
                                        log.push_str(&format!("ERROR: {}\n", e));
                                    }
                                    lang_state.install_result = Some(result);
                                    if lang_state.install_result.as_ref().expect("Install result should be Some here.").is_ok() {
                                        lang_state.current_status = "Installation complete!".to_string();
                                    } else {
                                        lang_state.current_status = "Installation failed.".to_string();
                                    }
                                }
                            }
                            ctx_clone.request_repaint(); 
                        });
                    }
                });

                ui.add_space(10.0);
                ui.heading("Current Status");
                ui.add_space(5.0);

                if current_state.is_installing {
                    ui.label(&current_state.current_status);
                    ui.add_space(5.0);
                    ui.add(egui::ProgressBar::new(current_state.download_progress).show_percentage().text("Downloading..."));
                    ui.add_space(5.0);
                    ui.add(egui::ProgressBar::new(current_state.extract_progress).show_percentage().text("Extracting..."));
                    
                    ui.add_space(10.0);
                    if ui.button("Cancel Installation").clicked() {
                        self.show_cancel_confirmation = true;
                    }

                } else if let Some(result) = &current_state.install_result {
                    match result {
                        Ok(_) => ui.label("Installation Complete!"),
                        Err(e) => ui.colored_label(egui::Color32::RED, format!("Installation Failed: {}", e)),
                    };
                }

                ui.add_space(10.0);
                ui.separator();

                // Conditional display of Python specific details vs general log
                if self.selected_vendor == "python" {
                    ui.add_space(10.0);
                    ui.heading("Python Details");
                    ui.add_space(5.0);

                    ui.horizontal(|ui| {
                        // Left column: Python Version
                        ui.with_layout(egui::Layout::top_down(egui::Align::LEFT).with_main_wrap(true), |ui| {
                            ui.set_width(ui.available_width() / 2.0 - 5.0);
                            ui.heading("Python Version");
                            ui.add_space(5.0);
                            egui::ScrollArea::vertical().id_source("python_version_scroll_area").stick_to_bottom(true).show(ui, |ui| {
                                let log_content = current_state.output_log.lock().expect("Failed to acquire log mutex for Python version display");
                                let filtered_log: String = log_content.lines()
                                    .filter(|line| {
                                        line.contains("Python") ||
                                        line.contains("PYTHON_HOME") ||
                                        line.contains("Checking version") ||
                                        line.contains("Installed Python version") ||
                                        line.contains("Python version mismatch") ||
                                        line.contains("pip installer") ||
                                        line.contains("get-pip.py") ||
                                        line.contains("Installing pip")
                                    })
                                    .collect::<Vec<&str>>()
                                    .join("\n");
                                ui.monospace(filtered_log);
                            });
                        });

                        ui.separator(); // Vertical separator

                        // Right column: Library Compatibility
                        ui.with_layout(egui::Layout::top_down(egui::Align::LEFT).with_main_wrap(true), |ui| {
                            ui.set_width(ui.available_width());
                            ui.heading("Library Compatibility");
                            ui.add_space(5.0);
                            egui::ScrollArea::vertical().id_source("library_compatibility_scroll_area").stick_to_bottom(true).show(ui, |ui| {
                                let log_content = current_state.output_log.lock().expect("Failed to acquire log mutex for library compatibility display");
                                let filtered_log: String = log_content.lines()
                                    .filter(|line| {
                                        line.contains("pip") ||
                                        line.contains("library") ||
                                        line.contains("Attempting to install") ||
                                        line.contains("Successfully installed") ||
                                        line.contains("Failed to install") ||
                                        line.contains("Checking library compatibility") ||
                                        line.contains("Installed version of")
                                    })
                                    .collect::<Vec<&str>>()
                                    .join("\n");
                                ui.monospace(filtered_log);
                            });
                        });
                    });
                    ui.add_space(10.0);
                    ui.separator();
                    ui.add_space(10.0);
                    ui.heading("Full Output Log (Python related only)");
                    ui.add_space(5.0);
                    egui::ScrollArea::vertical().stick_to_bottom(true).show(ui, |ui| {
                        let log_content = current_state.output_log.lock().expect("Failed to acquire log mutex for full Python log");
                        ui.monospace(&*log_content); // Display full log for Python, already filtered by vendor context
                    });
                } else {
                    // General log for other vendors
                    ui.add_space(10.0);
                    ui.heading(format!("Detailed Output Log ({})", match self.selected_vendor.as_str() {
                        "azul" => "Java (Azul Zulu)",
                        "temurin" => "Java (Temurin)",
                        "openjdk" => "Java (OpenJDK)",
                        "c_cpp" => "C/C++",
                        "rust" => "Rust",
                        "nodejs" => "Node.js",
                        "go" => "Go",
                        _ => "Unknown",
                    }));
                    ui.add_space(5.0);
                    egui::ScrollArea::vertical().stick_to_bottom(true).show(ui, |ui| {
                        let log_content = current_state.output_log.lock().expect("Failed to acquire log mutex for general log");
                        ui.monospace(&*log_content);
                    });
                }
            });
        });

        // Show cancel confirmation dialog (if requested)
        if self.show_cancel_confirmation {
            egui::Window::new("Cancel Confirmation")
                .collapsible(false)
                .resizable(false)
                .auto_sized()
                .show(ctx, |ui| {
                    ui.label("Are you sure you want to stop the installation?");
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        if ui.button("Yes, stop").clicked() {
                            let current_state = self.language_states.get_mut(&self.selected_vendor).expect("Failed to get language state for cancellation");
                            current_state.cancel_requested.store(true, Ordering::SeqCst);
                            current_state.is_installing = false;
                            current_state.install_result = Some(Err("Installation cancelled.".to_string()));
                            *current_state.output_log.lock().expect("Failed to acquire log mutex to clear cancel log") = String::new(); // Corrected line
                            current_state.download_progress = 0.0;
                            current_state.extract_progress = 0.0;
                            current_state.current_status = "Installation cancelled.".to_string();
                            self.show_cancel_confirmation = false;
                        }
                        if ui.button("No, continue").clicked() {
                            self.show_cancel_confirmation = false;
                        }
                    });
                });
        }

        // Show exit confirmation dialog (if requested)
        if self.show_exit_confirmation {
            egui::Window::new("Exit Confirmation")
                .collapsible(false)
                .resizable(false)
                .auto_sized()
                .show(ctx, |ui| {
                    ui.label("Are you sure you want to exit the application?");
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        if ui.button("Yes, exit").clicked() {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close); // Corrected line for exiting application
                        }
                        if ui.button("No, stay").clicked() {
                            self.show_exit_confirmation = false;
                        }
                    });
                });
        }
    }
}

impl JdkInstallerApp {
    fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        let mut language_configs = HashMap::new();
        let mut language_states = HashMap::new();

        // Initialize configs and states for all supported languages
        let vendors = vec![
            "azul", "temurin", "openjdk", "python", "c_cpp", "rust", "nodejs", "go"
        ];

        for vendor in vendors {
            let mut config = LanguageConfig {
                vendor: vendor.to_owned(),
                ..Default::default()
            };
            // Set default version based on vendor
            match vendor {
                "azul" | "temurin" | "openjdk" => config.version = "21".to_owned(),
                "python" => config.version = "3.12.4".to_owned(),
                "c_cpp" => {
                    config.version = "".to_owned(); // No specific version input for C/C++
                    config.install_latest = true; // Always install the fixed latest supported version
                },
                "rust" => {
                    config.version = "".to_owned(); // No specific version input for Rust
                    config.install_latest = true; // Always install latest stable via rustup
                },
                "nodejs" => {
                    config.version = "".to_owned(); // No specific version input for Node.js
                    config.install_latest = true; // Always install latest LTS
                },
                "go" => {
                    config.version = "".to_owned(); // No specific version input for Go
                    config.install_latest = true; // Always install latest stable
                },
                _ => {},
            }
            language_configs.insert(vendor.to_owned(), config);
            language_states.insert(vendor.to_owned(), LanguageState::default());
        }

        Self {
            language_configs,
            language_states,
            selected_vendor: "azul".to_owned(), // Default selected tab
            font_size: 16.0,
            show_cancel_confirmation: false,
            show_exit_confirmation: false,
        }
    }
}

fn main() {
    let native_options = eframe::NativeOptions::default();
    eframe::run_native(
        "Multi-Language Installer", // Updated window title
        native_options,
        Box::new(|cc| {
            let app = Arc::new(Mutex::new(JdkInstallerApp::new(cc)));
            // Store the Arc<Mutex<JdkInstallerApp>> in egui's data store.
            cc.egui_ctx.data_mut(|d| d.insert_temp(egui::Id::new("JdkInstallerAppState"), app.clone()));
            Ok(Box::new(JdkInstallerApp::new(cc)))
        }),
    ).expect("eframe application failed to run");
}