//! Start with Windows: a Task Scheduler task that runs `fan-lite.exe --tray` at sign-in.
//!
//! FanLite needs administrator rights, and Windows doesn't start such apps from the Run key
//! or the Startup folder ("Startup apps"). A logon task with the highest privileges starts it
//! elevated without a UAC prompt, the way FanControl and HWiNFO do it. The task is created and
//! removed with System32's `schtasks.exe` from an XML definition, because its command-line
//! flags can't lift the 72-hour run limit or the battery rules.

use std::path::{Path, PathBuf};

use crate::update::{run, system_tool};

/// The task's name in Task Scheduler.
pub const TASK: &str = "FanLite";

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// The task definition: at `user`'s sign-in (after 10 s, so USB devices and drivers are up),
/// run `exe --tray` with the highest privileges on their desktop, with no time limit, also
/// on battery, at normal priority (tasks default to below normal).
pub fn task_xml(exe: &Path, user: &str) -> String {
    let exe_s = escape(&exe.to_string_lossy());
    let dir = escape(
        &exe.parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
    );
    let user = escape(user);
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>Starts FanLite in the notification area at sign-in.</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{user}</UserId>
      <Delay>PT10S</Delay>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>HighestAvailable</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>false</AllowHardTerminate>
    <StartWhenAvailable>false</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>4</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{exe_s}</Command>
      <Arguments>--tray</Arguments>
      <WorkingDirectory>{dir}</WorkingDirectory>
    </Exec>
  </Actions>
</Task>
"#
    )
}

/// The exe a task definition (as `schtasks /query /xml` prints it) starts.
pub fn task_command(xml: &str) -> Option<PathBuf> {
    let start = xml.find("<Command>")? + "<Command>".len();
    let end = start + xml[start..].find("</Command>")?;
    let cmd = unescape(xml[start..end].trim());
    Some(PathBuf::from(cmd.trim_matches('"')))
}

/// `schtasks` output as text: it writes UTF-16 for `/xml`, the console code page otherwise.
fn text(out: &[u8]) -> String {
    let utf16 = out.len() >= 2
        && out.len().is_multiple_of(2)
        && out.iter().skip(1).step_by(2).filter(|&&b| b == 0).count() * 2 > out.len() / 2;
    if utf16 {
        let w: Vec<u16> = out.as_chunks::<2>().0.iter().map(|&c| u16::from_le_bytes(c)).collect();
        String::from_utf16_lossy(&w).trim_start_matches('\u{feff}').to_owned()
    } else {
        String::from_utf8_lossy(out).into_owned()
    }
}

/// The exe the task starts, or `None` if there is no task.
pub fn installed() -> Option<PathBuf> {
    let out = run(system_tool("schtasks").args(["/query", "/tn", TASK, "/xml"])).ok()?;
    task_command(&text(&out))
}

/// Create (or replace) the task for `exe`.
pub fn enable(exe: &Path) -> Result<(), String> {
    let user = match (std::env::var("USERDOMAIN"), std::env::var("USERNAME")) {
        (Ok(d), Ok(u)) => format!("{d}\\{u}"),
        (_, Ok(u)) => u,
        _ => return Err("can't tell which user to start FanLite for".into()),
    };
    let xml = task_xml(exe, &user);
    let path = std::env::temp_dir().join("fan-lite-task.xml");
    // UTF-16 with a BOM, as the XML declaration says.
    let mut bytes = vec![0xFF, 0xFE];
    bytes.extend(xml.encode_utf16().flat_map(u16::to_le_bytes));
    std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
    let r = run(system_tool("schtasks")
        .args(["/create", "/tn", TASK, "/f", "/xml"])
        .arg(&path));
    let _ = std::fs::remove_file(&path);
    r.map(|_| ())
}

/// Remove the task (no error if there is none).
pub fn disable() -> Result<(), String> {
    if installed().is_none() {
        return Ok(());
    }
    run(system_tool("schtasks").args(["/delete", "/tn", TASK, "/f"])).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_definition() {
        let exe = Path::new(r"C:\Tools\Fan & Co\fan-lite.exe");
        let xml = task_xml(exe, r"PC\Matias");
        for want in [
            "<LogonTrigger>",
            "<UserId>PC\\Matias</UserId>",
            "<Delay>PT10S</Delay>",
            "<RunLevel>HighestAvailable</RunLevel>",
            "<LogonType>InteractiveToken</LogonType>",
            "<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>",
            "<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>",
            "<StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>",
            "<AllowHardTerminate>false</AllowHardTerminate>",
            "<Priority>4</Priority>",
            "<Arguments>--tray</Arguments>",
            r"<Command>C:\Tools\Fan &amp; Co\fan-lite.exe</Command>",
            r"<WorkingDirectory>C:\Tools\Fan &amp; Co</WorkingDirectory>",
        ] {
            assert!(xml.contains(want), "missing {want}");
        }
        assert_eq!(task_command(&xml).as_deref(), Some(exe));
    }

    #[test]
    fn command_from_query_output() {
        let xml = "<Exec>\r\n  <Command>\"C:\\A B\\fan-lite.exe\"</Command>\r\n</Exec>";
        assert_eq!(task_command(xml), Some(PathBuf::from(r"C:\A B\fan-lite.exe")));
        assert_eq!(task_command("<Task/>"), None);
        // schtasks /xml writes UTF-16.
        let wide: Vec<u8> = "\u{feff}<Command>x.exe</Command>"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        assert_eq!(task_command(&text(&wide)), Some(PathBuf::from("x.exe")));
        assert_eq!(text(b"plain"), "plain");
    }
}

/// Creates and removes a real task (needs an elevated shell):
/// `cargo test --lib autostart_live -- --ignored --nocapture`
#[cfg(test)]
mod live {
    #[test]
    #[ignore]
    fn autostart_live() {
        if !crate::hw::win::is_elevated() {
            println!("skipped: needs an elevated shell");
            return;
        }
        let exe = std::path::Path::new(r"C:\fan-lite-test\fan-lite.exe");
        println!("enable: {:?}", super::enable(exe));
        println!("installed: {:?}", super::installed());
        println!("disable: {:?}", super::disable());
        println!("installed after: {:?}", super::installed());
    }
}
