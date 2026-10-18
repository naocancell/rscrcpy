//! Thin wrapper around the `adb` command line tool.

use anyhow::{Context, Result, bail};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Clone, Debug)]
pub struct Adb {
    program: PathBuf,
    serial: Option<String>,
}

/// One line of `adb devices -l`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceInfo {
    pub serial: String,
    /// `device`, `unauthorized`, `offline`, ...
    pub state: String,
    pub model: Option<String>,
    pub product: Option<String>,
}

impl DeviceInfo {
    pub fn is_ready(&self) -> bool {
        self.state == "device"
    }

    /// True for devices connected over the network (`ip:port` or mDNS serials).
    pub fn is_wireless(&self) -> bool {
        self.serial.contains(':') || self.serial.contains("._adb-tls-connect.")
    }

    pub fn display_name(&self) -> String {
        match &self.model {
            Some(m) => m.replace('_', " "),
            None => self.serial.clone(),
        }
    }
}

/// Finds adb: `$ADB`, next to the executable (also `platform-tools/`), the Android SDK,
/// then falls back to `adb` from `PATH`.
pub fn locate() -> PathBuf {
    if let Some(p) = std::env::var_os("ADB") {
        return p.into();
    }
    let name = if cfg!(windows) { "adb.exe" } else { "adb" };
    let mut dirs = Vec::new();
    if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
        dirs.push(dir.clone());
        dirs.push(dir.join("platform-tools"));
    }
    for var in ["ANDROID_HOME", "ANDROID_SDK_ROOT"] {
        if let Some(sdk) = std::env::var_os(var) {
            dirs.push(PathBuf::from(sdk).join("platform-tools"));
        }
    }
    dirs.into_iter().map(|d| d.join(name)).find(|p| p.is_file()).unwrap_or_else(|| name.into())
}

pub fn parse_devices(output: &str) -> Vec<DeviceInfo> {
    output
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("List of devices") && !l.starts_with('*'))
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let serial = parts.next()?.to_owned();
            let state = parts.next()?.to_owned();
            let mut info = DeviceInfo { serial, state, model: None, product: None };
            for kv in parts {
                if let Some((k, v)) = kv.split_once(':') {
                    match k {
                        "model" => info.model = Some(v.to_owned()),
                        "product" => info.product = Some(v.to_owned()),
                        _ => {}
                    }
                }
            }
            Some(info)
        })
        .collect()
}

impl Adb {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self { program: program.into(), serial: None }
    }

    pub fn with_serial(&self, serial: Option<&str>) -> Self {
        Self { program: self.program.clone(), serial: serial.map(str::to_owned) }
    }

    pub fn program(&self) -> &Path {
        &self.program
    }

    pub fn command(&self) -> Command {
        let mut c = Command::new(&self.program);
        if let Some(s) = &self.serial {
            c.args(["-s", s]);
        }
        c.stdin(Stdio::null());
        #[cfg(windows)]
        {
            // Do not flash a console window for every adb call from the GUI.
            use std::os::windows::process::CommandExt;
            c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        c
    }

    /// Runs adb and returns its stdout, or an error containing its stderr.
    pub fn run<S: AsRef<OsStr>>(&self, args: &[S]) -> Result<String> {
        let out = self.command().args(args).output().with_context(|| {
            format!(
                "无法运行 adb（{}）。请安装 Android platform-tools 并加入 PATH，或设置 ADB 环境变量",
                self.program.display()
            )
        })?;
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
            let args: Vec<_> = args.iter().map(|a| a.as_ref().to_string_lossy()).collect();
            bail!("adb {} 失败: {}", args.join(" "), if stderr.is_empty() { stdout } else { stderr });
        }
        Ok(stdout)
    }

    pub fn devices(&self) -> Result<Vec<DeviceInfo>> {
        Ok(parse_devices(&self.run(&["devices", "-l"])?))
    }

    /// `adb connect HOST[:PORT]` (adb exits with 0 even on failure, so check the text).
    pub fn connect(&self, addr: &str) -> Result<String> {
        let out = self.run(&["connect", addr])?;
        if out.contains("connected to") && !out.contains("failed") && !out.contains("cannot") {
            Ok(out)
        } else {
            bail!("{out}")
        }
    }

    /// `adb pair HOST:PORT CODE` (Android 11+ wireless debugging).
    pub fn pair(&self, addr: &str, code: &str) -> Result<String> {
        let out = self.run(&["pair", addr, code])?;
        if out.contains("Successfully paired") { Ok(out) } else { bail!("{out}") }
    }

    pub fn disconnect(&self, addr: &str) -> Result<String> {
        self.run(&["disconnect", addr])
    }

    pub fn install(&self, apk: &Path) -> Result<String> {
        let out = self.run(&[OsStr::new("install"), OsStr::new("-r"), apk.as_os_str()])?;
        if out.contains("Success") { Ok(out) } else { bail!("{out}") }
    }

    pub fn push(&self, local: &Path, remote: &str) -> Result<String> {
        self.run(&[OsStr::new("push"), local.as_os_str(), OsStr::new(remote)])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn devices_output() {
        let out = "* daemon not running; starting now at tcp:5037\n\
                   * daemon started successfully\n\
                   List of devices attached\n\
                   R58M12ABCDE            device usb:1-1 product:beyond1qltezc model:SM_G9730 device:beyond1q transport_id:1\n\
                   192.168.1.8:5555       device product:cepheus model:MI_9 device:cepheus transport_id:3\n\
                   0123456789             unauthorized usb:1-2 transport_id:2\n\n";
        let d = parse_devices(out);
        assert_eq!(d.len(), 3);
        assert_eq!(d[0].serial, "R58M12ABCDE");
        assert_eq!(d[0].display_name(), "SM G9730");
        assert!(d[0].is_ready() && !d[0].is_wireless());
        assert!(d[1].is_wireless());
        assert_eq!(d[2].state, "unauthorized");
        assert_eq!(d[2].model, None);
        assert_eq!(d[2].display_name(), "0123456789");
    }
}
