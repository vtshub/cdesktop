use crate::{command_ext::NoWindowExt, is_wsl2};

/// Open URL in browser with WSL2 support
pub async fn open_browser(url: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if is_wsl2() {
        // In WSL2, use PowerShell to open the browser
        tokio::process::Command::new("powershell.exe")
            .arg("-Command")
            .arg(format!("Start-Process '{url}'"))
            .no_window()
            .spawn()?;
        Ok(())
    } else {
        // Use the standard open crate for other platforms
        open::that(url).map_err(|e| e.into())
    }
}

/// Extensions that run code when "opened"; a click on such a path shows it in its folder instead.
const RUNNABLE_EXTENSIONS: &[&str] = &[
    "exe", "com", "bat", "cmd", "ps1", "psm1", "vbs", "vbe", "js", "jse", "wsf", "wsh", "msi", "msp",
    "scr", "lnk", "url", "hta", "cpl", "reg", "jar", "pif", "appref-ms", "sh", "command", "app",
];

/// True when opening this file with its default program could run code.
pub fn is_runnable_path(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| RUNNABLE_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// Show a file or folder in the system file manager (the file selected where the platform allows it).
pub async fn reveal_path(
    path: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if path.is_dir() {
        return open::that(path).map_err(|e| e.into());
    }
    #[cfg(windows)]
    {
        tokio::process::Command::new("explorer.exe")
            .raw_arg(format!("/select,\"{}\"", path.display()))
            .spawn()?;
        Ok(())
    }
    #[cfg(target_os = "macos")]
    {
        tokio::process::Command::new("open").arg("-R").arg(path).spawn()?;
        Ok(())
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let dir = path.parent().unwrap_or(path);
        open::that(dir).map_err(|e| e.into())
    }
}

/// Open a file with its default program, or a folder in the file manager. Files that could run code are only shown.
pub async fn open_path(
    path: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if path.is_dir() || is_runnable_path(path) {
        return reveal_path(path).await;
    }
    open::that(path).map_err(|e| e.into())
}
