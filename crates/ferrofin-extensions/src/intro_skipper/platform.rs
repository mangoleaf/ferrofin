//! The platform the plugin's support bundle names (`Helper/OperatingSystem.cs`).

/// .NET's `RuntimeInformation.OSDescription` on Linux: the kernel's
/// `uname -srv`.
fn os_description() -> String {
    let read = |name: &str| {
        std::fs::read_to_string(format!("/proc/sys/kernel/{name}"))
            .map(|value| value.trim().to_owned())
            .unwrap_or_default()
    };
    format!(
        "{} {} {}",
        read("ostype"),
        read("osrelease"),
        read("version")
    )
    .trim()
    .to_owned()
}

/// `Helper.OperatingSystem.DetermineOperatingSystem`.
pub(super) fn operating_system() -> String {
    match std::env::consts::OS {
        "windows" => "Windows".to_owned(),
        "macos" => "macOS".to_owned(),
        "linux" => {
            let docker = ["/.dockerenv", "/run/.containerenv"]
                .iter()
                .any(|marker| std::path::Path::new(marker).exists());
            if !docker {
                return os_description();
            }
            if std::env::var_os("ATTACHED_DEVICES_PERMS").is_some() {
                "LinuxServer.io image (Docker)".to_owned()
            } else if std::env::var_os("WEBUI_PORTS").is_some() {
                "hotio image (Docker)".to_owned()
            } else {
                "Linux (Docker)".to_owned()
            }
        }
        _ => "Unknown".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    /// On Linux: the kernel's `uname -srv` outside a container, one of the
    /// plugin's image names inside one.
    #[cfg(target_os = "linux")]
    #[test]
    fn linux_names_the_kernel_or_the_container_image() {
        let os = super::operating_system();
        assert!(super::os_description().starts_with("Linux "));
        assert!(
            os == super::os_description() || os.ends_with(" (Docker)"),
            "{os}"
        );
    }
}
